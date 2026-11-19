//! 宿主侧的 Bot 状态能力（C2）：`ctx.kv.*` 与 `ctx.history(n)` 的端口实现。
//!
//! 契约在 `hbot_core::bot_state`（**已冻结**）：两个同步阻塞端口 trait
//! [`BotState`] / [`HistoryReader`]、参数闸（`validate_*` / `history_limit`）与
//! `HBOT_STATE_*` 错误码。本模块只做"把它接到 SQLite 上"，**不重复任何判据**：
//!
//! * 参数闸一律调用 `hbot_core::bot_state` 里的那一份（键长/值大小/批量条数/TTL）；
//! * 能力声明闸（清单的 `capabilities.state.kv` / `.history`）在**每个方法的第一句**，
//!   这是"声明层 + 装配层两道闸"的声明层那一半（`hbot-code` 侧还会判一次装配）。
//!
//! ## 为什么存储层不解析 JSON
//!
//! `bot_kv.value_json` 是原样字符串（`crates/hbot-store/src/models.rs` 的
//! [`hbot_store::NewKvEntry`] 文档）：存储层不认识 `serde_json::Value`。所以"读出来的
//! 文本坏掉怎么办"这个问题只能在这里回答 —— 答案是
//! [`BotStateError::Failed`]，**不是** `Ok(None)`。
//!
//! 静默返回 `None` 会把"库里有坏数据"伪装成"这个键没存过"，于是 handler 会当它不存在
//! 重新写一遍，坏行被覆盖、事故被抹平、没有人知道曾经出过错。宁可让这次调用失败并
//! 在 message 里带上键名。
//!
//! ## 为什么装配是工厂（[`StateSource`]）而不是一个现成实例
//!
//! 两个事实的**作用域不同**：
//!
//! * 能力声明是**项目级**的（`hbot.json` 的 `capabilities.state`），项目可以在运行中
//!   被编辑、被热重载、被换掉；
//! * `Store` 与时钟是**宿主级**的（一个进程一份，从 `AppState` 来）。
//!
//! 把两者接起来的那个动作必须发生在"为新项目装一份状态"的时刻，所以配置里存的是
//! `Arc<dyn StateSource>`，而不是一个装好声明的 `Arc<dyn BotState>`。这与
//! `SandboxHandler` 每次 spawn 都重读 `device_apps(&self.project_root)` 是同一个理由：
//! 清单是项目文件，改了它不会产生别的信号，**每次起 isolate 现读一遍**最省事，也避免
//! "清单改了但沙箱还拿着旧声明"。
//!
//! 因此 [`StoreStateSource`] **不缓存清单**：`handles()` 每次都 `manifest::load`。
//! 读不到清单（文件缺失/非法）时按"全不声明"处理 —— 与 `net::http_config` 的
//! fail-closed 口径一致：装配不起来不等于可以放行。

use std::path::Path;
use std::sync::Arc;

use hbot_code::manifest;
use hbot_code::manifest::Capabilities;
use hbot_core::bot_state::{
    self, BotState, BotStateError, HistoryDirection, HistoryEntry, HistoryReader, KvEntry,
    KvWriteOptions, MAX_KV_LIST,
};
use hbot_core::{BotId, Clock, ConversationId, Timestamp};
use hbot_store::{HistoryRow, NewKvEntry, Store};
use serde_json::Value;

/// 每个 bot 项目一份状态装配：清单里的 `capabilities.state` 决定装什么。
///
/// 为什么是工厂见模块文档：声明是项目级的，`Store` 与时钟是宿主级的。
///
/// 实现方只做两件事：读清单、按声明造端口。**不要在实现里缓存清单**，否则热重载
/// 换掉的 `hbot.json` 不会生效；也**不要在实现里做 IO 之外的事**（不起后台任务、
/// 不写日志表）。
pub trait StateSource: Send + Sync {
    /// 为 `root` 这个项目造一份状态装配。
    ///
    /// 返回的两个端口分开给：宿主可以只声明 `kv` 不声明 `history`（声明是分开的两
    /// 个布尔），此时用不到的那个端口每次调用都回 `HBOT_STATE_NOT_DECLARED`。
    fn handles(&self, root: &Path) -> (Arc<dyn BotState>, Arc<dyn HistoryReader>);
}

