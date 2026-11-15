# S0 内存基线

- 状态：进行中（M0 期间补齐数字）
- 日期：2026-11-16
- 关联：[ADR-0002](../adr/0002-agent-in-rust.md) §4 内存目标、[02 架构](../02-architecture.md) §1.1、[04 计划](../04-implementation-plan.md) S0

## 1. 要回答的问题

ADR-0002 把“桌面空闲总 RSS < 220 MB、核心 < 60 MB”写成目标，但这些数字**还没有实测**。S0 只回答三件事：

1. 空核心（无 bot、无 isolate）常驻多少？
2. 每多一个 bot / 每多一个 QuickJS isolate，增加多少？
3. Tauri 窗口的 WebView 进程占多少（桌面整机的大头）？

**不回答**：旧 Electron 项目的完整对比（需要启动旧应用、写用户数据；当前按只读处理，留到 M6 影子运行再量）。

## 2. 口径

- 工作集（`WorkingSet64`）：任务管理器“内存”，含共享页，会重复计算 WebView 子进程共享的 DLL。
- 私有内存（`PrivateMemorySize64`）：提交的独占内存，更适合判断“这个进程自身增长”。
- 报告必须标明用哪个口径；比较不同进程时只用同一种。
- 采样 3 秒、200ms 一次，取 min/avg/max；数字保留一位小数（MB）。

工具：`scripts/mem-probe.ps1`（只用于测量，不参与产品运行时）。

```powershell
# 单进程
pwsh -File scripts/mem-probe.ps1 -Name hbot -IncludeChildren

# 已知 PID，输出 JSON 便于贴进本文
pwsh -File scripts/mem-probe.ps1 -Id 1234 -Seconds 5 -IncludeChildren -Json
```

## 3. 待填表

| 场景 | 进程/构建 | 工作集 MB (avg/max) | 私有 MB (avg/max) | 备注 |
|---|---|---|---|---|
| 空核心，无窗口 | `hbot` release，`serve` 空闲 | 待测 | 待测 | 基线 |
| 核心 + 1 bot 连接 | mock OneBot，空闲 | 待测 | 待测 | 增量/ bot |
| 核心 + 8 bot 连接 | mock OneBot，空闲 | 待测 | 待测 | 线性度 |
| 核心 + 8 bot + 8 isolate | 各 1 个 QuickJS worker | 待测 | 待测 | 每 isolate 成本 |
| Tauri 桌面空窗 | `hbot-desktop` release | 待测 | 待测 | 含 WebView2 子进程 |
| Tauri 桌面 + IDE 打开 | 一个项目、3 个标签 | 待测 | 待测 | 目标 < 220 MB |

## 4. 判定

- 核心（无窗口）< 60 MB、每 bot < 6 MB、每 isolate < 8 MB：目标成立，ADR 可转 Accepted（还需 S1/S2 数字）。
- 核心 60–120 MB：先查是否重复持有连接/缓存/日志缓冲，再决定是否接受。
- 核心 > 120 MB 或窗口整机 > 300 MB：按 ADR-0002 §11 的重估信号处理（换 UI / 换引擎），不要靠“优化一下”糊过去。

## 5. 记录纪律

- 每次测量写：commit、构建 profile（debug/release）、机器、Windows 版本、是否开启 Defender 实时扫描。
- 不用“开发者机器上大概”当结论；不把 debug 构建的数字写进验收。
- 数字变化 >10% 时，先确认测量条件一致，再怀疑代码。