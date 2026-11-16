//! Generates a project's `hbot.d.ts` — the type surface Monaco and the agent see.
//!
//! Three places describe the same contract and must agree
//! (`docs/02-architecture.md` §6.3，新增宿主 API 的顺序是硬规则):
//!
//! 1. [`crate::runtime::host_api::HOST_MODULE_SOURCE`] — what the virtual `hbot`
//!    module actually exports at runtime;
//! 2. `build_event` in `src/runtime/isolate.rs` — which host functions the
//!    `ctx` object carries;
//! 3. [`HOST_DTS`] here — what the editor promises.
//!
//! The tests at the bottom of this file read all three **from source** and fail
//! when one side grows a name the others do not have, so an API cannot be
//! half-shipped: a `d.ts` entry without a runtime export is a red squiggle in
//! Monaco followed by "not a function" at runtime, and a runtime export without
//! a `d.ts` entry makes the agent's correct code look wrong.
//!
//! `hbot.d.ts` is *generated*: it lives in the user's project so the editor can
//! see it, and `AGENTS.md` in the scaffold tells everyone not to hand-edit it.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::error::CodeError;

/// File name written into the project root.
pub const HOST_DTS_FILE: &str = "hbot.d.ts";

/// The generated `hbot.d.ts` text.
///
/// Kept as one `const` so the drift tests can compare it against the runtime
/// without a second copy of the truth: there is exactly one expected text — this
/// one — and it is checked, not duplicated.
pub const HOST_DTS: &str = r#"// hbot.d.ts —— 由 hbot 生成，请勿手改。
//
// 生成源：crates/hbot-code/src/dts.rs（类型）
//         crates/hbot-code/src/runtime/host_api.rs 的 HOST_MODULE_SOURCE（运行时实现）
//         crates/hbot-code/src/runtime/isolate.rs 的 build_event（ctx 形状）
// 三者不一致时 crates/hbot-code 的单元测试会红；`hbot check` 也会报不一致。
//
// 项目里只有 'hbot' 一个裸模块名是合法的；其余依赖只能是相对 import（'./x'、'../lib/y'）。
// 运行时没有 fs / net / process / require / eval / 动态 import()。

declare module 'hbot' {
  /** `onMessage` 收到的上下文：入站消息字段平铺 + 本次事件可用的宿主函数。 */
  export interface MessageCtx {
    /** 机器人 id。 */
    botId: string;
    /** 会话 id；同一会话串行执行。 */
    conversationId: string;
    /** 消息 id，幂等键。注意语义是 **at-least-once**：进程崩溃后启动补偿可能把同一条再执行一次，
     *  所以有副作用的操作要用它自己判重，不要假设只执行一次。
     *  **这不是平台消息 id**（平台消息 id 见 `externalId`）：它是本仓库 `inbound` 表的行主键，
     *  同一事件重放时取值不变，但拿去问平台"这条是哪条"是不对的。 */
    messageId: string;
    /** 平台侧消息 id。
     *  **平台没给 `message_id` 时，这里是本地生成的兜底幂等键**（形如
     *  `"{user_id}-{time}-{fnv1a64:016x}"`，见 `hbot_adapters::platforms::onebot::
     *  fallback_external_id`），**不是平台消息 id**：别拿它去做平台侧的引用回复
     *  （OneBot 的 `reply.data.id` 必须是真 `message_id`，否则整条消息发不出去）。
     *  要判断是不是兜底键，用 `hbot_adapters::platforms::onebot::is_synthetic_external_id`
     *  （框架内部已经用它决定发不发引用段，业务代码一般不需要再判一次）。 */
    externalId: string;
    /** 发送者平台 id。 */
    senderId: string;
    /** 已归一化的纯文本，@、图片、表情都换成了占位符。
     *  没有文本的入站（例如纯图片）是空串 `''`，不会是 `null` 或 `undefined`。 */
    text: string;
    /** 会话类型。 */
    kind: 'private' | 'group' | 'channel';
    /** 平台侧会话 id。 */
    conversationExternalId: string;
    /** 入站时间，unix 毫秒。 */
    receivedAt: number;
    /** 原始平台事件；形状随平台变化，别当契约用。 */
    payload: unknown;

    /** 回复这条消息，绑定 reply_to。 */
    reply(text: string): void;
    /** 写一条运行日志，进 RunLog，不进聊天。 */
    log(message: string): void;
    /** 等待若干毫秒；会被本次事件的预算截断。 */
    sleep(ms: number): Promise<void>;
  }

  /** 入口配置。当前只认 onMessage；生命周期钩子与定时器尚未实现，故不声明。 */
  export interface BotConfig {
    /** 收到消息时调用；可以返回 Promise。 */
    onMessage(ctx: MessageCtx): void | Promise<void>;
  }

