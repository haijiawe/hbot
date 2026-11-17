//! 用历史入站事件重跑**当前代码**并对比输出（`hbot replay` 与 IDE `replay_run` 的共用内核）。
//!
//! 本 crate 是 `docs/06-ide.md` §6.3（测试与回放）与 `docs/01-requirements.md` 的 FR-27 的
//! **后端**一半。命令行（`hbot-cli`）与桌面 / 无头 IDE（`hbot-rpc`）都调 [`replay`]，区别只在
//! 传进来的是哪一份库（见下）。
//!
//! # 两种库：库由调用方打开，本 crate 不自己开库
//!
//! 历史入站 / 出站存在 SQLite 里，但**库在哪儿取决于谁在跑**：
//!
//! - **项目库** `<project-dir>/.hbot/hbot.db`：`.hbot/` 是项目内运行数据目录（`hbot-code`
//!   生成的 `.gitignore` 就忽略它，`docs/03-directory.md` 同此约定）。`hbot serve` 起来的
//!   进程写的就是它，所以 `hbot replay <project-dir>` 读这里才与"这个项目的历史"对得上。
//! - **应用库** `<data_dir>/hbot.db`：桌面 / 无头 IDE 只有这一份（见
//!   `hbot_rpc::AppState::open`）。IDE 里几个项目的 bot 都在这一份库里，项目自己的
//!   `.hbot/hbot.db` 可能压根不存在——回放要是自己去开项目库，IDE 里就永远读不到历史。
//!
//! 所以 [`replay`] **接收调用方给的 `&Store`**，不打开库、也不推断路径：CLI 自己
//! `Store::open` 项目库后再传进来，RPC 直接把 `AppState` 的那份 `Arc<Store>` 传进来。
//! [`ReplayOptions::database`] 只是"这份历史来自哪个文件"的**文案**（报告与错误信息里要写
//! 给用户看），不参与打开、也不做存在性校验——库在不在、能不能打开，是调用方的事。
//!
//! # 流程
//!
//! 1. 调用方打开历史库并传进来（见上：项目库 / 应用库）；
//! 2. 按 `(received_at, rowid)` 升序读该 bot 的历史入站，**含已处理**——已处理的行才是
//!    "当时真的跑过"的历史证据；
//! 3. 按 `outbound.reply_to = inbound.external_id` 取每条入站对应的历史出站。被动回复的
//!    `reply_to` 就是入站 `external_id`（`hbot_rpc::app::runtime::sandbox_handler` 的
//!    `passive_reply_id`），这是库里唯一可靠的"这条回复来自哪条消息"的外键；用时间窗口
//!    做不到（同一毫秒可能多条入站，`ctx.sleep` 也能跨事件延迟回复）；
//! 4. 用**与生产同一条路径**在新 isolate 里重跑：`manifest::load` → `compile_project_with_entry`
//!    → `Sandbox::spawn` → 逐条 `dispatch_collecting`；事件 JSON 由 `hbot_bot::event_json`
//!    统一拼装（生产与回放共用同一份实现）；
//! 5. 逐条对比，归类为 **一致 / 多出 / 缺少 / 顺序不同**。
//!
//! 致命错误不 panic 也不返回 `Err`：写进 [`ReplayReport::error`]，由调用方映射到自己的错误
//! 通道（CLI 是退出码 2，RPC 是 `-32006`）。打印、退出码、`--json` 这些是 CLI 的事，本 crate
//! 不做。
//!
//! 只读：本 crate 不写 inbound / outbound / logs，也不改项目文件（`--save-fixture` 是用户
//! 显式指定的落点，例外；[`fixture::save_fixture`] 写的也只是用户点名的那个夹具文件）。

#![warn(missing_docs)]

pub mod fixture;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use hbot_bot::{EventSource, event_json};
use hbot_code::manifest;
use hbot_code::{Sandbox, compile_project_with_entry};
use hbot_core::ConversationKind;
use hbot_core::{MessageId, Timestamp};
use hbot_store::{BotId, InboundReplayRow, Store};

pub use fixture::{
    FixtureEntry, FixtureError, FixtureRunReport, SavedFixture, resolve_fixture_path, run_fixture,
    run_fixture_compiled, save_fixture,
};

/// 项目库相对项目根的位置。
const DATABASE_SUFFIX: &str = ".hbot/hbot.db";

/// 一次回放的输入。CLI 与 IDE 各自构造，测试也可以直接构造。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayOptions {
    /// 项目根目录（`hbot.json` 所在处）。
    pub project_dir: PathBuf,
    /// 这份历史来自哪个库文件（CLI 是项目库，IDE 是应用库）。
    ///
    /// **只用于报告与错误文案**，不参与打开、不校验存在性：库由调用方打开（见 crate 文档
    /// 的"两种库"）。
    pub database: PathBuf,
    /// 要回放的 bot id（UUID 文本）。
    pub bot: String,
    /// 最多回放多少条入站事件；`None` 表示不限。
    pub limit: Option<u32>,
    /// 只回放该时刻（Unix 毫秒，含）之后的事件；`None` 表示不限。
    pub from: Option<i64>,
    /// 把这一批入站 payload 存成 fixture 文件的落点；`None` 表示不存。
    pub save_fixture: Option<PathBuf>,
}

