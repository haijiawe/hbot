# hbot-desktop

> Tauri 桌面壳：一个进程里同时跑窗口与核心。

## 概述

只做**窗口、启动装配与传输**。命令逻辑一律在 `hbot_rpc::Dispatcher` 后面，壳层不实现任何命令分支 —— 桌面与后台进的是同一个 `dispatch`。

## 目录

- [关键文件](#关键文件)
- [启动顺序](#启动顺序)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [上游与下游](#上游与下游)
- [测试](#测试)
- [已知限制](#已知限制)

## 关键文件

| 文件 | 做什么 |
|---|---|
| `src/lib.rs` | Tauri 入口：`setup` 里开 `AppState`、给每个窗口起事件泵；`EVENT_CHANNEL` 恒定 |
| `src/shell.rs` | 托盘、单实例、窗口生命周期 |
| `src/autostart.rs` | 开机自启（注册表 / LaunchAgent） |
| `src/crash.rs` | 崩溃捕获与本地报告 |
| `src/dpi.rs` | 窗口 DPI 与缩放 |
| `src/settings.rs` | 壳层设置（窗口尺寸、托盘行为） |
| `src/cli.rs` | 桌面进程的命令行参数（如指定数据目录） |

## 启动顺序

1. Tauri 按 `tauri.conf.json` 建窗口（`frontendDist` 指向 `apps/web/dist`，所以 `cargo check --workspace --all-targets` 需要它存在）；
2. `setup` 里确定数据目录（只走 `hbot_core::paths::data_dir()`，不许写第四份算法）；
3. 后台任务 `spawn_blocking` 打开 `AppState`，失败弹系统对话框并以退出码 1 结束，**绝不 panic**；
4. 每个窗口一条事件泵：订阅 `EventBus`，把事件 `emit` 到 `EVENT_CHANNEL`。

## 边界：能放什么 / 不能放什么

- **能放**：窗口、托盘、单实例、自启、事件泵、崩溃报告。
- **不能放**：命令分支、业务规则、平台协议。
- `#[tauri::command]` 函数**不能是 `pub`**（crate root 会 E0255，坑 26）。

## 上游与下游

- 上游：`hbot-core`、`hbot-rpc`。
- 下游：无（它是二进制，被 `apps/web` 通过 Tauri IPC 驱动）。

## 测试

没有 Rust 单测：壳层行为靠 `scripts/verify-*.ps1` 与 `docs/spikes/S14-desktop-acceptance.md` 的装机验收（11 项）。

## 已知限制

- 只有 Windows 装机验收过；macOS bundle 走 CI，但没有真机 GUI 测试。
- 手机端零行代码（`docs/08-multi-host.md` §7 有 R1–R6 阻碍清单）。
