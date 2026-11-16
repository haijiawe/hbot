//! 本地崩溃上报（不联网、无遥测）。
//!
//! 只有两件事：
//!
//! 1. **写**：装一个 panic hook，崩溃时把现场写到 `<data_dir>/logs/crash-<时间戳>.log`。
//! 2. **读**：下次启动时把最近一份**还没上报过的**崩溃变成一条 `error` 的 `RunLog`
//!    事件，让用户知道"上次是崩了"，而不是以为程序自己退出了。
//!
//! 三条硬约束（来自任务书）：
//!
//! - **不联网**：这里没有任何 HTTP 客户端，也没有引入遥测 SDK。文件只留在本机。
//! - **脱敏**：报告正文过一遍 [`redact_text`]（复用 agent 日志那条脱敏链路），
//!   而且**不 dump 环境变量** —— 环境变量是最容易夹带 `HBOT_TOKEN` 的地方。
//! - **不覆盖用户数据**：报告写 `logs/` 子目录，文件名固定形状，不碰 `hbot.db`。
//!
//! 上报标记：读完把 `crash-X.log` 改名成 `crash-X.reported.log`。这样下次启动不会
//! 重复弹同一条错误，而**文件还在**（用户/开发者仍可手工翻）。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hbot_rpc::app::agent::log::redact_text;
use hbot_rpc::{AppState, Event, LogLevel};

/// 崩溃报告落在这个子目录下（与 `hbot.db` 同级）。
pub const LOGS_DIR: &str = "logs";

/// 报告文件名前缀。
const CRASH_PREFIX: &str = "crash-";

/// 报告文件名后缀。
const CRASH_SUFFIX: &str = ".log";

/// 已上报的标记后缀（`crash-X.reported.log`）。
const REPORTED_SUFFIX: &str = ".reported.log";

/// 等 WebView 订阅上来的最长时间（毫秒）：30 秒。窗口还没渲染完就有事件发出去的话，
/// `broadcast` 会把事件丢掉（没有重放），用户就看不到这条崩溃提示了。
const SUBSCRIBER_WAIT_MS: u64 = 30_000;

/// 轮询间隔（毫秒）。
const SUBSCRIBER_POLL_MS: u64 = 250;

/// 一条崩溃报告（纯数据，构造完就能渲染/落盘）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashReport {
    /// panic 的载荷（`&str` 或 `String` 的 `Display`）。
    pub message: String,
    /// `file:line:col`，取不到就是 `None`。
    pub location: Option<String>,
    /// 出事的线程名。
    pub thread: String,
    /// backtrace 文本（取不到就是 `None`）。
    pub backtrace: Option<String>,
    /// 发生时刻（Unix 毫秒，UTC 渲染）。
    pub at_millis: i64,
}

impl CrashReport {
    /// 从 panic hook 拿到的信息构造报告。
    pub fn capture(info: &std::panic::PanicHookInfo<'_>) -> Self {
        Self {
            message: panic_message(info),
            location: info.location().map(|loc| loc.to_string()),
            thread: std::thread::current()
                .name()
                .unwrap_or("unnamed")
                .to_string(),
            backtrace: Some(std::backtrace::Backtrace::force_capture().to_string()),
            at_millis: now_millis(),
        }
    }

    /// 渲染成给人看的文本（已经脱敏）。
    ///
    /// 字段顺序刻意固定：时间是解析/排序的唯一依据，版本与平台决定"能不能复现"，后面
    /// 才是现场。**不写环境变量**。
    pub fn render(&self) -> String {
        let mut text = String::new();
        text.push_str("hbot 崩溃报告（本机文件，未上传）\n");
        text.push_str(&format!(
            "时间: {} ({})\n",
            utc_stamp(self.at_millis),
            self.at_millis
        ));
        text.push_str(&format!("版本: {}\n", env!("CARGO_PKG_VERSION")));
        text.push_str(&format!("平台: {}\n", platform()));
        text.push_str(&format!("线程: {}\n", self.thread));
        text.push_str(&format!("panic: {}\n", self.message));
        match self.location.as_deref() {
            Some(location) => text.push_str(&format!("位置: {location}\n")),
            None => text.push_str("位置: 未知\n"),
        }
        match self.backtrace.as_deref() {
            Some(backtrace) => {
                text.push_str("backtrace:\n");
                text.push_str(backtrace);
                if !backtrace.ends_with('\n') {
                    text.push('\n');
                }
            }
            None => text.push_str("backtrace: 取不到（平台不支持）\n"),
        }
        redact_text(&text)
    }