/// 一条回复的差异分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    /// 一致：历史有、重跑也有，且相对顺序没变。
    Same,
    /// 多出：重跑产生了历史里没有的回复。
    Extra,
    /// 缺少：历史里有、这次重跑没产生。
    Missing,
    /// 顺序不同：同一批文本都在，但相对顺序变了。
    Reordered,
}

impl DiffKind {
    /// 给人看的中文标签（报告文本用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Same => "一致",
            Self::Extra => "多出",
            Self::Missing => "缺少",
            Self::Reordered => "顺序不同",
        }
    }
}

/// 一条"顺序不同"的回复：文本按多重集匹配上了，但位次变了。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReorderedReply {
    /// 回复文本。
    pub text: String,
    /// 在历史出站里的下标（0 起）。
    pub historical_index: usize,
    /// 在本次重跑结果里的下标（0 起）。
    pub replayed_index: usize,
}

/// 一次 [`diff_replies`] 的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepliesDiff {
    /// 一致：文本与相对顺序都对得上。
    pub same: Vec<String>,
    /// 多出：重跑有、历史没有。
    pub extra: Vec<String>,
    /// 缺少：历史有、重跑没有。
    pub missing: Vec<String>,
    /// 顺序不同：文本对得上、相对顺序变了。
    pub reordered: Vec<ReorderedReply>,
}

impl RepliesDiff {
    /// 是否完全一致（没有多出、缺少、顺序不同）。
    pub fn is_identical(&self) -> bool {
        self.extra.is_empty() && self.missing.is_empty() && self.reordered.is_empty()
    }
}

/// 单条事件的回放结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    /// 一致。
    Match,
    /// 有差异。
    Diff,
    /// 这条事件重跑失败（handler 抛异常、payload 坏了）。
    Error,
}

impl EntryStatus {
    /// 给人看的中文标签（报告文本用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Match => "一致",
            Self::Diff => "有差异",
            Self::Error => "出错",
        }
    }

    /// wire 上的取值（`match` / `diff` / `error`）。
    ///
    /// wire DTO 的 `status` 是**开放字符串**而不是枚举（见 `hbot-rpc` 的
    /// `dto/common.rs`）：加一个新的结论不该让旧 UI 解析失败。所以映射放在这里、
    /// 由调用方显式取值，而不是把 `EntryStatus` 本身塞进 wire 类型。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Diff => "diff",
            Self::Error => "error",
        }
    }
}

/// 一条历史入站事件的回放明细。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundReplay {
    /// 入站行主键。
    pub inbound_id: String,
    /// 入站行的 `external_id`（也是历史出站的 `reply_to`）。
    pub external_id: String,
    /// 会话的外部 id。
    pub conversation_external_id: String,
    /// 入站时间（Unix 毫秒）。
    pub received_at: i64,
    /// 这条事件的结论。
    pub status: EntryStatus,
    /// 历史出站文本（按 `created_at, rowid` 升序）。
    pub historical: Vec<String>,
    /// 本次重跑产生的回复（按 `reply(...)` 调用顺序）。
    pub replayed: Vec<String>,
    /// 逐条对比：一致。
    pub same: Vec<String>,
    /// 逐条对比：多出。
    pub extra: Vec<String>,
    /// 逐条对比：缺少。
    pub missing: Vec<String>,
    /// 逐条对比：顺序不同。
    pub reordered: Vec<ReorderedReply>,
    /// 结论为 [`EntryStatus::Error`] 时的原因。
    pub error: Option<String>,
}

/// 一次回放的完整报告（CLI 的 `--json` 直接序列化它，RPC 再映射成 wire DTO）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayReport {
    /// 是否一切正常（无差异且无事件级失败）。`error` 存在时恒为 `false`。
    pub ok: bool,
    /// 项目目录（原样回显，便于把报告与命令对应起来）。
    pub project: String,
    /// 这份历史来自哪个库文件（[`ReplayOptions::database`] 原样回显）。
    pub database: String,
    /// 回放的 bot id。
    pub bot: String,
    /// `--from` 解析成的 Unix 毫秒下界；`None` 表示不限。
    pub from: Option<i64>,
    /// `--limit` 原样回显。
    pub limit: Option<u32>,
    /// 实际回放的事件条数。
    pub events: usize,
    /// 其中一致的条数。
    pub matched_events: usize,
    /// 其中有差异的条数。
    pub differing_events: usize,
    /// 其中重跑失败的条数。
    pub failed_events: usize,
    /// 逐条回复统计：一致。
    pub same: usize,
    /// 逐条回复统计：多出。
    pub extra: usize,
    /// 逐条回复统计：缺少。
    pub missing: usize,
    /// 逐条回复统计：顺序不同。
    pub reordered: usize,
    /// 每条事件的明细。
    pub entries: Vec<InboundReplay>,
    /// 致命错误（退出码 2 的原因）；`None` 表示这次回放真的跑起来了。
    pub error: Option<String>,
}

