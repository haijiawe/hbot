//! `bots.json` / `agent.json` 未知字段告警的契约测试。
//!
//! 这些测试**必须能红**：如果没有人实现告警，只断言"解析成功"是永远绿的（不修复时它
//! 也成功）。所以每条都先把 `tracing` 输出抓到内存里，再断言文案——告警的有无、路径是否
//! 完整、建议是否给了、有没有重复，全部靠日志文本判。
//!
//! 抓日志的做法同 `crates/hbot-bot/tests/worker_panic.rs`：自定义 `MakeWriter` 写进
//! `Vec<u8>`，用**线程局部**的 `tracing::subscriber::with_default` 装/卸订阅者。不用
//! 全局 `set_global_default`：那会被并行执行的测试互相串味，也没法要求"这条测试一个字符
//! 都不该打到日志里"。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hbot_core::BotId;
use hbot_rpc::app::agent::config::ProviderConfig;
use hbot_rpc::app::runtime::config::read_bot_config;

/// 固定的 bot id（与 `tests/support/mod.rs` 的 `BOT_UUID` 同一个值）。
const BOT_UUID: &str = "22222222-2222-4222-8222-222222222222";

/// 捕获到的日志字节。
type Captured = Arc<Mutex<Vec<u8>>>;

/// 把 `tracing` 的格式化输出写进内存的 writer。
#[derive(Clone)]
struct CaptureWriter(Captured);

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log lock poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for CaptureWriter {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

/// 在装了内存订阅者的作用域里跑 `run`，返回它的结果和这段期间的全部日志文本。
fn with_captured_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
    let buffer: Captured = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(CaptureWriter(Arc::clone(&buffer)))
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let value = tracing::subscriber::with_default(subscriber, run);
    let logs = String::from_utf8_lossy(&buffer.lock().expect("log lock poisoned")).to_string();
    (value, logs)
}

