//! 文件能力（`C3.file`）在**生产装配路径**上的端到端测试。
//!
//! 契约（`hbot-core::bot_file`）、槽位与 JS 桥（`hbot-code`）、后端（`app::runtime::files`）
//! 各有单测；本文件补的是它们之间唯一没有别处覆盖的那一段：
//!
//! `AppState::start_bot` → `SandboxHandlerConfig::files` → `worker()` 的 `set_files`
//! → `ctx.files.*` → `ProjectFiles` → 项目根里的真实文件。
//!
//! 七条断言：
//!
//! 1. **没声明就没得用**：清单里没有 `capabilities.state.files` → `HBOT_FILE_NOT_DECLARED`；
//! 2. **读写分开声明**：只写 `read: true` 时 `write` 报 `HBOT_FILE_NOT_DECLARED`（capability 是 write）；
//! 3. **声明了真能写读**：`write` 落盘，`read` 读回同一段文本；
//! 4. **`..` 与绝对路径被拒**：`HBOT_FILE_DENIED`，且项目外什么都没被碰；
//! 5. **不存在是 `NOT_FOUND`**，不是空内容；
//! 6. **`list` 只报相对项目根的路径**；
//! 7. **非 UTF-8 明确失败**，不是悄悄替换成 U+FFFD；
//! 8. **按行范围读只回那几行**；
//! 9. **`readBytes` 回原样字节的 base64**，不做 UTF-8 校验；
//! 10. **按行写只替换那几行**，未触碰的行原样保留；
//! 11. **递归删除返回条目数**，非递归遇到非空目录仍拒；
//! 12. **行号越界在 JS 边界报错**；
//! 13. **超过清单里的 `limits.quotaMb` 时写被拒**（`HBOT_FILE_QUOTA`，且账本与磁盘都没动）；
//! 14. **删掉文件之后额度真的回来了**（同一个 bot 接着能再写）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hbot_adapters::SystemClock;
use hbot_core::{BotId, ConversationId};
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::dto::{Command, Response};

const BOT_UUID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const LOCAL_EXTERNAL_ID: &str = "local:default";
const REPLY_PREFIX: &str = "[reply] ";

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hbot-filecap-{label}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// handler：把文件结果/错误都回复出来，用例只读回复。
fn files_handler() -> &'static str {
    r#"
import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    const what = ctx.text;
    try {
      if (what === 'write') {
        const entry = await ctx.files.write('notes/a.txt', 'hello');
        reply('wrote:' + entry.path + ':' + entry.size);
        return;
      }
      if (what === 'read') {
        const file = await ctx.files.read('notes/a.txt');
        reply('read:' + file.text + ':' + file.truncated);
        return;
      }
      if (what === 'escape') {
        await ctx.files.read('../outside.txt');
        reply('escape-allowed');
        return;
      }
      if (what === 'absolute') {
        await ctx.files.write('/etc/passwd', 'x');
        reply('absolute-allowed');
        return;
      }
      if (what === 'missing') {
        await ctx.files.read('nope.txt');
        reply('missing-allowed');
        return;
      }
      if (what === 'list') {
        const entries = await ctx.files.list('');
        reply('list:' + entries.map((entry) => entry.path + '|' + entry.kind).join(','));
        return;
      }
      if (what === 'lines') {
        await ctx.files.write('notes/log.txt', 'l1\nl2\nl3\n');
        const file = await ctx.files.read('notes/log.txt', { startLine: 2, endLine: 2 });
        reply('lines:' + file.text.trim() + ':' + file.truncated);
        return;
      }
      if (what === 'badrange') {
        await ctx.files.read('notes/a.txt', { startLine: 5, endLine: 2 });
        reply('badrange-allowed');
        return;
      }
      if (what === 'binary') {
        await ctx.files.read('notes/binary.bin');
        reply('binary-allowed');
        return;
      }
      if (what === 'bytes') {
        const blob = await ctx.files.readBytes('notes/binary.bin');
        reply('bytes:' + blob.base64 + ':' + blob.bytes + ':' + blob.truncated);
        return;
      }
      if (what === 'linewrite') {
        await ctx.files.write('notes/log.txt', 'l1\nl2\nl3\n');
        await ctx.files.write('notes/log.txt', 'X2', { startLine: 2, endLine: 2 });
        const file = await ctx.files.read('notes/log.txt');
        reply('linewrite:' + file.text.replace(/\n/g, '|'));
        return;
      }
      if (what === 'recursive') {
        await ctx.files.write('tmp/a.txt', 'a');
        await ctx.files.write('tmp/sub/b.txt', 'b');
        const count = await ctx.files.remove('tmp', { recursive: true });
        reply('removed:' + count);
        return;
      }
      if (what === 'nonrecursive') {
        await ctx.files.write('tmp/a.txt', 'a');
        const count = await ctx.files.remove('tmp');
        reply('nonrecursive-allowed:' + count);
        return;
      }
      if (what === 'quotafill') {
        await ctx.files.write('notes/fill.txt', 'x'.repeat(1048576));
        reply('quotafill-ok');
        return;
      }
      if (what === 'quotaover') {
        await ctx.files.write('notes/over.txt', 'y');
        reply('quotaover-allowed');
        return;
      }
      if (what === 'quotarecycle') {
        let blocked = 'no';
        try {
          await ctx.files.write('notes/blocked.txt', 'y');
        } catch (error) {
          blocked = error.message.includes('HBOT_FILE_QUOTA') ? 'quota' : 'other';
        }
        await ctx.files.remove('notes/fill.txt');
        await ctx.files.write('notes/after.txt', 'y');
        reply('quotarecycle:' + blocked);
        return;
      }
      reply('unknown');
    } catch (error) {
      reply('files-error:' + error.message);
    }
  },
});
"#
}

