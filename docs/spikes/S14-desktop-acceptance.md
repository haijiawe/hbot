# S14 桌面端到端验收（2026-11-21）

这一份记录**这台 Windows 机器上真跑出来的数字**。所有退出码都是实测的，没有一行是「应该没问题」。

## 0 结论表

| 单元 | 命令 | 退出码 | 关键数字 |
| --- | --- | --- | --- |
| 安装包冒烟 | `scripts/smoke-nsis-install.ps1 -Installer _recovery/nsis-unzip/hbot_0.1.0_x64-setup.exe` | 0 | 11 项全 PASS；安装 1.8 s、卸载 0.9 s、装后 31.42 MiB（32943180 字节）、残留文件 0 |
| 单实例 | `scripts/verify-single-instance.ps1 -ExePath <debug>.exe` | 0 | 第二实例退出码 0、进程仍 1 个、窗口从最小化恢复 |
| 托盘关窗 | `scripts/verify-tray-close.ps1` | 0 | 关窗后 `IsWindowVisible=False` 而进程存活；`closeToTray=false` 时自行退出 |
| 托盘动作 | `scripts/verify-tray-actions.ps1` | 0 | show / autostart / quit 三动作全部派发成功，注册表已还原 |
| 开机自启 | `scripts/verify-autostart.ps1` | 0 | 写入幂等（Run 键只 1 条）、toggle、删除、未知参数退出码 2 |
| 崩溃报告 | `scripts/verify-crash-report.ps1` | 0 | panic 退出码 101、报告落盘、密钥 `<redacted>`、二次启动上报一次 |
| 桌面启动探针 | `scripts/startup-probe.ps1 -Runs 7` | 0 | `window_ms` 中位 107.8 / p95 136.8；`first_rpc_out_ms` 658.5 |
| 无头启动探针 | `scripts/headless-startup-probe.ps1 -Runs 7` | 0 | `proc_to_ready_ms` 中位 28.3 / p95 34.7 |
| 首启链路等价 | `cargo test -p hbot-rpc --test local_runtime` / `--test examples_e2e` | 0 / 0 | 真 SQLite + 真 isolate 走完整链路 |

## 1 安装包：用的是 CI 产出的那一份

包来自 CI 最后一次成功 run `44791029159`（head `6ca16cc`）的 artifact `hbot-nsis-x64`（10834477 字节），
解压后 `_recovery/nsis-unzip/hbot_0.1.0_x64-setup.exe`（10850318 字节）。**这是用户真会拿到的那份包**，
所以优先于本机构建。

冒烟脚本做的事（`scripts/smoke-nsis-install.ps1`）：静默安装 → 校验安装目录与两文件 → 启动 → 等窗口标题 → 量工作集 →
杀进程 → 静默卸载 → 检查目录/注册表/开始菜单/桌面四类残留。11 项全 PASS：

