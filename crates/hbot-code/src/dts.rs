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

    /* 设备能力面。
     *
     * **存在不等于被授权。** 这一层能不能用，由运行时在**每次调用**时决定：
     * ① 沙箱是 `read-only` 档 → 一律拒绝，且不碰 OS；② `hbot.json` 的
     * `permissions.device.sub` 没声明这项 → `HBOT_DEVICE_NOT_DECLARED`；
     * ③ 用户设置或设备会话没放行 → `HBOT_DEVICE_NOT_GRANTED` /
     * `HBOT_DEVICE_NOT_ENABLED` / `HBOT_DEVICE_EXPIRED` / `HBOT_DEVICE_REVOKED`。
     * 同一个 bot 的会话被撤销后，**下一次调用立刻**失败（不是下一个 isolate）。
     * 宿主没装配设备后端时抛 `HBOT_DEVICE_UNAVAILABLE`，不会静默返回空数组。
     *
     * 七个方法**都返回 Promise，失败一律是 rejection**，message 以
     * `HBOT_DEVICE_` 开头。 */
    readonly device: DeviceApi;

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

  /** `http.get` 的可选参数。 */
  export interface HttpGetOptions {
    /** 额外请求头；值不是字符串时整个调用被拒绝（不做隐式转换）。 */
    headers?: Record<string, string>;
    /** 本次请求的超时（毫秒）。超过 60000 按 60000 算，并且会被本次事件的剩余预算截断。 */
    timeoutMs?: number;
  }

  /** `http.get` 的响应。只有这三样：重定向链与 gzip 解压运行时都不处理。 */
  export interface HttpResponse {
    /** HTTP 状态码。3xx 原样返回，运行时不跟随重定向。 */
    status: number;
    /** 响应体（UTF-8）；超过上限（默认 256 KiB）时整次请求失败，不会给半截。 */
    body: string;
    /** 响应头，名字已转小写。 */
    headers: Record<string, string>;
  }

  /** `ctx.device.screen(options)` 的可选参数。 */
  export interface DeviceScreenOptions {
    /** 显示器序号；省略取主屏。超范围或不是非负整数则整次调用被拒绝。 */
    monitor?: number;
    /** 本帧字节上限（PNG 原始字节，不是 base64 长度）；默认 4 MiB，超过按 4 MiB 算。 */
    maxBytes?: number;
  }

  /** 一帧截图。 */
  export interface DeviceScreen {
    /** 物理像素宽。 */
    width: number;
    /** 物理像素高。 */
    height: number;
    /** PNG 字节的 base64 文本（不是 `Uint8Array`，隔离区没有 Buffer）。 */
    pngBase64: string;
    /** 这帧来自哪块屏（`monitor` 参数的解析结果）。 */
    monitor: number;
  }

  /** 一块可见窗口。 */
  export interface DeviceWindow {
    /** 平台窗口句柄。 */
    id: number;
    /** 标题。 */
    title: string;
    /** 进程名；输入闸按它比白名单。 */
    process: string;
    /** 是否是当前前台窗口。 */
    focused: boolean;
  }

  /** 设备能力面；每个方法都返回 Promise，失败一律 reject（`HBOT_DEVICE_` 开头）。
   *
   *  是否可用由运行时逐次判定，见 `MessageCtx.device` 的说明。 */
  export interface DeviceApi {
    /** 截一帧；`pngBase64` 超上限时整次失败，不会给半帧。 */
    screen(options?: DeviceScreenOptions): Promise<DeviceScreen>;
    /** 列出可见窗口。 */
    windows(): Promise<DeviceWindow[]>;
    /** 在物理像素坐标点一下；坐标可以是负数（多屏主屏左侧）。 */
    click(x: number, y: number, button?: 'left' | 'right' | 'middle'): Promise<void>;
    /** 输入一段文本。 */
    key(text: string): Promise<void>;
    /** 按一个组合键，例如 `['ctrl', 'c']`。 */
    keyCombo(keys: string[]): Promise<void>;
    /** 读剪贴板文本。 */
    clipboardRead(): Promise<string>;
    /** 写剪贴板文本；内容不会出现在任何异常或日志里。 */
    clipboardWrite(text: string): Promise<void>;
  }

  /** 注册入口，原样返回 config（只做校验，不包装）。 */
  export function defineBot(config: BotConfig): BotConfig;
  /** 写运行日志。 */
  export function log(message: string): void;
  /** 回复当前消息，等同 ctx.reply。 */
  export function reply(text: string): void;
  /** 等待若干毫秒，等同 ctx.sleep。 */
  export function sleep(ms: number): Promise<void>;
  /** HTTP 客户端。当前只有 GET，且 host 必须在 hbot.json 的 permissions.http 里。 */
  export const http: {
    /** 发一个 GET；失败时 reject，message 以 `HBOT_HTTP_DENIED` / `HBOT_HTTP_FAILED` 开头。 */
    get(url: string, options?: HttpGetOptions): Promise<HttpResponse>;
  };
  /** 设备能力，与 `ctx.device` 是同一份实现，等价于在事件外调用。
   *  声明了不等于被授权：能不能用由运行时逐次判定，见 `MessageCtx.device`。 */
  export const device: DeviceApi;
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
    use crate::runtime::device_api::DEVICE_BINDING_KEYS;
    use crate::runtime::host_api::{HOST_BINDING_KEYS, HOST_MODULE_SOURCE};

    /// `export function <name>` declarations in a piece of source.
    fn function_exports(source: &str) -> BTreeSet<String> {
        source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("export function "))
            .filter_map(|rest| rest.split_once('(').map(|(name, _)| name.trim().to_owned()))
            .collect()
    }

    /// `export const <name>` / `export let <name>` declarations in a piece of
    /// source.
    ///
    /// Value exports (`http`) are not functions, so
    /// [`function_exports`] cannot see them; both halves are needed before the
    /// module and the `d.ts` can be compared.
    fn value_exports(source: &str) -> BTreeSet<String> {
        source
            .lines()
            .map(str::trim)
            .filter_map(|line| {
                line.strip_prefix("export const ")
                    .or_else(|| line.strip_prefix("export let "))
            })
            .filter_map(|rest| {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
                    .collect();
                if name.is_empty() { None } else { Some(name) }
            })
            .collect()
    }

    /// Everything a module exports, by name, across both declaration forms.
    fn exported_names(source: &str) -> BTreeSet<String> {
        let mut names = function_exports(source);
        names.extend(value_exports(source));
        names
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

    /// `export function <name>` / `export const <name>` declarations inside
    /// `declare module 'hbot' { … }`.
    fn dts_exported_names() -> BTreeSet<String> {
        let start = HOST_DTS
            .find("declare module 'hbot' {")
            .expect("hbot.d.ts 应有 `declare module 'hbot'`");
        let block = &HOST_DTS[start..];
        let end = block.find("\n}\n").expect("declare 块应有收尾 `}`");
        exported_names(&block[..end])
    }

    /// The keys `install` sets on `globalThis.__hbot`, read from the real source
    /// rather than from a hand-kept list.
    fn installed_host_keys() -> BTreeSet<String> {
        include_str!("runtime/host_api.rs")
            .lines()
            .map(str::trim)
            .filter_map(|line| line.strip_prefix("host.set(\""))
            .filter_map(|rest| rest.split_once('"').map(|(name, _)| name.to_owned()))
            .collect()
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

    /// Property declarations (`botId: string;`) inside `MessageCtx`.
    ///
    /// Interface properties are the 4-space-indented `name: type;` lines; methods
    /// carry a `(` before the colon and are covered by [`message_ctx_functions`].
    fn message_ctx_properties() -> BTreeSet<String> {
        let start = HOST_DTS
            .find("export interface MessageCtx {")
            .expect("hbot.d.ts 应有 MessageCtx");
        let block = &HOST_DTS[start..];
        let end = block.find("\n  }").expect("MessageCtx 应有收尾 `}`");
        block[..end]
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("    ")?;
                let trimmed = rest.trim_start();
                // Skip the doc-comment lines above each property.
                if trimmed.starts_with('*') || trimmed.starts_with('/') {
                    return None;
                }
                let (name, after) = rest.split_once(':')?;
                if !after.trim_end().ends_with(';') || rest.contains('(') {
                    return None;
                }
                let name = name.trim();
                let is_property = !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
                is_property.then(|| name.to_owned())
            })
            .collect()
    }

    /// The object-valued host surfaces `MessageCtx` declares (`readonly device:
    /// DeviceApi;`), as opposed to the inbound platform fields.
    ///
    /// The split is the declaration convention: a property whose declared type
    /// starts with an uppercase letter is a host API object, one with a lowercase
    /// type (`string`, `number`, `unknown`) is inbound data. [`message_ctx_functions`]
    /// covers the callable half, so both are needed to describe what
    /// `build_event` puts on `ctx`.
    fn message_ctx_namespaces() -> BTreeSet<String> {
        let start = HOST_DTS
            .find("export interface MessageCtx {")
            .expect("hbot.d.ts 应有 MessageCtx");
        let block = &HOST_DTS[start..];
        let end = block.find("\n  }").expect("MessageCtx 应有收尾 `}`");
        block[..end]
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("    ")?;
                if rest.contains('(') {
                    return None;
                }
                let (name, after) = rest.split_once(':')?;
                if !after.trim_end().ends_with(';') {
                    return None;
                }
                let ty = after.trim().trim_end_matches(';').trim();
                let is_namespace = ty.chars().next().is_some_and(|c| c.is_ascii_uppercase());
                // `readonly` is part of the declaration, not the name.
                let name = name.trim().strip_prefix("readonly").unwrap_or(name.trim());
                let name = name.trim();
                let is_name = !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
                (is_namespace && is_name).then(|| name.to_owned())
            })
            .collect()
    }

    /// The keys `runtime/device_api.rs` installs on the `ctx.device` object, read
    /// from the real source.
    ///
    /// The scan targets that function's `entries` literal — the one place a key is
    /// paired with its host function — rather than the `object.set(..)` calls,
    /// because rustfmt wraps the long ones and a line-oriented scan would silently
    /// lose them.
    fn installed_device_keys() -> BTreeSet<String> {
        let source = include_str!("runtime/device_api.rs");
        let start = source
            .find("let entries: [(&str, Function<'js>);")
            .expect("device_api.rs 应有 entries 表");
        let block = &source[start..];
        let end = block.find("\n    ]").expect("entries 表应有收尾 `]`");
        block[..end]
            .lines()
            .filter_map(|line| line.trim().strip_prefix("(\""))
            .filter_map(|rest| rest.split_once('"').map(|(name, _)| name.to_owned()))
            .collect()
    }

    /// Method declarations inside `export interface DeviceApi {`.
    fn dts_device_methods() -> BTreeSet<String> {
        let start = HOST_DTS
            .find("export interface DeviceApi {")
            .expect("hbot.d.ts 应有 DeviceApi");
        let block = &HOST_DTS[start..];
        let end = block.find("\n  }").expect("DeviceApi 应有收尾 `}`");
        block[..end].lines().filter_map(method_name).collect()
    }

    /// Method keys of `export const device = { … }` in the runtime module.
    fn module_device_methods() -> BTreeSet<String> {
        let start = HOST_MODULE_SOURCE
            .find("export const device = {")
            .expect("HOST_MODULE_SOURCE 应有 device 命名空间");
        let block = &HOST_MODULE_SOURCE[start..];
        let end = block
            .find("\n};")
            .expect("HOST_MODULE_SOURCE 的 device 对象应有收尾 `};`");
        block[..end].lines().filter_map(method_name).collect()
    }

    /// A `name(...)` line's name, when it is a plain identifier.
    fn method_name(line: &str) -> Option<String> {
        let (name, _) = line.trim().split_once('(')?;
        let name = name.trim();
        let is_method = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
        is_method.then(|| name.to_owned())
    }

    #[test]
    fn hbot_dts_exports_match_the_runtime_module() {
        let runtime = exported_names(HOST_MODULE_SOURCE);
        let types = dts_exported_names();

        assert!(
            !runtime.is_empty(),
            "HOST_MODULE_SOURCE 里一个导出都没解析到，扫描逻辑失效了"
        );
        assert_eq!(
            runtime, types,
            "`hbot` 模块的导出与 hbot.d.ts 声明不一致：运行时 {runtime:?}，类型 {types:?}"
        );
    }

    #[test]
    fn hbot_dts_default_export_matches_the_named_exports() {
        let named = exported_names(HOST_MODULE_SOURCE);
        let default = runtime_default_exports();

        assert_eq!(
            named, default,
            "`export default {{...}}` 的键与具名导出不一致：{named:?} vs {default:?}"
        );
    }

    #[test]
    fn host_bindings_match_the_module_and_the_dts() {
        // The third leg of the drift test: what `install` actually puts on
        // `globalThis.__hbot`, what the `hbot` module re-exports, and what
        // hbot.d.ts declares. `defineBot` is module-level validation only, so it
        // is not a host binding.
        let installed = installed_host_keys();
        assert!(
            !installed.is_empty(),
            "host_api.rs 里没解析到 `host.set(\"...\")`，扫描逻辑失效了"
        );
        assert_eq!(
            installed,
            HOST_BINDING_KEYS
                .iter()
                .map(|key| (*key).to_owned())
                .collect::<BTreeSet<String>>(),
            "HOST_BINDING_KEYS 与 install 实际注入的键不一致"
        );

        let mut module = exported_names(HOST_MODULE_SOURCE);
        module.remove("defineBot");
        assert_eq!(
            installed, module,
            "`__hbot` 上注入的键与 `hbot` 模块导出的键不一致：运行时 {installed:?}，模块 {module:?}"
        );

        let mut declared = dts_exported_names();
        declared.remove("defineBot");
        assert_eq!(
            installed, declared,
            "`__hbot` 上注入的键与 hbot.d.ts 声明的键不一致：运行时 {installed:?}，类型 {declared:?}"
        );

        // And `http` must be the namespace shape the bridge installs, not a
        // function that happens to exist.
        assert!(
            HOST_DTS.contains("export const http: {") && HOST_DTS.contains("get(url: string"),
            "hbot.d.ts 没有把 http 声明成带 get 的对象"
        );
        assert!(
            HOST_MODULE_SOURCE.contains("get(url, options)"),
            "HOST_MODULE_SOURCE 的 http.get 参数与 d.ts 不一致"
        );
    }

    #[test]
    fn message_ctx_functions_match_the_event_object() {
        let ctx = ctx_function_keys();
        // `ctx.device` is a namespace object, not a method, so the callable half of
        // `MessageCtx` is `message_ctx_functions()` plus the namespace properties.
        // Its members get their own four-way check below.
        let mut declared = message_ctx_functions();
        declared.extend(message_ctx_namespaces());

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
    fn device_surface_matches_the_runtime_object_and_the_module() {
        // The four-way drift check `docs/09` §8.3 asks for: what
        // `runtime/device_api.rs` puts on `ctx.device`, what the `hbot` module's
        // `device` namespace forwards to, what hbot.d.ts declares, and the
        // hand-kept key list. Nothing here is copied by hand except
        // `DEVICE_BINDING_KEYS`, which is itself checked against the source.
        let installed = installed_device_keys();
        assert!(
            !installed.is_empty(),
            "device_api.rs 里没解析到 `object.set(\"...\")`，扫描逻辑失效了"
        );
        assert_eq!(
            installed,
            DEVICE_BINDING_KEYS
                .iter()
                .map(|key| (*key).to_owned())
                .collect::<BTreeSet<String>>(),
            "DEVICE_BINDING_KEYS 与 device_api.rs 实际注入的键不一致"
        );

        let module = module_device_methods();
        assert_eq!(
            installed, module,
            "`ctx.device` 的方法与 `hbot` 模块转发的方法不一致：运行时 {installed:?}，模块 {module:?}"
        );

        let declared = dts_device_methods();
        assert_eq!(
            installed, declared,
            "`ctx.device` 的方法与 hbot.d.ts 的 DeviceApi 不一致：运行时 {installed:?}，类型 {declared:?}"
        );
    }

    #[test]
    fn the_device_surface_is_declared_but_not_promised() {
        // §8.3: `ctx.device` 存在不代表被授权 —— 这句话必须在 d.ts 里写清楚，
        // 否则照着类型写代码的人会以为拿到类型就等于拿到权限。
        assert!(
            HOST_DTS.contains("readonly device: DeviceApi;"),
            "MessageCtx 应声明 readonly device"
        );
        assert!(
            HOST_DTS.contains("export const device: DeviceApi;"),
            "hbot 模块应声明 device"
        );
        for hint in [
            "HBOT_DEVICE_UNAVAILABLE",
            "HBOT_DEVICE_NOT_DECLARED",
            "read-only",
        ] {
            assert!(
                HOST_DTS.contains(hint),
                "hbot.d.ts 没有说明设备能力的失败面，缺少 `{hint}`"
            );
        }
    }

    #[test]
    fn ctx_carries_exactly_the_documented_inbound_fields() {
        // §6.3 的字段清单是声明侧的唯一真源；生产/回放真正输出的键由
        // `crates/hbot-rpc/tests/event_shape_dts.rs` 拿这份 d.ts 与
        // `hbot_bot::event_json` 的输出直接比对（hbot-code 只允许依赖 hbot-core，
        // 不能在这里调用事件拼装函数）。
        let documented: BTreeSet<String> = [
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
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();

        assert_eq!(
            message_ctx_properties(),
            documented,
            "MessageCtx 的字段与 §6.3 的文档清单不一致：新增/删除字段必须同时更新 \
             event_json 与文档"
        );
    }

    #[test]
    fn hbot_dts_does_not_promise_unimplemented_apis() {
        // §6.3: 未实现的 API 不得出现在 hbot.d.ts —— Agent 会照着写，然后在
        // 运行时报「不是函数」。This list is the negative half of the drift test.
        //
        // `http.get` shipped (S2), so the negative entry is no longer "any `http.`"
        // — it is the parts of HTTP that are still missing: POST, redirects, fetch
        // with an init bag. Keep this list precise; a broad entry would have to be
        // deleted instead of narrowed, and would take the whole check with it.
        for absent in [
            "onStart",
            "onStop",
            "onSchedule",
            "send(",
            "kv.",
            "http.post",
            "http.fetch",
            "http.request",
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