impl ReplayReport {
    /// 按选项构造一个空报告（`ok = true`，待汇总覆盖）。
    pub fn new(options: &ReplayOptions) -> Self {
        Self {
            ok: true,
            project: options.project_dir.display().to_string(),
            database: options.database.display().to_string(),
            bot: options.bot.clone(),
            from: None,
            limit: options.limit,
            events: 0,
            matched_events: 0,
            differing_events: 0,
            failed_events: 0,
            same: 0,
            extra: 0,
            missing: 0,
            reordered: 0,
            entries: Vec::new(),
            error: None,
        }
    }

    /// 按 `entries` 重算各项计数。
    fn summarize(&mut self) {
        self.events = self.entries.len();
        self.matched_events = self
            .entries
            .iter()
            .filter(|entry| entry.status == EntryStatus::Match)
            .count();
        self.differing_events = self
            .entries
            .iter()
            .filter(|entry| entry.status == EntryStatus::Diff)
            .count();
        self.failed_events = self
            .entries
            .iter()
            .filter(|entry| entry.status == EntryStatus::Error)
            .count();
        self.same = self.entries.iter().map(|entry| entry.same.len()).sum();
        self.extra = self.entries.iter().map(|entry| entry.extra.len()).sum();
        self.missing = self.entries.iter().map(|entry| entry.missing.len()).sum();
        self.reordered = self.entries.iter().map(|entry| entry.reordered.len()).sum();
        self.ok = self.error.is_none() && self.failed_events == 0 && self.differing_events == 0;
    }

    /// 按选项与一条致命错误构造报告（`ok = false`）。
    ///
    /// 给"还没开始跑就失败"的调用方用（例如 CLI 解析不了 `--from`）：不必自己拼字段，
    /// 也不会漏掉 [`Self::summarize`] 的计数一致性。
    pub fn failed(options: &ReplayOptions, error: impl Into<String>) -> Self {
        let mut report = Self::new(options);
        report.error = Some(error.into());
        report.summarize();
        report
    }
}

/// 项目库的固定路径：`<project-dir>/.hbot/hbot.db`。
pub fn database_path(project_dir: &Path) -> PathBuf {
    project_dir.join(Path::new(DATABASE_SUFFIX))
}

/// 把 `conversations.kind` 的存储字符串还原成领域枚举。
///
/// 取值与 `hbot_rpc` 的 `conversation_kind_from_str` 完全一致（`private` / `group` /
/// `channel`，其它一律 `Unknown`）。**刻意重复 6 行而不建共享函数**：本 crate 的依赖白名单
/// 里没有 `hbot-rpc`（回放是比 rpc 更低的一层，不能反过来依赖协议层），而
/// `hbot-bot::supervisor` 里已有一份同样的刻意重复，理由相同。存储侧的唯一真源是
/// `conversations.kind` 这一列本身（写入方是 `hbot-bot::ids` 与适配器）。
fn conversation_kind_of(raw: &str) -> ConversationKind {
    match raw {
        "private" => ConversationKind::Private,
        "group" => ConversationKind::Group,
        "channel" => ConversationKind::Channel,
        _ => ConversationKind::Unknown,
    }
}

/// 把一行历史入站还原成**生产链路同款**的事件 JSON。
///
/// 生产侧（`hbot-rpc` 的 sandbox handler）与回放/夹具侧都必须从这里出：这是"存下来的夹具"
/// 与"回放时喂进去的事件"共用的唯一构造点，另造一份就等着两边形状漂移。
pub(crate) fn event_of_row(row: &InboundReplayRow, bot_id: BotId, payload: &Value) -> Value {
    event_json(&EventSource {
        bot_id,
        conversation_id: row.message.conversation_id,
        message_id: MessageId::from_uuid(*row.message.id.as_uuid()),
        external_id: row.message.external_id.as_str(),
        sender_id: row.message.sender_id.as_str(),
        text: payload.get("text").and_then(Value::as_str),
        kind: conversation_kind_of(&row.conversation_kind),
        conversation_external_id: row.conversation_external_id.as_str(),
        received_at: Timestamp::from_unix_millis(row.message.received_at),
        payload,
    })
}