    /// 写到 `<data_dir>/logs/crash-<时间戳>.log`，返回写出的路径。
    pub fn write(&self, data_dir: &Path) -> io::Result<PathBuf> {
        let dir = data_dir.join(LOGS_DIR);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(self.file_name());
        std::fs::write(&path, self.render().as_bytes())?;
        Ok(path)
    }

    /// 报告文件名（不含目录）。**不能用冒号** —— Windows 文件名里 `:` 是保留字符。
    pub fn file_name(&self) -> String {
        format!("{CRASH_PREFIX}{}{CRASH_SUFFIX}", utc_stamp(self.at_millis))
    }
}

/// 装 panic hook：先写本机报告，再把控制权交回原来的 hook（默认那个会打 stderr）。
///
/// `data_dir` 为 `None` 时只装一个透传 hook —— 数据目录都不知道的情况下，
/// 写哪儿都可能是错的，不如不写。
pub fn install(data_dir: Option<PathBuf>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = data_dir.as_ref() {
            // hook 里再 panic 会直接 abort 进程，所以整段兜起来：报告写不出来是小事，
            // 因为写报告把进程搞死是大事。
            let wrote = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                CrashReport::capture(info).write(dir)
            }));
            match wrote {
                Ok(Ok(path)) => tracing::error!(path = %path.display(), "已写入崩溃报告"),
                Ok(Err(error)) => tracing::error!(%error, "写崩溃报告失败"),
                Err(_) => tracing::error!("写崩溃报告时自身 panic，已忽略"),
            }
        }
        previous(info);
    }));
}

/// 找最近一份**还没上报过**的崩溃报告。
pub fn pending(data_dir: &Path) -> Option<PathBuf> {
    let dir = data_dir.join(LOGS_DIR);
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut best: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_pending_name(name) {
            continue;
        }
        let path = entry.path();
        // 文件名里的时间戳是定宽的 UTC，字典序就是时间序。
        match &best {
            Some(current) => {
                if path.file_name() > current.file_name() {
                    best = Some(path);
                }
            }
            None => best = Some(path),
        }
    }
    best
}

/// 把报告标记成已上报（改名，不删除 —— 用户仍然能翻到）。
pub fn mark_reported(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("崩溃报告文件名不是 UTF-8"))?;
    let stem = name
        .strip_suffix(CRASH_SUFFIX)
        .ok_or_else(|| io::Error::other(format!("不是崩溃报告文件名: {name}")))?;
    let reported = path.with_file_name(format!("{stem}{REPORTED_SUFFIX}"));
    std::fs::rename(path, &reported)?;
    Ok(reported)
}

