//! `ctx.session.*` —— 会话等待（C2.session）在 JS 侧的那一层桥。
//!
//! 契约类型与参数闸在 `hbot_core::bot_session`，声明闸与端口在 `crate::session`。
//! 本模块只做两件事：把 JS 参数解析成 `WaitSpec`，把槽位的判定结果变成 rejection。
//!
//! # 顺序与 kv / ai / jobs 一致：先解析参数，后判装配
//!
//! 1. **同步**解析并校验参数（`hbot_core::bot_session` 的闸）；
//! 2. 才进 `crate::session::SessionSlot`（声明闸 -> 身份 -> 装配闸 -> 端口）。
//!
//! 第 1 步不过就直接 rejection，**库一次都不会被写**：坏参数不许变成一次 SQLite 事务。
//!
//! # 为什么入队是异步的
//!
//! `WaitQueue` 的实现方是 `hbot-rpc`：它把行写进 `bot_waits` 表，那是一次阻塞的 db 线程
//! 往返。所以端口调用走 `tokio::task::spawn_blocking` —— isolate 的运行时线程只跑 JS。
//!
//! # 存在不等于被授权
//!
//! `ctx.session` 出现在 `hbot.d.ts` 里只说明**项目声明了** `capabilities.state.session`；
//! 宿主没装配等待后端时抛 `HBOT_SESSION_UNAVAILABLE`（**不**退回一个内存等待表 —— 那样
//! 重启后等待就消失，正是旧项目 P0-3 的失败形状），清单没声明时抛
//! `HBOT_SESSION_NOT_DECLARED`。

use std::sync::Arc;

use rquickjs::prelude::{Async, Opt};
use rquickjs::{Ctx, Exception, Function, Object, Value};

use crate::error::CodeError;
use crate::session::SessionSlot;
use hbot_core::{SessionError, WaitHit, WaitScope, WaitSpec};

/// `ctx.session` 上的方法名，一处声明。
///
/// 这是数据不是散文：`crate::dts` 的漂移测试把它与 `install` 的 `entries` 表、
/// `HOST_MODULE_SOURCE` 的 `session` 命名空间、`hbot.d.ts` 的 `SessionApi` 四方对齐。
pub(crate) const SESSION_BINDING_KEYS: &[&str] = &["prompt", "current", "cancel"];