/// 面向人的路径文案：去掉 Windows `canonicalize` 带来的 `\\?\` 前缀。
///
/// 夹具路径会直接回显到 UI / 报告里，`\\?\C:\...` 只会让人以为路径坏了。
pub(crate) fn display_path(path: &Path) -> String {
    let raw = path.display().to_string();
    if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        raw
    }
}

/// 选出这一批要回放的历史入站：`from`（含）之后、最多 `limit` 条。
///
/// 单独抽出来是为了能在**不编译项目、不起 isolate** 的前提下钉住选批语义（见本文件测试）：
/// 回放的其余步骤都要真编译一遍项目，只有这一步是纯读库。
///
/// `limit = Some(0)` 是错误而不是"不限"：它一条都不会回放，用户想要的几乎一定是"不限"。
fn select_inbound(
    store: &Store,
    bot_id: &BotId,
    options: &ReplayOptions,
    database: &Path,
) -> Result<Vec<InboundReplayRow>, String> {
    let Some(limit) = options.limit else {
        return store
            .list_inbound_history_for_bot(bot_id, options.from, u32::MAX)
            .map_err(|error| format!("读历史入站失败（{}）：{error}", database.display()));
    };
    if limit == 0 {
        return Err("limit 为 0 会一条都不回放；要全量就别给 limit".to_owned());
    }
    store
        .list_inbound_history_for_bot(bot_id, options.from, limit)
        .map_err(|error| format!("读历史入站失败（{}）：{error}", database.display()))
}

/// 用历史入站重跑当前代码，返回报告（**不打印、不决定退出码**——那是调用方的事）。
///
/// `store` 是调用方打开的那份历史库（见 crate 文档的"两种库"）：CLI 传 `<project>/.hbot/hbot.db`
/// 那份，IDE 传 `AppState` 的应用库。本函数不打开库、也不清空它。
///
/// 致命错误不 panic 也不返回 `Err`：写进 `report.error`，由调用方映射（CLI → 退出码 2，
/// RPC → `-32006`）。
pub fn replay(store: &Store, options: &ReplayOptions) -> ReplayReport {
    let mut report = ReplayReport::new(options);
    match collect(store, options, &mut report) {
        Ok(entries) => {
            report.entries = entries;
            report.summarize();
        }
        Err(error) => {
            report.entries.clear();
            report.error = Some(error);
            report.summarize();
        }
    }
    report
}

