//! `ctx.sql.query` / `ctx.sql.exec` —— SQL 能力（`C2.sql`）在 JS 侧的那层桥。
//!
//! 契约类型、错误码与语句闸在 `hbot_core::bot_sql`，声明闸与端口在 `crate::sql`。
//! 本模块只做两件事：把 JS 参数解析成 `(sql, params)`，把槽位的判定结果变成 rejection。
//!
//! # 顺序与 kv / ai / jobs 一致：先解析参数，后判装配
//!
//! 1. **同步**解析参数（`sql` 必须是字符串、`params` 必须是数组、每个元素必须是五种标量之一）；
//! 2. 才进 `crate::sql::SqlSlot`（语句闸 → 声明闸 → 身份 → 装配闸 → 后端）。
//!
//! 第 1 步不过就直接 rejection，**一次数据库调用都不会发生**：`params: [{a:1}]` 不许变成
//! 一次「悄悄存成 `[object Object]`」的写库。
//!
//! # 返回值的形状
//!
//! `query` → `{ columns: string[], rows: unknown[][], truncated: boolean }`；
//! `exec` → `{ affected: number }`。两者都把 `SqlValue` 原样交给 JS（`null` / 数字 / 字符串 /
//! `Uint8Array`），不做类型改写。

use std::sync::Arc;

use rquickjs::prelude::{Async, Opt};
use rquickjs::{Ctx, Exception, Function, Object, Value};

use crate::error::CodeError;
use crate::sql::SqlSlot;
use hbot_core::{SqlChange, SqlError, SqlRows, SqlValue};

/// `ctx.sql` 上的方法名，一处声明。
///
/// 这是数据不是散文：`crate::dts` 的漂移测试把它与 `install` 的 `entries` 表、
/// `HOST_MODULE_SOURCE` 的 `sql` 命名空间、`hbot.d.ts` 的 `SqlApi` 四方对齐。
pub(crate) const SQL_BINDING_KEYS: &[&str] = &["query", "exec"];

/// 装 `ctx.sql` 对象，返回它。
pub(crate) fn install<'js>(ctx: &Ctx<'js>, slot: &Arc<SqlSlot>) -> Result<Object<'js>, CodeError> {
    let object = Object::new(ctx.clone()).map_err(super::js_error)?;

    let query = {
        let slot = Arc::clone(slot);
        Function::new(
            ctx.clone(),
            Async(
                // `Ctx` 作为首参取回（`FromParam` + `ParamRequirement::none()`，不占位置参数）。
                // **不许**把 `ctx.clone()` 存进闭包（`AGENTS.md` 坑 53）。
                move |handle: Ctx<'js>, sql: Value<'js>, params: Opt<Value<'js>>| {
                    let prepared = parse_call(&handle, &sql, params.0.as_ref(), "sql.query");
                    let slot = Arc::clone(&slot);
                    async move {
                        let (sql, params) = match prepared {
                            Ok(prepared) => prepared,
                            Err(message) => return RowsReply(Err(message)).into_js(&handle),
                        };
                        // 端口是阻塞的（SQLite），挪到阻塞线程池；isolate 线程只跑 JS。
                        let outcome =
                            tokio::task::spawn_blocking(move || slot.query(&sql, params)).await;
                        let result = match outcome {
                            Ok(result) => result.map_err(|error| error.to_string()),
                            Err(error) => Err(format!("SQL 查询的任务没能跑完：{error}")),
                        };
                        RowsReply(result).into_js(&handle)
                    }
                },
            ),
        )
        .and_then(|function| function.with_name("query"))
        .map_err(super::js_error)?
    };

    let exec = {
        let slot = Arc::clone(slot);
        Function::new(
            ctx.clone(),
            Async(
                move |handle: Ctx<'js>, sql: Value<'js>, params: Opt<Value<'js>>| {
                    let prepared = parse_call(&handle, &sql, params.0.as_ref(), "sql.exec");
                    let slot = Arc::clone(&slot);
                    async move {
                        let (sql, params) = match prepared {
                            Ok(prepared) => prepared,
                            Err(message) => return ChangeReply(Err(message)).into_js(&handle),
                        };
                        let outcome =
                            tokio::task::spawn_blocking(move || slot.exec(&sql, params)).await;
                        let result = match outcome {
                            Ok(result) => result.map_err(|error| error.to_string()),
                            Err(error) => Err(format!("SQL 执行的任务没能跑完：{error}")),
                        };
                        ChangeReply(result).into_js(&handle)
                    }
                },
            ),
        )
        .and_then(|function| function.with_name("exec"))
        .map_err(super::js_error)?
    };

    let entries: [(&str, Function<'js>); SQL_BINDING_KEYS.len()] =
        [("query", query), ("exec", exec)];
    for (name, function) in entries {
        object.set(name, function).map_err(super::js_error)?;
    }
    debug_assert_eq!(
        object
            .keys::<String>()
            .filter_map(Result::ok)
            .filter(|key| SQL_BINDING_KEYS.contains(&key.as_str()))
            .count(),
        SQL_BINDING_KEYS.len(),
        "every key in SQL_BINDING_KEYS must be installed: {SQL_BINDING_KEYS:?}"
    );
    Ok(object)
}

/// 把 `(sql, params?)` 解析成槽位要的 `(语句, 参数数组)`。
///
/// 不做隐式转换：`sql: 1` 不是 `"1"`，`params: [{}]` 不是 `"[object Object]"`。
fn parse_call<'js>(
    ctx: &Ctx<'js>,
    sql: &Value<'js>,
    params: Option<&Value<'js>>,
    what: &str,
) -> Result<(String, Vec<SqlValue>), String> {
    let sql = text_of(sql, &format!("{what} 的 sql"))?;
    let mut collected = Vec::new();
    if let Some(params) = params {
        if !params.is_undefined() && !params.is_null() {
            let Some(array) = params.as_array() else {
                return Err(invalid(format!("{what} 的 params 必须是数组")));
            };
            for index in 0..array.len() {
                let value = array.get::<Value<'js>>(index).map_err(js_message)?;
                collected.push(sql_value(ctx, &value, index + 1)?);
            }
        }
    }
    Ok((sql, collected))
}