  /** 注册入口，原样返回 config（只做校验，不包装）。 */
  export function defineBot(config: BotConfig): BotConfig;
  /** 写运行日志。 */
  export function log(message: string): void;
  /** 回复当前消息，等同 ctx.reply。 */
  export function reply(text: string): void;
  /** 等待若干毫秒，等同 ctx.sleep。 */
  export function sleep(ms: number): Promise<void>;
}
"#;

/// The generated `hbot.d.ts` text.
#[must_use]
pub fn host_dts() -> &'static str {
    HOST_DTS
}

/// Write `<dir>/hbot.d.ts`, replacing any previous copy.
///
/// The write is `tmp → fsync → rename` so an editor or watcher never observes a
/// half-written type file (`AGENTS.md` §7.6: 不要先覆盖再校验). On Windows the
/// rename can fail with `EPERM` while another process holds the file open; that
/// error is returned as [`CodeError::Io`] rather than being swallowed.
///
/// # Errors
///
/// [`CodeError::Io`] when the directory is missing or the file cannot be
/// replaced.
pub fn write_host_dts(dir: &Path) -> Result<PathBuf, CodeError> {
    let path = dir.join(HOST_DTS_FILE);
    write_atomic(&path, HOST_DTS)?;
    Ok(path)
}

/// Whether `<dir>/hbot.d.ts` is present and byte-identical to [`HOST_DTS`].
///
/// `Err` is only for "could not read"; a missing file is `Ok(false)` so callers
/// can distinguish "stale" from "unreadable".
///
/// # Errors
///
/// [`CodeError::Io`] when the file exists but cannot be read.
pub fn is_host_dts_current(dir: &Path) -> Result<bool, CodeError> {
    let path = dir.join(HOST_DTS_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text == HOST_DTS),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(CodeError::Io { path, source }),
    }
}

/// Write `contents` to `path` atomically (temp file in the same directory).
fn write_atomic(path: &Path, contents: &str) -> Result<(), CodeError> {
    let io = |path: &Path, source: std::io::Error| CodeError::Io {
        path: path.to_owned(),
        source,
    };

    let directory = path.parent().unwrap_or(Path::new("."));
    let name = match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => String::from(HOST_DTS_FILE),
    };
    // Same directory (rename cannot cross filesystems) and unique per process,
    // so two IDE windows writing the same project do not clobber each other's
    // temp file.
    let temporary = directory.join(format!(".{name}.tmp-{}", std::process::id()));

    {
        let mut file = fs::File::create(&temporary).map_err(|source| io(&temporary, source))?;
        file.write_all(contents.as_bytes())
            .map_err(|source| io(&temporary, source))?;
        // Durable before the rename: a crash mid-write must leave the old file,
        // never a truncated new one.
        file.sync_all().map_err(|source| io(&temporary, source))?;
    }

    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(source) => {
            let _ = fs::remove_file(&temporary);
            Err(io(path, source))
        }
    }
}

#[cfg(test)]
mod drift {
    use std::collections::BTreeSet;

    use super::*;
    use crate::runtime::host_api::HOST_MODULE_SOURCE;

