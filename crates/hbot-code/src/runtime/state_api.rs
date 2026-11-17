//! `ctx.kv.*` 与 `ctx.history(n)` —— 状态能力（C2）在 JS 侧的那一层桥。
//!
//! 契约类型、两个端口 trait 与**参数闸**都在 `hbot_core::bot_state`（已冻结），
//! 装配与 `spawn_blocking` 调用形状在 [`crate::state`]。本模块只做三件事：把 JS
//! 参数解析成端口要的类型、把端口的返回值摆成文档承诺的形状、把失败变成
//! rejection。
//!
//! # 先解析参数、后取装配
//!
//! 与 [`crate::runtime::device_api`] 相反的顺序，理由不是风格而是**判据归属**：
//! 设备能力的闸要问宿主（清单 / 设置 / 会话），所以"有没有权限"必须先判，否则
//! 一个未授权调用会先把参数错误报给 handler，让人以为是自己的参数写错了；而
//! 状态能力**没有第二道授权闸**——能不能用只取决于"宿主装没装后端"和"本项目
//! 声明没声明"，两者都与参数内容无关。于是这里按最省事、也最不容易误报的顺序
//! 走：
//!
//! 1. **同步**解析并校验参数（`validate_key` / `validate_value` /
//!    `validate_batch` / `validate_ttl` / [`history_limit`]）；
//! 2. 读槽位拿当前装配与本次事件的身份；
//! 3. 才 `spawn_blocking` 进后端。
//!
//! 第 1 步不过就直接 rejection，**后端一次都不会被调用**：坏参数不许占预算、
//! 不许变成一次 SQLite 事务，也不许在限流/超时文案里把真正的参数错误盖住。
//!
//! # 存在不等于被授权
//!
//! `ctx.kv` 出现在 `hbot.d.ts` 里只说明**这个项目声明了** `capabilities.state.kv`；
//! 宿主没装配后端时抛 `HBOT_STATE_UNAVAILABLE`（**不**退回一个内存实现——那会让
//! "重启后数据没了"变成没人发现的静默行为），清单没声明时由后端给
//! `HBOT_STATE_NOT_DECLARED`。两种都不静默返回空值。
//!
//! # 为什么身份在槽位里而不是 install 的参数里
//!
//! isolate 跨事件复用：`ctx.kv` 与 `ctx.history` 是在 `bootstrap` 时装进
//! `globalThis.__hbot` 的，而"这条事件属于哪个 bot / 哪个会话"每次都不一样。
//! 所以身份由 `build_event` 逐事件写进 [`StateSlot`]，这里在**每次调用**时读；
//! 读不到就报错，绝不沿用上一条事件的身份。

use std::sync::Arc;
use std::time::Duration;

use rquickjs::prelude::{Async, Opt};
use rquickjs::{Array, Ctx, Exception, Function, IntoJs, Object, Value};
use serde_json::Value as Json;

use crate::error::CodeError;
use crate::limits::Deadline;
use crate::state::{self, StateHandles, StateSlot, budget_of, unavailable};
use hbot_core::{
    BotId, BotStateError, ConversationId, HistoryEntry, KvEntry, KvWriteOptions, MAX_KV_LIST,
    history_limit, validate_batch, validate_key, validate_ttl, validate_value,
};

/// `ctx.kv` 上的方法名，一处声明。
///
/// 这是数据不是散文：`crate::dts` 的漂移测试把它与 `install` 的 `entries` 表、
/// `HOST_MODULE_SOURCE` 的 `kv` 命名空间、`hbot.d.ts` 的 `KvApi` 四方对齐，
/// 少一个多一个都红。
pub(crate) const KV_BINDING_KEYS: &[&str] = &["get", "set", "delete", "list", "setMany"];

/// `ctx.history()` 不传参数时的条数。
///
/// 刻意小于 [`MAX_HISTORY`](hbot_core::MAX_HISTORY)：默认值该是"看一眼上文"的
/// 量，不是"把上下文灌满"的量。要更多就显式传。
const DEFAULT_HISTORY_LIMIT: u32 = 20;

/// 宿主没装 `BotState` 时的说明（`HBOT_STATE_UNAVAILABLE` 前缀由 [`unavailable`] 加）。
const UNASSEMBLED_KV: &str = "宿主没有装配状态能力（kv 需要 hbot-rpc 注入 BotState）";
/// 同上，history 端口。
const UNASSEMBLED_HISTORY: &str =
    "宿主没有装配状态能力（history 需要 hbot-rpc 注入 HistoryReader）";

