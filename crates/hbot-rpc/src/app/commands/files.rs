//! 文件与搜索命令：`file_*`、`search_*`、`diagnostics`。
//!
//! 全部经由 [`hbot_workspace::Workspace`]——**不允许**在这里直接 `std::fs`：路径守卫
//! （`docs/02-architecture.md` §11.1）只在 workspace 里有一份，绕过它就等于绕过守卫。
//!
//! ## 几条约定
//!
//! * **阻塞调用一律走 [`AppState::blocking`]**：`Workspace` 与 `hbot_code` 都是同步
//!   API，在 `async fn` 里直接调用会占住 tokio worker（`crates/hbot-store/src/lib.rs`
//!   的调用约定）。搜索与整项目编译尤其要这样——它们是这条链路上最慢的两件事。
//! * **错误只从 [`super::failed`] 出去**：`HbotError` → `ApiError` 的映射只在一处，
//!   这样 `-32001/-32002/-32003` 的语义不会在命令层被改写成别的码。
//! * **返回形状只按 `crate::dto` 的表**（`crates/hbot-rpc/src/dto/mod.rs` 顶部）：
//!   一次性快照类命令返回 `data`，流式结果走事件；不要给同一条信息造两条通道。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hbot_core::{HbotError, SearchId};
use hbot_workspace::{
    DEFAULT_MAX_RESULTS, FileContent as WorkspaceFile, MAX_READ_BYTES, MAX_SEARCH_FILE_BYTES,
    SearchHit, SearchOptions, Workspace,
};

use super::{done, failed, unimplemented};
use crate::app::AppState;
use crate::dto::{
    Command, Diagnostic, DiagnosticSeverity, DirEntry, Event, FileChangeKind, FileContent,
    Response, SearchMatch,
};

/// 成功并携带 `data`；序列化失败只可能是**本仓库的 DTO 有 bug**，按 `internal` 报。
fn ok_data<T: serde::Serialize>(value: &T) -> Response {
    match serde_json::to_value(value) {
        Ok(data) => Response::with_data(data),
        Err(error) => failed(HbotError::internal(format!("序列化命令结果失败：{error}"))),
    }
}

/// [`crate::app::files::ActiveFiles`] 的键：统一成 UI 相对路径（`/` 分隔、去掉开头的
/// `./`），让 `read("./a.txt")` 与 `write("a.txt", …)` 落到同一条记录上。
fn active_key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    match normalized.strip_prefix("./") {
        Some(rest) => rest.to_owned(),
        None => normalized,
    }
}

/// 把一次 `Workspace` 调用挪到阻塞线程池，并把 `Arc<Workspace>` 的所有权传进去。
///
/// 只包"取数据 → 换成 DTO"这种一句话的读命令；写命令要在成功后顺带更新
/// `ActiveFiles` / 发事件，各自写清楚更不容易漏。
async fn read_via_workspace<T, F>(state: &AppState, workspace: Arc<Workspace>, f: F) -> Response
where
    T: serde::Serialize + Send + 'static,
    F: FnOnce(&Workspace) -> Result<T, HbotError> + Send + 'static,
{
    match state.blocking(move || f(&workspace)).await {
        Ok(value) => ok_data(&value),
        Err(error) => failed(error),
    }
}

/// `file_read`。
pub async fn read(state: &AppState, path: String) -> Response {
    let workspace = match state.workspace() {
        Ok(workspace) => workspace,
        Err(error) => return failed(error),
    };
    let rel = PathBuf::from(&path);
    match state
        .blocking(move || workspace.read_file(&rel, MAX_READ_BYTES))
        .await
    {
        Ok(content) => {
            // 记住读到的 hash：UI 之后**不带** `baseHash` 的写回也要能发现外部改动
            // （`AGENTS.md` §0.10：禁止静默覆盖）。
            state
                .files_mut()
                .remember(active_key(&path), content.hash.clone());
            ok_data(&FileContent {
                content: content.content,
                hash: content.hash,
            })
        }
        Err(error) => failed(error),
    }
}