/// 写清单。`declare`：None = 不声明；Some((read, write)) = 声明读写两半。
///
/// `quota_mb`：`limits.quotaMb`，`None` = 不写这个字段（走默认值 0 = 不限）。
///
/// 配额的单位是 MB，所以端到端用例最小也只能测到 1 MiB —— 一次 `write` 的上限
/// （`MAX_FILE_WRITE_BYTES`）恰好也是 1 MiB，于是「写满一次、再写一个字节被拒」
/// 正好卡在这个粒度上，不需要更大的文件。
fn manifest_with_quota(declare: Option<(bool, bool)>, quota_mb: Option<u64>) -> String {
    let mut manifest = serde_json::json!({
        "name": "files-bot",
        "platform": { "kind": "local" },
    });
    if let Some((read, write)) = declare {
        manifest["capabilities"] = serde_json::json!({
            "state": { "files": { "read": read, "write": write } }
        });
    }
    if let Some(quota_mb) = quota_mb {
        manifest["limits"] = serde_json::json!({ "quotaMb": quota_mb });
    }
    manifest.to_string()
}

fn bots_json() -> String {
    serde_json::json!({
        "bots": [{ "id": BOT_UUID, "platform": "local", "name": "Files" }]
    })
    .to_string()
}

fn bot_id() -> BotId {
    BotId::from_uuid(uuid::Uuid::parse_str(BOT_UUID).expect("bot uuid"))
}

struct Harness {
    _dir: TempDir,
    project: PathBuf,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
}

impl Harness {
    async fn new(label: &str, declare: Option<(bool, bool)>, binary: bool) -> Self {
        Self::new_with_quota(label, declare, binary, None).await
    }

    async fn new_with_quota(
        label: &str,
        declare: Option<(bool, bool)>,
        binary: bool,
        quota_mb: Option<u64>,
    ) -> Self {
        let dir = TempDir::new(label);
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("创建 src");
        std::fs::write(project.join("src").join("main.ts"), files_handler()).expect("写 handler");
        std::fs::write(
            project.join("hbot.json"),
            manifest_with_quota(declare, quota_mb),
        )
        .expect("写 hbot.json");
        std::fs::write(project.join("bots.json"), bots_json()).expect("写 bots.json");
        if binary {
            std::fs::create_dir_all(project.join("notes")).expect("创建 notes");
            std::fs::write(
                project.join("notes").join("binary.bin"),
                [0xff_u8, 0xfe, 0x00, 0x01],
            )
            .expect("写二进制文件");
        }

        let data_dir = dir.path().join("data");
        let state = AppState::open(data_dir, Arc::new(SystemClock), Arc::new(UuidIds))
            .expect("打开 AppState");
        state.open_project(&project).await.expect("打开项目");
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            project,
            state,
            dispatcher,
        }
    }

    async fn start(&self) -> Response {
        self.dispatcher
            .dispatch(Command::BotStart { bot_id: bot_id() })
            .await
    }

    fn conversation(&self) -> ConversationId {
        self.state
            .store()
            .get_conversation_by_external(&bot_id(), LOCAL_EXTERNAL_ID)
            .expect("读会话")
            .expect("local 会话行")
            .id
    }

    async fn inject(&self, text: &str) -> Response {
        let conversation = self.conversation();
        self.dispatcher
            .dispatch(Command::BotInject {
                bot_id: bot_id(),
                conversation_id: conversation,
                text: text.to_owned(),
            })
            .await
    }

    fn replies(&self) -> Vec<String> {
        self.state
            .store()
            .list_logs(Some(&bot_id()), 1024)
            .expect("读日志")
            .into_iter()
            .filter_map(|row| row.line.strip_prefix(REPLY_PREFIX).map(str::to_owned))
            .collect()
    }

    /// 发一条消息并等一条**新**回复。
    ///
    /// `list_logs` 是新的在前，所以不能按「旧长度」当索引取：那样第二次调用会取到
    /// 上一条回复。这里记下发送前的快照，取第一条不在快照里的。
    async fn say(&self, text: &str) -> String {
        let before = self.replies();
        self.inject(text).await;
        wait_for(&format!("回复 {text}"), || {
            self.replies().iter().any(|reply| !before.contains(reply))
        })
        .await;
        self.replies()
            .into_iter()
            .find(|reply| !before.contains(reply))
            .expect("新回复")
    }
}

