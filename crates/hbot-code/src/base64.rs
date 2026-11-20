//! 标准 base64（RFC 4648 §4）编码 —— 设备截图与 `ctx.files.readBytes` 共用。
//!
//! 实现只有一份：`hbot_core::base64`（出站附件也要用它编码，放 core 才不会让
//! `hbot-adapters` 再抄一遍）。这里只是把名字留成 crate 内的旧调用点。

pub(crate) use hbot_core::base64_encode;