/// `file_write`。
///
/// `base_hash` 缺省时用「上次读到的 hash」做期望值：调用方忘了带 hash 不等于可以
/// 静默覆盖磁盘上的新内容。两者都没有（从没读过这个文件）才走无校验写入，那是
/// "新建 / 明知故写"的显式选择。
pub async fn write(
    state: &AppState,
    path: String,
    content: String,
    base_hash: Option<String>,
) -> Response {
    let workspace = match state.workspace() {
        Ok(workspace) => workspace,
        Err(error) => return failed(error),
    };
    let rel = PathBuf::from(&path);
    let expected =
        base_hash.or_else(|| state.files().hash_of(&active_key(&path)).map(str::to_owned));
    match state
        .blocking(move || workspace.write_file(&rel, &content, expected.as_deref()))
        .await
    {
        Ok(written) => {
            state
                .files_mut()
                .remember(active_key(&path), written.hash.clone());
            // 没有事件：写入结果（新内容 + 新 hash）已经由 `data` 返回，同一份信息
            // 不再推一遍 `project.changed`（否则刷新会成对出现）。
            ok_data(&FileContent {
                content: written.content,
                hash: written.hash,
            })
        }
        Err(error) => failed(error),
    }
}

/// `file_list`。
pub async fn list(state: &AppState, path: String) -> Response {
    let workspace = match state.workspace() {
        Ok(workspace) => workspace,
        Err(error) => return failed(error),
    };
    let rel = PathBuf::from(&path);
    read_via_workspace(state, workspace, move |workspace| {
        // 显式映射成 [`DirEntry`]：`DirEntryInfo` 还带 `size`，直接序列化会多出一个
        // 契约里没有的字段（`dto/mod.rs` 的表：FileList → `Vec<DirEntry>`）。
        let entries = workspace.list_dir(&rel)?;
        Ok(entries
            .into_iter()
            .map(|entry| DirEntry {
                name: entry.name,
                path: entry.path,
                is_dir: entry.is_dir,
            })
            .collect::<Vec<DirEntry>>())
    })
    .await
}

/// `file_create`。
///
/// **绝不覆盖**：目标已存在就是 `-32003`（`AGENTS.md` §0.10）。实现见 [`create_new`]。
pub async fn create(state: &AppState, path: String, content: String) -> Response {
    let workspace = match state.workspace() {
        Ok(workspace) => workspace,
        Err(error) => return failed(error),
    };
    let rel = PathBuf::from(&path);
    let display = path.clone();
    match state
        .blocking(move || create_new(&workspace, &rel, &content, &display))
        .await
    {
        Ok(created) => {
            state
                .files_mut()
                .remember(active_key(&path), created.hash.clone());
            ok_data(&FileContent {
                content: created.content,
                hash: created.hash,
            })
        }
        Err(error) => failed(error),
    }
}

/// 写入一个**必须不存在**的文件。
///
/// `Workspace` 没有原子的 create-if-absent：`write_file` 的 `expected_hash` 只能表达
/// "磁盘内容仍等于这个 hash"，表达不了"必须不存在"（文件不存在时会直接 `Conflict`，
/// 方向正好相反）。所以这里先用 [`Workspace::resolve_existing`] 做存在性探测——它走
/// 的是同一条路径守卫 + `canonicalize`，不读内容——再写。
///
/// **已知窗口**：探测与写入之间若有人抢先建了同名文件，这里是"后写覆盖先建"。
/// 缓解是 `write_file` 内部串行化写（`lock_writes`）且 M0 只有一个 UI 连接按序发命令；
/// 真正的原子语义要等 `hbot-workspace` 补 `create_new`（应报 `Conflict`）。记在实现
/// 报告里，不在这里用 `std::fs` 自己补——那会绕过路径守卫。
fn create_new(
    workspace: &Workspace,
    rel: &Path,
    content: &str,
    display: &str,
) -> Result<WorkspaceFile, HbotError> {
    match workspace.resolve_existing(rel) {
        Ok(_) => {
            return Err(HbotError::conflict(format!(
                "`{display}` 已存在：file_create 不会覆盖（改内容请用 file_write）"
            )));
        }
        // 不存在才是正经的 create。
        Err(HbotError::NotFound { .. }) => {}
        Err(other) => return Err(other),
    }
    workspace.write_file(rel, content, None)
}

