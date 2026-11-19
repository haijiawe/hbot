//! `ctx.send(conversationId, text, options?)` 与 `ctx.notify(text)` —— 出站能力在 JS 侧的那层桥。
//!
//! 契约类型、错误码与参数闸都在 `hbot_core::bot_egress`（已冻结），声明闸、目标解析与
//! 端口在 `crate::send`。本模块只做两件事：把 JS 参数解析成槽位要的形状，
//! 把槽位的判定结果变成 rejection。
//!
//! # 顺序与 kv / ai / jobs 一致：先解析参数，后判装配
//!
//! 1. **同步**解析并校验参数（`hbot_core::validate_send / validate_notify`）；
//! 2. 才进 `crate::send::SendSlot`（声明闸 -> 目标闸 -> 装配闸 -> 端口）。
//!
//! 第 1 步不过就直接 rejection，**一次库写都不会发生**：正文超长或会话 ID 写错
//! 不许变成一次出站入队，也不许把"参数写错了"报成"宿主没装配出站队列"。
//!
//! # 为什么端口调用要 `spawn_blocking`
//!
//! `SendQueue` 的实现方是 `hbot-rpc`：它把请求塞进 supervisor 的有界通道
//! （`blocking_send`）。通道满时那是一次真正的阻塞等待，不能发生在 isolate 的运行时
//! 线程上 —— 那会把整个 handler（包括它的预算计时）按住。
//!
//! # 存在不等于被授权
//!
//! `ctx.send` / `ctx.notify` 出现在 `hbot.d.ts` 里只说明**项目声明了**
//! `capabilities.send` / `capabilities.notify`；宿主没装配出站队列时抛
//! `HBOT_SEND_UNAVAILABLE`（**不**退回一个内存队列 —— 那会让"进程一崩消息就没了"
//! 变成没人发现的静默行为），清单没声明时抛 `HBOT_SEND_NOT_DECLARED`。

use std::sync::Arc;

use rquickjs::prelude::{Async, Opt};
use rquickjs::{Ctx, Exception, Function, Value};

use crate::error::CodeError;
use crate::send::{SendSlot, parse_conversation_kind};
use hbot_core::{SendError, SendRequest, validate_notify, validate_send};

/// [`ctx`] 上的两个方法名，一处声明。
///
/// 这是数据不是散文：`crate::dts` 的漂移测试把它与 `host_api` 的 `host.set` 表、
/// `HOST_MODULE_SOURCE` 的导出、`hbot.d.ts` 的 `MessageCtx` 四方对齐。
#[cfg(test)]
pub(crate) const SEND_BINDING_KEYS: &[&str] = &["send", "notify"];

/// 装 `__hbot` 上的 `send` 与 `notify`，返回两个函数。
///
/// 返回两个而不是一个对象：`ctx.send` 与 `ctx.notify` 是 `ctx` 上的**两个**
/// 平铺函数（与 `ctx.reply` / `ctx.history` 同形），`host_api::install` 要按名字
/// 分别 `host.set`，`dts` 的 `installed_host_keys` 才能一行一行对上。
pub(crate) fn install<'js>(
    ctx: &Ctx<'js>,
    slot: &Arc<SendSlot>,
) -> Result<(Function<'js>, Function<'js>), CodeError> {
    let send = {
        let slot = Arc::clone(slot);
        Function::new(
            ctx.clone(),
            Async(
                // `Ctx` 作为首参取回（`FromParam + ParamRequirement::none()`，不占位置参数）。
                // **不许**把 `ctx.clone()` 存进闭包：那会让运行时释放时撞
                // `list_empty(&rt->gc_obj_list)` 断言（`AGENTS.md` 坑 53）。
                move |handle: Ctx<'js>,
                      conversation: Value<'js>,
                      text: Value<'js>,
                      options: Opt<Value<'js>>| {
                    let prepared = parse_send(&conversation, &text, options.0.as_ref());
                    let slot = Arc::clone(&slot);
                    async move {
                        let (conversation, text, kind) = match prepared {
                            Ok(prepared) => prepared,
                            Err(message) => return SendReply(Err(message)).into_js(&handle),
                        };
                        let outcome = tokio::task::spawn_blocking(move || {
                            slot.send(&conversation, &text, kind)
                        })
                        .await;
                        let result = match outcome {
                            Ok(result) => result.map_err(|error| error.to_string()),
                            Err(error) => Err(format!("出站队列的任务没能跑完：{error}")),
                        };
                        SendReply(result).into_js(&handle)
                    }
                },
            ),
        )
        .and_then(|function| function.with_name("send"))
        .map_err(super::js_error)?
    };

    let notify = {
        let slot = Arc::clone(slot);
        Function::new(
            ctx.clone(),
            Async(move |handle: Ctx<'js>, text: Value<'js>| {
                let prepared = parse_notify(&text);
                let slot = Arc::clone(&slot);
                async move {
                    let text = match prepared {
                        Ok(text) => text,
                        Err(message) => return SendReply(Err(message)).into_js(&handle),
                    };
                    let outcome = tokio::task::spawn_blocking(move || slot.notify(&text)).await;
                    let result = match outcome {
                        Ok(result) => result.map_err(|error| error.to_string()),
                        Err(error) => Err(format!("出站队列的任务没能跑完：{error}")),
                    };
                    SendReply(result).into_js(&handle)
                }
            }),
        )
        .and_then(|function| function.with_name("notify"))
        .map_err(super::js_error)?
    };

    Ok((send, notify))
}