/// 只给这个测试文件用的临时目录（`crates/*/src` 里的 `TempDir` 是 `cfg(test)` 私有的）。
///
/// 目录名带 label + pid + 纳秒：并行跑的测试各写各的，也保证"文件 + 字段"的去重键不会
/// 跨测试命中（去重状态是进程级的静态变量，路径撞了就会让人误以为没告警）。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hbot-config-warnings-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// 往目录里写一份配置夹具。
    fn write(&self, name: &str, contents: &str) {
        std::fs::write(self.path.join(name), contents).expect("写配置夹具");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn bot_id() -> BotId {
    BotId::from_str(BOT_UUID).expect("合法的 bot uuid")
}

/// 没有任何 `HBOT_*` 环境变量的解析环境。
fn no_env() -> impl Fn(&str) -> Option<String> {
    |_name| None
}

#[test]
fn unknown_key_in_bots_json_warns_with_full_path_and_suggestion() {
    let dir = TempDir::new("bots-unknown-key");
    // `clientSecret` 被写成 `clientSecert`：真实里最常见的一类手写 typo。
    dir.write(
        "bots.json",
        r#"{"bots":[{"id":"22222222-2222-4222-8222-222222222222","url":"ws://127.0.0.1:6700","clientSecert":"typo"}]}"#,
    );

    let (result, logs) = with_captured_logs(|| read_bot_config(dir.path(), &bot_id()));

    let config = result.expect("未知字段只该告警，不该让解析失败");
    assert_eq!(config.ws_url.as_deref(), Some("ws://127.0.0.1:6700"));
    assert!(
        logs.contains("bots[0].clientSecert"),
        "告警必须带完整路径（含数组下标）才有定位价值，实际日志：{logs}"
    );
    assert!(
        logs.contains("你是不是想写 `clientSecret`？"),
        "拼错一个字母要给出建议，实际日志：{logs}"
    );
}

#[test]
fn unknown_key_in_agent_json_warns_with_suggestion() {
    let dir = TempDir::new("agent-unknown-key");
    dir.write(
        "agent.json",
        r#"{"baseUrl":"http://127.0.0.1:8080/v1","model":"deepseek-chat","baseURL":"多余的一份"}"#,
    );

    let (result, logs) = with_captured_logs(|| ProviderConfig::resolve_with(dir.path(), &no_env()));

    result.expect("未知字段只该告警，不该让解析失败");
    assert!(
        logs.contains("`baseURL`"),
        "要报出被忽略的那个键，实际日志：{logs}"
    );
    assert!(
        logs.contains("你是不是想写 `baseUrl`？"),
        "只差大小写也要给出建议，实际日志：{logs}"
    );
}

#[test]
fn underscore_prefixed_keys_are_silently_ignored() {
    let dir = TempDir::new("underscore-note");
    // JSON 没有注释语法，手写配置的人就用 `_说明` 这类键留备注（演示项目
    // `projects/qqdemo/bots.json` 就是这么写的）。这是产品决定：既不告警也不失败。
    dir.write(
        "bots.json",
        r#"{"_说明":"顶层备注","_comment":"note","_note":123,"bots":[{"id":"22222222-2222-4222-8222-222222222222","url":"ws://127.0.0.1:6700","_说明":"条目备注"}]}"#,
    );

    let ((), logs) = with_captured_logs(|| {
        read_bot_config(dir.path(), &bot_id()).expect("bots.json 的备注键不该让解析失败");
        // agent.json 同一口径，一起钉住。
        dir.write(
            "agent.json",
            r#"{"_说明":"备注","baseUrl":"http://127.0.0.1:8080/v1","model":"deepseek-chat"}"#,
        );
        ProviderConfig::resolve_with(dir.path(), &no_env())
            .expect("agent.json 的备注键不该让解析失败");
    });

    assert!(
        !logs.contains("WARN"),
        "`_` 开头的键是备注，不许产生任何警告，实际日志：{logs}"
    );
    assert!(
        !logs.contains("未知字段"),
        "`_` 开头的键连未知字段都不该提，实际日志：{logs}"
    );
}

#[test]
fn no_suggestion_when_nothing_is_close() {
    let dir = TempDir::new("bots-far-key");
    // `foo` 与任何已知字段都不像（最近的 `url` 编辑距离 3）。宁可不猜，也不许乱指。
    dir.write(
        "bots.json",
        r#"{"bots":[{"id":"22222222-2222-4222-8222-222222222222","url":"ws://127.0.0.1:6700","foo":"bar"}]}"#,
    );

    let (result, logs) = with_captured_logs(|| read_bot_config(dir.path(), &bot_id()));

    result.expect("未知字段只该告警，不该让解析失败");
    assert!(
        logs.contains("未知字段"),
        "至少要报出「已忽略」，实际日志：{logs}"
    );
    assert!(
        logs.contains("bots[0].foo"),
        "路径要能对着文件找，实际日志：{logs}"
    );
    assert!(
        !logs.contains("你是不是想写"),
        "离所有已知字段都远，不许给建议，实际日志：{logs}"
    );
    assert!(
        !logs.contains("`url`") && !logs.contains("`name`"),
        "不许把 `foo` 猜成别的字段，实际日志：{logs}"
    );
}

#[test]
fn type_errors_still_fail() {
    let dir = TempDir::new("bots-type-error");
    // 未知字段走告警，但**类型错仍然必须是 -32002**：这两件事不能顺带一起松掉。
    dir.write(
        "bots.json",
        r#"{"bots":[{"id":"22222222-2222-4222-8222-222222222222","url":"ws://127.0.0.1:6700","limits":{"memoryMb":"abc"}}]}"#,
    );

    let (result, _logs) = with_captured_logs(|| read_bot_config(dir.path(), &bot_id()));

    let error = result.expect_err("limits.memoryMb 是字符串，必须报错");
    assert_eq!(error.code(), -32002, "仍然是 invalid：{error}");
    assert!(
        error.to_string().contains("无法解析"),
        "错误文案要保持既有口径，实际：{error}"
    );
}

#[test]
fn warnings_are_not_repeated_within_one_process() {
    let dir = TempDir::new("bots-warning-dedup");
    dir.write(
        "bots.json",
        r#"{"bots":[{"id":"22222222-2222-4222-8222-222222222222","url":"ws://127.0.0.1:6700","clientSecert":"typo"}]}"#,
    );

    // 解析入口会被重试路径反复调用（bot 重连、每次 agent_send 都重读 provider 配置）。
    // 同一份 typo 每次重试都刷一遍会把真正的运行日志淹掉，所以每进程每字段只告警一次。
    let ((), logs) = with_captured_logs(|| {
        read_bot_config(dir.path(), &bot_id()).expect("第一次解析");
        read_bot_config(dir.path(), &bot_id()).expect("第二次解析");
    });

    assert_eq!(
        logs.matches("clientSecert").count(),
        1,
        "同一文件同一字段只该告警一次，实际日志：{logs}"
    );
}