/// 装 `ctx.session` 对象，返回它。
pub(crate) fn install<'js>(
    ctx: &Ctx<'js>,
    slot: &Arc<SessionSlot>,
) -> Result<Object<'js>, CodeError> {
    let object = Object::new(ctx.clone()).map_err(super::js_error)?;

    let prompt = {
        let slot = Arc::clone(slot);
        Function::new(
            ctx.clone(),
            Async(
                // `Ctx` 作为首参取回（`FromParam` + `ParamRequirement::none()`，不占
                // 位置参数）。**不许**把 `ctx.clone()` 存进闭包：那会让 JS 函数对象
                // 活多久、`Ctx` 就活多久，运行时释放时撞 `list_empty(&rt->gc_obj_list)`。
                move |handle: Ctx<'js>,
                      key: Value<'js>,
                      text: Value<'js>,
                      options: Opt<Value<'js>>| {
                    let prepared = parse_prompt(&handle, &key, &text, options.0.as_ref());
                    let slot = Arc::clone(&slot);
                    async move {
                        let spec = match prepared {
                            Ok(spec) => spec,
                            Err(message) => return IdReply(Err(message)).into_js(&handle),
                        };
                        // 端口是阻塞的（写库），挪到阻塞线程池；isolate 线程只跑 JS。
                        let outcome = tokio::task::spawn_blocking(move || slot.prompt(&spec)).await;
                        let result = match outcome {
                            Ok(result) => result.map_err(|error| error.to_string()),
                            Err(error) => Err(format!("会话等待的任务没能跑完：{error}")),
                        };
                        IdReply(result).into_js(&handle)
                    }
                },
            ),
        )
        .and_then(|function| function.with_name("prompt"))
        .map_err(super::js_error)?
    };

    let current = {
        let slot = Arc::clone(slot);
        Function::new(
            ctx.clone(),
            Async(move |handle: Ctx<'js>| {
                let slot = Arc::clone(&slot);
                async move {
                    let outcome = tokio::task::spawn_blocking(move || slot.current()).await;
                    let result = match outcome {
                        Ok(result) => result.map_err(|error| error.to_string()),
                        Err(error) => Err(format!("会话等待的任务没能跑完：{error}")),
                    };
                    HitReply(result).into_js(&handle)
                }
            }),
        )
        .and_then(|function| function.with_name("current"))
        .map_err(super::js_error)?
    };

    let cancel = {
        let slot = Arc::clone(slot);
        Function::new(
            ctx.clone(),
            Async(move |handle: Ctx<'js>| {
                let slot = Arc::clone(&slot);
                async move {
                    let outcome = tokio::task::spawn_blocking(move || slot.cancel()).await;
                    let result = match outcome {
                        Ok(result) => result.map_err(|error| error.to_string()),
                        Err(error) => Err(format!("会话等待的任务没能跑完：{error}")),
                    };
                    BoolReply(result).into_js(&handle)
                }
            }),
        )
        .and_then(|function| function.with_name("cancel"))
        .map_err(super::js_error)?
    };

    let entries: [(&str, Function<'js>); SESSION_BINDING_KEYS.len()] =
        [("prompt", prompt), ("current", current), ("cancel", cancel)];
    for (name, function) in entries {
        object.set(name, function).map_err(super::js_error)?;
    }
    debug_assert_eq!(
        object
            .keys::<String>()
            .filter_map(Result::ok)
            .filter(|key| SESSION_BINDING_KEYS.contains(&key.as_str()))
            .count(),
        SESSION_BINDING_KEYS.len(),
        "every key in SESSION_BINDING_KEYS must be installed: {SESSION_BINDING_KEYS:?}"
    );
    Ok(object)
}

/// 把 `prompt(key, text, options?)` 解析成一个 [`WaitSpec`]。
///
/// 不做隐式转换：`key: 1` 不是 `"1"`。`options.scope` 只接受 `"sender"` / `"conversation"`
/// 两个字符串（写错就报，不猜一个默认值）。
fn parse_prompt<'js>(
    ctx: &Ctx<'js>,
    key: &Value<'js>,
    text: &Value<'js>,
    options: Option<&Value<'js>>,
) -> Result<WaitSpec, String> {
    let key = text_of(key, "session.prompt 的 key")?;
    let prompt = text_of(text, "session.prompt 的 text")?;
    // 参数闸就地做一次：键写错时，槽位的声明闸会先报「没声明」，把真正的错误盖掉。
    hbot_core::bot_session::validate_key(&key).map_err(|error| error.to_string())?;
    hbot_core::bot_session::validate_prompt(&prompt).map_err(|error| error.to_string())?;

    let mut spec = WaitSpec::new(key, prompt);
    if let Some(options) = options {
        if !options.is_undefined() && !options.is_null() {
            let Some(object) = options.as_object() else {
                return Err(invalid(
                    "session.prompt 的 options 必须是对象，例如 { scope: 'sender', timeoutMs: 60000 }",
                ));
            };
            if let Some(value) = object
                .get::<_, Option<Value<'js>>>("scope")
                .map_err(js_message)?
            {
                if !value.is_undefined() && !value.is_null() {
                    let text = text_of(&value, "session.prompt 的 scope")?;
                    spec.scope = WaitScope::parse(&text).ok_or_else(|| {
                        invalid(format!(
                            "session.prompt 的 scope 只接受 \"sender\" 或 \"conversation\"，收到 {text:?}"
                        ))
                    })?;
                }
            }
            if let Some(value) = object
                .get::<_, Option<Value<'js>>>("timeoutMs")
                .map_err(js_message)?
            {
                if !value.is_undefined() && !value.is_null() {
                    let Some(number) = value.as_number() else {
                        return Err(invalid("session.prompt 的 timeoutMs 必须是数字"));
                    };
                    // 负数 / 非整数一律拒：`as u64` 会把 -1 变成 0，那是一次静默的语义改写。
                    let Some(millis) = clamp_timeout(number) else {
                        return Err(invalid(format!(
                            "session.prompt 的 timeoutMs 必须是正整数毫秒，收到 {number}"
                        )));
                    };
                    spec.timeout_ms = Some(millis);
                }
            }
            if let Some(value) = object
                .get::<_, Option<Value<'js>>>("reminders")
                .map_err(js_message)?
            {
                if !value.is_undefined() && !value.is_null() {
                    let Some(number) = value.as_number() else {
                        return Err(invalid("session.prompt 的 reminders 必须是数字"));
                    };
                    // 与 timeoutMs 同口径：越界报错而不是夹取，否则用户以为设了 3 次、实际只重发 1 次。
                    let Some(reminders) = clamp_reminders(number) else {
                        return Err(invalid(format!(
                            "session.prompt 的 reminders 必须是 0..={} 的整数，收到 {number}",
                            hbot_core::bot_session::MAX_WAIT_REMINDERS
                        )));
                    };
                    spec.reminders = reminders;
                }
            }
        }
    }
    hbot_core::bot_session::validate_spec(&spec).map_err(|error| error.to_string())?;
    let _ = ctx;
    Ok(spec)
}

