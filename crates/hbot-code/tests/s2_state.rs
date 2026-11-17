//! S2: `ctx.kv.*` 与 `ctx.history(n)` 端到端 —— 真的 [`Sandbox`]、真的 `dispatch`。
//!
//! 每个测试都建一个真 isolate、编译真的 `src/main.ts`、把事件真的跑一圈。唯一被替
//! 换掉的是宿主那两个端口（`BotState`、`HistoryReader`），因为它们是**唯一**该被替
//! 换的部分：装配、超时、参数闸的顺序都在 `crates/hbot-code/src/runtime/state_api.rs`
//! 里实现一次，调用方不该能复现第二份。
//!
//! ① 未装配 → `HBOT_STATE_UNAVAILABLE`，且后端一次都没被调用；
//! ② 装配后 `set` → `get` 能拿回同一个值（对象 / 数组 / `null`）；
//! ③ `delete` 老实报告"这次到底删掉了一行没有"；
//! ④–⑥ 坏参数（坏键、超长 TTL、超批量）在**后端被碰之前**就被拒 —— 参数错不许占
//!    一次 `spawn_blocking`、也不许变成一次事务；
//! ⑦–⑨ `history` 的字段名、夹上限、缺 `conversationId` 的失败面；
//! ⑩ `list` 的前缀语义；⑪ 未装配时 `history` 的失败面。
//!
//! **没有 read-only 档的用例**：状态能力不受 `sandboxMode` 限制。它读写的是本 bot
//! 自己的命名空间（一个 SQLite 表），不是工程目录、不是外部副作用；把 `read-only`
//! 理解成"连自己的状态都不能写"会让只读档的 bot 连记忆都没有。真正限制它的是清单
//! 声明（`capabilities.state.*`）与宿主装配，两者都在这份测试里被钉住。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hbot_code::error::CodeError;
use hbot_code::{EventOutcome, HttpConfig, Limits, Sandbox, compile_project};
use hbot_core::{
    BotId, BotState, BotStateError, ConversationId, HistoryDirection, HistoryEntry, HistoryReader,
    KvEntry, KvWriteOptions, Timestamp,
};
use serde_json::{Value, json};

/// 能解析成 `BotId` 的 id；事件载荷里的 `botId`。
const BOT: &str = "00000000-0000-0000-0000-000000000001";
/// 同上，`conversationId`。**必须是合法 uuid**：`ctx.history` 要把它解析成
/// `ConversationId`，拿 `c-1` 这类字符串会被判成"本次事件没有 conversationId"。
const CONVERSATION: &str = "00000000-0000-0000-0000-000000000002";

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// 临时项目目录，Drop 时删掉。
struct Project(PathBuf);

