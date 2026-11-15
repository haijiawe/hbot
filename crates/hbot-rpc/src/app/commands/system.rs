//! 系统命令：`app_info`、`project_open`、`project_create`。
//!
//! 这三条是 UI 的启动握手：`app_info` 拿产品名/版本/平台/数据目录，`project_open`
//! 决定后续所有文件命令的相对根，`project_create` 从零建一个项目再把 workspace
//! 切过去。三条都在这里实现，因为它们共用"项目根"这一个概念。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hbot_code::{CodeError, dts, manifest};
use hbot_core::HbotError;
use hbot_store::Project;

use super::failed;
use crate::app::{AppState, store_failed};
use crate::dto::{AppInfo, ProjectSummary, Response};

/// 产品名（与 `tauri.conf.json` 的 `productName`、`Cargo.toml` 的包名分开写：
/// 前者是给用户看的，后者是给 cargo 看的）。
pub const PRODUCT_NAME: &str = "hbot";

/// `app_info`：真数据，不占位。
pub fn app_info(state: &AppState) -> Response {
    let info = AppInfo {
        name: PRODUCT_NAME.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        platform: std::env::consts::OS.to_owned(),
        data_dir: state.data_dir().display().to_string(),
    };

    // `AppInfo` 只有字符串字段，序列化不可能失败；仍然显式报错而不是 `unwrap`
    // （库代码禁 panic，`AGENTS.md` §5）。
    match serde_json::to_value(info) {
        Ok(value) => Response::with_data(value),
        Err(error) => failed(HbotError::internal(format!("序列化 AppInfo 失败：{error}"))),
    }
}

/// `project_open`：打开（或切换）项目根，返回规范化后的绝对路径。
///
/// 路径必须存在且是目录：`Workspace::open` 会 canonicalize，不存在时报 `-32001`，
/// 不是目录时报 `-32002`。不在这里 `create_dir_all`——"打开"和"新建"是两件事
/// （新建是 [`project_create`]），自动建目录会让打错路径悄悄变成一个空项目。
pub async fn project_open(state: &AppState, path: String) -> Response {
    match open_project_root(state, path).await {
        Ok(root) => Response::with_data(serde_json::Value::String(root.display().to_string())),
        Err(error) => failed(error),
    }
}

async fn open_project_root(state: &AppState, path: String) -> Result<PathBuf, HbotError> {
    if path.trim().is_empty() {
        return Err(HbotError::invalid("project_open: path 不能为空"));
    }
    let root = state.open_project(PathBuf::from(path)).await?;
    // 打开已有项目时顺带保证 `hbot.d.ts` 是当前生成器的版本（best effort）。
    refresh_host_dts(state, &root).await;
    Ok(root)
}

/// 打开项目后修一次 `hbot.d.ts`（**失败不影响打开项目**）。
///
/// 缺失或与当前生成器不一致时重写：否则 Agent 生成的代码在 Monaco 里
/// `defineBot` / `ctx.reply` 会整片红波浪线，用户会以为项目坏了
/// （`docs/06-ide.md`：类型声明由宿主生成，不随用户代码版本走）。
///
/// 只记 `warn`：文件只读、被杀软/编辑器占用、磁盘满都可能让这一步失败，
/// 而"打开项目"本身已经成功了——为一条声明文件把整个命令判失败是错的。
async fn refresh_host_dts(state: &AppState, root: &Path) {
    let dir = root.to_path_buf();
    let outcome = state
        .blocking(move || {
            if dts::is_host_dts_current(&dir).map_err(code_failed)? {
                return Ok(None);
            }
            dts::write_host_dts(&dir).map_err(code_failed).map(Some)
        })
        .await;

    match outcome {
        Ok(None) => {}
        Ok(Some(path)) => {
            tracing::info!(path = %path.display(), "project_open: hbot.d.ts 已重建");
        }
        Err(error) => {
            tracing::warn!(
                root = %root.display(),
                error = %error,
                "project_open: 重写 hbot.d.ts 失败，已继续（编辑器里 hbot 类型会缺声明）"
            );
        }
    }
}

/// `project_create`：建一个新项目（脚手架 → `hbot.d.ts` → 落库 → 切 workspace）。
///
/// 语义（验收点，不要自己发明）：
///
/// - 目标目录不存在 → 连父目录一起建；存在且**空** → 直接使用；
/// - 存在且**非空** → `-32003 conflict`，一个字节都不动（绝不覆盖用户文件）；
/// - `name` 为空 → `-32002 invalid`；
/// - 同一路径已经在 `projects` 表里 → `-32003 conflict`（不插第二行）；
/// - 成功后 workspace 已切到新项目，`file_list` / `file_read` 立刻可用。
pub async fn project_create(state: &AppState, path: String, name: String) -> Response {
    match create_project(state, path, name).await {
        Ok(summary) => match serde_json::to_value(&summary) {
            Ok(value) => Response::with_data(value),
            // `ProjectSummary` 只有字符串字段，理论上不会失败；仍然显式报错而不是
            // `unwrap`（库代码禁 panic，`AGENTS.md` §5）。
            Err(error) => failed(HbotError::internal(format!(
                "序列化 ProjectSummary 失败：{error}"
            ))),
        },
        Err(error) => failed(error),
    }
}