/// 装 `ctx.kv` 对象与 `ctx.history` 函数，返回两者。
///
/// 调用方（[`crate::runtime::host_api::install`]）负责 `host.set("kv", ..)` 与
/// `host.set("history", ..)`：名字留在那一处，`dts` 的漂移测试才能一行一行对上。
pub(crate) fn install<'js>(
    ctx: &Ctx<'js>,
    state: &Arc<StateSlot>,
    deadline: &Arc<Deadline>,
) -> Result<(Object<'js>, Function<'js>), CodeError> {
    let object = Object::new(ctx.clone()).map_err(super::js_error)?;

    let get = {
        let slot = Arc::clone(state);
        let deadline = Arc::clone(deadline);
        Function::new(
            ctx.clone(),
            Async(move |key: Value<'js>| {
                let prepared = (|| {
                    let key = text_of(&key, "kv.get 的 key")?;
                    validate_key(&key).map_err(|error| error.to_string())?;
                    let handles = assembled(&slot, &deadline, UNASSEMBLED_KV)?;
                    Ok((handles, key))
                })();
                async move {
                    let (handles, key) = match prepared {
                        Ok(prepared) => prepared,
                        Err(message) => return StateReply::Value(Err(message)),
                    };
                    let Prepared {
                        handles,
                        bot,
                        budget,
                    } = handles;
                    let outcome =
                        state::call(budget, move || handles.state().kv_get(&bot, &key)).await;
                    StateReply::Value(outcome)
                }
            }),
        )
        .and_then(|function| function.with_name("get"))
        .map_err(super::js_error)?
    };

    let set = {
        let slot = Arc::clone(state);
        let deadline = Arc::clone(deadline);
        Function::new(
            ctx.clone(),
            Async(
                // `Ctx` 作为首参取回（`FromParam` + `ParamRequirement::none()`，
                // 不占位置参数）。**不许**把 `ctx.clone()` 存进闭包：那会让 JS
                // 函数对象活多久、`Ctx` 就活多久，而 `Ctx` 是运行时引用计数的
                // 一环 —— 运行时释放时会撞 `list_empty(&rt->gc_obj_list)` 断言。
                move |handle: Ctx<'js>,
                      key: Value<'js>,
                      value: Value<'js>,
                      options: Opt<Value<'js>>| {
                    let prepared = (|| {
                        let key = text_of(&key, "kv.set 的 key")?;
                        validate_key(&key).map_err(|error| error.to_string())?;
                        // 值先过 JSON，再过 `validate_value`：上限判的是**落库后**的
                        // 字节数，不是 JS 里那个对象看起来多大。
                        let json = json_of(&handle, &value, "kv")?;
                        validate_value(&json).map_err(|error| error.to_string())?;
                        let options = parse_write_options(options.0.as_ref())?;
                        let prepared = assembled(&slot, &deadline, UNASSEMBLED_KV)?;
                        Ok((prepared, key, json, options))
                    })();
                    async move {
                        let (prepared, key, json, options) = match prepared {
                            Ok(prepared) => prepared,
                            Err(message) => return StateReply::Done(Err(message)),
                        };
                        let Prepared {
                            handles,
                            bot,
                            budget,
                        } = prepared;
                        let outcome = state::call(budget, move || {
                            handles.state().kv_set(&bot, &key, json, options)
                        })
                        .await;
                        StateReply::Done(outcome)
                    }
                },
            ),
        )
        .and_then(|function| function.with_name("set"))
        .map_err(super::js_error)?
    };

    let delete = {
        let slot = Arc::clone(state);
        let deadline = Arc::clone(deadline);
        Function::new(
            ctx.clone(),
            Async(move |key: Value<'js>| {
                let prepared = (|| {
                    let key = text_of(&key, "kv.delete 的 key")?;
                    validate_key(&key).map_err(|error| error.to_string())?;
                    let prepared = assembled(&slot, &deadline, UNASSEMBLED_KV)?;
                    Ok((prepared, key))
                })();
                async move {
                    let (prepared, key) = match prepared {
                        Ok(prepared) => prepared,
                        Err(message) => return StateReply::Flag(Err(message)),
                    };
                    let Prepared {
                        handles,
                        bot,
                        budget,
                    } = prepared;
                    let outcome =
                        state::call(budget, move || handles.state().kv_delete(&bot, &key)).await;
                    StateReply::Flag(outcome)
                }
            }),
        )
        .and_then(|function| function.with_name("delete"))
        .map_err(super::js_error)?
    };

    let list = {
        let slot = Arc::clone(state);
        let deadline = Arc::clone(deadline);
        Function::new(
            ctx.clone(),
            Async(move |prefix: Opt<Value<'js>>| {
                let prepared = (|| {
                    // 空串与未给是同一件事：`list()` 与 `list('')` 都表示"全部"。
                    let prefix = match prefix.0.as_ref() {
                        None => None,
                        Some(value) if value.is_undefined() || value.is_null() => None,
                        Some(value) => {
                            let text = text_of(value, "kv.list 的 prefix")?;
                            if text.is_empty() { None } else { Some(text) }
                        }
                    };
                    let prepared = assembled(&slot, &deadline, UNASSEMBLED_KV)?;
                    Ok((prepared, prefix))
                })();
                async move {
                    let (prepared, prefix) = match prepared {
                        Ok(prepared) => prepared,
                        Err(message) => return StateReply::Keys(Err(message)),
                    };
                    let Prepared {
                        handles,
                        bot,
                        budget,
                    } = prepared;
                    let outcome = state::call(budget, move || {
                        handles.state().kv_list(&bot, prefix.as_deref())
                    })
                    .await;
                    // 端口签名没有 limit（`BotState::kv_list` 的契约是"最多
                    // `MAX_KV_LIST` 个"），这里仍然截一次：契约的上界不能只靠
                    // 后端守，否则第二个宿主就是第二份真源。
                    StateReply::Keys(outcome.map(|keys| {
                        let mut keys = keys;
                        keys.truncate(limit_of(MAX_KV_LIST));
                        keys
                    }))
                }
            }),
        )
        .and_then(|function| function.with_name("list"))
        .map_err(super::js_error)?
    };

    let set_many = {
        let slot = Arc::clone(state);
        let deadline = Arc::clone(deadline);
        Function::new(
            ctx.clone(),
            // 同上：`Ctx` 走参数，不存进闭包。
            Async(
                move |handle: Ctx<'js>, entries: Value<'js>, options: Opt<Value<'js>>| {
                    let prepared = (|| {
                        let entries = parse_entries(&handle, &entries, "kv.setMany 的 entries")?;
                        let options = parse_write_options(options.0.as_ref())?;
                        let prepared = assembled(&slot, &deadline, UNASSEMBLED_KV)?;
                        Ok((prepared, entries, options))
                    })();
                    async move {
                        let (prepared, entries, options) = match prepared {
                            Ok(prepared) => prepared,
                            Err(message) => return StateReply::Done(Err(message)),
                        };
                        let Prepared {
                            handles,
                            bot,
                            budget,
                        } = prepared;
                        let outcome = state::call(budget, move || {
                            handles.state().kv_set_many(&bot, entries, options)
                        })
                        .await;
                        StateReply::Done(outcome)
                    }
                },
            ),
        )
        .and_then(|function| function.with_name("setMany"))
        .map_err(super::js_error)?
    };

    let history = {
        let slot = Arc::clone(state);
        let deadline = Arc::clone(deadline);
        Function::new(
            ctx.clone(),
            Async(move |limit: Opt<Value<'js>>| {
                let prepared = (|| {
                    let limit = parse_history_limit(limit.0.as_ref())?;
                    let assembled = assembled_with_conversation(&slot, &deadline)?;
                    Ok((assembled, limit))
                })();
                async move {
                    let (assembled, limit) = match prepared {
                        Ok(prepared) => prepared,
                        Err(message) => return StateReply::History(Err(message)),
                    };
                    let (
                        Prepared {
                            handles,
                            bot,
                            budget,
                        },
                        conversation,
                    ) = assembled;
                    let outcome = state::call(budget, move || {
                        handles.history().history(&bot, &conversation, limit)
                    })
                    .await;
                    StateReply::History(outcome)
                }
            }),
        )
        .and_then(|function| function.with_name("history"))
        .map_err(super::js_error)?
    };

    // 名字与函数成对写在一处，`crate::dts` 的漂移测试直接扫这张表：五个键少一个、
    // 多一个、或与 `KV_BINDING_KEYS` 不一致都会红。
    let entries: [(&str, Function<'js>); KV_BINDING_KEYS.len()] = [
        ("get", get),
        ("set", set),
        ("delete", delete),
        ("list", list),
        ("setMany", set_many),
    ];
    for (name, function) in entries {
        object.set(name, function).map_err(super::js_error)?;
    }
    debug_assert_eq!(
        object
            .keys::<String>()
            .filter_map(Result::ok)
            .filter(|key| KV_BINDING_KEYS.contains(&key.as_str()))
            .count(),
        KV_BINDING_KEYS.len(),
        "every key in KV_BINDING_KEYS must be installed: {KV_BINDING_KEYS:?}"
    );
    Ok((object, history))
}