impl Project {
    fn new(name: &str, source: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "hbot-s2-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("建项目目录");
        std::fs::write(src.join("main.ts"), source).expect("写 src/main.ts");
        Self(root)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 编译 `source` 并起一个 isolate。
fn sandbox(name: &str, source: &str) -> (Project, Sandbox) {
    let project = Project::new(name, source);
    let compiled = compile_project(&project.0).expect("编译项目");
    let sandbox = Sandbox::spawn_with_http(compiled, Limits::default(), HttpConfig::deny_all())
        .expect("启动 isolate");
    (project, sandbox)
}

/// 一条带 bot 身份与会话身份的事件。
fn dispatch(sandbox: &Sandbox) -> Result<EventOutcome, CodeError> {
    sandbox.dispatch(&json!({
        "botId": BOT,
        "conversationId": CONVERSATION,
        "messageId": "m-1",
        "text": "hi",
    }))
}

/// 不带 `conversationId` 的事件：`ctx.history` 必须因此报错，而不是沿用上一条。
fn dispatch_without_conversation(sandbox: &Sandbox) -> Result<EventOutcome, CodeError> {
    sandbox.dispatch(&json!({
        "botId": BOT,
        "messageId": "m-1",
        "text": "hi",
    }))
}

/// 抛出来的 JavaScript 异常 message。
fn thrown(error: &CodeError) -> &str {
    match error {
        CodeError::JsException { message, .. } => message,
        other => panic!("期望 JavaScript 异常，实际是 {other:?}"),
    }
}

/// 必须失败的那次 dispatch 的 message。
fn dispatch_error(sandbox: &Sandbox) -> String {
    match dispatch(sandbox) {
        Ok(outcome) => panic!("这次调用本该失败，实际成功：{:?}", outcome.logs),
        Err(error) => thrown(&error).to_owned(),
    }
}

// ---------------------------------------------------------------------------
// 宿主那两个端口
// ---------------------------------------------------------------------------

/// 一个同时实现 `BotState` 与 `HistoryReader` 的假后端。
///
/// 记的不只是返回值，还有**调用次数**：④–⑥ 要证明坏参数连一次后端调用都不占。
#[derive(Default)]
struct CountingState {
    /// 任何端口方法被进入的次数。
    calls: AtomicUsize,
    /// kv 的假存储。
    kv: Mutex<HashMap<String, Value>>,
    /// `kv_list` 收到的前缀。
    prefixes: Mutex<Vec<Option<String>>>,
    /// `kv_set` / `kv_set_many` 收到的 TTL。
    ttls: Mutex<Vec<Option<Duration>>>,
    /// `history` 收到的条数（夹没夹，看这里）。
    limits: Mutex<Vec<u32>>,
    /// `history` 收到的 `(bot, conversation)`。
    queries: Mutex<Vec<(String, String)>>,
    /// `history` 返回的行（按 index 升序）。
    rows: Mutex<Vec<HistoryEntry>>,
}

impl CountingState {
    /// 一个带 `rows` 条历史行的假后端。
    fn with_rows(rows: usize) -> Self {
        let state = Self::default();
        *state.rows.lock().expect("rows") = (0..rows)
            .map(|index| HistoryEntry {
                direction: HistoryDirection::Inbound,
                message_id: format!("m-{index}"),
                external_id: format!("ext-{index}"),
                sender_id: "user-1".to_owned(),
                text: format!("第 {index} 条"),
                at: Timestamp::from_unix_millis(1_700_000_000_000 + index as i64),
            })
            .collect();
        state
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl BotState for CountingState {
    fn kv_get(&self, _bot_id: &BotId, key: &str) -> Result<Option<Value>, BotStateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.kv.lock().expect("kv").get(key).cloned())
    }

    fn kv_set(
        &self,
        _bot_id: &BotId,
        key: &str,
        value: Value,
        options: KvWriteOptions,
    ) -> Result<(), BotStateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.ttls.lock().expect("ttls").push(options.ttl);
        self.kv.lock().expect("kv").insert(key.to_owned(), value);
        Ok(())
    }

    fn kv_delete(&self, _bot_id: &BotId, key: &str) -> Result<bool, BotStateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.kv.lock().expect("kv").remove(key).is_some())
    }

    fn kv_list(&self, _bot_id: &BotId, prefix: Option<&str>) -> Result<Vec<String>, BotStateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.prefixes
            .lock()
            .expect("prefixes")
            .push(prefix.map(str::to_owned));
        let kv = self.kv.lock().expect("kv");
        let mut keys: Vec<String> = kv
            .keys()
            .filter(|key| prefix.is_none_or(|prefix| key.starts_with(prefix)))
            .cloned()
            .collect();
        keys.sort();
        Ok(keys)
    }

    fn kv_set_many(
        &self,
        _bot_id: &BotId,
        entries: Vec<KvEntry>,
        options: KvWriteOptions,
    ) -> Result<(), BotStateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.ttls.lock().expect("ttls").push(options.ttl);
        let mut kv = self.kv.lock().expect("kv");
        for entry in entries {
            kv.insert(entry.key, entry.value);
        }
        Ok(())
    }
}

impl HistoryReader for CountingState {
    fn history(
        &self,
        bot_id: &BotId,
        conversation_id: &ConversationId,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>, BotStateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.limits.lock().expect("limits").push(limit);
        self.queries
            .lock()
            .expect("queries")
            .push((bot_id.to_string(), conversation_id.to_string()));
        Ok(self
            .rows
            .lock()
            .expect("rows")
            .iter()
            .take(limit as usize)
            .cloned()
            .collect())
    }
}

/// 装配假后端，返回它以便断言。
fn assemble(sandbox: &Sandbox, rows: usize) -> Arc<CountingState> {
    let state = Arc::new(CountingState::with_rows(rows));
    let backend: Arc<dyn BotState> = state.clone();
    let reader: Arc<dyn HistoryReader> = state.clone();
    sandbox.set_state(backend, reader);
    state
}

// ---------------------------------------------------------------------------
// ① 未装配
// ---------------------------------------------------------------------------