/// JS 数字 → 毫秒。超出 `u64` 范围或非正/非整数返回 `None`（调用方报错，不截断）。
/// JS 数字 → 重发次数。必须是 0..=`MAX_WAIT_REMINDERS` 的整数（同 `clamp_timeout`：不合法就 `None`）。
fn clamp_reminders(number: f64) -> Option<u8> {
    if !number.is_finite() || number.fract() != 0.0 || number < 0.0 {
        return None;
    }
    if number > u8::MAX as f64 {
        return None;
    }
    u8::try_from(number as u16)
        .ok()
        .filter(|value| *value <= hbot_core::bot_session::MAX_WAIT_REMINDERS)
}
fn clamp_timeout(number: f64) -> Option<u64> {
    if !number.is_finite() || number <= 0.0 || number.fract() != 0.0 {
        return None;
    }
    if number > u64::MAX as f64 {
        return None;
    }
    Some(number as u64)
}

/// 一次注册的结果：成功是行 id 字符串，失败是 rejection。
struct IdReply(Result<String, String>);

impl<'js> IdReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(id) => Ok(rquickjs::String::from_str(ctx.clone(), &id)?.into_value()),
            Err(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

/// 一次 `current()`：没有未结等待时是 `null`。
struct HitReply(Result<Option<WaitHit>, String>);

impl<'js> HitReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(None) => Ok(Value::new_null(ctx.clone())),
            Ok(Some(hit)) => {
                let object = Object::new(ctx.clone())?;
                object.set("id", hit.id.to_string())?;
                object.set("key", hit.key)?;
                object.set("prompt", hit.prompt)?;
                object.set("owner", hit.owner)?;
                Ok(object.into_value())
            }
            Err(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

/// 一次 `cancel()`：是否真的取消了一行。
struct BoolReply(Result<bool, String>);

impl<'js> BoolReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(value) => Ok(Value::new_bool(ctx.clone(), value)),
            Err(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

/// 读一个必须是字符串的 JS 值。
fn text_of<'js>(value: &Value<'js>, what: &str) -> Result<String, String> {
    match value.as_string() {
        Some(text) => text
            .to_string()
            .map_err(|_| invalid(format!("{what} 不是合法的 UTF-8"))),
        None => Err(invalid(format!("{what} 必须是字符串"))),
    }
}

/// 参数错误：`HBOT_SESSION_INVALID` 前缀。
///
/// 借 `hbot_core::SessionError` 的 `Display` 而不是手写前缀：错误码只该有一份。
fn invalid(detail: impl AsRef<str>) -> String {
    SessionError::Invalid {
        message: detail.as_ref().to_owned(),
    }
    .to_string()
}

fn js_message(error: rquickjs::Error) -> String {
    invalid(format!("读取参数失败：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_keys_are_exactly_the_documented_methods() {
        assert_eq!(SESSION_BINDING_KEYS, &["prompt", "current", "cancel"]);
    }

    #[test]
    fn parameter_errors_carry_the_session_prefix() {
        let message = invalid("keys must not be empty");
        assert!(message.starts_with("HBOT_SESSION_INVALID: "), "{message}");
    }

    #[test]
    fn the_timeout_parser_refuses_what_cast_would_silently_rewrite() {
        assert_eq!(clamp_timeout(60_000.0), Some(60_000));
        assert_eq!(clamp_timeout(0.0), None, "0 不是「不超时」，是写错了");
        assert_eq!(clamp_timeout(-1.0), None);
        assert_eq!(clamp_timeout(1.5), None, "毫秒不接受小数");
        assert_eq!(clamp_timeout(f64::NAN), None);
        assert_eq!(clamp_timeout(f64::INFINITY), None);
    }

    #[test]
    fn the_reminder_parser_refuses_what_cast_would_silently_rewrite() {
        assert_eq!(clamp_reminders(0.0), Some(0), "0 是合法值：到点直接结束");
        assert_eq!(clamp_reminders(3.0), Some(3));
        assert_eq!(clamp_reminders(4.0), None, "上限是 3，越界报错不夹取");
        assert_eq!(clamp_reminders(-1.0), None);
        assert_eq!(clamp_reminders(1.5), None, "次数不接受小数");
        assert_eq!(clamp_reminders(f64::NAN), None);
        assert_eq!(clamp_reminders(f64::INFINITY), None);
    }

    #[test]
    fn the_unassembled_hint_names_the_missing_host_side() {
        assert!(
            crate::session::UNASSEMBLED.contains("等待"),
            "{}",
            crate::session::UNASSEMBLED
        );
    }
}
