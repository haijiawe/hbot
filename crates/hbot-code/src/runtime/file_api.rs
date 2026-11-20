//! ctx.files.read / write / list / remove —— 文件能力（C3.file）在 JS 侧的那层桥。
//!
//! 契约类型、路径规范化与体积闸在 hbot_core::bot_file，声明闸与端口在 crate::file。
//! 本模块只做两件事：把 JS 参数解析成 (路径, 选项)，把槽位的判定结果变成 rejection。
//!
//! 顺序与 kv / sql / jobs 一致：先同步解析参数，后进槽位（规范化 → 参数闸 → 声明闸 →
//! 装配闸 → 端口）。第 1 步不过就 rejection，一次 IO 都不会发生。
//!
//! 返回值的形状：read → { path, text, bytes, truncated }；readBytes → { path, base64, bytes, truncated }；
//! write → 一条 FileEntry；list → FileEntry 数组；remove → 删掉的条目数（int）。

use std::sync::Arc;

use rquickjs::prelude::{Async, Opt};
use rquickjs::{Array, Ctx, Exception, Function, Object, Value};

use crate::error::CodeError;
use crate::file::FileSlot;
use hbot_core::{
    FileEntry, FileKind, LineRange, MAX_FILE_LIST_ENTRIES, MAX_FILE_READ_BYTES,
    MAX_FILE_READ_LINES, ReadBytes, ReadFile,
};

use crate::base64::base64_encode;

/// ctx.files 上的方法名，一处声明。
///
/// 这是数据不是散文：crate::dts 的漂移测试把它与 install 的 entries 表、
/// HOST_MODULE_SOURCE 的 files 命名空间、hbot.d.ts 的 FilesApi 四方对齐。
pub(crate) const FILES_BINDING_KEYS: &[&str] = &["read", "readBytes", "write", "list", "remove"];

