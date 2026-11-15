//! 平台适配器实现。
//!
//! 每个平台一个模块，只负责"平台协议 ↔ 归一化消息"的翻译，不含业务编排
//! （`docs/03-directory.md`：适配器不承载 supervisor/队列）。新增平台的步骤见
//! `AGENTS.md` §8。

pub mod onebot;
