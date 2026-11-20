//! [`Command`]：UI → 核心的唯一入口。
//!
//! 线格式：`"type"` 是变体名的 snake_case（`app_info`、`file_write`），
//! 字段一律 camelCase（`sessionId`、`baseHash`、`acceptHunks`）。
//!
//! 每个命令的 `data` 形状见 [`super`] 的"命令 → data"表；`Command` 与
//! [`super::Event`] 都不在这两个文件之外定义（唯一真源）。

use hbot_core::{ApprovalDecision, ApprovalId, BotId, ConversationId, EditId, SearchId, SessionId};
use serde::{Deserialize, Serialize};

use super::{DebugBreakpoint, StepKind};

/// UI 发往核心的一条命令。
///
/// 唯一入口是 `dispatch(command) -> Response`（桌面走 Tauri 命令，后台走
/// JSON-RPC 2.0 的 `dispatch` 方法）；不存在第二条通道。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    /// 应用信息：版本、平台、数据目录（启动时第一条命令）。
    AppInfo,

    /// 打开一个项目目录（校验 `hbot.json`、建 DB、起文件监听）。
    ProjectOpen {
        /// 项目根目录绝对路径。
        path: String,
    },

    /// 新建一个项目（写脚手架、登记到 `projects` 表、切到新项目）。
    ///
    /// 目标目录可以不存在（连父目录一起建），也可以是**空**目录；已存在且非空时
    /// 回 `-32003 conflict` —— 本项目从不覆盖用户文件（`hbot_code::manifest::scaffold`
    /// 的 `scaffold_never_overwrites` 保证这一点）。成功后后续 `file_list` /
    /// `file_read` 直接可用：workspace 已经切到新项目。
    ProjectCreate {
        /// 新项目根目录路径（绝对路径；不存在时自动创建）。
        path: String,
        /// 展示名，写进 `hbot.json` 的 `name`；空字符串回 `-32002`。
        name: String,
    },

    /// 向 Agent 会话发送一条用户消息。
    ///
    /// `sessionId` 为 `null` / 缺省时表示"新建一个会话并用它"：核心用
    /// [`IdGenerator`](hbot_core::ports::IdGenerator) 分配 ID，并在 `data.sessionId`
    /// 里返回，UI 后续消息带同一个 ID 就能接上同一段上下文。
    AgentSend {
        /// Agent 会话 ID；`None` 表示新建会话。
        #[serde(default)]
        session_id: Option<SessionId>,
        /// 用户输入。
        text: String,
    },

    /// 取消当前 turn（贯穿 provider / 工具 / 命令）。
    AgentCancel {
        /// Agent 会话 ID。
        session_id: SessionId,
    },

    /// 回应一次审批请求。
    AgentApprove {
        /// 审批请求 ID（来自 `approval.request` 事件）。
        approval_id: ApprovalId,
        /// 批准或拒绝。
        decision: ApprovalDecision,
        /// 是否登记"以后都按这个完整动作放行"（规则键含规范化参数与沙箱档）。
        always: bool,
    },

    /// 读文件。
    FileRead {
        /// 相对项目根（或绝对）的路径。
        path: String,
    },

    /// 写文件（原子写；`baseHash` 用于检测冲突）。
    FileWrite {
        /// 文件路径。
        path: String,
        /// 新内容（UTF-8 文本）。
        content: String,
        /// 打开时读到的内容哈希（来自 `FileRead` 的 `data.hash`）；不匹配时拒绝覆盖。
        #[serde(default)]
        base_hash: Option<String>,
    },

    /// 列目录（一层）。
    FileList {
        /// 目录路径。
        path: String,
    },

    /// 新建文件。
    FileCreate {
        /// 文件路径。
        path: String,
        /// 初始内容。
        content: String,
    },

    /// 重命名 / 移动文件。
    FileRename {
        /// 原路径。
        from: String,
        /// 新路径。
        to: String,
    },

    /// 删除文件。
    FileDelete {
        /// 文件路径。
        path: String,
        /// 是否移到回收站（否则永久删除）。
        to_trash: bool,
    },

    /// 应用一次 Agent 编辑中的若干块。
    EditApply {
        /// 编辑 ID（来自 `agent.diff` 事件）。
        edit_id: EditId,
        /// 接受的块序号（对应 `DiffHunk.index`）；空表示全部拒绝。
        accept_hunks: Vec<u32>,
    },

    /// 拒绝整次 Agent 编辑。
    EditReject {
        /// 编辑 ID。
        edit_id: EditId,
    },

    /// 开始全局搜索（结果通过 `file.search` 事件流式返回）。
    SearchStart {
        /// 查询串。
        query: String,
        /// 是否按正则解释。
        regex: bool,
        /// glob 过滤（空表示不过滤）。
        globs: Vec<String>,
    },

    /// 取消一次正在进行的搜索。
    SearchCancel {
        /// 搜索 ID（来自 `file.search` 事件）。
        search_id: SearchId,
    },

    /// 取诊断（编译错误 / 语言服务）。
    Diagnostics {
        /// 只要该文件的诊断；`None` 表示整个项目。
        #[serde(default)]
        path: Option<String>,
    },

    /// 启动 bot（起 supervisor、建连接、恢复 outbound）。
    BotStart {
        /// Bot ID。
        bot_id: BotId,
    },

    /// 停止 bot（停止读取、取消、flush）。
    BotStop {
        /// Bot ID。
        bot_id: BotId,
    },

    /// 向指定会话注入一条模拟入站消息（自测用，走与真实消息相同的链路）。
    BotInject {
        /// Bot ID。
        bot_id: BotId,
        /// 目标会话。
        conversation_id: ConversationId,
        /// 模拟的消息文本。
        text: String,
    },

    /// 列出当前项目的 bot 及其状态（UI 启动时播种运行态的唯一来源）。
    ///
    /// `data` 是 [`BotSummary`](super::BotSummary) 的数组；库里没有 bot 行时返回空数组，
    /// 不补建、不编造。
    BotList,

    /// 列出某个 bot 已登记的会话。
    ///
    /// `data` 是 [`ConversationSummary`](super::ConversationSummary) 的数组；没有会话时返回
    /// 空数组，**不补建、不编造**。存在的理由是 `bot_inject` 需要 `conversationId`，而 UI
    /// 此前唯一的获取途径是正则匹配日志里的一行——那是脏办法。
    ///
    /// `local` 平台启动后会有一条 `externalId = local:default` 的 `private` 会话，它就是
    /// 本地试跑要交给 `bot_inject` 的那个 id。
    BotConversations {
        /// Bot ID。
        bot_id: BotId,
    },

    /// 列最近的后台作业（`bot_jobs` 表），**新的在前**。
    ///
    /// `data` 是 [`JobView`](super::JobView) 的数组。终态行也回：面板要能看到
    /// 「刚才那条失败了」这类结果，只列 pending/running 等于让失败悄无声息。
    ///
    /// 这是**只读快照**，不派发、不改状态；`limit` 有上限（超了按上限算，不报错），
    /// 因为面板只需要最近若干条。
    BotJobs {
        /// Bot ID。
        bot_id: BotId,
        /// 最多返回多少条。
        limit: usize,
    },

    /// 列最近的会话等待（`bot_waits` 表），**新的在前**。
    ///
    /// `data` 是 [`WaitView`](super::WaitView) 的数组。与 `BotJobs` 同口径：
    /// 终态行（`fulfilled` / `expired` / `cancelled`）一起回，由 UI 决定怎么显示。
    BotWaits {
        /// Bot ID。
        bot_id: BotId,
        /// 最多返回多少条。
        limit: usize,
    },

    /// 查某个 bot 的设备会话状态。
    ///
    /// `data` 是 [`DeviceStatus`](super::DeviceStatus)；从没有过会话时 `state` 为
    /// `none`、其余字段为 `null`（**不是** `data: null` —— UI 需要 `sub`/`allowedApps`
    /// 这两条空数组来画设置页）。
    DeviceStatus {
        /// Bot ID。
        bot_id: BotId,
    },

    /// 开一个限时设备会话（**用户拍板**的那一层闸）。
    ///
    /// `sub` 是请求的子能力，最终生效的是**清单声明 ∩ 宿主设置 ∩ 本次请求**三者交集
    /// （交出来是空集就回 `-32002`，不"就近给一个"）。`minutes` 缺省按设置里的
    /// 默认值，硬上限 30 分钟。
    ///
    /// 用户可能从来没见过这条命令 —— 它由设置页/托盘/快捷键走，Agent 不能自己调
    /// （Agent 只能发 [`Command::DeviceRequest`]）。
    DeviceGrant {
        /// Bot ID。
        bot_id: BotId,
        /// 请求的子能力（`screen` / `ocr` / `input` / `window` / `clipboard`）。
        sub: Vec<String>,
        /// 允许的应用（映像名）；空数组 = 全拒。
        allowed_apps: Vec<String>,
        /// 会话时长（分钟）；`None` = 用设置的默认值。
        minutes: Option<u32>,
    },

    /// 立刻结束设备会话。**幂等**：没有活跃会话时也成功。
    ///
    /// 这是"一键全局撤销"的入口，四个触发点（设置页按钮、托盘菜单、快捷键、
    /// `device_revoke` 命令）都走它，不各写一份。
    DeviceRevoke {
        /// Bot ID。
        bot_id: BotId,
    },

    /// Agent 请求一次设备授权（**不是**授权本身）。
    ///
    /// `data` 是 `null`；结果经 `device.status`（若用户批准）或 `device.status`
    /// 的 `denied` 状态回来。存在的理由：Agent 不能用 [`Command::DeviceGrant`]
    /// 给自己开权限，否则"用户拍板"这一层就没有了。
    DeviceRequest {
        /// Bot ID。
        bot_id: BotId,
        /// 请求的子能力。
        sub: Vec<String>,
        /// 给用户看的理由（Agent 自己写，**不可信数据**，UI 只展示不执行）。
        reason: String,
    },

    /// 读宿主级设备设置（`<data_dir>/device.json`）。
    ///
    /// `data` 是 [`DeviceSettings`](super::DeviceSettings)。这是**宿主**设置，不是
    /// 某个 bot 的会话：会话由 [`Command::DeviceGrant`] 开，设置属于这台机器。
    DeviceSettingsGet,

    /// 写宿主级设备设置。
    ///
    /// 校验失败一律 `-32002`（不"就近取一个合法值"）：`capabilities` 里有不认识的
    /// 名字、`maxMinutes` 超过硬上限 30、`defaultMinutes` 大于 `maxMinutes`、应用名
    /// 是空串——都当场报错，因为静默修正会让用户以为自己设的就是生效的那份。
    ///
    /// 写入后立刻对**下一次**设备调用生效（闸不缓存设置），并且会发一条
    /// `device.settings` 事件让所有窗口对齐。已经开着的会话：时长与子能力不变
    /// （那是用户在不同时刻点头的两次决定），但 `allowedApps` 与总开关的收紧会在
    /// 下一次调用上就生效（`host_ceiling_allows` 每次重查）。
    DeviceSettingsSet {
        /// 新的整份设置（不是补丁：少写的字段按默认值算，调用方要传全量）。
        settings: super::DeviceSettings,
    },

    /// 回放历史入站消息，用**当前代码**重跑一遍并逐条对比。
    ///
    /// 这是只读操作：不写库、不联网、不动正在跑的那个 bot —— 它另起一个全新 isolate
    /// 逐条 dispatch，回复只收集在内存里。`data` 是
    /// [`ReplayReport`](super::ReplayReport)。
    ///
    /// 失败通道：没有可回放的历史 `-32001`、参数非法（`limit: 0`）`-32002`、
    /// 编译当前代码失败或 isolate 起不来 `-32006`（文案里带真实原因）。
    ReplayRun {
        /// Bot ID。
        bot_id: BotId,
        /// 时间下界（Unix 毫秒）；`null` 表示不限。
        from: Option<i64>,
        /// 最多回放多少条；`null` 表示全量（`0` 是 `-32002`，不"等于全量"）。
        limit: Option<u32>,
    },

    /// 把一批历史入站存成夹具文件（JSON 数组），供以后不依赖库重跑。
    ///
    /// 只读数据库、只写这一个文件；**不联网、不动正在跑的 bot**。
    /// `data` 是 [`SavedFixture`](super::SavedFixture)。
    ///
    /// 失败通道：没有可保存的历史 `-32001`、`limit: 0` 或路径为空 / 解析后落在项目外
    /// `-32002`、写盘失败 `-32006`。路径按项目根解析：相对路径落在项目内，
    /// **不允许**写到项目外（`-32002`）。
    ///
    /// 父目录**可以不存在**（连同父目录一起建，原子写：先写 `.<name>.tmp` 再 rename）：
    /// UI 的默认路径 `.hbot/fixtures/<bot>-<时间>.json` 第一次保存时目录必然还不存在，
    /// 若把它当参数错误，默认路径就永远存不下来。这条是 2026-11-17 与实现对齐后的口径
    /// （此前文档写"父目录不存在 → `-32002`"，是错的）。
    ReplaySaveFixture {
        /// Bot ID。
        bot_id: BotId,
        /// 时间下界（Unix 毫秒）；`null` 表示不限。
        from: Option<i64>,
        /// 最多导出多少条；`null` 表示全量。
        limit: Option<u32>,
        /// 目标文件（相对项目根，或项目内的绝对路径）。
        path: String,
    },

    /// 把一份夹具从头跑一遍（不查库、不需要历史）。
    ///
    /// 用途：把 `ReplaySaveFixture` 导出的、或从别处拿到的夹具当回归用例跑，
    /// 看当前代码会回什么。**不写库、不联网**。`data` 是
    /// [`FixtureRunReport`](super::FixtureRunReport)。
    ///
    /// 失败通道：路径为空/绝对路径在项目外/文件不存在或不是 JSON 数组 `-32002`、
    /// 编译当前代码失败或 isolate 起不来 `-32006`。
    FixtureRun {
        /// Bot ID。
        bot_id: BotId,
        /// 夹具文件（相对项目根，或项目内的绝对路径）。
        path: String,
    },

    /// 设置断点（调试构建）。
    ///
    /// 断点是**按 bot 的沙箱**生效的：同一份源码在不同 bot 的 isolate 里可以是不同版本。
    ///
    /// 整份替换语义：`breakpoints` 是这个文件的**全部**断点，不在列表里的行会被清掉。
    /// `breakpoints: []` 表示取消该文件的全部断点。
    DebugSetBreakpoints {
        /// Bot ID。
        bot_id: BotId,
        /// 源文件路径（项目内相对路径）。
        file: String,
        /// 这个文件的全部断点（行号 + 可选条件）。
        breakpoints: Vec<DebugBreakpoint>,
    },

    /// 继续执行（步进后的恢复）。
    DebugResume {
        /// Bot ID。
        bot_id: BotId,
    },

    /// 单步执行。
    DebugStep {
        /// Bot ID。
        bot_id: BotId,
        /// 步进方向。
        kind: StepKind,
    },

    /// 取最近 N 条运行日志。
    LogsTail {
        /// Bot ID；`None` 表示全项目（含 Agent 行）。
        bot_id: Option<BotId>,
        /// 条数上限。
        limit: usize,
    },
}