/// 真正干活的那一段：读库、编译、逐条 dispatch。返回 `Err(String)` 表示回放跑不起来。
fn collect(
    store: &Store,
    options: &ReplayOptions,
    report: &mut ReplayReport,
) -> Result<Vec<InboundReplay>, String> {
    let project_dir = options.project_dir.as_path();
    if !project_dir.is_dir() {
        return Err(format!(
            "项目目录不存在或不是目录：{}",
            project_dir.display()
        ));
    }
    report.from = options.from;
    let database = options.database.as_path();

    let manifest = manifest::load(project_dir).map_err(|error| {
        format!(
            "读不了项目清单 {}：{error}",
            project_dir.join("hbot.json").display()
        )
    })?;

    let bot_id = BotId::from_str(&options.bot).map_err(|error| {
        format!(
            "--bot 不是合法的 bot id（要 UUID）：{}：{error}",
            options.bot
        )
    })?;
    let bot = store
        .get_bot(&bot_id)
        .map_err(|error| format!("读 bot 失败（{}）：{error}", database.display()))?
        .ok_or_else(|| {
            format!(
                "库里没有 bot {}（库：{}）。用 `hbot serve` 起来的那个库里的 bot id 才对得上；\
                 也可以先 `SELECT id, name FROM bots` 查一下",
                options.bot,
                database.display()
            )
        })?;
    // 归属校验：库里的项目行按路径登记，路径写法可能带不带尾部分隔符、是不是 canonical
    // 都不一定，所以两种写法都试；都查不到就不拦（老库里可能压根没登记项目行）。
    if let Some(project) = find_project_row(store, project_dir) {
        if project.id != bot.project_id {
            return Err(format!(
                "bot {} 属于别的项目，不给 {} 回放：它的 project_id 是 {}，而这个项目行的 id 是 {}（{}）",
                options.bot,
                project_dir.display(),
                bot.project_id,
                project.id,
                project.path,
            ));
        }
    }

    let rows = select_inbound(store, &bot_id, options, database)?;
    if rows.is_empty() {
        let mut hint = format!(
            "bot {} 在 {} 里没有可回放的历史入站",
            options.bot,
            database.display()
        );
        if let Some(millis) = options.from {
            let _ = write!(hint, "（`--from` 下界 {millis} 毫秒之后一条都没有）");
        }
        hint.push_str("；先让它真跑几条消息再回放");
        return Err(hint);
    }

    // payload 先全部解析：坏行记在它自己那条事件上，不拖累别的条。
    let payloads: Vec<Result<Value, String>> = rows
        .iter()
        .map(|row| {
            serde_json::from_str::<Value>(&row.message.payload_json).map_err(|error| {
                format!(
                    "入站 {}（external_id={}）的 payload_json 不是合法 JSON：{error}",
                    row.message.id, row.message.external_id
                )
            })
        })
        .collect();

    if let Some(path) = &options.save_fixture {
        let broken = payloads.iter().filter(|payload| payload.is_err()).count();
        if broken > 0 {
            return Err(format!(
                "有 {broken} 条入站的 payload_json 不是合法 JSON，没法生成 fixture；\
                 先修库或换 --from 缩小范围"
            ));
        }
        let fixtures: Vec<&Value> = payloads
            .iter()
            .filter_map(|payload| payload.as_ref().ok())
            .collect();
        write_fixture(path, &fixtures)?;
    }

    let reply_to: Vec<String> = rows
        .iter()
        .map(|row| row.message.external_id.clone())
        .collect();
    let outbound = store
        .list_outbound_by_reply_to(&bot_id, &reply_to)
        .map_err(|error| format!("读历史出站失败（{}）：{error}", database.display()))?;
    // SQL 已按 (created_at, rowid) 升序，所以按 key 聚起来就保持了历史顺序。
    let mut historical: HashMap<String, Vec<String>> = HashMap::new();
    for row in outbound {
        if let Some(key) = row.reply_to {
            historical.entry(key).or_default().push(row.text);
        }
    }

    let entry_name = manifest.entry_name();
    let compiled = compile_project_with_entry(project_dir, &entry_name)
        .map_err(|error| format!("编译当前代码失败（{}）：{error}", project_dir.display()))?;
    let sandbox = Sandbox::spawn(compiled, manifest.sandbox_limits())
        .map_err(|error| format!("启动新 isolate 失败：{error}"))?;

    let mut entries = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let mut entry = InboundReplay {
            inbound_id: row.message.id.to_string(),
            external_id: row.message.external_id.clone(),
            conversation_external_id: row.conversation_external_id.clone(),
            received_at: row.message.received_at,
            status: EntryStatus::Match,
            historical: historical
                .get(&row.message.external_id)
                .cloned()
                .unwrap_or_default(),
            replayed: Vec::new(),
            same: Vec::new(),
            extra: Vec::new(),
            missing: Vec::new(),
            reordered: Vec::new(),
            error: None,
        };
        match &payloads[index] {
            Err(error) => {
                entry.status = EntryStatus::Error;
                entry.error = Some(error.clone());
            }
            Ok(payload) => {
                let event = event_of_row(row, bot_id, payload);
                match sandbox.dispatch_collecting(&event) {
                    Ok(outcome) => entry.replayed = outcome.replies,
                    Err(failure) => {
                        entry.status = EntryStatus::Error;
                        entry.error = Some(format!("handler 抛异常：{}", failure.error));
                    }
                }
            }
        }
        let diff = diff_replies(&entry.historical, &entry.replayed);
        entry.same = diff.same;
        entry.extra = diff.extra;
        entry.missing = diff.missing;
        entry.reordered = diff.reordered;
        if entry.status != EntryStatus::Error {
            entry.status =
                if entry.missing.is_empty() && entry.extra.is_empty() && entry.reordered.is_empty()
                {
                    EntryStatus::Match
                } else {
                    EntryStatus::Diff
                };
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// 按路径找项目行（原样与 canonical 两种写法都试）；找不到返回 `None`。
fn find_project_row(store: &Store, project_dir: &Path) -> Option<hbot_store::Project> {
    let mut candidates = vec![project_dir.to_string_lossy().into_owned()];
    if let Ok(canonical) = std::fs::canonicalize(project_dir) {
        candidates.push(canonical.to_string_lossy().into_owned());
    }
    for candidate in candidates {
        match store.get_project_by_path(&candidate) {
            Ok(Some(project)) => return Some(project),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %error, path = %candidate, "读项目行失败，跳过归属校验");
                return None;
            }
        }
    }
    None
}