async fn create_project(
    state: &AppState,
    path: String,
    name: String,
) -> Result<ProjectSummary, HbotError> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(HbotError::invalid("project_create: name 不能为空"));
    }

    let raw = path.trim();
    if raw.is_empty() {
        return Err(HbotError::invalid("project_create: path 不能为空"));
    }
    let dir = PathBuf::from(raw);

    // 先判目标目录，再写任何东西：`scaffold` 自己只拒绝它要写的 6 个文件名，
    // 目录里躺着别的文件（用户的数据）时它不吭声 —— 空目录判据必须在这里。
    let dir_existed = check_target_is_free(&dir)?;

    // 同一路径已经登记过也算冲突（典型：用户手工清空了项目目录再点"新建"）。
    // 不查这一下就会在 `projects.path` 的 UNIQUE 上炸成 `-32006`，或者更糟——
    // 真的插出第二行，UI 的项目列表从此分不清哪个是哪个。
    if let Some(existing) = lookup_project(state, &registration_key(&dir)).await? {
        return Err(HbotError::conflict(format!(
            "project_create: {} 已登记为项目「{}」（id={}），不会重复创建",
            existing.path, existing.name, existing.id
        )));
    }

    // Phase 1：脚手架 + 类型声明（文件 IO 全在阻塞线程上）。
    let scaffolded = {
        let dir = dir.clone();
        let name = name.clone();
        state
            .blocking(move || scaffold_project(&dir, &name))
            .await?
    };

    // Phase 2：落库。失败要回退并说清留下了什么——不假装原子。
    let project = Project {
        id: state.ids().project_id(),
        name: name.clone(),
        path: scaffolded.root.to_string_lossy().into_owned(),
        created_at: state.clock().now().unix_millis(),
    };
    let inserted = {
        let store = Arc::clone(state.store());
        let to_insert = project.clone();
        state
            .blocking(move || store.insert_project(&to_insert).map_err(store_failed))
            .await
    };
    if let Err(error) = inserted {
        rollback_scaffold(&dir, &scaffolded.files, dir_existed);
        return Err(HbotError::internal(format!(
            "project_create: 项目文件已写入 {}，但登记到 projects 表失败（{error}）；\
             已尽力回退（删除刚写的 {} 个文件），目录若还在请手工清理",
            scaffolded.root.display(),
            scaffolded.files.len()
        )));
    }

    // Phase 3：切 workspace（`file_list` / `file_read` 从此对着新项目）。
    let opened = match state.open_project(&scaffolded.root).await {
        Ok(opened) => opened,
        Err(error) => {
            // 文件和库行都已经落地，回退会把"能用的项目"删掉；如实报告更安全。
            return Err(HbotError::internal(format!(
                "project_create: 项目已建好并登记在 {}，但切换 workspace 失败（{error}）；\
                 可以再用 project_open 打开它",
                scaffolded.root.display()
            )));
        }
    };

    tracing::info!(
        project = %project.id,
        path = %opened.display(),
        entry = %scaffolded.entry,
        "项目已创建"
    );

    Ok(ProjectSummary {
        path: opened.display().to_string(),
        name,
        entry: scaffolded.entry,
    })
}

/// 一次 `scaffold` 的产物：给回退和返回值用。
struct Scaffolded {
    /// 规范化后的项目根。
    root: PathBuf,
    /// 入口文件的项目内相对路径（`src/main.ts`）。
    entry: String,
    /// 刚写下的文件（项目内相对路径），落库失败时按这个回退。
    files: Vec<String>,
}

/// 写脚手架与 `hbot.d.ts`，返回规范化根与入口。
fn scaffold_project(dir: &Path, name: &str) -> Result<Scaffolded, HbotError> {
    let report = manifest::scaffold(dir, name).map_err(code_failed)?;
    // `scaffold` 内部虽然也写了 `hbot.d.ts`，但"声明文件必须与当前生成器一致"
    // 这条不变量只由 `dts` 模块决定；再调一次 `write_host_dts`（原子写、幂等）
    // 让这条不变量只有一个来源。
    dts::write_host_dts(dir).map_err(code_failed)?;

    let root = std::fs::canonicalize(dir).map_err(|error| {
        HbotError::internal(format!(
            "project_create: 规范化项目根 {} 失败：{error}",
            dir.display()
        ))
    })?;
    let entry = manifest::load(&root).map_err(code_failed)?.entry_name();

    Ok(Scaffolded {
        root,
        entry,
        files: report.files,
    })
}