impl Command {
    /// 线格式的 `"type"` 值（`app_info`、`file_write`…）。
    ///
    /// 供日志、错误信息与 `dispatch` 分支使用。必须与 serde 的 `tag` 一致：
    /// `crates/hbot-rpc/tests/commands.rs` 里有一条测试逐条比对序列化结果，
    /// 改名忘了同步这里就会红。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AppInfo => "app_info",
            Self::ProjectOpen { .. } => "project_open",
            Self::ProjectCreate { .. } => "project_create",
            Self::AgentSend { .. } => "agent_send",
            Self::AgentCancel { .. } => "agent_cancel",
            Self::AgentApprove { .. } => "agent_approve",
            Self::FileRead { .. } => "file_read",
            Self::FileWrite { .. } => "file_write",
            Self::FileList { .. } => "file_list",
            Self::FileCreate { .. } => "file_create",
            Self::FileRename { .. } => "file_rename",
            Self::FileDelete { .. } => "file_delete",
            Self::EditApply { .. } => "edit_apply",
            Self::EditReject { .. } => "edit_reject",
            Self::SearchStart { .. } => "search_start",
            Self::SearchCancel { .. } => "search_cancel",
            Self::Diagnostics { .. } => "diagnostics",
            Self::BotStart { .. } => "bot_start",
            Self::BotStop { .. } => "bot_stop",
            Self::BotInject { .. } => "bot_inject",
            Self::BotList => "bot_list",
            Self::BotConversations { .. } => "bot_conversations",
            Self::BotJobs { .. } => "bot_jobs",
            Self::BotWaits { .. } => "bot_waits",
            Self::DeviceStatus { .. } => "device_status",
            Self::DeviceGrant { .. } => "device_grant",
            Self::DeviceRevoke { .. } => "device_revoke",
            Self::DeviceRequest { .. } => "device_request",
            Self::DeviceSettingsGet => "device_settings_get",
            Self::DeviceSettingsSet { .. } => "device_settings_set",
            Self::ReplayRun { .. } => "replay_run",
            Self::ReplaySaveFixture { .. } => "replay_save_fixture",
            Self::FixtureRun { .. } => "fixture_run",
            Self::DebugSetBreakpoints { .. } => "debug_set_breakpoints",
            Self::DebugResume { .. } => "debug_resume",
            Self::DebugStep { .. } => "debug_step",
            Self::LogsTail { .. } => "logs_tail",
        }
    }
}