/// 下次启动时把最近一份崩溃摆给用户看：一条 `error` 级 `RunLog`。
///
/// 为什么必须等订阅者：`EventBus` 是 `broadcast`，**没有订阅者时事件直接丢**（没有重放）。
/// 窗口还没把 `EVENT_CHANNEL` 订阅上就发出去的话，这条提示等于没发。
///
/// 为什么可能"报不出来"：`logs` 表的 `bot_id` 是外键（`REFERENCES bots(id)`），
/// 空数据库里没有任何 bot 时插不进日志行。这种情形只在"从没建过机器人就崩了"时发生，
/// 此时保留文件等下次启动再试，并明确告警，不静默吞掉。
pub async fn report_pending(data_dir: PathBuf, state: Arc<AppState>) {
    let Some(path) = pending(&data_dir) else {
        return;
    };

    let summary = match std::fs::read_to_string(&path) {
        Ok(text) => summarize(&text),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "读崩溃报告失败，跳过");
            return;
        }
    };

    wait_for_subscriber(&state).await;

    let store = Arc::clone(state.store());
    let bots = match tokio::task::spawn_blocking(move || store.list_bots()).await {
        Ok(Ok(bots)) => bots,
        Ok(Err(error)) => {
            tracing::warn!(%error, "列机器人失败，崩溃提示留到下次启动");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "列机器人的任务挂了，崩溃提示留到下次启动");
            return;
        }
    };

    let Some(bot) = bots.first() else {
        tracing::warn!(
            path = %path.display(),
            "上次运行崩溃过，但库里还没有机器人，日志外键插不进去；文件保留，下次启动再报"
        );
        return;
    };

    let at = state.clock().now();
    let line = format!("上次运行异常退出：{summary}（报告：{}）", path.display());

    if path.exists() {
        let bot_id = bot.id;
        // 写日志表走 `AppState::append_log`：壳只依赖 hbot-core + hbot-rpc，
        // 构造 `NewLog` 这件事留在 rpc 层（依赖方向由 `xtask deps` 钉着）。
        let state_for_store = Arc::clone(&state);
        let line_for_store = line.clone();
        let appended = tokio::task::spawn_blocking(move || {
            state_for_store.append_log(bot_id, "error", line_for_store)
        })
        .await;
        match appended {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, "崩溃提示写不进日志表");
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "写崩溃日志的任务挂了");
                return;
            }
        }

        // 事件同时发：面板靠它立刻显示；日志表那行留给下次打开时回放。
        let delivered = state.events().send(Event::RunLog {
            bot_id,
            level: LogLevel::Error,
            line,
            at,
        });
        tracing::info!(delivered, "已上报上次崩溃");
    }

    match mark_reported(&path) {
        Ok(reported) => tracing::info!(path = %reported.display(), "崩溃报告已标记为已上报"),
        Err(error) => tracing::warn!(%error, "标记崩溃报告失败，下次启动会重复提示"),
    }
}

/// 等 `EVENT_CHANNEL` 有订阅者上来（最多 [`SUBSCRIBER_WAIT_MS`]）。
async fn wait_for_subscriber(state: &Arc<AppState>) {
    let rounds = SUBSCRIBER_WAIT_MS / SUBSCRIBER_POLL_MS;
    for _ in 0..rounds {
        if state.events().receiver_count() > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(SUBSCRIBER_POLL_MS)).await;
    }
    tracing::warn!("等窗口订阅事件超时，崩溃提示仍会写日志表，但界面可能看不到");
}

/// 把报告正文压成一行摘要（日志行只有一行，不能塞整个 backtrace）。
fn summarize(text: &str) -> String {
    let mut line = String::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        // 跳过标题与 backtrace 段：摘要要的是"哪一行 panic 了"。
        if trimmed.is_empty()
            || trimmed.starts_with("hbot 崩溃报告")
            || trimmed.starts_with("backtrace:")
            || trimmed.starts_with("时间:")
            || trimmed.starts_with("版本:")
            || trimmed.starts_with("平台:")
            || trimmed.starts_with("线程:")
        {
            continue;
        }
        if !line.is_empty() {
            line.push_str(" / ");
        }
        line.push_str(trimmed);
        if line.chars().count() >= 200 {
            break;
        }
    }
    truncate_chars(&line, 240)
}

/// 按**字符**截断（按字节切会切碎 UTF-8）。
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn is_pending_name(name: &str) -> bool {
    name.starts_with(CRASH_PREFIX)
        && name.ends_with(CRASH_SUFFIX)
        && !name.ends_with(REPORTED_SUFFIX)
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "<非字符串 panic 载荷>".to_string()
}