    /// `export function <name>` declarations in a piece of source.
    fn function_exports(source: &str) -> BTreeSet<String> {
        source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("export function "))
            .filter_map(|rest| rest.split_once('(').map(|(name, _)| name.trim().to_owned()))
            .collect()
    }

    /// Keys of the runtime module's `export default { a, b, c }` object.
    fn runtime_default_exports() -> BTreeSet<String> {
        let start = HOST_MODULE_SOURCE
            .find("export default {")
            .expect("HOST_MODULE_SOURCE 应有 `export default { ... }`");
        let rest = &HOST_MODULE_SOURCE[start + "export default {".len()..];
        let end = rest
            .find('}')
            .expect("HOST_MODULE_SOURCE 的 default 导出应有收尾 `}`");
        rest[..end]
            .split(',')
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// `export function <name>` declarations inside `declare module 'hbot' { … }`.
    fn dts_function_exports() -> BTreeSet<String> {
        let start = HOST_DTS
            .find("declare module 'hbot' {")
            .expect("hbot.d.ts 应有 `declare module 'hbot'`");
        let block = &HOST_DTS[start..];
        let end = block.find("\n}\n").expect("declare 块应有收尾 `}`");
        function_exports(&block[..end])
    }

    /// The keys `build_event` copies onto the `ctx` object, read from the real
    /// source so a new host function cannot be added in only one place.
    fn ctx_function_keys() -> BTreeSet<String> {
        let source = include_str!("runtime/isolate.rs");
        source
            .lines()
            .map(str::trim)
            .filter_map(|line| line.strip_prefix("event.set(\""))
            .filter_map(|rest| rest.split_once('"').map(|(name, _)| name.to_owned()))
            .collect()
    }

    /// Method declarations (`reply(text: string): void;`) inside `MessageCtx`.
    fn message_ctx_functions() -> BTreeSet<String> {
        let start = HOST_DTS
            .find("export interface MessageCtx {")
            .expect("hbot.d.ts 应有 MessageCtx");
        let block = &HOST_DTS[start..];
        let end = block.find("\n  }").expect("MessageCtx 应有收尾 `}`");
        block[..end]
            .lines()
            .filter_map(|line| {
                // Methods are the 4-space-indented lines that carry a `(`
                // signature; interface *properties* have none.
                let rest = line.strip_prefix("    ")?;
                let (name, _) = rest.split_once('(')?;
                let name = name.trim();
                let is_method = !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
                if is_method {
                    Some(name.to_owned())
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn hbot_dts_exports_match_the_runtime_module() {
        let runtime = function_exports(HOST_MODULE_SOURCE);
        let types = dts_function_exports();

        assert!(
            !runtime.is_empty(),
            "HOST_MODULE_SOURCE 里一个 `export function` 都没解析到，扫描逻辑失效了"
        );
        assert_eq!(
            runtime, types,
            "`hbot` 模块的导出与 hbot.d.ts 声明不一致：运行时 {runtime:?}，类型 {types:?}"
        );
    }

    #[test]
    fn hbot_dts_default_export_matches_the_named_exports() {
        let named = function_exports(HOST_MODULE_SOURCE);
        let default = runtime_default_exports();

        assert_eq!(
            named, default,
            "`export default {{...}}` 的键与具名导出不一致：{named:?} vs {default:?}"
        );
    }

    #[test]
    fn message_ctx_functions_match_the_event_object() {
        let ctx = ctx_function_keys();
        let declared = message_ctx_functions();

        assert!(
            !ctx.is_empty(),
            "isolate.rs 里没解析到 `event.set(\"...\")`，扫描逻辑失效了"
        );
        assert_eq!(
            ctx, declared,
            "ctx 上挂的宿主函数与 MessageCtx 声明不一致：运行时 {ctx:?}，类型 {declared:?}"
        );
    }

    #[test]
    fn ctx_carries_exactly_the_documented_inbound_fields() {
        // Pinned by `docs/02-architecture.md` §6.3 and produced by
        // `hbot-rpc/src/app/runtime/sandbox_handler.rs`; the d.ts must say
        // `string` where the JSON carries a string, etc. Field *names* are only
        // checkable here against the documented set, because the producer lives
        // in another crate (a shared const there is the M1 follow-up).
        for field in [
            "botId",
            "conversationId",
            "messageId",
            "externalId",
            "senderId",
            "text",
            "kind",
            "conversationExternalId",
            "receivedAt",
            "payload",
        ] {
            assert!(
                HOST_DTS.contains(field),
                "MessageCtx 缺少文档规定的字段 `{field}`"
            );
            assert_eq!(
                HOST_DTS.matches(&format!("    {field}:")).count(),
                1,
                "MessageCtx 里 `{field}` 应恰好声明一次"
            );
        }
    }

    #[test]
    fn hbot_dts_does_not_promise_unimplemented_apis() {
        // §6.3: 未实现的 API 不得出现在 hbot.d.ts —— Agent 会照着写，然后在
        // 运行时报「不是函数」。This list is the negative half of the drift test.
        for absent in [
            "onStart",
            "onStop",
            "onSchedule",
            "send(",
            "kv.",
            "http.",
            "fetch(",
            "schedule(",
        ] {
            assert!(
                !HOST_DTS.contains(absent),
                "hbot.d.ts 声明了未实现的 API：`{absent}`"
            );
        }
    }

    #[test]
    fn writing_the_dts_is_idempotent_and_atomic() {
        let directory = std::env::temp_dir().join(format!(
            "hbot-dts-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&directory).expect("建临时目录");

        let path = write_host_dts(&directory).expect("写 hbot.d.ts");
        assert_eq!(std::fs::read_to_string(&path).expect("读回"), HOST_DTS);
        assert!(is_host_dts_current(&directory).expect("检查"), "应已是最新");

        // A second write must replace, not append or fail on an existing file.
        write_host_dts(&directory).expect("覆盖写");
        assert_eq!(std::fs::read_to_string(&path).expect("读回"), HOST_DTS);

        // No temp files left behind.
        let leftovers = std::fs::read_dir(&directory)
            .expect("列目录")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0, "临时文件没有清理");

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_hand_edited_dts_is_reported_as_stale() {
        let directory = std::env::temp_dir().join(format!(
            "hbot-dts-stale-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&directory).expect("建临时目录");

        assert!(
            !is_host_dts_current(&directory).expect("缺失不算错误"),
            "没有文件时应报 stale"
        );

        std::fs::write(directory.join(HOST_DTS_FILE), "// 手改过\n").expect("写");
        assert!(
            !is_host_dts_current(&directory).expect("可读"),
            "手改应报 stale"
        );

        write_host_dts(&directory).expect("重写");
        assert!(is_host_dts_current(&directory).expect("检查"));

        std::fs::remove_dir_all(&directory).ok();
    }
}