/// 装 ctx.files 对象，返回它。
pub(crate) fn install<'js>(ctx: &Ctx<'js>, slot: &Arc<FileSlot>) -> Result<Object<'js>, CodeError> {
    let object = Object::new(ctx.clone()).map_err(super::js_error)?;

    let read = {
        let slot = Arc::clone(slot);
        Function::new(
            ctx.clone(),
            Async(
                move |handle: Ctx<'js>, path: Value<'js>, options: Opt<Value<'js>>| {
                    let prepared = parse_read(&path, options.0.as_ref());
                    let slot = Arc::clone(&slot);
                    async move {
                        let (path, max_bytes, range) = match prepared {
                            Ok(prepared) => prepared,
                            Err(message) => return ReadReply(Err(message)).into_js(&handle),
                        };
                        let outcome =
                            tokio::task::spawn_blocking(move || slot.read(&path, max_bytes, range))
                                .await;
                        let result = match outcome {
                            Ok(result) => result.map_err(|error| error.to_string()),
                            Err(error) => Err(format!("文件读取的任务没能跑完：{error}")),
                        };
                        ReadReply(result).into_js(&handle)
                    }
                },
            ),
        )
        .and_then(|function| function.with_name("read"))
        .map_err(super::js_error)?
    };

    let read_bytes = {
        let slot = Arc::clone(slot);
        Function::new(
            ctx.clone(),
            Async(
                move |handle: Ctx<'js>, path: Value<'js>, options: Opt<Value<'js>>| {
                    let prepared = parse_read_bytes(&path, options.0.as_ref());
                    let slot = Arc::clone(&slot);
                    async move {
                        let (path, max_bytes) = match prepared {
                            Ok(prepared) => prepared,
                            Err(message) => return BytesReply(Err(message)).into_js(&handle),
                        };
                        let outcome =
                            tokio::task::spawn_blocking(move || slot.read_bytes(&path, max_bytes))
                                .await;
                        let result = match outcome {
                            Ok(Ok(bytes)) => Ok(bytes),
                            Ok(Err(error)) => Err(error.to_string()),
                            Err(error) => Err(format!("二进制读取的任务没能跑完：{error}")),
                        };
                        BytesReply(result).into_js(&handle)
                    }
                },
            ),
        )
        .and_then(|function| function.with_name("readBytes"))
        .map_err(super::js_error)?
    };

    let write = {
        let slot = Arc::clone(slot);
        Function::new(
            ctx.clone(),
            Async(
                move |handle: Ctx<'js>,
                      path: Value<'js>,
                      text: Value<'js>,
                      options: Opt<Value<'js>>| {
                    let prepared = parse_write(&path, &text, options.0.as_ref());
                    let slot = Arc::clone(&slot);
                    async move {
                        let (path, text, range) = match prepared {
                            Ok(prepared) => prepared,
                            Err(message) => return EntryReply(Err(message)).into_js(&handle),
                        };
                        let outcome = tokio::task::spawn_blocking(move || match range {
                            Some(range) => slot.write_range(&path, &text, range),
                            None => slot.write(&path, &text),
                        })
                        .await;
                        let result = match outcome {
                            Ok(result) => result.map_err(|error| error.to_string()),
                            Err(error) => Err(format!("文件写入的任务没能跑完：{error}")),
                        };
                        EntryReply(result).into_js(&handle)
                    }
                },
            ),
        )
        .and_then(|function| function.with_name("write"))
        .map_err(super::js_error)?
    };

    let list = {
        let slot = Arc::clone(slot);
        Function::new(
            ctx.clone(),
            Async(
                move |handle: Ctx<'js>, path: Opt<Value<'js>>, options: Opt<Value<'js>>| {
                    let prepared = parse_list(path.0.as_ref(), options.0.as_ref());
                    let slot = Arc::clone(&slot);
                    async move {
                        let (path, limit) = match prepared {
                            Ok(prepared) => prepared,
                            Err(message) => return ListReply(Err(message)).into_js(&handle),
                        };
                        let outcome =
                            tokio::task::spawn_blocking(move || slot.list(&path, limit)).await;
                        let result = match outcome {
                            Ok(result) => result.map_err(|error| error.to_string()),
                            Err(error) => Err(format!("目录列表的任务没能跑完：{error}")),
                        };
                        ListReply(result).into_js(&handle)
                    }
                },
            ),
        )
        .and_then(|function| function.with_name("list"))
        .map_err(super::js_error)?
    };

    let remove = {
        let slot = Arc::clone(slot);
        Function::new(
            ctx.clone(),
            Async(
                move |handle: Ctx<'js>, path: Value<'js>, options: Opt<Value<'js>>| {
                    let prepared = parse_remove(&path, options.0.as_ref());
                    let slot = Arc::clone(&slot);
                    async move {
                        let (path, recursive) = match prepared {
                            Ok(prepared) => prepared,
                            Err(message) => return RemovedReply(Err(message)).into_js(&handle),
                        };
                        let outcome =
                            tokio::task::spawn_blocking(move || slot.remove(&path, recursive))
                                .await;
                        let result = match outcome {
                            Ok(result) => result.map_err(|error| error.to_string()),
                            Err(error) => Err(format!("删除的任务没能跑完：{error}")),
                        };
                        RemovedReply(result).into_js(&handle)
                    }
                },
            ),
        )
        .and_then(|function| function.with_name("remove"))
        .map_err(super::js_error)?
    };

    let entries: [(&str, Function<'js>); FILES_BINDING_KEYS.len()] = [
        ("read", read),
        ("readBytes", read_bytes),
        ("write", write),
        ("list", list),
        ("remove", remove),
    ];
    for (name, function) in entries {
        object.set(name, function).map_err(super::js_error)?;
    }
    debug_assert_eq!(
        object
            .keys::<String>()
            .filter_map(Result::ok)
            .filter(|key| FILES_BINDING_KEYS.contains(&key.as_str()))
            .count(),
        FILES_BINDING_KEYS.len(),
        "every key in FILES_BINDING_KEYS must be installed: {FILES_BINDING_KEYS:?}"
    );
    Ok(object)
}

fn parse_read<'js>(
    path: &Value<'js>,
    options: Option<&Value<'js>>,
) -> Result<(String, usize, Option<LineRange>), String> {
    let path = text_of(path, "files.read(path, options)")?;
    let max_bytes = match options {
        None => MAX_FILE_READ_BYTES,
        Some(value) if value.is_undefined() || value.is_null() => MAX_FILE_READ_BYTES,
        Some(value) => {
            let object = value
                .as_object()
                .ok_or_else(|| invalid("files.read 的 options 必须是对象"))?;
            match object.get::<_, Option<Value<'js>>>("maxBytes") {
                Ok(Some(raw)) if !raw.is_undefined() && !raw.is_null() => {
                    let number = raw
                        .as_number()
                        .ok_or_else(|| invalid("files.read 的 maxBytes 必须是数字"))?;
                    if !number.is_finite() || number <= 0.0 {
                        return Err(invalid("files.read 的 maxBytes 必须是正数"));
                    }
                    usize::try_from(number as u64)
                        .map_err(|_| invalid("files.read 的 maxBytes 太大"))?
                }
                Ok(_) => MAX_FILE_READ_BYTES,
                Err(error) => return Err(js_message(error)),
            }
        }
    };
    let range = match options {
        None => None,
        Some(value) if value.is_undefined() || value.is_null() => None,
        Some(value) => {
            let object = value
                .as_object()
                .ok_or_else(|| invalid("files.read 的 options 必须是对象"))?;
            parse_line_range(object, false)?
        }
    };
    Ok((path, max_bytes, range))
}