fn platform() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// Unix 毫秒 → `YYYYMMDDTHHMMSSZ`（UTC）。
///
/// 手写而不用 `chrono`：只为一次格式化拉一个日期库不值当，而 `time` 也在
/// workspace 里没有。算法是 Howard Hinnant 的 civil_from_days。
fn utc_stamp(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);

    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// 自 1970-01-01 起的天数 → (年, 月, 日)。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hbot-desktop-crash-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ))
    }

    fn report(at_millis: i64) -> CrashReport {
        CrashReport {
            message: "boom".to_string(),
            location: Some("crates/hbot-desktop/src/lib.rs:10:5".to_string()),
            thread: "main".to_string(),
            backtrace: Some("  0: hbot_desktop::boom\n".to_string()),
            at_millis,
        }
    }

    #[test]
    fn utc_stamp_matches_known_instants() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(1_700_000_000_000), "20231114T221320Z");
        // 毫秒被丢掉，秒是下取整。
        assert_eq!(utc_stamp(1_700_000_000_999), "20231114T221320Z");
    }

    #[test]
    fn file_name_has_no_colons_and_sorts_by_time() {
        let early = report(1_700_000_000_000).file_name();
        let late = report(1_700_000_100_000).file_name();
        assert_eq!(early, "crash-20231114T221320Z.log");
        assert!(!early.contains(':'), "Windows 文件名里不能有冒号");
        assert!(early < late, "定宽时间戳必须能按字典序排时间");
    }

    #[test]
    fn render_carries_the_five_facts_a_report_needs() {
        let text = report(1_700_000_000_000).render();
        assert!(text.contains("时间: 20231114T221320Z"), "{text}");
        assert!(text.contains(concat!("版本: ", env!("CARGO_PKG_VERSION"))));
        assert!(text.contains(&format!("平台: {}", platform())));
        assert!(text.contains("线程: main"));
        assert!(text.contains("panic: boom"));
        assert!(text.contains("位置: crates/hbot-desktop/src/lib.rs:10:5"));
        assert!(text.contains("backtrace:"));
        assert!(
            !text.contains("PATH=") && !text.contains("APPDATA"),
            "不许 dump 环境变量"
        );
    }

    #[test]
    fn render_redacts_tokens() {
        let mut with_secret = report(1_700_000_000_000);
        with_secret.message =
            "connect failed: Authorization: Bearer sk-live-abcdef0123456789abcdef".to_string();
        let text = with_secret.render();
        assert!(
            !text.contains("sk-live-abcdef0123456789abcdef"),
            "token 不许出现在报告里: {text}"
        );
    }

    #[test]
    fn write_lands_in_the_logs_subdir_then_reports_once() {
        let dir = unique_dir("write");
        let written = report(1_700_000_000_000).write(&dir).expect("写报告");
        assert_eq!(
            written,
            dir.join(LOGS_DIR).join("crash-20231114T221320Z.log")
        );
        assert!(written.exists());

        // 第一次能看见。
        assert_eq!(pending(&dir).as_deref(), Some(written.as_path()));

        // 标记后不再重复提示，但文件还在。
        let reported = mark_reported(&written).expect("标记已上报");
        assert!(!written.exists());
        assert!(reported.exists());
        assert_eq!(
            reported.file_name().unwrap(),
            "crash-20231114T221320Z.reported.log"
        );
        assert_eq!(pending(&dir), None, "已上报的不能再提示");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_picks_the_newest_unreported_report() {
        let dir = unique_dir("pending");
        report(1_700_000_000_000).write(&dir).expect("写第一份");
        let newest = report(1_700_000_100_000).write(&dir).expect("写第二份");
        assert_eq!(pending(&dir).as_deref(), Some(newest.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_is_none_when_the_dir_does_not_exist() {
        assert_eq!(pending(&unique_dir("missing")), None);
    }

    #[test]
    fn summarize_keeps_panic_and_location_but_drops_noise() {
        let text = report(1_700_000_000_000).render();
        let summary = summarize(&text);
        assert!(summary.contains("panic: boom"), "{summary}");
        assert!(summary.contains("位置:"), "{summary}");
        assert!(!summary.contains("hbot 崩溃报告"), "标题是噪音");
        assert!(!summary.contains('\n'), "摘要必须是一行");
    }

    #[test]
    fn truncate_chars_never_splits_a_utf8_scalar() {
        let text = "崩溃报告".repeat(50);
        let cut = truncate_chars(&text, 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn install_writes_a_report_when_a_panic_happens() {
        let dir = unique_dir("hook");
        let saved = std::panic::take_hook();
        install(Some(dir.clone()));
        let panicked = std::panic::catch_unwind(|| panic!("boom-from-test"));
        std::panic::set_hook(saved);
        assert!(panicked.is_err());

        let found = pending(&dir).expect("panic 之后必须有报告文件");
        let text = std::fs::read_to_string(&found).expect("报告要能读出来");
        assert!(text.contains("boom-from-test"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
