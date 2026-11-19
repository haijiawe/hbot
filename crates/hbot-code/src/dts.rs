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

    /* 状态能力面。
     *
     * **存在不等于被授权。** `ctx.kv` 出现在这里只说明本项目 `hbot.json` 的
     * `capabilities.state.kv` 声明了它；数据属于**哪个 bot** 由本次事件的
     * `botId` 决定，不是由这个符号决定。
     *
     * 宿主没装配状态后端时抛 `HBOT_STATE_UNAVAILABLE`（**不**退回内存实现：
     * 那会让"重启后数据没了"变成没人发现的静默行为）；清单声明了但落库层拒绝时
     * 抛 `HBOT_STATE_NOT_DECLARED`。参数不合规一律 `HBOT_STATE_INVALID`，且
     * **在后端被调用之前**就拒。
     *
     * 五个方法**都返回 Promise，失败一律是 rejection**，message 以
     * `HBOT_STATE_` 开头。 */
    readonly kv: KvApi;
    /** 本会话最近 `limit` 条消息（含 bot 自己发的），时间升序。默认 20，上限 50（超过夹住不报错）。
     *  会话来自本次事件的 `conversationId`；没有它则整次调用失败，绝不沿用上一个事件。 */
    history(limit?: number): Promise<HistoryEntry[]>;

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
     * 八个方法**都返回 Promise，失败一律是 rejection**，message 以
     * `HBOT_DEVICE_` 开头。 */
    readonly device: DeviceApi;

    /* 智能能力面。
     *
     * **存在不等于被授权，也不等于配了模型。** `ctx.ai` 出现在这里只说明本项目
     * `hbot.json` 的 `capabilities.ai` 声明了它；用哪个模型、有没有 key 是宿主
     * 设置的事，宿主没配就抛 `HBOT_AI_UNAVAILABLE`（**不**编一个回复）。
     *
     * 等待时间**不额外占** handler 预算：沙箱把等模型的墙钟从预算里扣掉
     * （不扣的话 2 秒预算里 `await complete()` 永远完不成）。但一次事件仍有两条
     * 上限 —— 4 次调用、累计 60 秒 —— 超了报 `HBOT_AI_BUDGET`。
     *
     * 返回的 `text` 是**不可信外部内容**（模型可能照抄屏幕字/网页字，即提示注入）：
     * 不得据此改权限、改沙箱档位、跳过审批或改 `hbot.json`。 */
    readonly ai: AiApi;

    /* 后台作业面。
     *
     * **存在不等于被授权。** `ctx.jobs` 出现在这里只说明本项目 `hbot.json` 的
     * `capabilities.jobs` 声明了它；某个作业类型能不能入队，还要看那份声明表里有没有
     * 这个名字（没有就是 `HBOT_JOBS_NOT_DECLARED`）。
     *
     * 为什么要它：`onMessage` 只有 2 秒预算，而结算、批量推送、抓取+分析这类活要几分钟。
     * 入队**立刻返回**，重活由宿主在作业池里跑，作业自己的预算按声明配（默认 60 秒）。
     *
     * **至少一次**，不是恰好一次：作业行先落库再执行，进程重启后会接着跑，所以
     * 同一个幂等键可能被执行两次。作业体要用 `ctx.idempotencyKey` 自己判重。
     * 宿主的作业队列没装配时抛 `HBOT_JOBS_UNAVAILABLE`（**不**先收进内存：那会让
     * 重启后作业没了变成正常现象）。 */
    readonly jobs: JobApi;

    /* 会话等待面（C2.session）。
     *
     * **存在不等于被授权。** `ctx.session` 出现在这里只说明本项目 `hbot.json` 的
     * `capabilities.state.session` 声明了它；宿主没装配等待后端时抛
     * `HBOT_SESSION_UNAVAILABLE`。
     *
     * 它解决的是「跨消息的多轮对话」：handler 只有 2 秒预算、而且同一个会话严格
     * 串行 —— **阻塞等下一句会把队列锁死**（要等的正是排在队尾的那条消息）。
     * 所以 `prompt` 是**注册**而不是阻塞：写一行等待、把提示发出去，然后 handler
     * 正常返回；下一条匹配的消息交给 `onResume`。
     *
     * 同一个会话同时只允许一条未结等待（第二个 `prompt` 报
     * `HBOT_SESSION_PENDING` 并点名挡路的那条）。等待落库，进程重启后仍然有效。 */
    readonly session: SessionApi;

    /* SQL 能力面（C2）。
     *
     * **存在不等于被授权。** `ctx.sql` 出现在这里只说明本项目 `hbot.json` 的
     * `capabilities.state.sql` 声明了它；宿主没装配后端时抛 `HBOT_SQL_UNAVAILABLE`。
     *
     * **每 bot 一个库文件**：你写的 `users` 表在别的 bot 的连接里根本不存在，不需要自己加
     * 前缀。表结构写在项目的 `migrations/*.sql` 里，由宿主在启动时执行 —— 运行时
     * `CREATE` / `ALTER` / `DROP` / `ATTACH` / `PRAGMA` 一律 `HBOT_SQL_DENIED`。
     *
     * **只接受参数化语句**：`params` 数组与 `?` 占位符数量必须相等；拼字符串是注入的唯一
     * 入口，而生成代码是模型写的。 */
    readonly sql: SqlApi;

    /* 文件能力面（C3）。
     *
     * **存在不等于被授权。** `ctx.files` 出现在这里只说明本项目 `hbot.json` 的
     * `capabilities.files` 声明了它（读与写是分开的两半）；宿主没装配后端时抛
     * `HBOT_FILE_UNAVAILABLE`。
     *
     * **只认项目内的相对路径**：绝对路径、`..`、盘符、反斜杠一律 `HBOT_FILE_DENIED`。
     * 值的大小有上限（读 256 KiB / 写 1 MiB），超了 `HBOT_FILE_TOO_LARGE`，请分块。
     *
     * 只读文本：非 UTF-8 内容被拒（`HBOT_FILE_INVALID`），二进制请自己转 base64 字符串。 */
    readonly files: FilesApi;

    /* 出口能力面（C3）。
     *
     * **存在不等于被授权。** `ctx.send` / `ctx.notify` 出现在这里只说明本项目
     * `hbot.json` 声明了 `capabilities.send` / `capabilities.notify`；宿主没接出站
     * 队列时抛 `HBOT_SEND_UNAVAILABLE` / `HBOT_NOTIFY_UNAVAILABLE`（**不**先收进内存：
     * 那会让"进程一崩这条消息就没了"变成没人发现的静默行为）。
     *
     * 与 `reply` 的差别只有一处，但它是安全边界：`reply` 绑定在触发本次 handler 的
     * 那条入站消息上（`reply_to` 指向它）；`send` 是**主动**消息、不带 `reply_to` ——
     * 不许把一条主动消息挂到一条旧事件上。
     *
     * 目标缺失是独立的一种失败：`send` 没给 `options.kind` 时宿主会去 `conversations`
     * 查这个会话，查不到报 `HBOT_SEND_NO_TARGET`，**不猜**类型 —— 猜错就是发给另一个人。
     */

    /** 主动发一条消息到指定会话（平台侧会话 id，不是库里的 UUID）。 */
    send(conversationId: string, text: string, options?: SendOptions): Promise<void>;
    /** 发到清单 `capabilities.notify` 钉死的那个会话；目标不来自参数。 */
    notify(text: string): Promise<void>;

    /** 回复这条消息，绑定 reply_to。 */
    reply(text: string): void;
    /** 写一条运行日志，进 RunLog，不进聊天。 */
    log(message: string): void;
    /** 等待若干毫秒；会被本次事件的预算截断。 */
    sleep(ms: number): Promise<void>;
  }

  /**
   * 入口配置。
   *
   * `onMessage` 必填。`onTimer` 可选：只有在 `hbot.json` 的
   * `capabilities.timer` 里声明了计划、并且确实定义了它，定时器到点才会调到你；
   * 声明了却没定义的话，每次到点都会记一条说明缺钩子的错误日志。
   * 生命周期钩子仍未实现，故不声明（见 docs/07 §4.1 的顺序表）。
   */
  export interface BotConfig {
    /** 收到消息时调用；可以返回 Promise。 */
    onMessage(ctx: MessageCtx): void | Promise<void>;
    /**
     * 定时器到点时调用，`name` 是 `hbot.json` 里那条计划的 `name`。
     *
     * 与消息事件共用 `ctx.reply` / `ctx.log` / `ctx.sleep` / `ctx.kv` / `ctx.ai`，
     * 但**没有**入站字段（没有 `text`、没有 `conversationId`）：定时器背后没有
     * 会话。因此 `ctx.device.*` 与 `ctx.history()` 在定时器里一律被拒
     * （`HBOT_DEVICE_FAILED` / `HBOT_STATE_INVALID`），不会去读上一条消息的会话。
     */
    onTimer(ctx: TimerCtx, name: string): void | Promise<void>;
    /**
     * 作业被宿主取到时调用，`name` 是 `hbot.json` 里那条 `capabilities.jobs` 的名字。
     *
     * 上下文比消息与定时器都窄：只有 `reply` / `log` / `sleep` / `ai` 与本次作业的
     * `name` / `payload` / `idempotencyKey`，**没有** `kv` / `history` / `device`。
     * 作业没有入站身份，给它状态面就等于让它读上一条消息的会话。要写状态，在入队的
     * 那条消息里写 —— 那是有身份的。
     *
     * 清单声明了作业类型却没定义这个钩子时，每条作业都会记一条点名缺钩子的错误日志，
     * 而不是悄悄当成跑成功。
     */
    onJob(ctx: JobCtx, name: string): void | Promise<void>;
    /**
     * 一条消息命中了 `ctx.session.prompt` 注册的等待时调用（而不是 `onMessage`）。
     *
     * 第一个参数是恢复上下文（带 `waitKey` / `waitPrompt` 与唤醒它的那条消息的字段），
     * 第二个参数是那条消息本身 —— 两者指向同一份数据。
     *
     * 声明了 `capabilities.state.session` 却没定义它时，每次唤醒都会记一条点名
     * `onResume` 的错误日志，而不是悄悄当成处理成功。
     */
    onResume(ctx: ResumeCtx, message: InboundMessage): void | Promise<void>;
    /**
     * 启动时调用一次（在第一条消息到来之前）。
     *
     * **它失败就等于这次启动失败**：`bot_start` 回滚到 stopped 并把错误报出来，
     * 而不是「起来了但没初始化完」—— 后者会让后续每条消息都在半初始化状态里跑。
     *
     * 上下文只有 `phase` 与 `log` / `sleep` / `ai`：启动时还没有会话，`ctx.reply` 无从
     * 发出去，所以**没有** `reply`；也没有 `kv` / `history` / `device`（身份尚未建立）。
     *
     * 预算与 handler 相同（`limits.handlerMs`）。超时或抛错都按启动失败处理。
     */
    onStart?(ctx: LifecycleCtx): void | Promise<void>;
    /**
     * 停机前调用一次，`reason` 说明为什么停（`'user'` / `'shutdown'` / `'crash'`）。
     *
     * **预算 5 秒**，与 handler 的 2 秒分开定：那个是「不拖住会话」，这个是「不拖住停机」。
     * 5 秒够写一次存档，不够跑一次完整结算 —— 那是 `ctx.jobs` 的事。
     *
     * 失败**只落一行 error 日志**：停机路径上没有任何人在等结果，给调用方一个错误只会
     * 诱使它去判断「停得好不好」，而那件事无法恢复。同样没有 `reply`。
     */
    onStop?(ctx: LifecycleCtx, reason: 'user' | 'shutdown' | 'crash'): void | Promise<void>;
    /**
     * 外部系统的一次 HTTP 请求打进来时调用（需要 `capabilities.webhook` 里声明了对应路由）。
     *
     * **返回值就是 HTTP 应答**：`undefined` / `null` → 204（只确认收到）；字符串 →
     * `text/plain` + 200；其它值序列化成 JSON + 200；要改状态码就返回信封
     * `{ status: 418, body: 'teapot', contentType: 'text/plain' }`。
     *
     * 上下文比所有其它事件都窄：只有这次请求的 `route` / `method` / `contentType` /
     * `body` / `bodyJson` 与 `log` / `sleep` / `ai`。**没有** `reply`：
     * 外部系统没有会话，应答走返回值；想主动推消息用 `ctx.send`（那是另一个能力，另一次判据）。
     *
     * 声明了路由却没定义这个钩子时，每条请求都会记一条点名 `onWebhook` 的错误日志，
     * 并向调用方回 500 —— 而不是悄悄回一个 204 让外部系统以为成功了。
     */
    onWebhook?(ctx: WebhookCtx): unknown | Promise<unknown>;
  }

  /** 定时器事件的上下文：只有宿主能力，没有入站字段。 */
  export interface TimerCtx {
    /** 本次计划的 `name`，与第二个参数相同。 */
    readonly name: string;
    /** 往当前会话发一条回复。定时器没有入站消息，所以这是唯一能主动开口的口子。 */
    reply(text: string): void;
    /** 写一行运行日志。 */
    log(message: string): void;
    /** 让出 `ms` 毫秒；超过 600000 按 600000 算，并且会被定时器预算截断。 */
    sleep(ms: number): Promise<void>;
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

  /** `ctx.kv.set` / `ctx.kv.setMany` 的可选参数。 */
  export interface KvWriteOptions {
    /** 过期时间（毫秒），必须是有限正整数；非正、非整数、非有限一律拒绝，超过 30 天也拒绝（不静默截断）。 */
    ttlMs?: number;
  }

  /** `ctx.kv.setMany(entries)` 里的一条。 */
  export interface KvEntry {
    /** 键，1–128 字节的非空 UTF-8。 */
    key: string;
    /** 任意可 JSON 化的值；`undefined` 会被拒绝（不静默当 `null` 存）。 */
    value: unknown;
  }

  /** `ctx.kv`：本 bot 的键值状态，值是 JSON。 */
  export interface KvApi {
    /** 读一个键。**不存在或已过期都解析成 `null`**，不是 `undefined`。 */
    get(key: string): Promise<unknown | null>;
    /** 写一个键。值先序列化成 JSON，再按落库后的字节数判上限。 */
    set(key: string, value: unknown, options?: KvWriteOptions): Promise<void>;
    /** 删一个键；解析成"这次是否真的删掉了一行"，本来就没有则是 `false`。 */
    delete(key: string): Promise<boolean>;
    /** 列出键名（最多 1000 个）。`prefix` 省略或空串表示全部。 */
    list(prefix?: string): Promise<string[]>;
    /** 一次写多条，要么整批成功要么整批失败；上限 100 条。 */
    setMany(entries: KvEntry[], options?: KvWriteOptions): Promise<void>;
  }

  /** 一条会话历史。字段名是 camelCase，`at` 是 unix 毫秒。 */
  export interface HistoryEntry {
    /** 这条消息是用户发来的还是 bot 自己发的。 */
    direction: 'inbound' | 'outbound';
    /** 平台侧消息 id。 */
    messageId: string;
    /** 平台侧外部 id。 */
    externalId: string;
    /** 发送者 id。 */
    senderId: string;
    /** 消息文本。 */
    text: string;
    /** 发送时间，unix 毫秒。 */
    at: number;
  }

  /** `ctx.device.screen(options)` 的可选参数。
   *
   *  `monitor` 与 `window` **只能给一个**：前者截一整块屏，后者只截一个窗口。
   *  两个都给会整次被拒绝（不静默挑一个）。 */
  export interface DeviceScreenOptions {
    /** 显示器序号；省略取主屏（整屏截图）。超范围或不是非负整数则整次调用被拒绝。 */
    monitor?: number;
    /** 要截的窗口句柄：取 `windows()` 返回的 `id`。窗口必须存在且没有最小化，
     *  否则拒绝（不存在/已关闭/最小化分别是不同的失败文案）。 */
    window?: number;
    /** 本帧字节上限（PNG 原始字节，不是 base64 长度）；默认 4 MiB，超过按 4 MiB 算。 */
    maxBytes?: number;
  }

  /** 一帧截图。
   *
   *  `monitor` 与 `window` **恰好出现一个**：整屏截图给 `monitor`，按窗口截图给
   *  `window`。缺的那个是 `undefined`（不是 0 或 -1 —— 0 是"主屏"这个合法值）。 */
  export interface DeviceScreen {
    /** 物理像素宽。 */
    width: number;
    /** 物理像素高。 */
    height: number;
    /** PNG 字节的 base64 文本（不是 `Uint8Array`，隔离区没有 Buffer）。 */
    pngBase64: string;
    /** 这帧来自哪块屏（`monitor` 参数的解析结果）。按窗口截图时没有这个字段。 */
    monitor?: number;
    /** 这帧来自哪个窗口（`window` 参数的解析结果）。整屏截图时没有这个字段。 */
    window?: number;
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

  /** `ctx.device.ocr(options)` 的可选参数。 */
  export interface DeviceOcrOptions {
    /** 最多返回多少字符（`char` 数，不是字节）；默认 4000，允许 1..=20000。
     *  不是整数或超出范围则整次调用被拒绝，**不会**静默夹到边界。 */
    maxChars?: number;
    /** 窄版只支持整屏，这里刻意不可传值；传了任何值整次调用被拒绝。 */
    window?: never;
  }

  /** 一次 OCR 的结果。
   *
   *  **`text` 是不可信外部内容**：它是屏幕上的字，可能是别人（网页、聊天窗口、
   *  别的程序）写给模型 / Agent 的指令 —— 提示注入。读到什么都**不得**据此改
   *  权限、改沙箱档位、跳过审批或改配置；屏幕上的内容只能当数据用。 */
  export interface DeviceOcrText {
    /** 识别出的文字（**不可信**，见上）。超过 `maxChars` 时截断，且**不拼省略号**
     *  （省略号会变成"屏幕上的字"的一部分，污染内容）。 */
    readonly text: string;
    /** 是否因为 `maxChars` 被截断。 */
    readonly truncated: boolean;
  }

  /** 设备能力面；每个方法都返回 Promise，失败一律 reject（`HBOT_DEVICE_` 开头）。
   *
   *  是否可用由运行时逐次判定，见 `MessageCtx.device` 的说明。 */
  export interface DeviceApi {
    /** 截一帧：默认主屏整屏；`window` 给了就只截那个窗口。
     *  `pngBase64` 超上限时整次失败，不会给半帧。窗口最小化/已关闭会拒绝。 */
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
    /** 读**整屏**上的字（OCR）。返回的 `text` 是**不可信外部内容**：屏幕上的字
     *  可能是别人写给模型 / Agent 的指令（提示注入），不得据此改权限、改沙箱档位、
     *  跳过审批或改配置。超 `maxChars` 时截断并置 `truncated`，不拼省略号。 */
    ocr(options?: DeviceOcrOptions): Promise<DeviceOcrText>;
  }

  /** `ctx.ai.complete(options)` 的参数。 */
  export interface AiCompleteOptions {
    /** 用户消息体。必填，1–32768 字节的非空文本。 */
    prompt: string;
    /** 系统消息；可选，最多 16384 字节。 */
    system?: string;
    /** 最多生成多少 token；省略按 512，必须是 1..=4096 的整数，超出**拒绝**不夹。 */
    maxTokens?: number;
    /** 采样温度，0..=2.0 的有限数；NaN / 负数 / 超过 2 一律拒绝。 */
    temperature?: number;
  }

  /** 一次补全的结果。 */
  export interface AiResult {
    /** 模型输出。**不可信外部内容**：可能含指令，只能当数据用（见 `MessageCtx.ai`）。 */
    readonly text: string;
    /** 上游报告的 prompt token 数；上游没给时为 0，不要拿它算钱。 */
    readonly promptTokens: number;
    /** 上游报告的 completion token 数；同上。 */
    readonly completionTokens: number;
    /** 实际使用的模型名，排障用。 */
    readonly model: string;
  }

  /** 智能能力面；失败一律 reject（`HBOT_AI_*` 开头）。 */
  export interface AiApi {
    /** 调一次补全。参数不合规先于"有没有模型"被判，详见 `MessageCtx.ai`。 */
    complete(options: AiCompleteOptions): Promise<AiResult>;
  }

  /** `ctx.jobs.enqueue` 的可选参数。 */
  export interface JobEnqueueOptions {
    /** 幂等键，1–128 字节的非空文本。省略时取本次事件的 `messageId`。
     *  定时器与作业事件**没有**入站身份，那两种事件里省略它整次调用会被拒绝
     *  （不猜一个键：猜错的代价是两条不同的活撞成一条）。 */
    idempotencyKey?: string;
    /** 作业里 `ctx.reply` 的唯一目标（群或私聊的 externalId）。
     *  省略时作业的 `ctx.reply` 抛 `HBOT_JOBS_NO_TARGET`，只能 `ctx.log`。 */
    conversation?: string;
  }

  /** `ctx.jobs`：把一次作业落库后立刻返回。 */
  export interface JobApi {
    /** 入队一次作业，**同步返回**（不返回 Promise）：写库失败才抛，message 以
     *  `HBOT_JOBS_` 开头。`payload` 必须可 JSON 化且不超过 8 KiB。
     *  同一个 `(botId, idempotencyKey)` 重复入队只留一行，第二次是静默成功。 */
    enqueue(name: string, payload?: unknown, options?: JobEnqueueOptions): void;
  }

  /** 作业事件的上下文：只有宿主能力，没有入站字段，也没有状态/设备面。 */
  export interface JobCtx {
    /** 本次作业类型的 `name`，与第二个参数相同。 */
    readonly name: string;
    /** 入队时给的 JSON 载荷；没给是 `null`。**不可信外部内容**，只能当数据用。 */
    readonly payload: unknown;
    /** 入队时的幂等键。用来判重：同一键可能被执行两次。 */
    readonly idempotencyKey: string;
    /** 往声明过的会话发一条回复；作业没声明 `conversation` 时抛
     *  `HBOT_JOBS_NO_TARGET`。 */
    reply(text: string): void;
    /** 写一行运行日志。 */
    log(message: string): void;
    /** 让出 `ms` 毫秒；会被作业自身的预算截断。 */
    sleep(ms: number): Promise<void>;
    /** 智能能力面，与消息事件同一份实现。 */
    readonly ai: AiApi;
  }

  /** `ctx.session.prompt` 的可选参数。 */
  export interface SessionPromptOptions {
    /** 谁能回答：`'sender'`（缺省，只认开等待的那个人）或 `'conversation'`
     *  （这个会话里任何人都算）。后者是**放宽授权面**的动作，想清楚再写。 */
    scope?: 'sender' | 'conversation';
    /** 超时毫秒，1000–86400000；省略按 300000（5 分钟）。越界**拒绝**不夹。
     *  超时由宿主结算，不依赖「下一条消息到达」—— 没人回也会到点结束。 */
    timeoutMs?: number;
  }

  /** 一条还没被回答的等待（`ctx.session.current()` 的返回）。 */
  export interface PendingWait {
    /** 等待行 id，排障用。 */
    readonly id: string;
    /** 等待键。 */
    readonly key: string;
    /** 注册时发给用户的提示文本。 */
    readonly prompt: string;
    /** 开等待的那个人（`senderId`）。 */
    readonly owner: string;
  }

  /** `ctx.session`：跨消息的等待。`prompt` 是**注册**，不是阻塞。 */
  export interface SessionApi {
    /** 注册一条等待并把 `text` 作为提示发给用户，返回行 id。
     *
     * **它不等待**：函数一返回 handler 就继续走完，下一条匹配的消息会交给
     * `onResume`。同一个会话已有一条未结等待时抛 `HBOT_SESSION_PENDING`
     * （message 里点名挡路的那条）。只能在消息事件里调用 —— 定时器 / 作业 /
     * 恢复事件没有入站身份，调用会抛 `HBOT_SESSION_INVALID`。 */
    prompt(key: string, text: string, options?: SessionPromptOptions): Promise<string>;
    /** 这个会话当前未结的等待；没有返回 `null`。 */
    current(): Promise<PendingWait | null>;
    /** 取消这个会话未结的等待；返回是否真的取消了一条（已终态时为 `false`）。 */
    cancel(): Promise<boolean>;
  }

  /** 恢复事件的上下文：等待信息 + 唤醒它的那条消息的字段。 */
  export interface ResumeCtx {
    /** 被唤醒的那条等待的键（`ctx.session.prompt` 的第一个参数）。 */
    readonly waitKey: string;
    /** 注册等待时发给用户的提示文本 —— 用户可能已经忘了自己被问过什么。 */
    readonly waitPrompt: string;
    /** 唤醒它的那条消息的完整字段。 */
    readonly message: unknown;
    /** 那条消息的文本。 */
    readonly text: string;
    /** 发送者平台 id。 */
    readonly senderId: string;
    /** 发送者显示名（平台没给时是空串）。 */
    readonly senderName: string;
    /** 会话的平台 side id（回复目标就是它）。 */
    readonly conversationExternalId: string;
    /** 那条消息的平台 id。 */
    readonly messageId: string;
    /** 往等待声明的会话发一条回复；没有目标时抛 `HBOT_SESSION_NO_TARGET`。 */
    reply(text: string): void;
    /** 写一行运行日志。 */
    log(message: string): void;
    /** 让出 `ms` 毫秒；会被本次事件的预算截断。 */
    sleep(ms: number): Promise<void>;
    /** 智能能力面，与消息事件同一份实现。 */
    readonly ai: AiApi;
  }
  /** `ctx.send` 的第三个参数。 */
  export interface SendOptions {
    /** 目标会话的类型。省略时宿主去 `conversations` 表里查；查不到报
     *  `HBOT_SEND_NO_TARGET`。写上它就省掉那次查询，也省掉"猜"的机会。 */
    kind?: 'private' | 'group' | 'channel';
  }

  /** 生命周期钩子的上下文（`onStart` / `onStop`）：只有阶段与宿主能力。 */
  export interface LifecycleCtx {
    /** `'start'` 或 `'stop'`。 */
    readonly phase: 'start' | 'stop';
    /** 仅 `onStop`：停机原因（与第二个参数相同）。`onStart` 时是 `undefined`。 */
    readonly reason?: 'user' | 'shutdown' | 'crash';
    /** 写一行运行日志。 */
    log(message: string): void;
    /** 让出 `ms` 毫秒；会被本次钩子的预算截断。 */
    sleep(ms: number): Promise<void>;
    /** 智能能力面，与消息事件同一份实现。 */
    readonly ai: AiApi;
  }
  /** 外部系统打进来的一次请求（`onWebhook` 的上下文）。 */
  export interface WebhookCtx {
    /** 命中的路由路径，与 `hbot.json` 的 `capabilities.webhook[].path` 一致。 */
    readonly route: string;
    /** HTTP 方法；v1 只可能是 `'POST'`。 */
    readonly method: string;
    /** 调用方给的 `Content-Type`；没给是 `undefined`。 */
    readonly contentType?: string;
    /** 请求体原文（已按 UTF-8 校验，非法编码在传输层就变成 400）。 */
    readonly body: string;
    /** 请求体解析成 JSON 的结果；不是 JSON 时是 `undefined`（不是 `null`，也不抛）。 */
    readonly bodyJson?: unknown;
    /** 写一行运行日志。 */
    log(message: string): void;
    /** 让出 `ms` 毫秒；会被本次请求的预算截断。 */
    sleep(ms: number): Promise<void>;
    /** 智能能力面，与消息事件同一份实现。 */
    readonly ai: AiApi;
  }

  /** `onWebhook` 可以返回的应答信封（其余返回值按状态码 200 处理）。 */
  export interface WebhookEnvelope {
    /** HTTP 状态码，200–599 的整数；字符串会被拒绝（不做隐式转换）。 */
    status?: number;
    /** 应答体；字符串原样发，其它值序列化成 JSON。 */
    body?: unknown;
    /** 内容类型，缺省 `application/json`。 */
    contentType?: string;
  }
  /** `ctx.sql.query` 的结果。 */
  export interface SqlRows {
    /** 列名，与每行的顺序一致。 */
    readonly columns: string[];
    /** 行；每行长度等于 `columns.length`。值只可能是 null / 数字 / 字符串 / 数字数组。 */
    readonly rows: unknown[][];
    /** 是否因为行数上限被截断（**不是错误**）。 */
    readonly truncated: boolean;
  }

  /** `ctx.sql.exec` 的结果。 */
  export interface SqlChange {
    /** 受影响的行数。 */
    readonly affected: number;
  }

  /** `ctx.sql`：参数化读写这个 bot 自己的库。 */
  export interface SqlApi {
    /** 跑一条 SELECT，最多 500 行（超出截断并标记）。
     *  参数只接受 null / 布尔 / 数字 / 字符串；拼字符串会被拒（占位符数量必须与参数相等）。 */
    query(sql: string, params?: unknown[]): Promise<SqlRows>;
    /** 跑一条写语句（INSERT / UPDATE / DELETE / WITH），返回受影响行数。
     *  超过 10000 行**回滚并报错**；DDL 与多条语句一律 `HBOT_SQL_DENIED`。 */
    exec(sql: string, params?: unknown[]): Promise<SqlChange>;
  }
  /** 一条目录记录（`ctx.files.list` 的元素，也是 `write` 的返回值）。 */
  export interface FileEntry {
    /** 相对项目根的路径，用 / 分隔。 */
    readonly path: string;
    /** `file` 或 `dir`。 */
    readonly kind: 'file' | 'dir';
    /** 字节数；目录恒为 0。 */
    readonly size: number;
  }

  /** `ctx.files.read` 的结果。 */
  export interface FileText {
    /** 相对项目根的路径。 */
    readonly path: string;
    /** 文件内容。 */
    readonly text: string;
    /** 实际读到的字节数。 */
    readonly bytes: number;
    /** 是否在 `maxBytes` 处被截断（**不是错误**）。 */
    readonly truncated: boolean;
  }

  /** `ctx.files.read` 的选项。 */
  export interface FileReadOptions {
    /** 最多读多少字节，1..=262144，缺省 262144。 */
    maxBytes?: number;
  }

  /** `ctx.files.list` 的选项。 */
  export interface FileListOptions {
    /** 最多返回多少条，1..=200，缺省 200。 */
    limit?: number;
  }

  /** `ctx.files`：读写项目内的文件（只认相对路径）。 */
  export interface FilesApi {
    /** 读一个文本文件。不存在 `HBOT_FILE_NOT_FOUND`，不是 UTF-8 `HBOT_FILE_INVALID`。 */
    read(path: string, options?: FileReadOptions): Promise<FileText>;
    /** 覆盖写一个文本文件（父目录不存在会建）。超过 1 MiB `HBOT_FILE_TOO_LARGE`。 */
    write(path: string, text: string): Promise<FileEntry>;
    /** 列一个目录（不递归）；不传路径就是项目根。 */
    list(path?: string, options?: FileListOptions): Promise<FileEntry[]>;
    /** 删一个文件或空目录；不存在 `HBOT_FILE_NOT_FOUND`。 */
    remove(path: string): Promise<void>;
  }
  /** 注册入口，原样返回 config（只做校验，不包装）。 */
  export function defineBot(config: BotConfig): BotConfig;
  /** 写运行日志。 */
  export function log(message: string): void;
  /** 回复当前消息，等同 ctx.reply。 */
  export function reply(text: string): void;
  /** 等待若干毫秒，等同 ctx.sleep。 */
  export function sleep(ms: number): Promise<void>;
  /** 主动发消息，等同 `ctx.send`。 */
  export function send(
    conversationId: string,
    text: string,
    options?: SendOptions,
  ): Promise<void>;
  /** 发到清单钉死的会话，等同 `ctx.notify`。 */
  export function notify(text: string): Promise<void>;
  /** HTTP 客户端。当前只有 GET，且 host 必须在 hbot.json 的 permissions.http 里。 */
  export const http: {
    /** 发一个 GET；失败时 reject，message 以 `HBOT_HTTP_DENIED` / `HBOT_HTTP_FAILED` 开头。 */
    get(url: string, options?: HttpGetOptions): Promise<HttpResponse>;
  };
  /** 设备能力，与 `ctx.device` 是同一份实现，等价于在事件外调用。
   *  声明了不等于被授权：能不能用由运行时逐次判定，见 `MessageCtx.device`。 */
  export const device: DeviceApi;
  /** 状态能力，与 `ctx.kv` 是同一份实现，等价于在事件外调用。
   *  声明了不等于被授权：宿主没装配后端时 `HBOT_STATE_UNAVAILABLE`。 */
  export const kv: KvApi;
  /** 会话历史，与 `ctx.history` 是同一份实现。会话仍取自本次事件。 */
  export function history(limit?: number): Promise<HistoryEntry[]>;
  /** 智能能力，与 `ctx.ai` 是同一份实现。
   *  声明了不等于配了模型：宿主没装配客户端时 `HBOT_AI_UNAVAILABLE`。 */
  export const ai: AiApi;
  /** 后台作业，与 `ctx.jobs` 是同一份实现，等价于在事件外调用。
   *  声明了不等于被授权：作业队列没装配时 `HBOT_JOBS_UNAVAILABLE`。 */
  export const jobs: JobApi;
  /** 会话等待面，与 `ctx.session` 是同一份实现。 */
  export const session: SessionApi;
  /** SQL 能力，与 `ctx.sql` 是同一份实现。声明了不等于被授权：后端没装配时 `HBOT_SQL_UNAVAILABLE`。 */
  export const sql: SqlApi;
  /** 文件能力，与 `ctx.files` 是同一份实现。声明了不等于被授权：后端没装配时 `HBOT_FILE_UNAVAILABLE`。 */
  export const files: FilesApi;
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
    use crate::runtime::ai_api::AI_BINDING_KEYS;
    use crate::runtime::device_api::DEVICE_BINDING_KEYS;
    use crate::runtime::host_api::{HOST_BINDING_KEYS, HOST_MODULE_SOURCE};
    use crate::runtime::state_api::KV_BINDING_KEYS;

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

    /// The keys `runtime/state_api.rs` installs on the `ctx.kv` object.
    ///
    /// Same scan and same reason as [`installed_device_keys`]: the `entries`
    /// literal is the one place a key sits next to its function, and rustfmt does
    /// wrap long `object.set(..)` calls.
    fn installed_kv_keys() -> BTreeSet<String> {
        let source = include_str!("runtime/state_api.rs");
        let start = source
            .find("let entries: [(&str, Function<'js>);")
            .expect("state_api.rs 应有 entries 表");
        let block = &source[start..];
        let end = block.find("\n    ]").expect("entries 表应有收尾 `]`");
        block[..end]
            .lines()
            .filter_map(|line| line.trim().strip_prefix("(\""))
            .filter_map(|rest| rest.split_once('"').map(|(name, _)| name.to_owned()))
            .collect()
    }

    /// Method declarations inside `export interface KvApi {`.
    fn dts_kv_methods() -> BTreeSet<String> {
        let start = HOST_DTS
            .find("export interface KvApi {")
            .expect("hbot.d.ts 应有 KvApi");
        let block = &HOST_DTS[start..];
        let end = block.find("\n  }").expect("KvApi 应有收尾 `}`");
        block[..end].lines().filter_map(method_name).collect()
    }

    /// Method keys of `export const kv = { … }` in the runtime module.
    fn module_kv_methods() -> BTreeSet<String> {
        let start = HOST_MODULE_SOURCE
            .find("export const kv = {")
            .expect("HOST_MODULE_SOURCE 应有 kv 命名空间");
        let block = &HOST_MODULE_SOURCE[start..];
        let end = block
            .find("\n};")
            .expect("HOST_MODULE_SOURCE 的 kv 对象应有收尾 `};`");
        block[..end].lines().filter_map(method_name).collect()
    }

    /// The keys `runtime/ai_api.rs` installs on the `ctx.ai` object.
    ///
    /// Same scan and same reason as [`installed_kv_keys`].
    fn installed_ai_keys() -> BTreeSet<String> {
        let source = include_str!("runtime/ai_api.rs");
        let start = source
            .find("let entries: [(&str, Function<'js>);")
            .expect("ai_api.rs 应有 entries 表");
        let block = &source[start..];
        // 类型注解里已经有一个 `]`，所以要从 `= [` 之后才开始找收尾；只有一个
        // 条目时 rustfmt 把整张表折成一行（`state_api.rs` 有五个条目才保持多行），
        // 两种形状都要认。
        let after_equals = block
            .find("= [")
            .map(|at| &block[at + 3..])
            .expect("entries 表应有 `= [`");
        let body = &after_equals[..after_equals.find(']').expect("entries 表应有收尾 `]`")];
        body.split("(\"")
            .skip(1)
            .filter_map(|rest| rest.split_once('"').map(|(name, _)| name.to_owned()))
            .collect()
    }

    /// Method declarations inside `export interface AiApi {`.
    fn dts_ai_methods() -> BTreeSet<String> {
        let start = HOST_DTS
            .find("export interface AiApi {")
            .expect("hbot.d.ts 应有 AiApi");
        let block = &HOST_DTS[start..];
        let end = block.find("\n  }").expect("AiApi 应有收尾 `}`");
        block[..end].lines().filter_map(method_name).collect()
    }

    /// Method keys of `export const ai = { … }` in the runtime module.
    fn module_ai_methods() -> BTreeSet<String> {
        let start = HOST_MODULE_SOURCE
            .find("export const ai = {")
            .expect("HOST_MODULE_SOURCE 应有 ai 命名空间");
        let block = &HOST_MODULE_SOURCE[start..];
        let end = block
            .find("\n};")
            .expect("HOST_MODULE_SOURCE 的 ai 对象应有收尾 `};`");
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
    fn kv_surface_matches_the_runtime_object_and_the_module() {
        // `ctx.kv` 的四方一致：state_api.rs 的 entries 表、手写的
        // KV_BINDING_KEYS、`hbot` 模块的 kv 命名空间、hbot.d.ts 的 KvApi。
        let installed = installed_kv_keys();
        assert!(
            !installed.is_empty(),
            "state_api.rs 里没解析到 entries 表，扫描逻辑失效了"
        );
        assert_eq!(
            installed,
            KV_BINDING_KEYS
                .iter()
                .map(|key| (*key).to_owned())
                .collect::<BTreeSet<String>>(),
            "KV_BINDING_KEYS 与 state_api.rs 实际注入的键不一致"
        );

        let module = module_kv_methods();
        assert_eq!(
            installed, module,
            "`ctx.kv` 的方法与 `hbot` 模块转发的方法不一致：运行时 {installed:?}，模块 {module:?}"
        );

        let declared = dts_kv_methods();
        assert_eq!(
            installed, declared,
            "`ctx.kv` 的方法与 hbot.d.ts 的 KvApi 不一致：运行时 {installed:?}，类型 {declared:?}"
        );
    }

    #[test]
    fn ai_surface_matches_the_runtime_object_and_the_module() {
        // `ctx.ai` 的四方一致：ai_api.rs 的 entries 表、手写的 AI_BINDING_KEYS、
        // `hbot` 模块的 ai 命名空间、hbot.d.ts 的 AiApi。
        let installed = installed_ai_keys();
        assert!(
            !installed.is_empty(),
            "ai_api.rs 里没解析到 entries 表，扫描逻辑失效了"
        );
        assert_eq!(
            installed,
            AI_BINDING_KEYS
                .iter()
                .map(|key| (*key).to_owned())
                .collect::<BTreeSet<String>>(),
            "AI_BINDING_KEYS 与 ai_api.rs 实际注入的键不一致"
        );

        let module = module_ai_methods();
        assert_eq!(
            installed, module,
            "`ctx.ai` 的方法与 `hbot` 模块转发的方法不一致：运行时 {installed:?}，模块 {module:?}"
        );

        let declared = dts_ai_methods();
        assert_eq!(
            installed, declared,
            "`ctx.ai` 的方法与 hbot.d.ts 的 AiApi 不一致：运行时 {installed:?}，类型 {declared:?}"
        );
    }

    #[test]
    fn the_ai_surface_says_presence_is_not_authorization() {
        // 与设备面那条同一条道理：d.ts 声明了不代表配了模型，也不代表这一轮
        // 还有额度。三条事实必须写在类型里，否则 Agent 会照着写然后在运行时
        // 撞墙，而且看不懂为什么。
        assert!(
            HOST_DTS.contains("readonly ai: AiApi;"),
            "MessageCtx 应声明 ai（带 readonly 且名字含空格，两个扫描器靠这一条把它与入站字段分开）"
        );
        assert!(
            HOST_DTS.contains("export const ai: AiApi;"),
            "hbot 模块应声明 ai"
        );
        for hint in ["HBOT_AI_UNAVAILABLE", "HBOT_AI_BUDGET", "不可信外部内容"] {
            assert!(
                HOST_DTS.contains(hint),
                "hbot.d.ts 没有说明智能能力的失败面或不可信性，缺少 `{hint}`"
            );
        }
    }

    #[test]
    fn the_state_surface_says_presence_is_not_authorization() {
        // 与设备面同一条规矩：`ctx.kv` 出现在类型里只说明**声明了**能力，
        // 不代表宿主装了后端、也不代表这份数据属于你。三种失败面都得写进 d.ts。
        assert!(
            HOST_DTS.contains("readonly kv: KvApi;"),
            "MessageCtx 应声明 readonly kv"
        );
        assert!(
            HOST_DTS.contains("export const kv: KvApi;"),
            "hbot 模块应声明 kv"
        );
        for hint in [
            "capabilities.state.kv",
            "HBOT_STATE_NOT_DECLARED",
            "HBOT_STATE_UNAVAILABLE",
        ] {
            assert!(
                HOST_DTS.contains(hint),
                "hbot.d.ts 没有说明状态能力的失败面，缺少 `{hint}`"
            );
        }
        // 缺省条数与上限是契约值，写错一个数字就与 `history_limit` 脱钩。
        assert!(
            HOST_DTS.contains("默认 20，上限 50"),
            "hbot.d.ts 应写明 history 的默认值 20 与上限 50"
        );
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
        //
        // `send(` was removed when C3.send shipped (2026-11-19): it is false by design now.
        // What is still missing on the egress side is **attachments** — text is all a caller
        // can push — so the entries below are the precise remainder instead of the broad one
        // (`AGENTS.md` §8.7: a broad entry must be deleted, not kept).
        //
        // `kv.` was removed for the same reason: `ctx.kv` shipped with C2, so
        // "`kv.` must not appear" is now false by design. What is still missing is
        // **cross-conversation** history and any streaming/watch API — those are
        // the entries a broad "`history`" would have covered, so spell them out.
        // `onStart` / `onStop` 已落地（2026-11-19，`C1.lifecycle`）：宽泛条目按 `AGENTS.md` §8.7
        // 必须**整条删**，否则它会在能力实现后继续假装「这个没做」。仍在缺的那一项单独列出。
        for absent in [
            "onSchedule",
            "sendFile(",
            "sendImage(",
            "notifyFile(",
            "http.post",
            "http.fetch",
            "http.request",
            "fetch(",
            "schedule(",
            // 跨会话历史（`history(conversationId)`）未实现：本次事件之外的历史读不到。
            "history(conversationId",
            // 状态能力没有 watch / subscribe；有的话得先解决"谁持有回调"的归属问题。
            "watch(",
            "subscribe(",
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