/// `file_rename`。
pub async fn rename(state: &AppState, from: String, to: String) -> Response {
    let workspace = match state.workspace() {
        Ok(workspace) => workspace,
        Err(error) => return failed(error),
    };
    let from_rel = PathBuf::from(&from);
    let to_rel = PathBuf::from(&to);
    match state
        .blocking(move || workspace.rename(&from_rel, &to_rel))
        .await
    {
        Ok(()) => {
            state.files_mut().forget(&active_key(&from));
            // `data` 是 `null`，结果经 `project.changed` 回 UI（`dto/mod.rs` 的表）；
            // 旧路径与新路径各发一次（[`FileChangeKind::Renamed`] 的约定）。
            for path in [from, to] {
                state.events().send(Event::ProjectChanged {
                    path: active_key(&path),
                    kind: FileChangeKind::Renamed,
                });
            }
            done()
        }
        Err(error) => failed(error),
    }
}

/// `file_delete`。
pub async fn delete(state: &AppState, path: String, to_trash: bool) -> Response {
    if !to_trash {
        // 待补：永久删除需要 `hbot_workspace` 提供 API（如 `delete_permanent`），这里不写
        // `std::fs::remove_file` 短路——路径守卫必须只有一份。
        //
        // 契约（`Command::FileDelete` 的 `toTrash`）说 `false` 是**永久删除**，但
        // `hbot-workspace` 只提供 [`Workspace::delete_to_trash`]——"项目文件不真删"
        // 是刻意的产品策略（见 `crates/hbot-workspace/src/lib.rs` 的模块文档：移入
        // `.hbot/trash`，不真删）。在这里用 `std::fs` 补一个永久删除就是绕过路径守卫
        // 与回收站，所以宁可不做：等 workspace 提供 `delete_permanent` 再接。
        // **不假装成功**——UI 必须知道这次没删任何东西。
        return unimplemented(
            "file_delete（toTrash=false 永久删除：hbot-workspace 只提供移入回收站）",
        );
    }
    let workspace = match state.workspace() {
        Ok(workspace) => workspace,
        Err(error) => return failed(error),
    };
    let key = active_key(&path);
    let rel = PathBuf::from(&path);
    match state
        .blocking(move || workspace.delete_to_trash(&rel))
        .await
    {
        Ok(_trashed) => {
            state.files_mut().forget(&key);
            state.events().send(Event::ProjectChanged {
                path: key,
                kind: FileChangeKind::Deleted,
            });
            done()
        }
        Err(error) => failed(error),
    }
}

/// `search_start`。
///
/// 同步搜索一次跑完，所以只发**一次**完整结果（`Event::FileSearch` 的文档允许
/// "可能多次"，一次也是它的合法实例）。取消走 [`crate::app::search::SearchRegistry`]：
/// 旗标置位后 `Workspace::search` 会在下一个检查点停下，此时**不推**任何匹配。
pub async fn search(state: &AppState, query: String, regex: bool, globs: Vec<String>) -> Response {
    let workspace = match state.workspace() {
        Ok(workspace) => workspace,
        Err(error) => return failed(error),
    };
    let search_id = state.ids().search_id();
    // 先登记再开跑：否则取消命令可能落在"还没登记"的空档里，白取消。
    let cancel = state.searches().begin(search_id);
    let options = SearchOptions {
        regex,
        case_sensitive: false,
        globs,
        max_results: DEFAULT_MAX_RESULTS,
        max_file_size: MAX_SEARCH_FILE_BYTES,
    };

    let outcome = state
        .blocking(move || {
            let mut hits: Vec<SearchHit> = Vec::new();
            let summary = workspace.search(&query, &options, cancel, |hit| hits.push(hit))?;
            Ok((hits, summary))
        })
        .await;
    // 不论成败都注销：否则被取消 / 出错的 ID 会永远留在表里。
    state.searches().finish(search_id);

    let (hits, summary) = match outcome {
        Ok(pair) => pair,
        Err(error) => return failed(error),
    };

    if summary.cancelled {
        tracing::debug!(
            search_id = %search_id,
            hits = summary.hits,
            "搜索被取消：不推送 file.search（取消后 UI 不该再收到这一路的匹配）"
        );
        return done();
    }

    let matches: Vec<SearchMatch> = hits
        .into_iter()
        .map(|hit| SearchMatch {
            path: hit.path,
            // `SearchHit::line_number` 是 `u64`，DTO 是 `u32`；溢出只可能是坏数据，
            // 饱和到 `u32::MAX` 而不是回绕。
            line: u32::try_from(hit.line_number).unwrap_or(u32::MAX),
            // 搜索层不产出列号（`SearchHit` 只有行文本），约定 1 = 行首。
            column: 1,
            text: hit.line,
        })
        .collect();

    // 截断 / 跳过计数在 DTO 里没有对应字段（`file.search` 只有 matches）：写日志，
    // 不为了它改契约（见实现报告里的"契约缺口"）。
    tracing::debug!(
        search_id = %search_id,
        hits = matches.len(),
        scanned = summary.files_scanned,
        skipped_binary = summary.files_skipped_binary,
        skipped_too_large = summary.files_skipped_too_large,
        limit_reached = summary.limit_reached,
        "搜索完成"
    );

    state
        .events()
        .send(Event::FileSearch { search_id, matches });
    done()
}