/// 用 SQLite 支撑的 [`BotState`]：每 bot 一个命名空间，值序列化成 JSON 落 `bot_kv`。
///
/// 同一个结构也实现 [`HistoryReader`]：两者共享的只有 `store` / `clock` / 声明，
/// 拆成两个结构不会让任何一边更简单，反而要求装配方记住"两个都要用同一个声明"。
pub struct StoreBotState {
    store: Arc<Store>,
    /// 清单声明（`capabilities.state`）。**每次调用都重新判**：声明是权威，
    /// 不是构造时的一次性快照。
    declared: Capabilities,
    clock: Arc<dyn Clock>,
}

impl StoreBotState {
    /// 用存储层 + 声明 + 时钟造一个端口实现。
    #[must_use]
    pub fn new(store: Arc<Store>, declared: Capabilities, clock: Arc<dyn Clock>) -> Self {
        Self {
            store,
            declared,
            clock,
        }
    }

    /// 现在（Unix 毫秒）。TTL 的到期判定要能注入，所以时间只能从端口取。
    fn now(&self) -> i64 {
        self.clock.now().unix_millis()
    }

    /// 声明闸：`kv` 没声明就是 `HBOT_STATE_NOT_DECLARED`。
    ///
    /// `capability` 是 `&'static str`：错误要能被 `match` / 被生成代码按前缀区分，
    /// 不能是运行期拼出来的字符串。
    fn require_kv(&self) -> Result<(), BotStateError> {
        if self.declared.kv() {
            Ok(())
        } else {
            Err(BotStateError::NotDeclared { capability: "kv" })
        }
    }

    /// 声明闸：`history` 没声明就是 `HBOT_STATE_NOT_DECLARED`。
    fn require_history(&self) -> Result<(), BotStateError> {
        if self.declared.history() {
            Ok(())
        } else {
            Err(BotStateError::NotDeclared {
                capability: "history",
            })
        }
    }

    /// `options.ttl` → `expires_at`（Unix 毫秒）。
    ///
    /// `None` 必须映射成 `None`（永不过期），不能"保留旧的到期时间"：调用方把 TTL
    /// 去掉的意图就是让这个键留下来。存储层的 upsert 会原样覆盖这一列，
    /// 见 `crates/hbot-store/src/repositories/kv.rs` 的模块文档。
    fn expires_at(&self, options: KvWriteOptions, now: i64) -> Option<i64> {
        options
            .ttl
            .map(|ttl| now.saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)))
    }
}

/// 存储层错误 → `HBOT_STATE_FAILED`。
///
/// 一律 `Failed`：磁盘满、库损坏、writer 线程退出都是环境故障或 bug，不是调用方
/// 能改参数解决的问题（"改个键名再试"是假建议）。
fn failed(error: hbot_store::StoreError) -> BotStateError {
    BotStateError::Failed {
        message: error.to_string(),
    }
}

/// 值文本 → `serde_json::Value`；坏数据报 `Failed`，不伪装成"键不存在"。
fn parse_value(key: &str, text: &str) -> Result<Value, BotStateError> {
    serde_json::from_str(text).map_err(|error| BotStateError::Failed {
        message: format!("kv 键 `{key}` 的值不是合法 JSON（库里的坏数据）：{error}"),
    })
}

impl BotState for StoreBotState {
    fn kv_get(&self, bot_id: &BotId, key: &str) -> Result<Option<Value>, BotStateError> {
        self.require_kv()?;
        bot_state::validate_key(key)?;
        let now = self.now();
        let stored = self.store.get_kv(bot_id, key, now).map_err(failed)?;
        match stored {
            None => Ok(None),
            Some(text) => parse_value(key, &text).map(Some),
        }
    }

    fn kv_set(
        &self,
        bot_id: &BotId,
        key: &str,
        value: Value,
        options: KvWriteOptions,
    ) -> Result<(), BotStateError> {
        self.require_kv()?;
        bot_state::validate_key(key)?;
        bot_state::validate_value(&value)?;
        bot_state::validate_ttl(options.ttl)?;
        let now = self.now();
        let expires_at = self.expires_at(options, now);
        // 存储层只认文本：序列化失败（理论上不可能，`Value` 总能序列化）也是 Failed。
        let text = serde_json::to_string(&value).map_err(|error| BotStateError::Failed {
            message: format!("kv 值无法序列化成 JSON：{error}"),
        })?;
        self.store
            .set_kv(bot_id, key, &text, expires_at, now)
            .map_err(failed)
    }

