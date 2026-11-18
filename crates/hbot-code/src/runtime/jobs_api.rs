//! `ctx.jobs.enqueue(name, payload, options)` —— 后台作业（C4）在 JS 侧的那一层桥。
//!
//! 契约类型、错误码与参数闸都在 `hbot_core::bot_jobs（已冻结），声明闸与入队端口
//! 在 `crate::jobs。本模块只做两件事：把 JS 参数解析成 `(name, payload, key,
//! conversation)，把槽位的判定结果变成 rejection。
//!
//! # 顺序与 kv / ai 一致：先解析参数，后判装配
//!
//! 1. **同步**解析并校验参数（`hbot_core::validate_job_name / validate_payload /
//!    validate_idempotency_key）；
//! 2. 才进 `crate::jobs::JobSlot::enqueue（声明闸 -> 幂等键缺省 -> 装配闸 -> 端口）。
//!
//! 第 1 步不过就直接 rejection，**库一次都不会被写**：坏参数（名字写错、载荷超 8 KiB）
//! 不许变成一次 SQLite 事务，也不许把"名字写错了"报成"宿主没装配作业队列"。
//!
//! # 入队是异步的，因为端口是阻塞的
//!
//! `JobQueue 的实现方是 `hbot-rpc：它把行写进 `bot_jobs 表，那是一次阻塞的 db 线程
//! 往返。所以端口调用走 `tokio::task::spawn_blocking —— 与 `crate::state 的
//! `call 同一条理由：isolate 的运行时线程只跑 JS，不能被一次库写按住。
//!
//! # 存在不等于被授权
//!
//! `ctx.jobs 出现在 `hbot.d.ts 里只说明**项目声明了** `capabilities.jobs；宿主没
//! 装配作业队列时抛 `HBOT_JOBS_UNAVAILABLE（**不**退回一个内存队列 —— 那会让
//! "进程一崩作业就没了"变成没人发现的静默行为），清单没声明时抛
//! `HBOT_JOBS_NOT_DECLARED。

use std::sync::Arc;

use rquickjs::prelude::{Async, Opt};
use rquickjs::{Ctx, Exception, Function, Object, Value};
use serde_json::Value as Json;

use crate::error::CodeError;
use crate::jobs::JobSlot;
use hbot_core::JobError;

/// `ctx.jobs 上的方法名，一处声明。
///
/// 这是数据不是散文：`crate::dts 的漂移测试把它与 `install 的 `entries 表、
/// `HOST_MODULE_SOURCE 的 `jobs 命名空间、`hbot.d.ts 的 `JobApi 四方对齐。
pub(crate) const JOBS_BINDING_KEYS: &[&str] = &["enqueue"];