fn parse_read_bytes<'js>(
    path: &Value<'js>,
    options: Option<&Value<'js>>,
) -> Result<(String, usize), String> {
    let path = text_of(path, "files.readBytes(path, options)")?;
    let max_bytes = match options {
        None => MAX_FILE_READ_BYTES,
        Some(value) if value.is_undefined() || value.is_null() => MAX_FILE_READ_BYTES,
        Some(value) => {
            let object = value
                .as_object()
                .ok_or_else(|| invalid("files.readBytes 的 options 必须是对象"))?;
            match object.get::<_, Option<Value<'js>>>("maxBytes") {
                Ok(Some(raw)) if !raw.is_undefined() && !raw.is_null() => {
                    let number = raw
                        .as_number()
                        .ok_or_else(|| invalid("files.readBytes 的 maxBytes 必须是数字"))?;
                    if !number.is_finite() || number <= 0.0 {
                        return Err(invalid("files.readBytes 的 maxBytes 必须是正数"));
                    }
                    usize::try_from(number as u64)
                        .map_err(|_| invalid("files.readBytes 的 maxBytes 太大"))?
                }
                Ok(_) => MAX_FILE_READ_BYTES,
                Err(error) => return Err(js_message(error)),
            }
        }
    };
    Ok((path, max_bytes))
}

/// 从 options 里取 `startLine` / `endLine`（1 起、两端都含）。
///
/// 读的时候只给 startLine 就返回 startLine 起最多 `MAX_FILE_READ_LINES` 行；
/// 写的时候只给 startLine 就**只替换那一行**（`single_line`）。默认值只在
/// 这一层决定一次，core 的 `LineRange` 永远是一个两端都实的区间。
///
/// 越界（0、负数、小数、逆序、跨度超限）一律拒绝，不做夹取：夹取会让
/// 「我要第 10001 行」静默变成「给你第 10000 行」，用户以为读到了那一行。
fn parse_line_range<'js>(
    options: &Object<'js>,
    single_line: bool,
) -> Result<Option<LineRange>, String> {
    let start = match options.get::<_, Option<Value<'js>>>("startLine") {
        Ok(Some(raw)) if !raw.is_undefined() && !raw.is_null() => {
            Some(line_number(&raw, "startLine")?)
        }
        Ok(_) => None,
        Err(error) => return Err(js_message(error)),
    };
    let end = match options.get::<_, Option<Value<'js>>>("endLine") {
        Ok(Some(raw)) if !raw.is_undefined() && !raw.is_null() => {
            Some(line_number(&raw, "endLine")?)
        }
        Ok(_) => None,
        Err(error) => return Err(js_message(error)),
    };
    if start.is_none() && end.is_none() {
        return Ok(None);
    }
    let start = start.unwrap_or(1);
    let end = match end {
        Some(end) => end,
        None if single_line => start,
        None => start.saturating_add(MAX_FILE_READ_LINES - 1),
    };
    Ok(Some(LineRange::new(start, end)))
}

fn line_number(raw: &Value<'_>, field: &str) -> Result<u64, String> {
    let number = raw
        .as_number()
        .ok_or_else(|| invalid(format!("files 的 {field} 必须是整数")))?;
    if !number.is_finite() || number < 1.0 || number.fract() != 0.0 {
        return Err(invalid(format!("files 的 {field} 必须是 1 起的整数")));
    }
    // JS 的数字只有 2^53 的整数精度，超过这个数就不要再假装它是精确行号了。
    if number > 9_007_199_254_740_992.0 {
        return Err(invalid(format!("files 的 {field} 太大")));
    }
    Ok(number as u64)
}

fn parse_write<'js>(
    path: &Value<'js>,
    text: &Value<'js>,
    options: Option<&Value<'js>>,
) -> Result<(String, String, Option<LineRange>), String> {
    let path = text_of(path, "files.write(path, text)")?;
    let text = text_of(text, "files.write(path, text)")?;
    let range = match options {
        None => None,
        Some(value) if value.is_undefined() || value.is_null() => None,
        Some(value) => {
            let object = value
                .as_object()
                .ok_or_else(|| invalid("files.write 的 options 必须是对象"))?;
            parse_line_range(object, true)?
        }
    };
    Ok((path, text, range))
}

fn parse_remove<'js>(
    path: &Value<'js>,
    options: Option<&Value<'js>>,
) -> Result<(String, bool), String> {
    let path = text_of(path, "files.remove(path, options)")?;
    let recursive = match options {
        None => false,
        Some(value) if value.is_undefined() || value.is_null() => false,
        Some(value) => {
            let object = value
                .as_object()
                .ok_or_else(|| invalid("files.remove 的 options 必须是对象"))?;
            match object.get::<_, Option<Value<'js>>>("recursive") {
                Ok(Some(raw)) if !raw.is_undefined() && !raw.is_null() => raw
                    .as_bool()
                    .ok_or_else(|| invalid("files.remove 的 recursive 必须是布尔值"))?,
                Ok(_) => false,
                Err(error) => return Err(js_message(error)),
            }
        }
    };
    Ok((path, recursive))
}