    fn kv_delete(&self, bot_id: &BotId, key: &str) -> Result<bool, BotStateError> {
        self.require_kv()?;
        bot_state::validate_key(key)?;
        self.store.delete_kv(bot_id, key).map_err(failed)
    }

    fn kv_list(&self, bot_id: &BotId, prefix: Option<&str>) -> Result<Vec<String>, BotStateError> {
        self.require_kv()?;
        // 空前缀 = 不过滤（列出全部）；非空前缀按键名同一套上限校验。
        if let Some(prefix) = prefix.filter(|prefix| !prefix.is_empty()) {
            bot_state::validate_key(prefix)?;
        }
        let now = self.now();
        self.store
            .list_kv(
                bot_id,
                prefix,
                u32::try_from(MAX_KV_LIST).unwrap_or(u32::MAX),
                now,
            )
            .map_err(failed)
    }

    fn kv_set_many(
        &self,
        bot_id: &BotId,
        entries: Vec<KvEntry>,
        options: KvWriteOptions,
    ) -> Result<(), BotStateError> {
        self.require_kv()?;
        bot_state::validate_batch(entries.len())?;
        bot_state::validate_ttl(options.ttl)?;
        let now = self.now();
        let expires_at = self.expires_at(options, now);
        let mut rows = Vec::with_capacity(entries.len());
        for entry in entries {
            bot_state::validate_key(&entry.key)?;
            bot_state::validate_value(&entry.value)?;
            let text =
                serde_json::to_string(&entry.value).map_err(|error| BotStateError::Failed {
                    message: format!("kv 值无法序列化成 JSON：{error}"),
                })?;
            rows.push(NewKvEntry {
                key: entry.key,
                value_json: text,
                expires_at,
            });
        }
        self.store.set_kv_many(bot_id, &rows, now).map_err(failed)
    }
}