/// 一次调用在同步段准备好的三样东西：装配、本次事件的 bot、本次调用的预算。
///
/// 三样都在 `spawn_blocking` **之前**取好：`await` 期间不持锁（[`StateHandles`]
/// 是克隆出来的），也不在阻塞线程里回头读槽位（那时可能已经换了一次事件）。
struct Prepared {
    handles: StateHandles,
    bot: BotId,
    budget: Duration,
}

/// 读装配 + 本次事件的 `botId` + 预算。同步、无 IO。
fn assembled(slot: &StateSlot, deadline: &Deadline, unassembled: &str) -> Result<Prepared, String> {
    let Some(handles) = slot.handles() else {
        return Err(unavailable(unassembled));
    };
    let (bot, _) = slot.current_identity();
    let Some(bot) = bot else {
        return Err(invalid("本次事件没有 botId，状态能力不知道属于哪个 bot"));
    };
    Ok(Prepared {
        handles,
        bot,
        budget: budget_of(deadline.remaining()),
    })
}

/// [`assembled`] 的会话版：`history` 两个身份都要。
///
/// 没有 `botId` 与没有 `conversationId` 分开报：**不许**拿上一条事件的会话顶替
/// （那会让 handler 读到别人的聊天记录），也不许猜一个空会话。
fn assembled_with_conversation(
    slot: &StateSlot,
    deadline: &Deadline,
) -> Result<(Prepared, ConversationId), String> {
    let prepared = assembled(slot, deadline, UNASSEMBLED_HISTORY)?;
    let (_, conversation) = slot.current_identity();
    let Some(conversation) = conversation else {
        return Err(invalid("本次事件没有 conversationId"));
    };
    Ok((prepared, conversation))
}