fn parse_list<'js>(
    path: Option<&Value<'js>>,
    options: Option<&Value<'js>>,
) -> Result<(String, usize), String> {
    let path = match path {
        None => String::new(),
        Some(value) if value.is_undefined() || value.is_null() => String::new(),
        Some(value) => text_of(value, "files.list(path, options)")?,
    };
    let limit = match options {
        None => MAX_FILE_LIST_ENTRIES,
        Some(value) if value.is_undefined() || value.is_null() => MAX_FILE_LIST_ENTRIES,
        Some(value) => {
            let object = value
                .as_object()
                .ok_or_else(|| invalid("files.list 的 options 必须是对象"))?;
            match object.get::<_, Option<Value<'js>>>("limit") {
                Ok(Some(raw)) if !raw.is_undefined() && !raw.is_null() => {
                    let number = raw
                        .as_number()
                        .ok_or_else(|| invalid("files.list 的 limit 必须是数字"))?;
                    if !number.is_finite() || number <= 0.0 {
                        return Err(invalid("files.list 的 limit 必须是正数"));
                    }
                    usize::try_from(number as u64)
                        .map_err(|_| invalid("files.list 的 limit 太大"))?
                }
                Ok(_) => MAX_FILE_LIST_ENTRIES,
                Err(error) => return Err(js_message(error)),
            }
        }
    };
    Ok((path, limit))
}

fn text_of<'js>(value: &Value<'js>, what: &str) -> Result<String, String> {
    value
        .as_string()
        .and_then(|string| string.to_string().ok())
        .ok_or_else(|| invalid(format!("{what}: 参数必须是字符串")))
}

fn invalid(detail: impl AsRef<str>) -> String {
    hbot_core::FileError::Invalid {
        message: detail.as_ref().to_owned(),
    }
    .to_string()
}

fn js_message(error: rquickjs::Error) -> String {
    error.to_string()
}

struct ReadReply(Result<ReadFile, String>);

impl<'js> ReadReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(file) => {
                let object = Object::new(ctx.clone())?;
                object.set("path", file.path)?;
                object.set("text", file.text)?;
                object.set("bytes", file.bytes as u64)?;
                object.set("truncated", file.truncated)?;
                Ok(object.into_value())
            }
            Err(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

struct EntryReply(Result<FileEntry, String>);

impl<'js> EntryReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(entry) => entry_into_js(ctx, &entry),
            Err(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

struct ListReply(Result<Vec<FileEntry>, String>);

impl<'js> ListReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(entries) => {
                let array = Array::new(ctx.clone())?;
                for (index, entry) in entries.iter().enumerate() {
                    array.set(index, entry_into_js(ctx, entry)?)?;
                }
                Ok(array.into_value())
            }
            Err(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

struct RemovedReply(Result<u32, String>);

impl<'js> RemovedReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(count) => Ok(Value::new_number(ctx.clone(), f64::from(count))),
            Err(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

struct BytesReply(Result<ReadBytes, String>);

impl<'js> BytesReply {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.0 {
            Ok(bytes) => {
                let object = Object::new(ctx.clone())?;
                object.set("path", bytes.path)?;
                object.set("base64", base64_encode(&bytes.bytes))?;
                object.set("bytes", bytes.bytes.len() as u64)?;
                object.set("truncated", bytes.truncated)?;
                Ok(object.into_value())
            }
            Err(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

fn entry_into_js<'js>(ctx: &Ctx<'js>, entry: &FileEntry) -> rquickjs::Result<Value<'js>> {
    let object = Object::new(ctx.clone())?;
    object.set("path", entry.path.as_str())?;
    object.set("kind", kind_str(entry.kind))?;
    object.set("size", entry.size)?;
    Ok(object.into_value())
}

fn kind_str(kind: FileKind) -> &'static str {
    kind.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_method_list_is_exactly_what_the_bridge_installs() {
        assert_eq!(FILES_BINDING_KEYS.len(), 5);
        assert!(FILES_BINDING_KEYS.contains(&"read"));
        assert!(FILES_BINDING_KEYS.contains(&"readBytes"));
        assert!(FILES_BINDING_KEYS.contains(&"write"));
        assert!(FILES_BINDING_KEYS.contains(&"list"));
        assert!(FILES_BINDING_KEYS.contains(&"remove"));
    }

    #[test]
    fn parameter_errors_carry_the_file_prefix() {
        let message = invalid("x");
        assert!(message.starts_with("HBOT_FILE_INVALID"));
    }
}