#[test]
fn without_an_assembled_state_every_call_is_unavailable() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    await ctx.kv.get('a');
  },
});
"#;
    let (_project, sandbox) = sandbox("unavailable", source);

    assert!(
        !sandbox.is_state_assembled(),
        "新建的 Sandbox 不该自带状态后端"
    );

    let message = dispatch_error(&sandbox);
    assert!(
        message.starts_with("HBOT_STATE_UNAVAILABLE:"),
        "未装配时必须是 HBOT_STATE_UNAVAILABLE，实际：{message}"
    );
    assert!(
        message.contains("宿主没有装配状态能力"),
        "错误信息应说清状态能力还没被装配，实际：{message}"
    );
}

#[test]
fn without_an_assembled_history_the_call_is_unavailable() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    await ctx.history();
  },
});
"#;
    let (_project, sandbox) = sandbox("history-unavailable", source);

    let message = dispatch_error(&sandbox);
    assert!(
        message.starts_with("HBOT_STATE_UNAVAILABLE:"),
        "未装配时 history 也必须是 HBOT_STATE_UNAVAILABLE，实际：{message}"
    );
    assert!(
        message.contains("HistoryReader"),
        "错误信息应指名缺的是哪个端口，实际：{message}"
    );
}

// ---------------------------------------------------------------------------
// ② 存进去、读回来
// ---------------------------------------------------------------------------

#[test]
fn a_set_then_get_round_trips_objects_arrays_and_null() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    await ctx.kv.set('object', { a: 1, b: [true, null] });
    ctx.log(JSON.stringify(await ctx.kv.get('object')));
    await ctx.kv.set('array', [1, 'two', { three: 3 }]);
    ctx.log(JSON.stringify(await ctx.kv.get('array')));
    // `null` 能存也能取回；它与"没有这个键"在 JS 侧同样是 `null`，所以这条只钉
    // "能存、不报错、读回还是 null"，不假装能区分两者。
    await ctx.kv.set('null', null);
    ctx.log(String((await ctx.kv.get('null')) === null));
    ctx.log(String((await ctx.kv.get('missing')) === null));
  },
});
"#;
    let (_project, sandbox) = sandbox("round-trip", source);
    let state = assemble(&sandbox, 0);

    let outcome = dispatch(&sandbox).expect("装配后应成功");
    assert_eq!(outcome.logs.len(), 4, "四次 log：{:?}", outcome.logs);
    assert_eq!(outcome.logs[0], r#"{"a":1,"b":[true,null]}"#);
    assert_eq!(outcome.logs[1], r#"[1,"two",{"three":3}]"#);
    assert_eq!(outcome.logs[2], "true", "null 应能存进去并读回 null");
    assert_eq!(outcome.logs[3], "true", "不存在的键必须解析成 null");
    // 三次 set + 四次 get（object / array / null / 不存在的键），每一次都真的进后端。
    assert_eq!(state.calls(), 7, "三次 set + 四次 get 应各调一次后端");
}

// ---------------------------------------------------------------------------
// ③ delete
// ---------------------------------------------------------------------------

#[test]
fn delete_reports_whether_a_row_existed() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    ctx.log(String(await ctx.kv.delete('k')));
    await ctx.kv.set('k', 1);
    ctx.log(String(await ctx.kv.delete('k')));
    ctx.log(String(await ctx.kv.delete('k')));
  },
});
"#;
    let (_project, sandbox) = sandbox("delete", source);
    assemble(&sandbox, 0);

    let outcome = dispatch(&sandbox).expect("应成功");
    assert_eq!(
        outcome.logs,
        vec!["false".to_owned(), "true".into(), "false".into()],
        "delete 的返回值必须是「这次真的删掉了一行」，实际：{:?}",
        outcome.logs
    );
}

// ---------------------------------------------------------------------------
// ④ 坏键：后端一次都不许被调用
// ---------------------------------------------------------------------------

#[test]
fn a_bad_key_is_refused_before_the_backend_is_called() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    for (const key of ['', 'x'.repeat(129)]) {
      try {
        await ctx.kv.set(key, 1);
      } catch (error) {
        ctx.log(error.message);
      }
    }
    // 合法的键照旧能写：闸不是把整条路径关掉。
    await ctx.kv.set('fine', 1);
    ctx.log('ok');
  },
});
"#;
    let (_project, sandbox) = sandbox("bad-key", source);
    let state = assemble(&sandbox, 0);

    let outcome = dispatch(&sandbox).expect("坏键被拒，但 handler 自己接住了，事件本身成功");
    assert_eq!(
        outcome.logs.len(),
        3,
        "两次错误 + 一次 ok：{:?}",
        outcome.logs
    );
    for log in &outcome.logs[..2] {
        assert!(
            log.starts_with("HBOT_STATE_INVALID:"),
            "坏键必须是 HBOT_STATE_INVALID，实际：{log}"
        );
    }
    assert_eq!(outcome.logs[2], "ok");
    assert_eq!(
        state.calls(),
        1,
        "两次坏键都不许占后端调用，只有最后那次合法写才该进后端"
    );
}