/// 把 JS 值转成落库用的 JSON。
///
/// 走 `JSON.stringify` 而不是自己走一遍对象树（与 `debug_host::render_value`
/// 同一手法）：它认得 `toJSON`、循环引用会抛错、也不会把 `[object Object]` 当值
/// 存进去。`Ok(None)` 是 `undefined`（或 `toJSON` 返回 `undefined`）—— 状态里没有
/// "已删除"之外的空值，所以整次调用报错，不静默当 `null`（那会让"存了 null"和
/// "没存"变成同一件事）。
fn json_of<'js>(ctx: &Ctx<'js>, value: &Value<'js>, what: &str) -> Result<Json, String> {
    match ctx.json_stringify(value.clone()) {
        Ok(Some(text)) => {
            let text = text
                .to_string()
                .map_err(|_| invalid(format!("{what} 的值不是合法的 UTF-8")))?;
            serde_json::from_str(&text)
                .map_err(|error| invalid(format!("{what} 的值无法还原成 JSON：{error}")))
        }
        Ok(None) => Err(invalid(format!("{what} 不能存 undefined"))),
        Err(error) => Err(js_message(error)),
    }
}

/// 读 `ctx.kv.set` / `ctx.kv.setMany` 的第三个参数。
fn parse_write_options(options: Option<&Value<'_>>) -> Result<KvWriteOptions, String> {
    let Some(options) = options else {
        return Ok(KvWriteOptions::default());
    };
    if options.is_undefined() || options.is_null() {
        return Ok(KvWriteOptions::default());
    }
    let Some(object) = options.as_object() else {
        return Err(invalid("kv 的 options 必须是对象，例如 { ttlMs: 1000 }"));
    };
    let Some(ms) = object.get::<_, Option<f64>>("ttlMs").map_err(js_message)? else {
        return Ok(KvWriteOptions::default());
    };
    // 不静默夹：`ttlMs: -1` 与 `ttlMs: 0` 的意图完全不同，猜错就是把"永不过期"
    // 悄悄变成"立刻过期"（或者反过来）。
    if !ms.is_finite() || ms <= 0.0 || ms.fract() != 0.0 {
        return Err(invalid(format!(
            "kv 的 ttlMs 必须是有限正整数毫秒，收到 {ms}"
        )));
    }
    // `as u64` 在 Rust 里是饱和转换：`1e300` 变成 `u64::MAX`，随后 TTL 闸报错。
    let ttl = Duration::from_millis(ms as u64);
    validate_ttl(Some(ttl)).map_err(|error| error.to_string())?;
    Ok(KvWriteOptions { ttl: Some(ttl) })
}