fn assert_ok(response: &Response, what: &str) {
    match response {
        Response::Ok { .. } => {}
        Response::Error { error } => {
            panic!(
                "{what} 期望成功，实际失败 {}：{}",
                error.code, error.message
            );
        }
    }
}

async fn wait_for(label: &str, mut predicate: impl FnMut() -> bool) {
    for _ in 0..240 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("等待超时：{label}");
}

#[tokio::test]
async fn an_undeclared_capability_reports_not_declared() {
    let harness = Harness::new("undeclared", None, false).await;
    assert_ok(&harness.start().await, "bot_start");
    let reply = harness.say("read").await;
    assert!(
        reply.contains("HBOT_FILE_NOT_DECLARED"),
        "期望声明层错误，实际：{reply}"
    );
}

#[tokio::test]
async fn a_read_only_declaration_still_refuses_writes() {
    let harness = Harness::new("readonly", Some((true, false)), false).await;
    assert_ok(&harness.start().await, "bot_start");
    let reply = harness.say("write").await;
    assert!(
        reply.contains("HBOT_FILE_NOT_DECLARED") && reply.contains("write"),
        "期望 write 半没声明，实际：{reply}"
    );
    assert!(
        !harness.project.join("notes").join("a.txt").exists(),
        "被拒绝的写不应该落盘"
    );
}

#[tokio::test]
async fn a_declared_capability_writes_and_reads_inside_the_project_root() {
    let harness = Harness::new("roundtrip", Some((true, true)), false).await;
    assert_ok(&harness.start().await, "bot_start");
    let written = harness.say("write").await;
    assert_eq!(written, "wrote:notes/a.txt:5");
    assert!(harness.project.join("notes").join("a.txt").exists());
    let read = harness.say("read").await;
    assert_eq!(read, "read:hello:false");
}

#[tokio::test]
async fn escaping_the_project_root_is_denied() {
    let harness = Harness::new("escape", Some((true, true)), false).await;
    assert_ok(&harness.start().await, "bot_start");
    assert!(harness.say("escape").await.contains("HBOT_FILE_DENIED"));
    assert!(harness.say("absolute").await.contains("HBOT_FILE_DENIED"));
}

#[tokio::test]
async fn a_missing_file_is_not_found_rather_than_empty() {
    let harness = Harness::new("missing", Some((true, true)), false).await;
    assert_ok(&harness.start().await, "bot_start");
    let reply = harness.say("missing").await;
    assert!(reply.contains("HBOT_FILE_NOT_FOUND"), "实际：{reply}");
}

#[tokio::test]
async fn listing_reports_relative_paths_only() {
    let harness = Harness::new("list", Some((true, true)), false).await;
    assert_ok(&harness.start().await, "bot_start");
    assert_eq!(harness.say("write").await, "wrote:notes/a.txt:5");
    let reply = harness.say("list").await;
    assert!(reply.starts_with("list:"), "实际：{reply}");
    assert!(reply.contains("notes|dir"), "实际：{reply}");
    assert!(
        !reply.contains(":\\") && !reply.contains(":/"),
        "不许出现绝对路径：{reply}"
    );
}

#[tokio::test]
async fn binary_content_is_refused_instead_of_replaced() {
    let harness = Harness::new("binary", Some((true, true)), true).await;
    assert_ok(&harness.start().await, "bot_start");
    let reply = harness.say("binary").await;
    assert!(reply.contains("HBOT_FILE_INVALID"), "实际：{reply}");
}

#[tokio::test]
async fn a_line_range_read_only_returns_those_lines() {
    let harness = Harness::new("lines", Some((true, true)), false).await;
    assert_ok(&harness.start().await, "bot_start");
    // log.txt 三行，只读第 2 行：内容就是 l2，后面还有第 3 行所以 truncated 为 true。
    assert_eq!(harness.say("lines").await, "lines:l2:true");
}