/// 检查目标目录能不能用来建项目；返回"调用前它是否已存在"。
///
/// 已存在且非空 → `-32003`（`AGENTS.md` §0.10：禁止静默覆盖）。已存在的文件路径
/// 不是"目录非空"问题，按 `-32002 invalid` 报。
fn check_target_is_free(dir: &Path) -> Result<bool, HbotError> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(HbotError::invalid(format!(
                    "project_create: {} 已存在且不是目录",
                    dir.display()
                )));
            }
            let mut entries = std::fs::read_dir(dir).map_err(|error| {
                HbotError::internal(format!(
                    "project_create: 读不了目录 {}：{error}",
                    dir.display()
                ))
            })?;
            if entries.next().is_some() {
                return Err(HbotError::conflict(format!(
                    "project_create: 目标目录已存在且非空：{}；不会覆盖用户文件，\
                     请换一个空目录或新路径",
                    dir.display()
                )));
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(HbotError::internal(format!(
            "project_create: 检查 {} 失败：{error}",
            dir.display()
        ))),
    }
}

/// 落库用的路径键。
///
/// 目录已存在时取真实路径（与 `project_open` 落库的形状一致）；不存在时取
/// [`std::path::absolute`]（不解析符号链接，够用来查重）。
fn registration_key(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .or_else(|_| std::path::absolute(dir))
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// 落库失败后回退脚手架。
///
/// 只做能确定是自己的动作：删掉 [`Scaffolded::files`] 里那些刚写的文件；目录
/// 原本就存在就留着，目录后来变非空（用户又往里放了东西）也不递归删——宁可留
/// 一个半成品目录并在错误信息里说清，也不删用户数据。
fn rollback_scaffold(dir: &Path, files: &[String], dir_existed: bool) {
    for relative in files.iter().rev() {
        let path = dir.join(relative);
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), %error, "project_create: 回退删文件失败");
        }
    }
    if dir_existed {
        return;
    }
    for directory in [dir.join("src"), dir.to_path_buf()] {
        match std::fs::remove_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %directory.display(), %error, "project_create: 回退删目录失败");
            }
        }
    }
}

/// `hbot-code` 的错误 → RPC 错误。
///
/// [`CodeError::Scaffold`] 是"目标已存在/写不进去"这一类（用户可修），映射成
/// `-32003 conflict`；其余（清单坏了、IO 失败、编译错）是内部错误。
fn code_failed(error: CodeError) -> HbotError {
    match &error {
        CodeError::Scaffold { .. } => HbotError::conflict(error.to_string()),
        _ => HbotError::internal(error.to_string()),
    }
}

