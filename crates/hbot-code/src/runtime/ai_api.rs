//! `ctx.ai.complete(options)` —— 智能能力（C5）在 JS 侧的那一层桥。
//!
//! 契约类型、错误码与**参数闸**都在 `hbot_core::bot_ai`（已冻结），装配、每事件
//! 预算与时间抵账在 [`crate::ai`]。本模块只做三件事：把 JS 参数解析成
//! [`AiRequest`]、把 [`AiText`] 摆成文档承诺的形状、把失败变成 rejection。
//!
//! # 顺序与 `kv` 一致
//!
//! **先同步解析并校验参数，再取装配与预算**（与 [`crate::runtime::state_api`]
//! 同一顺序、与 `device_api` 相反）。理由相同：AI 没有第二道授权闸，"能不能用"
//! 只取决于宿主装没装配客户端，"参数对不对"是另一件事 —— 让坏参数先报错，
//! 才不会把"prompt 写错了"变成"模型不可用"，也不会让坏参数白占一次配额。
//!
//! # 存在不等于被授权
//!
//! `ctx.ai` 出现在 `hbot.d.ts` 里只说明**项目声明了** `capabilities.ai`；宿主没
//! 装配客户端时抛 `HBOT_AI_UNAVAILABLE`（**不**退回一个假补全 —— 那会把"没配置
//! 模型"变成没人发现的行为），清单没声明时由宿主侧给 `HBOT_AI_NOT_DECLARED`。

use std::sync::Arc;

use rquickjs::prelude::Async;
use rquickjs::{Ctx, Exception, Function, IntoJs, Object, Value};

use crate::ai::{self, AiSlot};
use crate::error::CodeError;
use crate::limits::Deadline;
use hbot_core::{
    AiError, AiRequest, AiText, validate_max_tokens, validate_prompt, validate_system,
    validate_temperature,
};

/// `ctx.ai` 上的方法名，一处声明。
///
/// 这是数据不是散文：`crate::dts` 的漂移测试把它与 `install` 的 `entries` 表、
/// `HOST_MODULE_SOURCE` 的 `ai` 命名空间、`hbot.d.ts` 的 `AiApi` 四方对齐。
pub(crate) const AI_BINDING_KEYS: &[&str] = &["complete"];

/// 宿主没装模型客户端时的说明（`HBOT_AI_UNAVAILABLE` 前缀由 [`ai::unavailable`] 加）。
const UNASSEMBLED_AI: &str = ai::UNASSEMBLED;