- 安装退出码 0、耗时 1.8 s；落点 `C:\Users\Administrator\AppData\Local\hbot-smoke\` 下 `hbot-desktop.exe` 与 `uninstall.exe`；
- 装后占用 31.42 MiB（32943180 字节）；
- 启动 0.4 s 出现窗口，标题 `hbot`，工作集 40.3 MiB；
- `Stop-Process -Force` 后 800 ms 内进程消失；
- 卸载退出码 0、耗时 0.9 s，安装目录残留 0 文件；HKCU/HKLM 的 Uninstall 无 hbot 键，开始菜单与桌面无 `hbot.lnk`。

> 注意：这个脚本跑完会**卸载**。别的脚本要用那份 exe 得加 `-KeepInstalled`（第 7 节就是这么做的），否则路径失效。

## 2 单实例（`scripts/verify-single-instance.ps1`）

断言：第二个实例退出码必须 0（不是崩溃）、同 exe 进程数仍是 1、stderr 里**不能**出现「桌面壳数据目录」
（出现就说明它进了 setup、也就是锁没拦住）、日志里出现「检测到第二次启动」、窗口从 `IsIconic=True` 恢复成 False。

实测全过。唯一一行 `[WARN] 主窗口没能成为前台窗口（Windows 前台锁允许拒绝）` 是脚本自己标注的
**非硬断言**——Windows 的前台锁允许系统拒绝抢焦点，加 `-StrictForeground` 才当失败。

## 3 托盘关窗（`scripts/verify-tray-close.ps1`）

三个方向都验：A 发 `WM_CLOSE` 后窗口隐藏而进程存活（默认 `closeToTray=true`）；B 再起一个实例把窗口重新显示；
C 把 `closeToTray` 设成 false 后关窗**进程自己退出**且退出码 0、stderr 有「桌面端开始优雅退出」与「空库（没有机器人）」。

## 4 托盘菜单动作（`scripts/verify-tray-actions.ps1`）

show / autostart / quit 三个菜单 id 各触发一次，看日志里 `source="cli" menu_id="desktop/tray/show" action="show"` 这类记录；
autostart 会写 HKCU Run 指向当前 exe，脚本随后翻回来并删除，收尾核对注册表已清理。

> 脚本自述的边界：它验的是**菜单 id → 动作的派发**，不是「鼠标真的点中了托盘图标」。后者需要真人或 UI 自动化，没有自动化证据。

## 5 开机自启（`scripts/verify-autostart.ps1`）

覆盖：注册表写入、幂等（Run 键只有 1 条）、`--set-autostart toggle`、删除、未知参数退出码 2、
`--print-settings` 里的 `systemAutostart`。这套用了**真实用户目录** `C:\Users\Administrator\AppData\Roaming\hbot`，
脚本末尾自己把 desktop.json 与注册表还原。

## 6 崩溃报告（`scripts/verify-crash-report.ps1`）

必须用 **debug 构建**：`HBOT_TEST_PANIC_ON_START=1` 只在 debug 编译。实测：

- 首启 panic，退出码 101；写出 `logs/crash-20261120T190319Z.log`，里面有 marker `S6-CRASH-E2E`，密钥写成 `<redacted>`；
- 二次启动 stderr 出现「上次运行异常退出」与「已上报上次崩溃（logs.source=agent）」，报告改名成 `*.reported.log`；
- `--logs-tail 50` 能读到 `level=error` `source=agent` 那行，且不含密钥原文。

## 7 桌面启动探针（7 次，用安装包里的 release exe）

`smoke-nsis-install.ps1 -KeepInstalled -SkipWindowCheck` 装一份 → `startup-probe.ps1 -Exe <装出来的 exe> -Runs 7` → 手工卸载。

| 指标 | 中位 | p95 | min | max |
| --- | --- | --- | --- | --- |
| `window_ms`（进程起到窗口出现） | 107.8 | 136.8 | 98.8 | 136.8 |
| `setup_enter_ms`（进 setup，含 WebView2 初始化） | 526.2 | 556.5 | — | — |
| `open_begin_ms` | 150.2 | — | — | — |
| `core_ready_ms`（核心就绪） | 179.2 | 206.5 | — | — |
| `first_rpc_out_ms`（首个 RPC 出去） | 658.5 | 700.4 | — | — |
| `s5_window_rpc` | 556.5 | — | — | — |

库侧：`dur_store_open_total` 中位 24 ms、`dur_store_migrations` 4.6 ms。

> **`first_paint_ms` 一列是空的**：脚本没采到首帧标记。报告里必须写「未采到」，不许拿 `window_ms` 顶替。

## 8 无头启动探针（`hbot serve`，7 次）

`cargo build --release -p hbot-cli` 出 `E:/hbot-target/desk/release/hbot.exe`（22976512 字节），
`headless-startup-probe.ps1 -Exe <它> -Runs 7` → `plain` 档 `proc_to_ready_ms` 中位 **28.3**、p95 34.7、min 26.2、max 34.7。

这个数字的意义：无头宿主的启动成本基本就是「进程起 + 打开 SQLite + 跑迁移」，没有 WebView2 那 500 ms。

## 9 首启链路的等价证据

GUI 的点击路径没法在 CI 里点，但「点完之后真的会发生什么」有等价证据：

- `cargo test -p hbot-rpc --test local_runtime`：真 SQLite、真 isolate、真 handler，注入一条消息并断言回复写进 run log；
- `cargo test -p hbot-rpc --test examples_e2e`：仓库自带的示例项目整体跑通。

两者的退出码都是 0。**它们证明的是核心链路，不是 Tauri 窗口里的按钮**（见下一节）。

## 10 没验到的部分（务必写清，别让读者以为都验了）

1. **GUI 点击路径**：新建项目、打开设置、点托盘图标这些没有任何自动化证据。需要 UI 自动化或真人操作。
2. **`first_paint_ms`**：探针没采到首帧标记，只有「窗口出现」的时间。
3. **前台锁那条 WARN**：Windows 允许拒绝抢焦点，所以「主窗口成为前台」不是硬断言。
4. **Ctrl-C 优雅退出**：`crates/hbot-cli/src/serve.rs` 的 `wait_for_shutdown()` 在 Windows 只等 Ctrl-C，
   脚本无法给独立进程发 Ctrl-C，所以压测里的停止是强杀。优雅路径由
   `crates/hbot-rpc/tests/lifecycle_capability.rs` 里「`AppState::shutdown()` 把 reason=shutdown 交给 `onStop`」那条覆盖。
5. **macOS / Linux 桌面宿主**：本机只有 Windows。macOS 的 dmg 从未真打出来（见 `docs/05-quality-gates.md`）。
6. **真 QQ 账号**：所有平台适配器的真机行为都需要一个 OneBot 服务（NapCat 之类）+ 一个真 QQ 号。

## 11 复跑方式

```powershell
$env:CARGO_TARGET_DIR="E:/hbot-target/desk"
cargo build -j 2 -p hbot-desktop                # 五支 verify 要 debug exe
cargo build --release -j 2 -p hbot-cli           # 无头探针要 hbot.exe

$exe = "E:/hbot-target/desk/debug/hbot-desktop.exe"
pwsh -File scripts/verify-single-instance.ps1 -ExePath $exe
pwsh -File scripts/verify-tray-close.ps1 -ExePath $exe
pwsh -File scripts/verify-tray-actions.ps1 -ExePath $exe
pwsh -File scripts/verify-autostart.ps1 -ExePath $exe
pwsh -File scripts/verify-crash-report.ps1 -ExePath $exe   # 必须 debug
```

跑之前先清掉所有 `hbot-desktop` 进程：单实例锁是全局的，残留进程会让「第一实例」变成第二实例。
五支脚本都 `#Requires -Version 7.0`（5.1 会丢 `Start-Process` 的退出码，并按 GBK 解 UTF-8 日志）。