#[tokio::test]
async fn a_reversed_line_range_is_refused_at_the_js_boundary() {
    let harness = Harness::new("badrange", Some((true, true)), false).await;
    assert_ok(&harness.start().await, "bot_start");
    let reply = harness.say("badrange").await;
    assert!(
        reply.contains("HBOT_FILE_INVALID") && reply.contains("endLine"),
        "期望行号闸在 JS 边界报错，实际：{reply}"
    );
}
#[tokio::test]
async fn read_bytes_returns_the_raw_bytes_as_base64() {
    let harness = Harness::new("bytes", Some((true, true)), true).await;
    assert_ok(&harness.start().await, "bot_start");
    // [0xff, 0xfe, 0x00, 0x01] 的 base64 是 "//4AAQ=="；文本读会因不是 UTF-8 报错，
    // 二进制读必须原样给出。
    assert_eq!(harness.say("bytes").await, "bytes://4AAQ==:4:false");
}

#[tokio::test]
async fn a_line_range_write_replaces_only_that_line() {
    let harness = Harness::new("linewrite", Some((true, true)), false).await;
    assert_ok(&harness.start().await, "bot_start");
    assert_eq!(harness.say("linewrite").await, "linewrite:l1|X2|l3|");
    let raw = std::fs::read_to_string(harness.project.join("notes").join("log.txt"))
        .expect("读回 log.txt");
    assert_eq!(raw, "l1\nX2\nl3\n");
}

#[tokio::test]
async fn a_recursive_remove_counts_the_entries_it_took() {
    let harness = Harness::new("recursive", Some((true, true)), false).await;
    assert_ok(&harness.start().await, "bot_start");
    // tmp + tmp/a.txt + tmp/sub + tmp/sub/b.txt
    assert_eq!(harness.say("recursive").await, "removed:4");
    assert!(!harness.project.join("tmp").exists());
}

#[tokio::test]
async fn a_non_recursive_remove_still_refuses_a_non_empty_directory() {
    let harness = Harness::new("nonrecursive", Some((true, true)), false).await;
    assert_ok(&harness.start().await, "bot_start");
    let reply = harness.say("nonrecursive").await;
    assert!(reply.contains("HBOT_FILE_DENIED"), "实际：{reply}");
    assert!(
        harness.project.join("tmp").join("a.txt").exists(),
        "被拒绝的删除不应该动到里面的文件"
    );
}

#[tokio::test]
async fn a_write_beyond_the_declared_quota_is_refused() {
    // 1 MB 配额：先写满 1 MiB（等于上限，放行），再写 1 字节必须被拒。
    let harness = Harness::new_with_quota("quotaover", Some((true, true)), false, Some(1)).await;
    assert_ok(&harness.start().await, "bot_start");
    assert_eq!(harness.say("quotafill").await, "quotafill-ok");
    assert_eq!(
        harness.state.store().file_usage(&bot_id()).expect("读账本"),
        1024 * 1024,
        "写满一次 1 MiB 之后账本要记到上限"
    );
    let reply = harness.say("quotaover").await;
    assert!(
        reply.contains("HBOT_FILE_QUOTA"),
        "超过配额要报配额码而不是别的错，实际：{reply}"
    );
    assert!(
        reply.contains("quotaMb"),
        "错误文案要指出是清单里的哪个字段，实际：{reply}"
    );
    assert!(
        !harness.project.join("notes").join("over.txt").exists(),
        "被配额拒绝的写不能落盘"
    );
    assert_eq!(
        harness.state.store().file_usage(&bot_id()).expect("读账本"),
        1024 * 1024,
        "被拒的写不该改账本"
    );
}

#[tokio::test]
async fn removing_a_file_gives_the_quota_back() {
    let harness = Harness::new_with_quota("quotarecycle", Some((true, true)), false, Some(1)).await;
    assert_ok(&harness.start().await, "bot_start");
    // 先把额度写满（1 MiB），再由同一条消息里的三步验证回收：
    // 满额时写不动 → 删掉 fill.txt → 再写 1 字节必须成功。
    assert_eq!(harness.say("quotafill").await, "quotafill-ok");
    assert_eq!(harness.say("quotarecycle").await, "quotarecycle:quota");
    assert!(
        !harness.project.join("notes").join("fill.txt").exists(),
        "删掉的文件要真的不在"
    );
    assert!(
        harness.project.join("notes").join("after.txt").exists(),
        "删掉 1 MiB 之后额度要能再用"
    );
    assert_eq!(
        harness.state.store().file_usage(&bot_id()).expect("读账本"),
        1,
        "退账之后账本只剩 after.txt 那 1 字节"
    );
}