/// `search_cancel`。
///
/// 幂等：搜索已经结束（或 ID 不存在）时什么也不做，仍返回成功——"取消一个已经结束
/// 的东西"在用户看来与成功一致，报 `-32001` 只会让 UI 弹一个没人看得懂的错。
/// 也不伪造 `file.search` 事件：事件只有一个来源（搜索自己跑完时的那次推送）。
pub async fn search_cancel(state: &AppState, search_id: SearchId) -> Response {
    let cancelled = state.searches().cancel(search_id);
    tracing::debug!(search_id = %search_id, cancelled, "search_cancel");
    done()
}

/// `diagnostics`。
pub async fn diagnostics(state: &AppState, path: Option<String>) -> Response {
    let root = match state.project_root() {
        Some(root) => root,
        None => return failed(HbotError::invalid("没有打开的项目：先发 project_open")),
    };
    let filter = path.clone();
    let items = match state
        .blocking(move || Ok(diagnostics_of(&root, filter.as_deref())))
        .await
    {
        Ok(items) => items,
        Err(error) => return failed(error),
    };

    // 事件里的 `path` 是"这批诊断属于哪个文件"：点名了就照抄；没点名就用诊断自己
    // 的文件（项目级错误的 path 为空串）。
    let event_path = path.unwrap_or_else(|| {
        items
            .first()
            .map(|item| item.path.clone())
            .unwrap_or_default()
    });
    state.events().send(Event::Diagnostics {
        path: event_path,
        items: items.clone(),
    });
    ok_data(&items)
}

/// M0 的诊断来源：整项目编译（`hbot_code::compile_project`）。
///
/// 当前编译器一次只报**第一个**错（`Result<_, CodeError>`，不是 `Vec`），所以这里
/// 最多产出一条诊断；换成多错收集后这个函数不用改形状。
fn diagnostics_of(root: &Path, only: Option<&str>) -> Vec<Diagnostic> {
    let error = match hbot_code::compile_project(root) {
        Ok(_compiled) => return Vec::new(),
        Err(error) => error,
    };

    let position = error.position();
    let code = error.code();
    let path = position
        .as_ref()
        .map(|position| to_ui_path(root, &position.file))
        .unwrap_or_default();
    let rendered = error.to_string();
    // `Display` 是 `"HBOT_COMPILE: file:line:col: message"`：`code()` 已经在 `code` 字段里，
    // 剥掉前缀，别让 UI 把机器码显示两遍。
    let message = match rendered
        .strip_prefix(code)
        .and_then(|rest| rest.strip_prefix(':'))
    {
        Some(rest) => rest.trim_start().to_owned(),
        None => rendered.clone(),
    };

    let diagnostic = Diagnostic {
        path,
        // 没有位置信息的错误（`NoEntry` / `Io`）用 0 表示"未知"，UI 只做展示。
        line: position.as_ref().map_or(0, |position| position.line),
        column: position.as_ref().map_or(0, |position| position.column),
        severity: DiagnosticSeverity::Error,
        message,
        code: Some(code.to_owned()),
    };

    match only {
        Some(wanted) if normalize_ui(wanted) != diagnostic.path => Vec::new(),
        _ => vec![diagnostic],
    }
}