/// 对比历史出站与本次重跑结果（纯函数，不含 IO）。
///
/// 算法：先按文本做**多重集贪心匹配**（重跑的第 i 条找历史里第一个还没被用掉的同文本行），
/// 剩下没配上的历史行 = 缺少、没配上的重跑行 = 多出；配上的对按历史下标排序后取**最长递增
/// 子序列**，落在子序列里的是"顺序也对"，其余是"顺序不同"。O(n²)，事件内回复条数个位数，
/// 不值为它上 LIS 的 O(n log n)。
pub fn diff_replies(historical: &[String], replayed: &[String]) -> RepliesDiff {
    let mut used = vec![false; historical.len()];
    // (历史下标, 重跑下标)，已按重跑顺序（= 文本匹配的贪心顺序）排列。
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (replayed_index, text) in replayed.iter().enumerate() {
        let mut matched = None;
        for (historical_index, candidate) in historical.iter().enumerate() {
            if !used[historical_index] && candidate == text {
                matched = Some(historical_index);
                break;
            }
        }
        if let Some(historical_index) = matched {
            used[historical_index] = true;
            pairs.push((historical_index, replayed_index));
        }
    }

    let mut diff = RepliesDiff {
        extra: replayed
            .iter()
            .enumerate()
            .filter(|(replayed_index, _)| !pairs.iter().any(|(_, paired)| paired == replayed_index))
            .map(|(_, text)| text.clone())
            .collect(),
        missing: historical
            .iter()
            .enumerate()
            .filter(|(historical_index, _)| !used[*historical_index])
            .map(|(_, text)| text.clone())
            .collect(),
        ..RepliesDiff::default()
    };

    pairs.sort_by_key(|(historical_index, _)| *historical_index);
    let positions: Vec<usize> = pairs
        .iter()
        .map(|(_, replayed_index)| *replayed_index)
        .collect();
    let keep = longest_increasing_subsequence(&positions);
    for (position, (_, replayed_index)) in pairs.iter().enumerate() {
        let text = replayed[*replayed_index].clone();
        if keep.contains(&position) {
            diff.same.push(text);
        } else {
            diff.reordered.push(ReorderedReply {
                text,
                historical_index: pairs[position].0,
                replayed_index: *replayed_index,
            });
        }
    }
    diff
}

/// 返回 `values` 的**最长严格递增子序列**各元素下标（`O(n²)`，重建路径）。
fn longest_increasing_subsequence(values: &[usize]) -> Vec<usize> {
    if values.is_empty() {
        return Vec::new();
    }
    let mut lengths = vec![1usize; values.len()];
    let mut previous: Vec<Option<usize>> = vec![None; values.len()];
    for current in 0..values.len() {
        for earlier in 0..current {
            if values[earlier] < values[current] && lengths[earlier] + 1 > lengths[current] {
                lengths[current] = lengths[earlier] + 1;
                previous[current] = Some(earlier);
            }
        }
    }
    let mut tail = 0usize;
    for (index, length) in lengths.iter().enumerate() {
        if *length > lengths[tail] {
            tail = index;
        }
    }
    let mut chain = Vec::with_capacity(lengths[tail]);
    let mut cursor = Some(tail);
    while let Some(index) = cursor {
        chain.push(index);
        cursor = previous[index];
    }
    chain.reverse();
    chain
}

/// 把这一批 payload 存成 fixture：**JSON 数组**，pretty 打印，结尾换行。
fn write_fixture(path: &Path, payloads: &[&Value]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("建 fixture 目录失败（{}）：{error}", parent.display()))?;
        }
    }
    let mut text = serde_json::to_string_pretty(payloads)
        .map_err(|error| format!("序列化 fixture 失败：{error}"))?;
    text.push('\n');
    std::fs::write(path, text)
        .map_err(|error| format!("写 fixture 失败（{}）：{error}", path.display()))?;
    tracing::info!(path = %path.display(), events = payloads.len(), "fixture 已保存");
    Ok(())
}