/// 装 `ctx.jobs 对象，返回它。
///
/// 调用方（`crate::runtime::host_api::install`）负责 `host.set("jobs", ..)：名字
/// 留在那一处，`dts 的漂移测试才能一行一行对上。
pub(crate) fn install<'js>(ctx: &Ctx<'js>, slot: &Arc<JobSlot>) -> Result<Object<'js>, CodeError> {
    let object = Object::new(ctx.clone()).map_err(super::js_error)?;

    let enqueue = {
        let slot = Arc::clone(slot);
        Function::new(
            ctx.clone(),
            Async(
                // `Ctx 作为首参取回（`FromParam + `ParamRequirement::none()`，不占
                // 位置参数）。**不许**把 `ctx.clone()` 存进闭包：那会让 JS 函数对象
                // 活多久、`Ctx` 就活多久，运行时释放时会撞
                // `list_empty(&rt->gc_obj_list)` 断言。
                move |handle: Ctx<'js>,
                      name: Value<'js>,
                      payload: Opt<Value<'js>>,
                      options: Opt<Value<'js>>| {
                    let prepared =
                        parse_enqueue(&handle, &name, payload.0.as_ref(), options.0.as_ref());
                    let slot = Arc::clone(&slot);
                    async move {
                        let (name, payload, key, conversation) = match prepared {
                            Ok(prepared) => prepared,
                            Err(message) => return JobReply(Err(message)).into_js(&handle),
                        };
                        // 端口是阻塞的（写库），挪到阻塞线程池；isolate 线程只跑 JS。
                        let outcome = tokio::task::spawn_blocking(move || {
                            slot.enqueue(&name, payload, key, conversation)
                        })
                        .await;
                        let result = match outcome {
                            Ok(result) => result.map_err(|error| error.to_string()),
                            Err(error) => Err(format!("作业队列的任务没能跑完：{error}")),
                        };
                        JobReply(result).into_js(&handle)
                    }
                },
            ),
        )
        .and_then(|function| function.with_name("enqueue"))
        .map_err(super::js_error)?
    };

    let entries: [(&str, Function<'js>); JOBS_BINDING_KEYS.len()] = [("enqueue", enqueue)];
    for (name, function) in entries {
        object.set(name, function).map_err(super::js_error)?;
    }
    debug_assert_eq!(
        object
            .keys::<String>()
            .filter_map(Result::ok)
            .filter(|key| JOBS_BINDING_KEYS.contains(&key.as_str()))
            .count(),
        JOBS_BINDING_KEYS.len(),
        "every key in JOBS_BINDING_KEYS must be installed: {JOBS_BINDING_KEYS:?}"
    );
    Ok(object)
}

/// 把 `enqueue(name, payload?, options?)` 解析成槽位要的四个参数。
///
/// 不做隐式转换：`name: 1` 不是 `"1"`。载荷可选，缺省是 `null`（"这次入队没有
/// 附带数据"是合法的一次入队，与 `"载荷是 null"` 在库里是同一个字节序列，不需要
/// 两个语义）。
fn parse_enqueue<'js>(
    ctx: &Ctx<'js>,
    name: &Value<'js>,
    payload: Option<&Value<'js>>,
    options: Option<&Value<'js>>,
) -> Result<(String, Json, Option<String>, Option<String>), String> {
    let name = text_of(name, "jobs.enqueue 的 name")?;
    // 参数闸的第一道在这里就地做一次：名字写错时，`JobSlot::enqueue` 那句
    // 声明闸会报"没有声明"，把真正的错误盖掉。
    hbot_core::validate_job_name(&name).map_err(|error| error.to_string())?;

    let payload = match payload {
        None => Json::Null,
        Some(value) if value.is_undefined() => Json::Null,
        Some(value) => json_of(ctx, value, "作业载荷")?,
    };
    hbot_core::validate_payload(&payload).map_err(|error| error.to_string())?;

    let mut key = None;
    let mut conversation = None;
    if let Some(options) = options {
        if !options.is_undefined() && !options.is_null() {
            let Some(object) = options.as_object() else {
                return Err(invalid(
                    "jobs.enqueue 的 options 必须是对象，例如 { idempotencyKey: 'k-1' }",
                ));
            };
            if let Some(value) = object
                .get::<_, Option<Value<'js>>>("idempotencyKey")
                .map_err(js_message)?
            {
                if !value.is_undefined() && !value.is_null() {
                    let text = text_of(&value, "jobs.enqueue 的 idempotencyKey")?;
                    hbot_core::validate_idempotency_key(&text)
                        .map_err(|error| error.to_string())?;
                    key = Some(text);
                }
            }
            if let Some(value) = object
                .get::<_, Option<Value<'js>>>("conversation")
                .map_err(js_message)?
            {
                if !value.is_undefined() && !value.is_null() {
                    let text = text_of(&value, "jobs.enqueue 的 conversation")?;
                    if text.trim().is_empty() || text.trim() != text {
                        return Err(invalid(
                            "jobs.enqueue 的 conversation 不能是空串或带首尾空白",
                        ));
                    }
                    conversation = Some(text);
                }
            }
        }
    }

    Ok((name, payload, key, conversation))
}

/// 一次入队的结果：成功是 `undefined`，失败是 rejection。
struct JobReply(Result<(), String>);

impl<'js> JobReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(()) => Ok(Value::new_undefined(ctx.clone())),
            Err(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

/// 把 JS 值转成落库用的 JSON（与 `crate::runtime::state_api` 的 json_of 同一手法）。
fn json_of<'js>(ctx: &Ctx<'js>, value: &Value<'js>, what: &str) -> Result<Json, String> {
    match ctx.json_stringify(value.clone()) {
        Ok(Some(text)) => {
            let text = text
                .to_string()
                .map_err(|_| invalid(format!("{what}不是合法的 UTF-8")))?;
            serde_json::from_str(&text)
                .map_err(|error| invalid(format!("{what}无法还原成 JSON：{error}")))
        }
        Ok(None) => Err(invalid(format!("{what}不能是 undefined"))),
        Err(error) => Err(js_message(error)),
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

/// 参数错误：`HBOT_JOBS_INVALID` 前缀。
///
/// 借 `hbot_core::JobError` 的 `Display` 而不是手写前缀：错误码只该有一份。
fn invalid(detail: impl AsRef<str>) -> String {
    JobError::Invalid {
        message: detail.as_ref().to_owned(),
    }
    .to_string()
}

fn js_message(error: rquickjs::Error) -> String {
    invalid(format!("读取参数失败：{error}"))
}

/// 槽位没装配时的说明，测试与宿主共用同一句。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_keys_are_exactly_the_documented_methods() {
        assert_eq!(JOBS_BINDING_KEYS, &["enqueue"]);
    }

    #[test]
    fn parameter_errors_carry_the_jobs_prefix() {
        let message = invalid("names must not be empty");
        assert!(message.starts_with("HBOT_JOBS_INVALID: "), "{message}");
    }

    #[test]
    fn the_unassembled_hint_names_the_missing_host_side() {
        assert!(
            crate::jobs::UNASSEMBLED.contains("作业队列"),
            "{}",
            crate::jobs::UNASSEMBLED
        );
    }
}