/// 编译器的文件名 → UI 相对路径（`/` 分隔、去掉根前缀）。
fn to_ui_path(root: &Path, file: &str) -> String {
    let candidate = Path::new(file);
    let relative = candidate.strip_prefix(root).unwrap_or(candidate);
    normalize_ui(&relative.to_string_lossy())
}

/// 统一成 UI 用的路径写法：`\` → `/`、去掉开头的 `./`。
fn normalize_ui(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    match normalized.strip_prefix("./") {
        Some(rest) => rest.to_owned(),
        None => normalized,
    }
}

/// 路由入口：由 [`crate::Dispatcher`] 调用。
pub async fn handle(state: &AppState, command: Command) -> Response {
    match command {
        Command::FileRead { path } => read(state, path).await,
        Command::FileWrite {
            path,
            content,
            base_hash,
        } => write(state, path, content, base_hash).await,
        Command::FileList { path } => list(state, path).await,
        Command::FileCreate { path, content } => create(state, path, content).await,
        Command::FileRename { from, to } => rename(state, from, to).await,
        Command::FileDelete { path, to_trash } => delete(state, path, to_trash).await,
        Command::SearchStart {
            query,
            regex,
            globs,
        } => search(state, query, regex, globs).await,
        Command::SearchCancel { search_id } => search_cancel(state, search_id).await,
        Command::Diagnostics { path } => diagnostics(state, path).await,
        other => unimplemented(other.kind()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::app::test_support::{temp_state, temp_state_with_project};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("测试用 tokio runtime")
    }

    fn workspace_of(state: &AppState) -> Arc<Workspace> {
        state.workspace().expect("夹具应已打开项目")
    }

    /// 铺夹具：也走 `Workspace`，测试里不绕过路径守卫。
    fn seed(state: &AppState, path: &str, content: &str) {
        let workspace = workspace_of(state);
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                // 目录可能已经被上一个 fixture 建过：`Conflict` 不是错误。
                match workspace.create_dir(parent) {
                    Ok(_) | Err(HbotError::Conflict { .. }) => {}
                    Err(error) => panic!("建夹具目录失败：{error}"),
                }
            }
        }
        workspace
            .write_file(Path::new(path), content, None)
            .expect("写夹具文件");
    }

    fn expect_error(response: Response) -> i32 {
        match response.into_result() {
            Err(error) => error.code,
            Ok(data) => panic!("期望失败，实际成功：{data:?}"),
        }
    }

    fn expect_ok(response: Response) -> Option<Value> {
        match response.into_result() {
            Ok(data) => data,
            Err(error) => panic!("期望成功，实际失败：{error:?}"),
        }
    }

    fn expect_data(response: Response) -> Value {
        match expect_ok(response) {
            Some(data) => data,
            None => panic!("期望 data，实际是 null"),
        }
    }

    fn next_file_search(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> Vec<SearchMatch> {
        match rx.try_recv() {
            Ok(Event::FileSearch { matches, .. }) => matches,
            Ok(other) => panic!("意外事件：{other:?}"),
            Err(error) => panic!("期望 file.search 事件，实际：{error}"),
        }
    }

    fn next_diagnostics(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> Vec<Diagnostic> {
        match rx.try_recv() {
            Ok(Event::Diagnostics { items, .. }) => items,
            Ok(other) => panic!("意外事件：{other:?}"),
            Err(error) => panic!("期望 diagnostics 事件，实际：{error}"),
        }
    }

    fn parse_id(text: &str) -> SearchId {
        text.parse().expect("搜索 ID 应是合法 UUID")
    }

    #[test]
    fn read_missing_file_reports_not_found() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        let code = expect_error(rt.block_on(read(&state, "nope.txt".to_owned())));
        assert_eq!(code, -32001, "缺失文件应是 not_found");
    }

    #[test]
    fn commands_without_an_open_project_are_invalid() {
        let (_dir, state) = temp_state();
        let rt = runtime();
        assert_eq!(
            expect_error(rt.block_on(read(&state, "a.txt".to_owned()))),
            -32002,
            "没打开项目是状态错误，不是文件不存在"
        );
        assert_eq!(
            expect_error(rt.block_on(search(&state, "x".to_owned(), false, Vec::new()))),
            -32002
        );
        assert_eq!(expect_error(rt.block_on(diagnostics(&state, None))), -32002);
    }

    #[test]
    fn path_escape_is_rejected() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        for path in ["../x", "..", "src/../../x", "/etc/passwd"] {
            let code = expect_error(rt.block_on(read(&state, path.to_owned())));
            assert!(
                code == -32001 || code == -32002,
                "`{path}` 应被拒绝，实际码 {code}"
            );
            let code =
                expect_error(rt.block_on(write(&state, path.to_owned(), "x".to_owned(), None)));
            assert!(
                code == -32001 || code == -32002,
                "写入 `{path}` 应被拒绝，实际码 {code}"
            );
            let code = expect_error(rt.block_on(create(&state, path.to_owned(), "x".to_owned())));
            assert!(
                code == -32001 || code == -32002,
                "新建 `{path}` 应被拒绝，实际码 {code}"
            );
        }
    }

    #[test]
    fn write_with_a_stale_base_hash_conflicts() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        seed(&state, "a.txt", "one");

        let first = expect_data(rt.block_on(read(&state, "a.txt".to_owned())));
        let stale = first["hash"].as_str().expect("hash 应是字符串").to_owned();

        // 外部改动（绕过 RPC）：磁盘上的 hash 已经不是 `stale` 了。
        workspace_of(&state)
            .write_file(Path::new("a.txt"), "two", None)
            .expect("外部改动");

        let code = expect_error(rt.block_on(write(
            &state,
            "a.txt".to_owned(),
            "three".to_owned(),
            Some(stale),
        )));
        assert_eq!(code, -32003, "过期的 baseHash 必须冲突，不能静默覆盖");
    }

    #[test]
    fn read_then_write_without_base_hash_uses_the_remembered_hash() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        seed(&state, "a.txt", "one");

        let _ = expect_data(rt.block_on(read(&state, "a.txt".to_owned())));
        let written =
            expect_data(rt.block_on(write(&state, "a.txt".to_owned(), "two".to_owned(), None)));
        assert_eq!(written["content"], "two");

        let reread = expect_data(rt.block_on(read(&state, "a.txt".to_owned())));
        assert_eq!(reread["hash"], written["hash"], "写回的 hash 应与再读一致");

        // 记住的 hash 真的生效：外部改动后不带 baseHash 的写必须冲突。
        workspace_of(&state)
            .write_file(Path::new("a.txt"), "external", None)
            .expect("外部改动");
        let code =
            expect_error(rt.block_on(write(&state, "a.txt".to_owned(), "three".to_owned(), None)));
        assert_eq!(code, -32003, "记忆里的 hash 应挡住静默覆盖");
    }

    #[test]
    fn create_on_an_existing_file_conflicts_and_keeps_the_content() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        seed(&state, "a.txt", "one");

        let code = expect_error(rt.block_on(create(&state, "a.txt".to_owned(), "clobber".into())));
        assert_eq!(code, -32003, "file_create 必须拒绝覆盖已有文件");

        let content = expect_data(rt.block_on(read(&state, "a.txt".to_owned())));
        assert_eq!(content["content"], "one", "冲突时不允许改动原文件");

        let created = expect_data(rt.block_on(create(&state, "b.txt".to_owned(), "hi".into())));
        assert_eq!(created["content"], "hi");
    }

    #[test]
    fn list_returns_entries_for_a_directory() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        seed(&state, "src/main.ts", "let a = 1;\n");
        seed(&state, "readme.md", "# hi\n");

        let entries = expect_data(rt.block_on(list(&state, ".".to_owned())));
        let names: Vec<String> = entries
            .as_array()
            .expect("data 应是数组")
            .iter()
            .filter_map(|entry| entry["name"].as_str().map(str::to_owned))
            .collect();
        assert!(
            names.contains(&"src".to_owned()),
            "根目录应有 src：{names:?}"
        );
        assert!(names.contains(&"readme.md".to_owned()));

        let nested = expect_data(rt.block_on(list(&state, "src".to_owned())));
        assert_eq!(nested.as_array().expect("数组").len(), 1);
        assert_eq!(nested[0]["path"], "src/main.ts");
        assert!(
            !nested[0]["isDir"].as_bool().unwrap_or(true),
            "main.ts 不是目录"
        );
    }

    #[test]
    fn rename_moves_the_file_and_reports_both_paths() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        seed(&state, "a.txt", "one");
        let mut rx = state.events().subscribe();

        assert!(
            expect_ok(rt.block_on(rename(&state, "a.txt".to_owned(), "b.txt".to_owned())))
                .is_none()
        );
        assert_eq!(
            expect_data(rt.block_on(read(&state, "b.txt".to_owned())))["content"],
            "one"
        );
        assert_eq!(
            expect_error(rt.block_on(read(&state, "a.txt".to_owned()))),
            -32001,
            "旧路径应已不存在"
        );

        let mut changed: Vec<String> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let Event::ProjectChanged { path, kind } = event {
                assert_eq!(kind, FileChangeKind::Renamed);
                changed.push(path);
            }
        }
        assert_eq!(changed, vec!["a.txt".to_owned(), "b.txt".to_owned()]);
    }

    #[test]
    fn delete_moves_to_trash_and_reports_it() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        seed(&state, "a.txt", "one");
        let mut rx = state.events().subscribe();

        assert!(expect_ok(rt.block_on(delete(&state, "a.txt".to_owned(), true))).is_none());
        assert_eq!(
            expect_error(rt.block_on(read(&state, "a.txt".to_owned()))),
            -32001
        );

        let mut deleted = 0;
        while let Ok(event) = rx.try_recv() {
            if let Event::ProjectChanged { kind, .. } = event {
                assert_eq!(kind, FileChangeKind::Deleted);
                deleted += 1;
            }
        }
        assert_eq!(deleted, 1);
    }

    #[test]
    fn permanent_delete_is_refused_instead_of_faked() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        seed(&state, "a.txt", "one");

        let code = expect_error(rt.block_on(delete(&state, "a.txt".to_owned(), false)));
        assert_eq!(
            code, -32006,
            "永久删除没有 Workspace API：应报尚未实现，而不是假装删了"
        );
        assert_eq!(
            expect_data(rt.block_on(read(&state, "a.txt".to_owned())))["content"],
            "one",
            "拒绝之后文件必须还在"
        );
    }

    #[test]
    fn search_finds_matches_and_respects_globs() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        seed(&state, "src/main.ts", "const needle = 1;\n");
        seed(&state, "notes.md", "needle in markdown\n");
        let mut rx = state.events().subscribe();

        let response = rt.block_on(search(
            &state,
            "needle".to_owned(),
            false,
            vec!["*.ts".to_owned()],
        ));
        assert!(
            expect_ok(response).is_none(),
            "search_start 的 data 是 null"
        );

        let matches = next_file_search(&mut rx);
        assert_eq!(matches.len(), 1, "glob *.ts 应只留下 ts 文件：{matches:?}");
        assert_eq!(matches[0].path, "src/main.ts");
        assert_eq!(matches[0].line, 1);
        assert_eq!(matches[0].column, 1);
        assert!(matches[0].text.contains("needle"));

        // 不给 glob：两个文件都该命中。
        let response = rt.block_on(search(&state, "needle".to_owned(), false, Vec::new()));
        assert!(expect_ok(response).is_none());
        let matches = next_file_search(&mut rx);
        let mut paths: Vec<&str> = matches.iter().map(|item| item.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["notes.md", "src/main.ts"]);
    }

    #[test]
    fn search_supports_regex() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        seed(&state, "src/main.ts", "const answer = 42;\n");
        let mut rx = state.events().subscribe();

        let response = rt.block_on(search(&state, r"answer = \d+".to_owned(), true, Vec::new()));
        assert!(expect_ok(response).is_none());
        let matches = next_file_search(&mut rx);
        assert_eq!(matches.len(), 1);
        assert!(matches[0].text.contains("42"));
    }

    #[test]
    fn search_cancel_on_an_unknown_id_is_a_noop() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        let response = rt.block_on(search_cancel(
            &state,
            parse_id("33333333-3333-3333-3333-333333333333"),
        ));
        assert!(expect_ok(response).is_none(), "取消未知 ID 应成功（幂等）");
        assert!(state.searches().is_empty(), "取消不应留下登记");
    }

    #[test]
    fn diagnostics_reports_a_compile_error() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        seed(&state, "src/main.ts", "const x: = 1;\n");
        let mut rx = state.events().subscribe();

        let items = expect_data(rt.block_on(diagnostics(&state, None)));
        let items = items.as_array().expect("data 应是数组").clone();
        assert!(!items.is_empty(), "语法错误应至少产出一条诊断");
        assert_eq!(items[0]["path"], "src/main.ts");
        assert_eq!(items[0]["severity"], "error");
        assert!(items[0]["code"].is_string(), "诊断应带机器码");
        assert!(
            items[0]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty())
        );

        let emitted = next_diagnostics(&mut rx);
        assert_eq!(emitted.len(), items.len(), "事件与 data 应是同一批诊断");

        // 只看这个文件：一条；看别的文件：空。
        let same = expect_data(rt.block_on(diagnostics(&state, Some("src/main.ts".to_owned()))));
        assert_eq!(same.as_array().expect("数组").len(), items.len());
        let other = expect_data(rt.block_on(diagnostics(&state, Some("src/other.ts".to_owned()))));
        assert!(other.as_array().expect("数组").is_empty());
    }

    #[test]
    fn diagnostics_on_a_clean_project_is_empty() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        seed(&state, "src/main.ts", "export const answer: number = 42;\n");

        let items = expect_data(rt.block_on(diagnostics(&state, None)));
        assert!(
            items.as_array().expect("数组").is_empty(),
            "能编译通过的项目不应有诊断：{items}"
        );
    }

    #[test]
    fn search_start_emits_the_pinned_file_search_shape() {
        let (_dir, state, _root) = temp_state_with_project();
        let rt = runtime();
        seed(&state, "src/main.ts", "const needle = 1;\n");
        let mut rx = state.events().subscribe();

        let response = rt.block_on(search(&state, "needle".to_owned(), false, Vec::new()));
        assert!(
            expect_ok(response).is_none(),
            "search_start 的 data 是 null"
        );

        // 钉住 `file.search` 的线上形状：别的层（前端、CLI）按这个写代码，注释不顶用。
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(error) => panic!("期望 file.search 事件，实际：{error}"),
        };
        let value = serde_json::to_value(&event).expect("事件应可序列化");
        let object = value.as_object().expect("事件应是 JSON 对象");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["matches", "searchId", "type"],
            "事件字段集合是契约"
        );
        assert_eq!(object["type"], "file.search");
        assert!(
            object["searchId"].as_str().is_some_and(|id| !id.is_empty()),
            "searchId 应是 UUID 字符串：{}",
            object["searchId"]
        );

        let matches = object["matches"].as_array().expect("matches 应是数组");
        assert_eq!(matches.len(), 1);
        let mut match_keys: Vec<&str> = matches[0]
            .as_object()
            .expect("单条匹配应是 JSON 对象")
            .keys()
            .map(String::as_str)
            .collect();
        match_keys.sort_unstable();
        assert_eq!(
            match_keys,
            vec!["column", "line", "path", "text"],
            "单条匹配的字段集合也是契约"
        );
        assert_eq!(matches[0]["path"], "src/main.ts");
        assert_eq!(matches[0]["line"], 1);
        assert_eq!(matches[0]["column"], 1);
        assert_eq!(matches[0]["text"], "const needle = 1;");
    }

    #[test]
    fn active_key_normalises_separators_and_prefix() {
        assert_eq!(active_key("src/main.ts"), "src/main.ts");
        assert_eq!(active_key("./src/main.ts"), "src/main.ts");
        assert_eq!(active_key(r"src\main.ts"), "src/main.ts");
    }
}