/// 按路径查 `projects` 行（同步存储调用必须进阻塞线程）。
async fn lookup_project(state: &AppState, path: &str) -> Result<Option<Project>, HbotError> {
    let store = Arc::clone(state.store());
    let path = path.to_owned();
    state
        .blocking(move || store.get_project_by_path(&path).map_err(store_failed))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::temp_state;
    use crate::dto::Command;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
    }

    fn create(state: &AppState, path: &Path, name: &str) -> Response {
        runtime().block_on(project_create(
            state,
            path.display().to_string(),
            name.to_owned(),
        ))
    }

    fn expect_ok(response: Response) -> ProjectSummary {
        let Response::Ok { data } = response else {
            panic!("命令必须成功，实际 {response:?}");
        };
        serde_json::from_value(data.expect("成功必须带 data")).expect("data 应是 ProjectSummary")
    }

    fn expect_error_code(response: Response) -> i32 {
        let Response::Error { error } = response else {
            panic!("命令必须失败，实际 {response:?}");
        };
        error.code
    }

    #[test]
    fn app_info_reports_real_data_dir() {
        let (_dir, state) = temp_state();
        let response = app_info(&state);
        let info = {
            let Response::Ok { data } = response else {
                panic!("app_info 必须成功");
            };
            let value: AppInfo = serde_json::from_value(data.expect("app_info 有 data")).unwrap();
            value
        };
        assert_eq!(info.name, PRODUCT_NAME);
        assert!(!info.version.is_empty());
        assert!(!info.platform.is_empty());
        assert!(std::path::Path::new(&info.data_dir).is_dir());
    }

    #[test]
    fn empty_project_path_is_invalid_not_missing() {
        let runtime = runtime();
        let (_dir, state) = temp_state();
        let response = runtime.block_on(project_open(&state, "   ".to_owned()));
        assert_eq!(expect_error_code(response), -32002);
    }

    /// 最重要的一条：脚手架写出来**必须能编译**，而且 workspace 真的切过去了。
    #[test]
    fn project_create_writes_a_compilable_scaffold() {
        let (dir, state) = temp_state();
        // 深层路径：父目录也不存在，`project_create` 要连父目录一起建。
        let target = dir.path().join("deep").join("nested").join("新建项目");
        let summary = expect_ok(create(&state, &target, "演示项目"));

        assert_eq!(summary.name, "演示项目");
        assert_eq!(summary.entry, "src/main.ts");
        let root = PathBuf::from(&summary.path);
        assert!(root.is_dir(), "项目根必须存在：{}", summary.path);

        // 脚手架本身就是一份合法项目：清单能解析、源码能编译。
        hbot_code::compile_project(&root).expect("脚手架必须可编译");
        assert!(
            hbot_code::dts::is_host_dts_current(&root).expect("读 hbot.d.ts"),
            "创建后 hbot.d.ts 必须与生成器一致"
        );

        // workspace 已经切到新项目：后续命令不用再 open。
        assert_eq!(state.project_root().as_deref(), Some(root.as_path()));
        let listing = runtime().block_on(crate::app::commands::files::handle(
            &state,
            Command::FileList {
                path: "src".to_owned(),
            },
        ));
        assert!(
            matches!(listing, Response::Ok { .. }),
            "file_list 必须立刻可用，实际 {listing:?}"
        );
        let read = runtime().block_on(crate::app::commands::files::handle(
            &state,
            Command::FileRead {
                path: "src/main.ts".to_owned(),
            },
        ));
        assert!(
            matches!(read, Response::Ok { .. }),
            "file_read 必须立刻可用，实际 {read:?}"
        );
    }

    /// 非空目录一律拒绝，且里面的用户文件一个字节都不许动。
    #[test]
    fn project_create_refuses_a_non_empty_directory() {
        let (dir, state) = temp_state();
        let target = dir.path().join("已有内容");
        std::fs::create_dir_all(&target).expect("建目录");
        let keep = target.join("keep.txt");
        std::fs::write(&keep, b"user data").expect("写用户文件");

        let response = create(&state, &target, "新项目");

        assert_eq!(expect_error_code(response), -32003);
        assert_eq!(std::fs::read(&keep).expect("读用户文件"), b"user data");
        let mut names: Vec<String> = std::fs::read_dir(&target)
            .expect("列目录")
            .map(|entry| {
                entry
                    .expect("目录项")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["keep.txt".to_owned()], "拒绝时不能留下任何文件");
        assert!(
            state.project_root().is_none(),
            "创建失败不能把 workspace 切到半成品目录"
        );
    }

    #[test]
    fn project_create_rejects_an_empty_name() {
        let (dir, state) = temp_state();
        let target = dir.path().join("没名字");
        let response = create(&state, &target, "   ");
        assert_eq!(expect_error_code(response), -32002);
        assert!(!target.exists(), "参数不合格时不能留下目录");
    }

    /// 打开已有项目时，过期/丢失的 `hbot.d.ts` 要被就地重写（且不让命令失败）。
    #[test]
    fn project_open_rewrites_a_stale_hbot_dts() {
        let (dir, state) = temp_state();
        let target = dir.path().join("类型过期");
        expect_ok(create(&state, &target, "类型过期"));

        let dts_path = target.join("hbot.d.ts");
        std::fs::write(&dts_path, "// 被手工改坏了\n").expect("改坏 d.ts");
        assert!(!hbot_code::dts::is_host_dts_current(&target).expect("读 d.ts"));

        // 先切到别的目录，保证这次 open 真的走一遍（而不是"已经是当前项目"）。
        let other = dir.path().join("空目录");
        std::fs::create_dir_all(&other).expect("建目录");
        let runtime = runtime();
        runtime.block_on(project_open(&state, other.display().to_string()));

        let response = runtime.block_on(project_open(&state, target.display().to_string()));
        assert!(
            matches!(response, Response::Ok { .. }),
            "project_open 必须成功，实际 {response:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&dts_path).expect("读 d.ts"),
            hbot_code::dts::host_dts(),
            "过期 d.ts 必须被就地重写成生成器版本"
        );
        assert!(hbot_code::dts::is_host_dts_current(&target).expect("读 d.ts"));
    }

    /// 同一路径第二次创建 = 冲突（不是覆盖，也不是插出第二行）。
    #[test]
    fn project_create_is_conflict_on_second_call_with_same_path() {
        let (dir, state) = temp_state();
        let target = dir.path().join("重复");
        let first = expect_ok(create(&state, &target, "重复"));

        let second = create(&state, &target, "重复");
        assert_eq!(expect_error_code(second), -32003);

        // 库里仍然只有一行，且名字还是第一次那个。
        let key = registration_key(&target);
        let stored = runtime()
            .block_on(lookup_project(&state, &key))
            .expect("查库")
            .expect("第一次创建必须已落库");
        assert_eq!(stored.path, first.path);
        assert_eq!(stored.name, "重复");
    }
}
