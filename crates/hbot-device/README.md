# hbot-device

> 设备能力（`C6`）的执行端口实现：截图 / 注入输入 / 前台窗口 / 剪贴板 / OCR。

## 概述

只实现 `hbot_core::device::DeviceBackend`，**不实现判据**。授权判据只有一份，在 `hbot_core::device::DeviceGate`（实现落在 `hbot-rpc`）：清单声明 → 宿主设置 → 限时会话三层都由它判。
两份判据会打架，最终解释权只能有一份（`AGENTS.md` 坑 48）。

## 目录

- [关键文件](#关键文件)
- [平台矩阵](#平台矩阵)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [上游与下游](#上游与下游)
- [测试](#测试)
- [已知限制](#已知限制)

## 关键文件

| 文件 | 做什么 |
|---|---|
| `src/lib.rs` | `DeviceBackend` 的重导出、`displays()` / `system_dpi()` 与平台后端选择 |
| `src/windows.rs` | Windows 实现：多显示器截图、注入、窗口枚举、剪贴板、OCR（整屏 + 按窗口） |
| `src/png.rs` | 截图编码（PNG）与尺寸换算 |
| `src/unsupported.rs` | 其它平台：六能力全 `false`，每个入口返回 `HostLimit` |

## 平台矩阵

| 平台 | 状态 |
|---|---|
| Windows 10/11 | 截图、注入、窗口枚举、剪贴板、OCR 都实现了 |
| 其它 | `UnsupportedBackend`：能力全 false，每个调用报 `DeviceError::HostLimit` |

## 边界：能放什么 / 不能放什么

- **能放**：系统调用封装、编码、DPI 换算。
- **不能放**：授权判据、审批、`BotId`（本 crate 不认识 bot）。

## 上游与下游

- 上游：`hbot-core`（`DeviceBackend`/`DeviceError`）。
- 下游：`hbot-rpc`（`app/device/*` 是判据与装配）、`hbot-agent`（`tools/device.rs`）。

## 测试

`cargo test -p hbot-device`。Windows 上的用例依赖真实桌面会话，CI 的 Windows runner 有交互桌面；
无设备实现的宿主由 `tests/api.rs` 的 `an_unsupported_host_refuses_every_device_call_with_a_host_limit` 钉住。

## 已知限制

- 混合 DPI 的跨屏拼图不做：本 crate 刻意不替调用方选基准坐标空间。
- OCR 只覆盖中文（其它语言没有语言包）。
- 只有 Windows 实现。