/// 装 `ctx.ai` 对象，返回它。
///
/// 调用方（[`crate::runtime::host_api::install`]）负责 `host.set("ai", ..)`：名字
/// 留在那一处，`dts` 的漂移测试才能一行一行对上。
pub(crate) fn install<'js>(
    ctx: &Ctx<'js>,
    slot: &Arc<AiSlot>,
    deadline: &Arc<Deadline>,
) -> Result<Object<'js>, CodeError> {
    let object = Object::new(ctx.clone()).map_err(super::js_error)?;

    let complete = {
        let slot = Arc::clone(slot);
        let deadline = Arc::clone(deadline);
        Function::new(
            ctx.clone(),
            Async(move |options: Value<'js>| {
                // 同步段：解析 + 两道参数闸。失败直接带着 `HBOT_AI_INVALID`
                // 回到 rejection，配额一次都不占。
                let prepared = parse_request(&options).and_then(|request| {
                    if !slot.is_assembled() {
                        return Err(ai::unavailable(UNASSEMBLED_AI));
                    }
                    Ok(request)
                });
                let slot = Arc::clone(&slot);
                let deadline = Arc::clone(&deadline);
                async move {
                    let request = match prepared {
                        Ok(request) => request,
                        Err(message) => return AiReply(Err(message)),
                    };
                    AiReply(ai::call(&slot, &deadline, request).await)
                }
            }),
        )
        .and_then(|function| function.with_name("complete"))
        .map_err(super::js_error)?
    };

    // 名字与函数成对写在一处，`crate::dts` 的漂移测试直接扫这张表。**一行一个键**
    // ——扫描器靠行首 `("` 取名字，写在单行数组里它就扫不到（`dts` 那条测试会红）。
    let entries: [(&str, Function<'js>); AI_BINDING_KEYS.len()] = [("complete", complete)];
    for (name, function) in entries {
        object.set(name, function).map_err(super::js_error)?;
    }
    debug_assert_eq!(
        object
            .keys::<String>()
            .filter_map(Result::ok)
            .filter(|key| AI_BINDING_KEYS.contains(&key.as_str()))
            .count(),
        AI_BINDING_KEYS.len(),
        "every key in AI_BINDING_KEYS must be installed: {AI_BINDING_KEYS:?}"
    );
    Ok(object)
}

/// 把 `complete({ prompt, system?, maxTokens?, temperature? })` 解析成 [`AiRequest`]。
///
/// 不做隐式转换：`prompt: 1` 不是 `"1"`，`maxTokens: "512"` 不是 `512`。与
/// `state_api` 的 `text_of` 同一条理由 —— 类型写错是最常见的复制粘贴错误，替人
/// 转一下会让人找很久。
fn parse_request<'js>(options: &Value<'js>) -> Result<AiRequest, String> {
    let Some(object) = options.as_object() else {
        return Err(invalid(
            "ai.complete 的参数必须是对象，例如 { prompt: '...' }",
        ));
    };
    let Some(prompt) = object
        .get::<_, Option<String>>("prompt")
        .map_err(js_message)?
    else {
        return Err(invalid("ai.complete 缺少必填参数 prompt"));
    };
    validate_prompt(&prompt).map_err(|error| error.to_string())?;

    let system = match object
        .get::<_, Option<String>>("system")
        .map_err(js_message)?
    {
        Some(system) => {
            validate_system(Some(&system)).map_err(|error| error.to_string())?;
            Some(system)
        }
        None => None,
    };

    let max_tokens = match object
        .get::<_, Option<Value<'js>>>("maxTokens")
        .map_err(js_message)?
    {
        None => None,
        Some(value) if value.is_undefined() || value.is_null() => None,
        Some(value) => {
            let Some(number) = value.as_number() else {
                return Err(invalid("ai.complete 的 maxTokens 必须是数字"));
            };
            if !number.is_finite() || number.fract() != 0.0 || number < 0.0 {
                return Err(invalid(format!(
                    "ai.complete 的 maxTokens 必须是有限非负整数，收到 {number}"
                )));
            }
            // 饱和转换：`1e300` 变成 `u32::MAX`，随后 `validate_max_tokens` 报错。
            Some(number as u32)
        }
    };
    let max_tokens = validate_max_tokens(max_tokens).map_err(|error| error.to_string())?;

    let temperature = match object
        .get::<_, Option<Value<'js>>>("temperature")
        .map_err(js_message)?
    {
        None => None,
        Some(value) if value.is_undefined() || value.is_null() => None,
        Some(value) => {
            let Some(number) = value.as_number() else {
                return Err(invalid("ai.complete 的 temperature 必须是数字"));
            };
            // NaN 与 Infinity 在这里就拦下：`validate_temperature` 判的是
            // `is_finite()`，但把 `f64::NAN` 交给它是让错误文案晚一步出现。
            if !number.is_finite() {
                return Err(invalid("ai.complete 的 temperature 必须是有限数字"));
            }
            // `f64 as f32` 溢出会变成 `inf`，所以再判一次有限性；`validate_temperature`
            // 的界（0..=2.0）远比 f32 范围小，这里只挡"传了 1e400"这种脏值。
            let number = number as f32;
            if !number.is_finite() {
                return Err(invalid("ai.complete 的 temperature 超出 f32 范围"));
            }
            validate_temperature(Some(number)).map_err(|error| error.to_string())?;
            Some(number)
        }
    };

    Ok(AiRequest {
        system,
        prompt,
        max_tokens,
        temperature,
    })
}

/// `complete` 的结果：一个对象，或一个 rejection。
struct AiReply(Result<AiText, String>);

impl<'js> IntoJs<'js> for AiReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(text) => {
                let object = Object::new(ctx.clone())?;
                object.set("text", text.text.as_str())?;
                object.set("promptTokens", text.usage.prompt_tokens)?;
                object.set("completionTokens", text.usage.completion_tokens)?;
                object.set("model", text.model.as_str())?;
                Ok(object.into_value())
            }
            Err(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

/// 参数错误：`HBOT_AI_INVALID` 前缀。
///
/// 借 `hbot_core::AiError` 的 `Display` 而不是手写前缀：错误码只该有一份。
fn invalid(detail: impl AsRef<str>) -> String {
    AiError::Invalid {
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
    fn ai_keys_are_exactly_the_documented_methods() {
        assert_eq!(AI_BINDING_KEYS, &["complete"]);
    }

    #[test]
    fn parameter_errors_carry_the_ai_prefix() {
        let message = invalid("prompt 不能是空串");
        assert!(message.starts_with("HBOT_AI_INVALID: "), "{message}");
    }
}