/// 读 `ctx.kv.setMany(entries)`：必须是 `[{ key, value }, ...]`。
///
/// 批量上限用 [`validate_batch`] 判**条目数**，而且在解析每一条之前就判：101 条
/// 里第 1 条就是坏的，也不该先去序列化 100 个值。
fn parse_entries<'js>(
    ctx: &Ctx<'js>,
    entries: &Value<'js>,
    what: &str,
) -> Result<Vec<KvEntry>, String> {
    let Some(array) = entries.as_array() else {
        return Err(invalid(format!("{what} 必须是数组")));
    };
    let len = array.len();
    validate_batch(len).map_err(|error| error.to_string())?;

    let mut parsed = Vec::with_capacity(len);
    for (index, item) in array.iter::<Value<'js>>().enumerate() {
        let item = item.map_err(js_message)?;
        let Some(object) = item.as_object() else {
            return Err(invalid(format!("{what} 的第 {index} 条不是对象")));
        };
        let key_value: Value<'js> = object.get("key").map_err(js_message)?;
        let key = text_of(&key_value, &format!("{what} 第 {index} 条的 key"))?;
        validate_key(&key).map_err(|error| error.to_string())?;
        let value: Value<'js> = object.get("value").map_err(js_message)?;
        let json = json_of(ctx, &value, &format!("{what} 第 {index} 条的 value"))?;
        validate_value(&json).map_err(|error| error.to_string())?;
        parsed.push(KvEntry::new(key, json));
    }
    Ok(parsed)
}

/// 读 `ctx.history(limit)`。
///
/// 未给 → [`DEFAULT_HISTORY_LIMIT`]；非有限 / 非整数 / 小于 1 → 报错（不猜）；
/// 超过上限**夹住**（[`history_limit`]）——这是契约里唯一一处"夹而不报"，理由
/// 写在 `hbot_core::bot_state::history_limit` 的文档里，这里不重写判据。
fn parse_history_limit(value: Option<&Value<'_>>) -> Result<u32, String> {
    let Some(value) = value else {
        return Ok(DEFAULT_HISTORY_LIMIT);
    };
    if value.is_undefined() || value.is_null() {
        return Ok(DEFAULT_HISTORY_LIMIT);
    }
    let Some(raw) = value.as_number() else {
        return Err(invalid("ctx.history 的 limit 必须是数字"));
    };
    if !raw.is_finite() || raw.fract() != 0.0 || raw < 1.0 {
        return Err(invalid(format!(
            "ctx.history 的 limit 必须是大于等于 1 的整数，收到 {raw}"
        )));
    }
    // 饱和转换：`history(1e18)` 先变 `u32::MAX`，再由 `history_limit` 夹到上限。
    Ok(history_limit(raw.min(f64::from(u32::MAX)) as u32))
}

/// 一次状态调用的结果，摆成 JS 侧文档承诺的形状。
///
/// 一个枚举而不是六个闭包各写一份 `IntoJs`：失败只有一种表现（rejection），
/// 成功才有形状差异。
enum StateReply {
    /// `kv.get`：解析后的值，或 `null`（不存在 / 已过期）。
    Value(Result<Option<Json>, String>),
    /// `kv.set` / `kv.setMany`：`undefined`。
    Done(Result<(), String>),
    /// `kv.delete`：本次是否真的删掉了一行。
    Flag(Result<bool, String>),
    /// `kv.list`：键名数组。
    Keys(Result<Vec<String>, String>),
    /// `history`：消息数组。
    History(Result<Vec<HistoryEntry>, String>),
}