/// 把一个 JS 值转成 SQL 参数。只收五种标量。
fn sql_value<'js>(ctx: &Ctx<'js>, value: &Value<'js>, index: usize) -> Result<SqlValue, String> {
    if value.is_null() || value.is_undefined() {
        return Ok(SqlValue::Null);
    }
    if let Some(boolean) = value.as_bool() {
        // SQLite 没有布尔：0 / 1 是它的惯例，而 JS 的 true 落到整数不会丢信息。
        return Ok(SqlValue::Integer(i64::from(boolean)));
    }
    if let Some(number) = value.as_number() {
        if !number.is_finite() {
            return Err(invalid(format!(
                "第 {index} 个参数是 NaN 或无穷大，SQLite 存不下"
            )));
        }
        // 整数值存成 INTEGER：JS 里所有数字都是 double，不区分的话 `id: 7` 会变成 7.0，
        // 而 SQLite 的 INTEGER 主键与 REAL 7.0 比较时会走类型亲和性，读回来的结果就变了。
        if number.fract() == 0.0 && number.abs() < 9_007_199_254_740_992.0 {
            #[allow(clippy::cast_possible_truncation)]
            return Ok(SqlValue::Integer(number as i64));
        }
        return Ok(SqlValue::Real(number));
    }
    if let Some(text) = value.as_string() {
        return text
            .to_string()
            .map(SqlValue::Text)
            .map_err(|_| invalid(format!("第 {index} 个参数不是合法的 UTF-8")));
    }
    let _ = ctx;
    // 二进制不走这条路：rquickjs 的 ArrayBuffer 两向转换各有一套 API，为它多写一条
    // 路径不值得。要存二进制就存 base64 字符串、读回来自己解码 —— 这是**明确**的约定，
    // 而不是悄悄把对象变成 "[object Object]"。
    Err(invalid(format!(
        "第 {index} 个参数是对象、数组或函数：SQL 参数只接受 null / 布尔 / 数字 / 字符串；\
         结构化数据请存成 JSON 字符串（JSON.stringify），二进制请存 base64 字符串"
    )))
}

/// 一次查询的结果：成功是 `{ columns, rows, truncated }`，失败是 rejection。
struct RowsReply(Result<SqlRows, String>);

impl<'js> RowsReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(rows) => rows_into_js(ctx, &rows),
            Err(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

/// 一次执行的结果：成功是 `{ affected }`，失败是 rejection。
struct ChangeReply(Result<SqlChange, String>);

impl<'js> ChangeReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(change) => {
                let object = Object::new(ctx.clone())?;
                object.set("affected", change.affected as f64)?;
                Ok(object.into_value())
            }
            Err(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

fn rows_into_js<'js>(ctx: &Ctx<'js>, rows: &SqlRows) -> rquickjs::Result<Value<'js>> {
    let object = Object::new(ctx.clone())?;
    let columns = rquickjs::Array::new(ctx.clone())?;
    for (index, name) in rows.columns.iter().enumerate() {
        columns.set(index, name.as_str())?;
    }
    object.set("columns", columns)?;
    let outer = rquickjs::Array::new(ctx.clone())?;
    for (row_index, row) in rows.rows.iter().enumerate() {
        let inner = rquickjs::Array::new(ctx.clone())?;
        for (cell_index, cell) in row.iter().enumerate() {
            inner.set(cell_index, value_into_js(ctx, cell)?)?;
        }
        outer.set(row_index, inner)?;
    }
    object.set("rows", outer)?;
    object.set("truncated", rows.truncated)?;
    Ok(object.into_value())
}

fn value_into_js<'js>(ctx: &Ctx<'js>, value: &SqlValue) -> rquickjs::Result<Value<'js>> {
    Ok(match value {
        SqlValue::Null => Value::new_null(ctx.clone()),
        SqlValue::Integer(number) => Value::new_number(ctx.clone(), *number as f64),
        SqlValue::Real(number) => Value::new_number(ctx.clone(), *number),
        SqlValue::Text(text) => rquickjs::String::from_str(ctx.clone(), text)?.into_value(),
        SqlValue::Blob(bytes) => {
            // 与输入侧对称：二进制以数字数组交给 JS（调用方自己拼 Uint8Array）。
            let array = rquickjs::Array::new(ctx.clone())?;
            for (index, byte) in bytes.iter().enumerate() {
                array.set(index, f64::from(*byte))?;
            }
            array.into_value()
        }
    })
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

/// 参数错误：`HBOT_SQL_INVALID` 前缀（借 `hbot_core::SqlError` 的 Display，错误码只该有一份）。
fn invalid(detail: impl AsRef<str>) -> String {
    SqlError::Invalid {
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
    fn sql_keys_are_exactly_the_documented_methods() {
        assert_eq!(SQL_BINDING_KEYS, &["query", "exec"]);
    }

    #[test]
    fn parameter_errors_carry_the_sql_prefix() {
        let message = invalid("params must be an array");
        assert!(message.starts_with("HBOT_SQL_INVALID: "), "{message}");
    }

    #[test]
    fn the_unassembled_hint_names_the_missing_host_side() {
        assert!(
            crate::sql::UNASSEMBLED.contains("SQL 后端"),
            "{}",
            crate::sql::UNASSEMBLED
        );
    }
}