// ---------------------------------------------------------------------------
// ⑤ TTL 超上限：报错，不静默截断
// ---------------------------------------------------------------------------

#[test]
fn an_over_long_ttl_is_refused_instead_of_being_clamped() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    // 31 天：比 30 天上限多一点点，夹住就是"用户以为 31 天、实际 30 天"。
    try {
      await ctx.kv.set('k', 1, { ttlMs: 31 * 24 * 60 * 60 * 1000 });
    } catch (error) {
      ctx.log(error.message);
    }
    // 非正 / 非整数 / 非有限也一律拒绝，不猜。
    for (const ttlMs of [0, -1, 1.5, Number.POSITIVE_INFINITY]) {
      try {
        await ctx.kv.set('k', 1, { ttlMs });
      } catch (error) {
        ctx.log(error.message);
      }
    }
    // 上限之内照旧能写。
    await ctx.kv.set('k', 1, { ttlMs: 1000 });
    ctx.log('ok');
  },
});
"#;
    let (_project, sandbox) = sandbox("ttl", source);
    let state = assemble(&sandbox, 0);

    let outcome = dispatch(&sandbox).expect("TTL 被拒，handler 接住了");
    assert_eq!(outcome.logs.len(), 6, "五次错误 + ok：{:?}", outcome.logs);
    for log in &outcome.logs[..5] {
        assert!(
            log.starts_with("HBOT_STATE_INVALID:"),
            "非法 TTL 必须是 HBOT_STATE_INVALID，实际：{log}"
        );
    }
    assert_eq!(outcome.logs[5], "ok");
    assert_eq!(state.calls(), 1, "只有上限之内那次该进后端");
    assert_eq!(
        state.ttls.lock().expect("ttls").as_slice(),
        &[Some(Duration::from_millis(1000))],
        "合法 TTL 要原样换算成 Duration 传给端口"
    );
}

// ---------------------------------------------------------------------------
// ⑥ setMany 超批量
// ---------------------------------------------------------------------------

#[test]
fn set_many_refuses_more_than_the_batch_cap() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    const tooMany = Array.from({ length: 101 }, (_, index) => ({ key: `k${index}`, value: index }));
    try {
      await ctx.kv.setMany(tooMany);
    } catch (error) {
      ctx.log(error.message);
    }
    await ctx.kv.setMany([{ key: 'a', value: 1 }, { key: 'b', value: { two: 2 } }]);
    ctx.log(JSON.stringify(await ctx.kv.get('b')));
  },
});
"#;
    let (_project, sandbox) = sandbox("batch", source);
    let state = assemble(&sandbox, 0);

    let outcome = dispatch(&sandbox).expect("超批量被拒，handler 接住了");
    assert_eq!(outcome.logs.len(), 2, "{:?}", outcome.logs);
    assert!(
        outcome.logs[0].starts_with("HBOT_STATE_INVALID:"),
        "101 条必须是 HBOT_STATE_INVALID，实际：{}",
        outcome.logs[0]
    );
    assert_eq!(outcome.logs[1], r#"{"two":2}"#);
    assert_eq!(
        state.calls(),
        2,
        "被拒的那批不许占后端调用（只剩 setMany + get）"
    );
}

// ---------------------------------------------------------------------------
// ⑦ history 的字段名
// ---------------------------------------------------------------------------