/// 把 RFC3339 时间戳解析成 Unix 毫秒。
///
/// workspace 里没有 `chrono` / `time`（根 `Cargo.toml` 的 `workspace.dependencies` 不在本
/// 任务的可改范围内），而 `--from` 只需要一个下界，所以手写最小解析器：只接受
/// `YYYY-MM-DDThh:mm:ss[.fff…][Z|±hh:mm]`（`T`/`t`/空格分隔、`Z`/`z` 都认）。
/// 秒允许 `60`（闰秒）。不校验"这一天是否真的存在"（2 月 30 日会被当成 3 月 2 日）——
/// 它是下界过滤条件，宽一点比拒掉用户的时间戳好。
pub fn parse_rfc3339_millis(text: &str) -> Result<i64, String> {
    let bytes = text.as_bytes();
    let invalid = |reason: &str| {
        format!("--from 不是合法的 RFC3339 时间：{text}（{reason}；例：2026-11-16T07:00:00Z）")
    };
    let number = |start: usize, count: usize| -> Result<i64, String> {
        let end = start + count;
        if end > bytes.len() {
            return Err(invalid(&format!(
                "读到偏移 {end} 就越界了，缺 {count} 位数字"
            )));
        }
        let mut value = 0i64;
        for byte in &bytes[start..end] {
            if !byte.is_ascii_digit() {
                return Err(invalid(&format!("偏移 {start} 起的 {count} 位不是数字")));
            }
            value = value * 10 + i64::from(byte - b'0');
        }
        Ok(value)
    };
    let expect = |at: usize, byte: u8| -> Result<usize, String> {
        if bytes.get(at) == Some(&byte) {
            Ok(at + 1)
        } else {
            Err(invalid(&format!("偏移 {at} 处应当是 `{}`", byte as char)))
        }
    };

    let year = number(0, 4)?;
    let mut at = expect(4, b'-')?;
    let month = number(at, 2)?;
    at = expect(at + 2, b'-')?;
    let day = number(at, 2)?;
    at += 2;
    match bytes.get(at) {
        Some(b'T' | b't' | b' ') => at += 1,
        _ => return Err(invalid("日期与时间之间缺 `T` 分隔符")),
    }
    let hour = number(at, 2)?;
    at = expect(at + 2, b':')?;
    let minute = number(at, 2)?;
    at = expect(at + 2, b':')?;
    let second = number(at, 2)?;
    at += 2;
    let mut millis = 0i64;
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        let start = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == start {
            return Err(invalid("小数点后缺数字"));
        }
        let fraction = &text[start..at];
        let mut padded = fraction.chars().take(3).collect::<String>();
        while padded.len() < 3 {
            padded.push('0');
        }
        millis = padded
            .parse::<i64>()
            .map_err(|_| invalid("小数部分不是数字"))?;
    }
    let offset_minutes = match bytes.get(at) {
        Some(b'Z' | b'z') => {
            at += 1;
            0
        }
        Some(sign @ (b'+' | b'-')) => {
            let sign = if *sign == b'-' { -1 } else { 1 };
            let hours = number(at + 1, 2)?;
            let minutes = number(at + 4, 2)?;
            at += 6;
            sign * (hours * 60 + minutes)
        }
        _ => return Err(invalid("缺时区（`Z` 或 `±hh:mm`）")),
    };
    if at != bytes.len() {
        return Err(invalid(&format!("末尾多出 {} 个字符", bytes.len() - at)));
    }
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return Err(invalid(&format!(
            "字段超范围（{year}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}）"
        )));
    }

    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
        - offset_minutes * 60;
    Ok(seconds * 1_000 + millis)
}