/// 把 `send(conversationId, text, options?)` 解析成槽位要的三个参数。
///
/// 不做隐式转换：`conversationId: 123` 不是 `"123"` —— 会话 ID 的类型是平台给的，
/// 猜错一次就是把消息发给别人。
fn parse_send<'js>(
    conversation: &Value<'js>,
    text: &Value<'js>,
    options: Option<&Value<'js>>,
) -> Result<(String, String, Option<hbot_core::ConversationKind>), String> {
    let conversation = text_of(conversation, "send 的 conversationId")?;
    let body = text_of(text, "send 的 text")?;

    let mut kind = None;
    if let Some(options) = options {
        if !options.is_undefined() && !options.is_null() {
            let Some(object) = options.as_object() else {
                return Err(invalid(
                    "send 的 options 必须是对象，例如 { kind: 'group' }",
                ));
            };
            if let Some(value) = object
                .get::<_, Option<Value<'js>>>("kind")
                .map_err(js_message)?
            {
                if !value.is_undefined() && !value.is_null() {
                    let raw = text_of(&value, "send 的 kind")?;
                    kind = Some(parse_conversation_kind(&raw).map_err(|error| error.to_string())?);
                }
            }
        }
    }

    // 参数闸就地做一次：正文超长时不该先落到"没声明 send"那个分支上。
    let request = SendRequest {
        conversation,
        kind,
        text: body,
    };
    validate_send(&request).map_err(|error| error.to_string())?;
    let kind = request.effective_kind();
    Ok((request.conversation, request.text, kind))
}

/// 把 `notify(text)` 解析成正文。
fn parse_notify<'js>(text: &Value<'js>) -> Result<String, String> {
    let body = text_of(text, "notify 的 text")?;
    validate_notify(&hbot_core::NotifyRequest::new(body.clone()))
        .map_err(|error| error.to_string())?;
    Ok(body)
}

/// 一次发送的结果：成功是 `undefined`，失败是 rejection。
struct SendReply(Result<(), String>);

impl<'js> SendReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(()) => Ok(Value::new_undefined(ctx.clone())),
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

/// 参数错误：`HBOT_SEND_INVALID` 前缀。
///
/// 借 `hbot_core::SendError` 的 `Display` 而不是手写前缀：错误码只该有一份。
fn invalid(detail: impl AsRef<str>) -> String {
    SendError::Invalid {
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
    fn send_keys_are_exactly_the_documented_methods() {
        assert_eq!(SEND_BINDING_KEYS, &["send", "notify"]);
    }

    #[test]
    fn parameter_errors_carry_the_send_prefix() {
        let message = invalid("conversationId must not be empty");
        assert!(message.starts_with("HBOT_SEND_INVALID: "), "{message}");
    }

    #[test]
    fn the_unassembled_hint_names_the_missing_host_side() {
        assert!(
            crate::send::UNASSEMBLED.contains("出站队列"),
            "{}",
            crate::send::UNASSEMBLED
        );
    }

    #[test]
    fn the_two_methods_are_exactly_what_the_host_installs() {
        // 与 `host_api` 的 `HOST_BINDING_KEYS` 尾部必须一致；跨文件的四面漂移
        // 由 `dts` 的测试负责，这里只钉住本模块自己的形状。
        assert_eq!(SEND_BINDING_KEYS, &["send", "notify"]);
    }
}