impl<'js> IntoJs<'js> for StateReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self {
            // **不存在解析成 `null`，不是 `undefined`**：d.ts 写的是 `| null`，
            // 而 `undefined` 会让 `value === null` 的判重静默失效。
            Self::Value(result) => match result {
                Ok(None) => Ok(Value::new_null(ctx.clone())),
                Ok(Some(value)) => match serde_json::to_string(&value) {
                    Ok(text) => ctx.json_parse(text),
                    Err(error) => rejected(ctx, &invalid(format!("kv 的值无法还原成 JS：{error}"))),
                },
                Err(message) => rejected(ctx, &message),
            },
            Self::Done(result) => match result {
                Ok(()) => Ok(Value::new_undefined(ctx.clone())),
                Err(message) => rejected(ctx, &message),
            },
            Self::Flag(result) => match result {
                Ok(deleted) => Ok(deleted.into_js(ctx)?),
                Err(message) => rejected(ctx, &message),
            },
            Self::Keys(result) => match result {
                Ok(keys) => {
                    let array = Array::new(ctx.clone())?;
                    for (index, key) in keys.iter().enumerate() {
                        array.set(index, key.as_str())?;
                    }
                    Ok(array.into_value())
                }
                Err(message) => rejected(ctx, &message),
            },
            Self::History(result) => match result {
                Ok(entries) => {
                    let array = Array::new(ctx.clone())?;
                    for (index, entry) in entries.iter().enumerate() {
                        // 手搓对象而不是 `serde::Serialize`：`HistoryEntry` 的 Rust
                        // 字段是 snake_case，d.ts 承诺的是 camelCase，中间那一层
                        // 改名必须**看得见**，不能指望某个 serde 属性还在。
                        let object = Object::new(ctx.clone())?;
                        object.set("direction", entry.direction.as_str())?;
                        object.set("messageId", entry.message_id.as_str())?;
                        object.set("externalId", entry.external_id.as_str())?;
                        object.set("senderId", entry.sender_id.as_str())?;
                        object.set("text", entry.text.as_str())?;
                        object.set("at", entry.at.unix_millis())?;
                        array.set(index, object)?;
                    }
                    Ok(array.into_value())
                }
                Err(message) => rejected(ctx, &message),
            },
        }
    }
}

/// 一次状态调用的失败：`Exception::throw_message` 让 handler 的 `catch` /
/// `.catch()` 拿到 `HBOT_STATE_*` 开头的 message。
fn rejected<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Result<Value<'js>> {
    Err(Exception::throw_message(ctx, message))
}

/// 参数错误：`HBOT_STATE_INVALID` 前缀。
///
/// 借 `hbot_core::BotStateError` 的 `Display` 而不是手写前缀：错误码只该有一份。
fn invalid(detail: impl AsRef<str>) -> String {
    BotStateError::Invalid {
        message: detail.as_ref().to_owned(),
    }
    .to_string()
}

fn js_message(error: rquickjs::Error) -> String {
    invalid(format!("读取参数失败：{error}"))
}

/// 读一个必须是字符串的参数。
///
/// 不做隐式转换（`kv.get(1)` 不是 `kv.get('1')`）：键名写错成数字是最常见的
/// 复制粘贴错误，把它变成 `"1"` 会让人找很久。
fn text_of<'js>(value: &Value<'js>, what: &str) -> Result<String, String> {
    value
        .as_string()
        .and_then(|text| text.to_string().ok())
        .ok_or_else(|| invalid(format!("{what} 必须是字符串")))
}

/// 契约里那个上界直接当 `truncate` 的实参用。
///
/// `MAX_KV_LIST` 在契约里就是 `usize`（它比的是 `Vec` 的长度），所以这里只是一层
/// 说明性的命名，不是类型转换 —— 早先写成 `u32 -> usize` 的版本与常量类型不符，
/// 编译期就红了。
fn limit_of(limit: usize) -> usize {
    limit
}

#[cfg(test)]
mod tests {
    use super::*;

    // 纯函数先测：这些判据在 JS 侧没法构造（NaN、无穷、非整数），只有在这里
    // 能一条条钉住。

    #[test]
    fn the_default_history_limit_is_smaller_than_the_cap() {
        assert_eq!(DEFAULT_HISTORY_LIMIT, 20);
        // 默认值不是上限：`history()` 不该悄悄读到 50 条。
        const { assert!(DEFAULT_HISTORY_LIMIT < hbot_core::MAX_HISTORY) };
    }

    #[test]
    fn kv_keys_are_exactly_the_five_documented_methods() {
        assert_eq!(
            KV_BINDING_KEYS,
            &["get", "set", "delete", "list", "setMany"]
        );
    }

    #[test]
    fn parameter_errors_carry_the_state_prefix() {
        let message = invalid("kv 键不能是空串");
        assert!(message.starts_with("HBOT_STATE_INVALID: "), "{message}");
        assert!(!message.contains("HBOT_DEVICE"), "{message}");
    }

    #[test]
    fn the_list_cap_comes_from_the_frozen_contract() {
        assert_eq!(limit_of(MAX_KV_LIST), 1000);
        assert_eq!(limit_of(MAX_KV_LIST), MAX_KV_LIST);
    }
}