/// 公历日期距 1970-01-01 的天数（Howard Hinnant 的 `days_from_civil`）。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn rfc3339_matches_the_epoch_anchor() {
        assert_eq!(
            parse_rfc3339_millis("2023-11-14T22:13:20Z").unwrap(),
            1_700_000_000_000
        );
        assert_eq!(parse_rfc3339_millis("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(
            parse_rfc3339_millis("2023-11-14T22:13:20.250Z").unwrap(),
            1_700_000_000_250
        );
        // +08:00 的同一时刻比 UTC 早 8 小时写出。
        assert_eq!(
            parse_rfc3339_millis("2023-11-15T06:13:20+08:00").unwrap(),
            1_700_000_000_000
        );
    }

    #[test]
    fn rfc3339_rejects_junk() {
        assert!(parse_rfc3339_millis("2023-11-14").is_err());
        assert!(parse_rfc3339_millis("2023-13-01T00:00:00Z").is_err());
        assert!(parse_rfc3339_millis("2023-11-14T25:00:00Z").is_err());
        assert!(parse_rfc3339_millis("2023-11-14T22:13:20").is_err());
        assert!(parse_rfc3339_millis("2023-11-14T22:13:20Zx").is_err());
    }

    #[test]
    fn identical_replies_are_all_same() {
        let diff = diff_replies(&texts(&["a", "b"]), &texts(&["a", "b"]));
        assert_eq!(diff.same, texts(&["a", "b"]));
        assert!(diff.is_identical());
    }

    #[test]
    fn changed_text_is_missing_plus_extra() {
        let diff = diff_replies(&texts(&["v1"]), &texts(&["v2"]));
        assert_eq!(diff.missing, texts(&["v1"]));
        assert_eq!(diff.extra, texts(&["v2"]));
        assert!(diff.same.is_empty());
    }

    #[test]
    fn flipped_order_is_reported_as_reordered() {
        let diff = diff_replies(&texts(&["a", "b"]), &texts(&["b", "a"]));
        assert!(diff.missing.is_empty());
        assert!(diff.extra.is_empty());
        // 最长递增子序列会留下一条，另一条算顺序不同。
        assert_eq!(diff.same.len() + diff.reordered.len(), 2);
        assert_eq!(diff.reordered.len(), 1);
        assert!(!diff.is_identical());
    }

    #[test]
    fn duplicates_are_matched_one_to_one() {
        let diff = diff_replies(&texts(&["a", "a"]), &texts(&["a"]));
        assert_eq!(diff.same, texts(&["a"]));
        assert_eq!(diff.missing, texts(&["a"]));
        assert!(diff.extra.is_empty());
    }

    #[test]
    fn extra_reply_when_history_was_empty() {
        let diff = diff_replies(&[], &texts(&["hi"]));
        assert_eq!(diff.extra, texts(&["hi"]));
        assert!(diff.missing.is_empty());
        assert!(diff.same.is_empty());
    }

    /// 本 crate 的依赖白名单里没有第三方 tempdir，测试自己建一个（`Drop` 时清掉）。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hbot-replay-{tag}-{}-{sequence}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("建临时目录");
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

    /// 造一份只含历史入站的最小库：`select_inbound` 只需要 projects / bots /
    /// conversations / inbound 四张表，**不编译项目、不起 isolate**。
    fn seed_store(dir: &Path) -> (Store, BotId) {
        let store = Store::open(dir.join("hbot.db")).expect("打开测试库");
        let project_id = hbot_store::ProjectId::from_str("11111111-1111-4111-8111-111111111111")
            .expect("固定 UUID");
        let bot_id = BotId::from_str("22222222-2222-4222-8222-222222222222").expect("固定 UUID");
        let conversation_id =
            hbot_store::ConversationId::from_str("33333333-3333-4333-8333-333333333333")
                .expect("固定 UUID");
        store
            .insert_project(&hbot_store::Project {
                id: project_id,
                name: "replay-select".to_owned(),
                path: dir.display().to_string(),
                created_at: 0,
            })
            .expect("插 projects 行");
        store
            .insert_bot(&hbot_store::Bot {
                id: bot_id,
                project_id,
                name: "replay-bot".to_owned(),
                platform: "onebot11".to_owned(),
                config_json: "{}".to_owned(),
                enabled: true,
                created_at: 0,
            })
            .expect("插 bots 行");
        store
            .insert_conversation(&hbot_store::Conversation {
                id: conversation_id,
                bot_id,
                external_id: "30001".to_owned(),
                kind: "group".to_owned(),
                created_at: 0,
            })
            .expect("插 conversations 行");
        for (index, external_id) in ["m1", "m2", "m3"].iter().enumerate() {
            store
                .insert_inbound_with_sender(
                    hbot_store::NewInbound {
                        id: hbot_store::InboundId::from_str(&format!(
                            "00000000-0000-4000-8000-00000000000{index}"
                        ))
                        .expect("固定 UUID"),
                        bot_id,
                        external_id: (*external_id).to_owned(),
                        conversation_id,
                        payload_json: format!("{{\"text\":\"{external_id}\"}}"),
                        // 每条隔 1 秒，便于用 from 精确卡边界。
                        received_at: 1_000 + index as i64 * 1_000,
                    },
                    "10086".to_owned(),
                )
                .expect("插 inbound 行");
        }
        (store, bot_id)
    }

    /// 取到的行的 external_id，断言里直接看批次。
    fn selected(rows: &[InboundReplayRow]) -> Vec<&str> {
        rows.iter()
            .map(|row| row.message.external_id.as_str())
            .collect()
    }

    /// 同一份 `Store` 上的选批语义：`limit` 从头截、`from` 是"含"的下界、`None` 是全量，
    /// 而 `limit = Some(0)` 是**错误**（用户想说的几乎一定是"不限"）。
    #[test]
    fn selection_honours_limit_and_from_on_the_same_store() {
        let dir = TempDir::new("select");
        let (store, bot_id) = seed_store(dir.path());
        let database = dir.path().join("hbot.db");
        let options = |limit: Option<u32>, from: Option<i64>| ReplayOptions {
            project_dir: dir.path().to_path_buf(),
            database: database.clone(),
            bot: bot_id.to_string(),
            limit,
            from,
            save_fixture: None,
        };
        let select = |limit: Option<u32>, from: Option<i64>| {
            select_inbound(&store, &bot_id, &options(limit, from), &database)
        };

        // `limit = Some(0)` 报错，文案要让用户知道该怎么改。
        let error = select(Some(0), None).expect_err("limit=0 应当报错");
        assert!(error.contains("limit"), "错误文案要提到 limit：{error}");

        // `None` 是全量。
        assert_eq!(
            selected(&select(None, None).expect("全量取历史")),
            ["m1", "m2", "m3"]
        );

        // `limit` 从头截。
        assert_eq!(
            selected(&select(Some(2), None).expect("取前两条")),
            ["m1", "m2"]
        );

        // `from` 是"含"该时刻的下界。
        assert_eq!(
            selected(&select(None, Some(2_000)).expect("取 from 之后")),
            ["m2", "m3"]
        );

        // 两者一起用：先按 from 过滤，再截 limit。
        assert_eq!(
            selected(&select(Some(1), Some(2_000)).expect("取 from 之后的第一条")),
            ["m2"]
        );

        // 下界越过后一条都没有：那是空批次，不是错误（要不要提示是调用方的事）。
        assert!(select(None, Some(9_999)).expect("空批次不报错").is_empty());
    }
}