#[test]
fn history_returns_the_documented_field_names() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    const rows = await ctx.history(3);
    ctx.log(JSON.stringify(rows));
  },
});
"#;
    let (_project, sandbox) = sandbox("history-fields", source);
    let state = assemble(&sandbox, 5);

    let outcome = dispatch(&sandbox).expect("应成功");
    assert_eq!(outcome.logs.len(), 1, "{:?}", outcome.logs);

    let rows: Value = serde_json::from_str(&outcome.logs[0]).expect("history 结果是 JSON 数组");
    let rows = rows.as_array().expect("history 必须返回数组");
    assert_eq!(rows.len(), 3, "limit 3 应只要 3 条");
    assert_eq!(
        state.queries.lock().expect("queries").as_slice(),
        &[(BOT.to_owned(), CONVERSATION.to_owned())],
        "bot 与会话都必须来自本次事件"
    );

    let row = &rows[0];
    assert_eq!(
        row["direction"],
        json!("inbound"),
        "direction 是字面量字符串"
    );
    assert_eq!(row["messageId"], json!("m-0"));
    assert_eq!(row["externalId"], json!("ext-0"));
    assert_eq!(row["senderId"], json!("user-1"));
    assert_eq!(row["text"], json!("第 0 条"));
    assert_eq!(
        row["at"],
        json!(1_700_000_000_000_i64),
        "at 是 unix 毫秒数字"
    );
    assert!(row["at"].is_number(), "at 必须是数字，不是字符串");
    // 手搓对象的理由：`HistoryEntry` 的 Rust 字段是 snake_case，序列化后是
    // `message_id`；JS 侧承诺的是 camelCase。两个名字都在就是写错了。
    assert!(
        row.get("message_id").is_none(),
        "不该把 Rust 的 snake_case 字段名漏到 JS 侧：{row:?}"
    );
}

// ---------------------------------------------------------------------------
// ⑧ limit：缺省 20、超上限夹住
// ---------------------------------------------------------------------------

#[test]
fn history_clamps_the_limit_instead_of_reporting() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    ctx.log(String((await ctx.history()).length));
    ctx.log(String((await ctx.history(999)).length));
  },
});
"#;
    let (_project, sandbox) = sandbox("history-limit", source);
    let state = assemble(&sandbox, 60);

    let outcome = dispatch(&sandbox).expect("应成功");
    assert_eq!(
        state.limits.lock().expect("limits").as_slice(),
        &[20, 50],
        "不传 → 20；999 → 夹到 50（契约里唯一一处「夹而不报」）"
    );
    assert_eq!(
        outcome.logs,
        vec!["20".to_owned(), "50".into()],
        "夹住之后拿到的条数也要对得上：{:?}",
        outcome.logs
    );
}

// ---------------------------------------------------------------------------
// ⑨ 缺 conversationId：报错，且不碰后端
// ---------------------------------------------------------------------------

#[test]
fn history_without_a_conversation_id_is_refused() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    try {
      await ctx.history();
    } catch (error) {
      ctx.log(error.message);
    }
  },
});
"#;
    let (_project, sandbox) = sandbox("history-no-conversation", source);
    let state = assemble(&sandbox, 5);

    let outcome = match dispatch_without_conversation(&sandbox) {
        Ok(outcome) => outcome,
        Err(error) => panic!("handler 自己接住了，事件不该失败：{error:?}"),
    };
    assert_eq!(outcome.logs.len(), 1, "{:?}", outcome.logs);
    assert!(
        outcome.logs[0].starts_with("HBOT_STATE_INVALID:"),
        "缺 conversationId 必须是 HBOT_STATE_INVALID，实际：{}",
        outcome.logs[0]
    );
    assert!(
        outcome.logs[0].contains("conversationId"),
        "错误信息应点名缺的是 conversationId，实际：{}",
        outcome.logs[0]
    );
    assert_eq!(
        state.calls(),
        0,
        "不知道该读哪个会话时就不许读——更不能拿上一条事件的会话顶替"
    );
}

// ---------------------------------------------------------------------------
// ⑩ list 的前缀
// ---------------------------------------------------------------------------

#[test]
fn list_treats_a_missing_or_empty_prefix_as_all_keys() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    await ctx.kv.set('a:1', 1);
    await ctx.kv.set('a:2', 2);
    await ctx.kv.set('b:1', 3);
    ctx.log(JSON.stringify(await ctx.kv.list()));
    ctx.log(JSON.stringify(await ctx.kv.list('')));
    ctx.log(JSON.stringify(await ctx.kv.list('a:')));
  },
});
"#;
    let (_project, sandbox) = sandbox("list", source);
    let state = assemble(&sandbox, 0);

    let outcome = dispatch(&sandbox).expect("应成功");
    assert_eq!(outcome.logs.len(), 3, "{:?}", outcome.logs);
    assert_eq!(outcome.logs[0], r#"["a:1","a:2","b:1"]"#);
    assert_eq!(
        outcome.logs[1], outcome.logs[0],
        "空串前缀与不传必须是同一件事"
    );
    assert_eq!(outcome.logs[2], r#"["a:1","a:2"]"#);
    assert_eq!(
        state.prefixes.lock().expect("prefixes").as_slice(),
        &[None, None, Some("a:".to_owned())],
        "空串要按 None 传给端口，不是 Some(\"\")"
    );
}