impl HistoryReader for StoreBotState {
    fn history(
        &self,
        bot_id: &BotId,
        conversation_id: &ConversationId,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>, BotStateError> {
        self.require_history()?;
        // 夹住而不是报错（`history_limit` 的文档写了为什么与 TTL 不一致）。
        let limit = bot_state::history_limit(limit);
        let rows = self
            .store
            .list_history(bot_id, conversation_id, limit)
            .map_err(failed)?;
        Ok(rows.into_iter().map(entry_of).collect())
    }
}

/// 存储行 → 契约条目。
///
/// `direction` 只认 `"inbound"`；其它一律 `Outbound`（含 `"outbound"` 本身与将来可能
/// 出现的第三值）。理由：契约枚举只有两个方向，而**把入站误判成出站**与
/// **把出站误判成入站**的后果不对称 —— 后者会让 handler 以为"别人刚说了这句话"。
/// 存储层写这一列的地方只有两处，见 `list_history` 的实现，所以"其它"实际不会出现；
/// 真出现了说明有人在 SQL 里手改了数据，归到出站是较安全的一侧。
fn entry_of(row: HistoryRow) -> HistoryEntry {
    let direction = if row.direction == "inbound" {
        HistoryDirection::Inbound
    } else {
        HistoryDirection::Outbound
    };
    HistoryEntry {
        direction,
        message_id: row.message_id,
        external_id: row.external_id,
        sender_id: row.sender_id,
        text: row.text,
        at: Timestamp::from_unix_millis(row.at),
    }
}

/// 生产用的 [`StateSource`]：`Store` + 宿主时钟，按项目清单里的声明装配。
///
/// 组合根在 `crate::app::commands::bots` 的 `bot_start` 里造一个塞进
/// `SandboxHandlerConfig::state`。
pub struct StoreStateSource {
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
}

impl StoreStateSource {
    /// 用宿主级的两个端口造工厂。
    #[must_use]
    pub fn new(store: Arc<Store>, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }
}

impl StateSource for StoreStateSource {
    fn handles(&self, root: &Path) -> (Arc<dyn BotState>, Arc<dyn HistoryReader>) {
        // 读不到清单 = 全不声明（fail-closed）。**不缓存**：热重载要能换声明。
        let declared = manifest::load(root)
            .map(|manifest| manifest.capabilities)
            .unwrap_or_default();
        let state: Arc<StoreBotState> = Arc::new(StoreBotState::new(
            Arc::clone(&self.store),
            declared,
            Arc::clone(&self.clock),
        ));
        // 两个 trait 各有自己的 `Arc<dyn _>`：显式标注目标类型走 unsize coercion。
        // 不能写 `Arc::clone(&state)` —— 那会把期望类型推成 `Arc<StoreBotState>`。
        let history: Arc<dyn HistoryReader> = state.clone();
        let state: Arc<dyn BotState> = state;
        (state, history)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use hbot_core::{ProjectId, Timestamp};
    use hbot_store::{Bot, Project};
    use serde_json::json;

    use crate::app::ids::ManualClock;

    /// 临时目录（Drop 时删干净）。
    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "hbot-state-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or_default()
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

    const START_MS: i64 = 1_000_000;

    fn declared(kv: bool, history: bool) -> Capabilities {
        // 用 `..Default::default()` 而不是列全字段：清单的 `capabilities` 还会长
        // （C5 加了 `ai` 就把这个字面量红过），而这里只关心 `state` 那两位。
        Capabilities {
            state: hbot_code::manifest::StateCapabilities {
                kv,
                history,
                session: false,
                sql: false,
            },
            ..Capabilities::default()
        }
    }

    fn bot_id(value: u128) -> BotId {
        BotId::from_uuid(uuid::Uuid::from_u128(value))
    }

    /// 一个真 `Store` + 一个落库的 bot + 声明全开的端口。
    fn store_with_bot(label: &str) -> (TempDir, Arc<Store>, BotId) {
        let dir = TempDir::new(label);
        let store = Arc::new(Store::open(dir.path().join("hbot.db")).expect("打开测试库"));
        let project_id = ProjectId::from_uuid(uuid::Uuid::from_u128(0x5700_0000_0000_0001));
        let bot = bot_id(0x5700_0000_0000_0002);
        store
            .insert_project(&Project {
                id: project_id,
                name: "state-test".to_owned(),
                path: dir.path().display().to_string(),
                created_at: 1,
            })
            .expect("插入项目");
        store
            .insert_bot(&Bot {
                id: bot,
                project_id,
                name: "state-bot".to_owned(),
                platform: "local".to_owned(),
                config_json: "{}".to_owned(),
                enabled: true,
                created_at: 1,
            })
            .expect("插入 bot");
        (dir, store, bot)
    }

    /// 测试用时钟：返回 `ManualClock`（能拨）与它的端口视图（能注入）。
    ///
    /// 两次 `Arc::clone` 共享同一个 `Mutex<Timestamp>`，所以拨动对端口立即可见。
    fn clock() -> (Arc<ManualClock>, Arc<dyn Clock>) {
        let manual = Arc::new(ManualClock::new(Timestamp::from_unix_millis(START_MS)));
        let port: Arc<dyn Clock> = Arc::clone(&manual) as Arc<dyn Clock>;
        (manual, port)
    }

    /// 测试装配：只关心时钟端口，手动时钟由需要拨表的用例自己持有。
    fn state_of(
        store: &Arc<Store>,
        clock: &Arc<dyn Clock>,
        capabilities: Capabilities,
    ) -> StoreBotState {
        StoreBotState::new(Arc::clone(store), capabilities, Arc::clone(clock))
    }

    // ① 声明闸：没声明 kv 的每个方法都拒，错误码是 NOT_DECLARED。
    #[test]
    fn an_undeclared_kv_refuses_every_method_with_the_stable_code() {
        let (_dir, store, bot) = store_with_bot("undeclared-kv");
        let (_manual, clock) = clock();
        let state = state_of(&store, &clock, declared(false, false));

        let cases: [BotStateError; 4] = [
            state.kv_get(&bot, "k").expect_err("get 应被拒"),
            state
                .kv_set(&bot, "k", json!(1), KvWriteOptions::default())
                .expect_err("set 应被拒"),
            state.kv_delete(&bot, "k").expect_err("delete 应被拒"),
            state.kv_list(&bot, None).expect_err("list 应被拒"),
        ];
        for error in cases {
            assert_eq!(error, BotStateError::NotDeclared { capability: "kv" });
            assert_eq!(error.code(), "HBOT_STATE_NOT_DECLARED");
        }
        assert_eq!(
            state
                .kv_set_many(
                    &bot,
                    vec![KvEntry::new("k", json!(1))],
                    KvWriteOptions::default()
                )
                .expect_err("setMany 应被拒"),
            BotStateError::NotDeclared { capability: "kv" }
        );
    }

    // ② set 之后 get 拿到解析后的值；空表 get 是 None。
    #[test]
    fn set_then_get_round_trips_the_json_value() {
        let (_dir, store, bot) = store_with_bot("round-trip");
        let (_manual, clock) = clock();
        let state = state_of(&store, &clock, declared(true, true));

        assert_eq!(state.kv_get(&bot, "missing").expect("读不存在的键"), None);
        state
            .kv_set(&bot, "counter", json!({"n": 3}), KvWriteOptions::default())
            .expect("写");
        assert_eq!(
            state.kv_get(&bot, "counter").expect("读回"),
            Some(json!({"n": 3}))
        );
        // 覆盖写。
        state
            .kv_set(&bot, "counter", json!("text"), KvWriteOptions::default())
            .expect("覆盖写");
        assert_eq!(
            state.kv_get(&bot, "counter").expect("读回"),
            Some(json!("text"))
        );
        assert!(state.kv_delete(&bot, "counter").expect("删"));
        assert!(!state.kv_delete(&bot, "counter").expect("再删"));
        assert_eq!(state.kv_get(&bot, "counter").expect("读回"), None);
    }

    // ③ 坏参数在碰 store 之前就被拒：错误码 INVALID 且 message 不提 SQLite。
    //
    // 用**一个不存在的 bot_id** 证明这一点：外键不满足时真去写库会报 SQLite 外键
    // 失败（Failed），而这里必须拿到 Invalid —— 说明参数闸排在存储调用之前。
    #[test]
    fn bad_arguments_are_refused_before_the_store_is_touched() {
        let (_dir, store, _bot) = store_with_bot("bad-args");
        let (_manual, clock) = clock();
        let state = state_of(&store, &clock, declared(true, true));
        let ghost = bot_id(0x5700_0000_0000_00ff);

        let empty_key = state.kv_set(&ghost, "", json!(1), KvWriteOptions::default());
        let error = empty_key.expect_err("空键应被拒");
        assert_eq!(error.code(), "HBOT_STATE_INVALID");
        assert!(
            !error.to_string().to_lowercase().contains("sqlite"),
            "{error}"
        );

        let long_key = "k".repeat(bot_state::MAX_KV_KEY_BYTES + 1);
        assert_eq!(
            state
                .kv_set(&ghost, &long_key, json!(1), KvWriteOptions::default())
                .expect_err("超长键应被拒")
                .code(),
            "HBOT_STATE_INVALID"
        );

        let huge = json!("x".repeat(bot_state::MAX_KV_VALUE_BYTES + 1));
        let error = state
            .kv_set(&ghost, "k", huge, KvWriteOptions::default())
            .expect_err("超长值应被拒");
        assert_eq!(error.code(), "HBOT_STATE_INVALID");

        let too_long_ttl = KvWriteOptions {
            ttl: Some(bot_state::MAX_KV_TTL + std::time::Duration::from_secs(1)),
        };
        assert_eq!(
            state
                .kv_set(&ghost, "k", json!(1), too_long_ttl)
                .expect_err("超长 TTL 应被拒")
                .code(),
            "HBOT_STATE_INVALID"
        );

        let too_many: Vec<KvEntry> = (0..=bot_state::MAX_KV_BATCH)
            .map(|index| KvEntry::new(format!("k{index}"), json!(index)))
            .collect();
        assert_eq!(
            state
                .kv_set_many(&ghost, too_many, KvWriteOptions::default())
                .expect_err("超量批量应被拒")
                .code(),
            "HBOT_STATE_INVALID"
        );

        // 参数闸漏了的话这里会是 Failed（外键约束），不是 Invalid。
        assert_eq!(
            state
                .kv_set(&ghost, "ok", json!(1), KvWriteOptions::default())
                .expect_err("外键不满足应报 Failed")
                .code(),
            "HBOT_STATE_FAILED"
        );
    }

    // ④ TTL：写时带 ttl，时间拨过到期点后 get 拿到 None；把 ttl 去掉（None）能
    // 把旧的到期时间清成永不过期。
    #[test]
    fn a_ttl_key_disappears_once_the_clock_passes_the_deadline() {
        let (_dir, store, bot) = store_with_bot("ttl");
        let (manual, clock) = clock();
        let state = state_of(&store, &clock, declared(true, true));

        state
            .kv_set(
                &bot,
                "short",
                json!(7),
                KvWriteOptions {
                    ttl: Some(std::time::Duration::from_millis(50)),
                },
            )
            .expect("写带 TTL 的键");
        assert_eq!(state.kv_get(&bot, "short").expect("立即读"), Some(json!(7)));

        manual.advance(50);
        assert_eq!(state.kv_get(&bot, "short").expect("到期后读"), None);

        // 覆盖写成永不过期：到期时间必须被清掉，不能"记住旧值"。
        state
            .kv_set(
                &bot,
                "short",
                json!(8),
                KvWriteOptions {
                    ttl: Some(std::time::Duration::from_millis(50)),
                },
            )
            .expect("再写一次");
        state
            .kv_set(&bot, "short", json!(9), KvWriteOptions::default())
            .expect("改成永不过期");
        manual.advance(1_000);
        assert_eq!(
            state.kv_get(&bot, "short").expect("永不过期"),
            Some(json!(9))
        );
    }

    // ⑤ 隔离性：同一个 Store 上两个 bot 的 kv 互不可见。
    #[test]
    fn two_bots_on_the_same_store_do_not_see_each_other() {
        let (_dir, store, first) = store_with_bot("isolation");
        let project_id = ProjectId::from_uuid(uuid::Uuid::from_u128(0x5700_0000_0000_0001));
        let second = bot_id(0x5700_0000_0000_0003);
        store
            .insert_bot(&Bot {
                id: second,
                project_id,
                name: "second-bot".to_owned(),
                platform: "local".to_owned(),
                config_json: "{}".to_owned(),
                enabled: true,
                created_at: 1,
            })
            .expect("插入第二个 bot");
        let state = state_of(&store, &clock().1, declared(true, true));

        state
            .kv_set(
                &first,
                "same-key",
                json!("first"),
                KvWriteOptions::default(),
            )
            .expect("写第一个 bot");
        assert_eq!(
            state.kv_get(&second, "same-key").expect("读第二个 bot"),
            None
        );
        assert_eq!(
            state.kv_get(&first, "same-key").expect("读第一个 bot"),
            Some(json!("first"))
        );
        assert_eq!(
            state.kv_list(&second, None).expect("列第二个 bot"),
            Vec::<String>::new()
        );
        assert_eq!(
            state.kv_list(&first, None).expect("列第一个 bot"),
            vec![String::from("same-key")]
        );
    }

    // ⑥ list 按前缀过滤 + setMany 是一批写入。
    #[test]
    fn list_filters_by_prefix_and_set_many_writes_the_whole_batch() {
        let (_dir, store, bot) = store_with_bot("batch");
        let state = state_of(&store, &clock().1, declared(true, true));

        state
            .kv_set_many(
                &bot,
                vec![
                    KvEntry::new("user:1:step", json!(1)),
                    KvEntry::new("user:2:step", json!(2)),
                    KvEntry::new("global", json!(true)),
                ],
                KvWriteOptions::default(),
            )
            .expect("批量写");

        assert_eq!(
            state.kv_list(&bot, Some("user:")).expect("按前缀列"),
            vec![String::from("user:1:step"), String::from("user:2:step")]
        );
        assert_eq!(
            state.kv_list(&bot, None).expect("列全部"),
            vec![
                String::from("global"),
                String::from("user:1:step"),
                String::from("user:2:step")
            ]
        );
        assert_eq!(
            state.kv_get(&bot, "user:2:step").expect("读批量写的键"),
            Some(json!(2))
        );
    }

    // ⑦ history：声明闸 + 行映射（direction 只认 "inbound"）+ 条数夹到 1..=MAX_HISTORY。
    #[test]
    fn history_maps_rows_and_respects_the_declaration() {
        let (dir, store, bot) = store_with_bot("history");
        let conversation = ConversationId::from_uuid(uuid::Uuid::from_u128(0x5700_0000_0000_0004));

        let undeclared = state_of(&store, &clock().1, declared(true, false));
        assert_eq!(
            undeclared
                .history(&bot, &conversation, 10)
                .expect_err("没声明应被拒"),
            BotStateError::NotDeclared {
                capability: "history"
            }
        );

        // 没有消息时是空表（会话不存在不是错误）。
        let state = state_of(&store, &clock().1, declared(true, true));
        assert_eq!(
            state.history(&bot, &conversation, 10).expect("空历史"),
            Vec::new()
        );

        // 造两条入站 + 一条出站，验证方向映射与时间。
        store
            .insert_conversation(&hbot_store::Conversation {
                id: conversation,
                bot_id: bot,
                external_id: "conv-1".to_owned(),
                kind: "private".to_owned(),
                created_at: 1,
            })
            .expect("插入会话");
        for (index, text) in ["你好", "在吗"].iter().enumerate() {
            store
                .insert_inbound_with_sender(
                    hbot_store::NewInbound {
                        id: hbot_store::InboundId::from_uuid(uuid::Uuid::from_u128(
                            0x5700_0000_0000_0100 + index as u128,
                        )),
                        bot_id: bot,
                        external_id: format!("ext-{index}"),
                        conversation_id: conversation,
                        payload_json: json!({ "text": text }).to_string(),
                        received_at: START_MS + index as i64,
                    },
                    "sender-1".to_owned(),
                )
                .expect("插入入站");
        }
        let rows = state.history(&bot, &conversation, 500).expect("读历史");
        assert!(!rows.is_empty(), "至少应有刚插入的入站行");
        assert!(
            rows.iter()
                .all(|row| row.direction == HistoryDirection::Inbound)
        );
        assert!(
            rows.len() <= bot_state::MAX_HISTORY as usize,
            "条数应被夹住"
        );
        let first = rows.first().expect("至少一条");
        assert_eq!(first.at, Timestamp::from_unix_millis(START_MS));
        assert_eq!(first.text, "你好");
        assert_eq!(first.sender_id, "sender-1");
        assert_eq!(first.external_id, "ext-0");

        // `direction` 的映射单独钉：`"outbound"` 与任何未知值都归出站。
        let outbound = entry_of(HistoryRow {
            direction: String::from("outbound"),
            message_id: String::from("m"),
            external_id: String::new(),
            sender_id: String::new(),
            text: String::new(),
            at: START_MS,
        });
        assert_eq!(outbound.direction, HistoryDirection::Outbound);
        let unknown = entry_of(HistoryRow {
            direction: String::from("weird"),
            message_id: String::from("m"),
            external_id: String::new(),
            sender_id: String::new(),
            text: String::new(),
            at: START_MS,
        });
        assert_eq!(unknown.direction, HistoryDirection::Outbound);
        drop(dir);
    }

    // ⑧ 工厂：读不到清单 → 全不声明；清单声明了 → 真的能用。
    #[test]
    fn the_source_reads_the_manifest_on_every_call() {
        let dir = TempDir::new("source");
        let store = Arc::new(Store::open(dir.path().join("hbot.db")).expect("打开测试库"));
        let project_id = ProjectId::from_uuid(uuid::Uuid::from_u128(0x5700_0000_0000_0005));
        let bot = bot_id(0x5700_0000_0000_0006);
        store
            .insert_project(&Project {
                id: project_id,
                name: "source-project".to_owned(),
                path: dir.path().display().to_string(),
                created_at: 1,
            })
            .expect("插入项目");
        store
            .insert_bot(&Bot {
                id: bot,
                project_id,
                name: "source-bot".to_owned(),
                platform: "local".to_owned(),
                config_json: "{}".to_owned(),
                enabled: true,
                created_at: 1,
            })
            .expect("插入 bot");

        let clock = clock();
        let source = StoreStateSource::new(Arc::clone(&store), Arc::clone(&clock.1));
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("建项目目录");
        std::fs::write(project.join("src").join("main.ts"), "export {};\n").expect("写入口");

        // 没有 hbot.json：全不声明，kv 被拒。
        let (state, history) = source.handles(&project);
        assert_eq!(
            state.kv_get(&bot, "k").expect_err("没声明应被拒"),
            BotStateError::NotDeclared { capability: "kv" }
        );
        assert_eq!(
            history
                .history(
                    &bot,
                    &ConversationId::from_uuid(uuid::Uuid::from_u128(1)),
                    5
                )
                .expect_err("没声明应被拒"),
            BotStateError::NotDeclared {
                capability: "history"
            }
        );

        // 写上清单后**同一个工厂**再问一次：必须读到新声明（不缓存）。
        std::fs::write(
            project.join("hbot.json"),
            r#"{
              "name": "state-test",
              "platform": { "kind": "local" },
              "capabilities": { "state": { "kv": true, "history": true } }
            }"#,
        )
        .expect("写清单");
        let (state, _history) = source.handles(&project);
        state
            .kv_set(&bot, "k", json!("v"), KvWriteOptions::default())
            .expect("声明后可写");
        assert_eq!(
            state.kv_get(&bot, "k").expect("声明后可读"),
            Some(json!("v"))
        );
    }
}
