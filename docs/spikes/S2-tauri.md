# S2 Tauri 同进程 + Monaco

- 状态：待开始（等 `apps/web` 可构建）
- 日期：
- 环境：Windows / WebView2 版本 / Rust / release profile
- 关联：[ADR-0002](../adr/0002-agent-in-rust.md) S2、[02 架构](../02-architecture.md) §1、[06 IDE](../06-ide.md) 性能预算

## 1. 目的

验证“Tauri 2 直接链接 Rust 核心（无 sidecar）”这条形态在桌面端成立，并量出 WebView 的真实成本：

1. 单一进程能否跑起窗口 + 核心，命令/事件走 Tauri IPC；
2. Monaco 多文件（3 个 model）打开、编辑、保存是否满足 [06](../06-ide.md) 的预算（按键 p95 < 16ms、1MB 文件 < 150ms）；
3. 空窗口与“IDE 打开一个项目”的工作集，是否落在 S0 判定区间。

## 2. 方法（要跑的命令）

```powershell
# 1) 构建前端
cd apps/web; pnpm install; pnpm run build

# 2) 跑桌面壳（dev 或 release）
cd ../..; cargo run -p hbot-desktop --release

# 3) 采样（另开一个终端）
pwsh -File scripts/mem-probe.ps1 -Name hbot-desktop -Seconds 5 -IncludeChildren
```

按键延迟：Chrome DevTools 里录 Performance，或临时在 `editor` feature 埋 `performance.mark`，取 200 次按键的 p95。**不要用“感觉流畅”当结论。**

## 3. 结果

| 项 | 目标 | 实测 | 备注 |
|---|---|---|---|
| 空窗口工作集（含 WebView2 子进程） | — | 待测 | 基线 |
| IDE 打开 3 标签 | < 220 MB 整机 | 待测 | S0 判定线 |
| 按键 p95 | < 16 ms | 待测 | 大文件另测 |
| 1 MB 文件打开 | < 150 ms | 待测 | |
| 冷启动到可编辑 | < 1.5 s | 待测 | 从进程启动到 Monaco ready |

## 4. 结论

待填。

## 5. 对设计的影响

- 若整机 > 300 MB：按 ADR-0002 §11 评估原生 UI（这是最大的重估信号）。
- 若按键 p95 > 50 ms：先查是否每次按键都触发全量诊断；仍超则记录，M3 再优化。
- 若 Tauri 同进程做不到（例如 IPC 吞吐不足）：退路是核心跑在独立进程 + 本地 socket（`hbot serve` 形态），代价是多一次序列化与生命周期管理。