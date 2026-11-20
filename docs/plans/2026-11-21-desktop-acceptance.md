# 2026-11-21 桌面端验收收尾（A/B/C 三单元）

> 用户 2026-11-21 02:51 指示：「你把他们做了吧，写执行文档吧，准备子代理开发吧」。
> 本文是三个单元的**完整任务书**：每个单元自带背景、入口、判据、产出与禁止项，**不需要读别的对话**就能开工。
> 决策权在 lead；子代理只在被指派单元内做决定，跨界必须先问。
>
> **执行状态（2026-11-21 05:45）：三个单元全部完成。** A → `docs/spikes/S14-desktop-acceptance.md`（安装包冒烟 11 项 PASS、五支 `scripts/verify-*.ps1` 全 0）；B → `scripts/soak-serve.ps1` + `scripts/soak-client.mjs` + `docs/spikes/S15-soak.md`（120 分钟九条判据全 PASS，首轮抓到无头宿主缺事件桥，已修 commit `5082afc`）；C → `docs/quickstart-desktop.md` + 两条零配置修复（新建项目自带 `bots.json`、`bot_list` 合并声明，commit `8009162`）。

## 0. 三个单元总览

| 单元 | 做什么 | 预计 | 能否并行 | 改代码 | 产出 |
|---|---|---|---|---|---|
| **A** | 在今天的 HEAD 上重跑桌面端八项既有验证 + 一条首启链路 | 1–2 h | 与 B/C 可并行 | 只在发现缺陷时 | ~docs/spikes/S14-desktop-acceptance.md~ + README 一行 |
| **B** | 长时稳定性压测（serve + local 平台，默认 2 h，可配） | 2–4 h（含等待） | 与 A 并行有资源竞争 | 新增 ~scripts/soak-serve.ps1~ | 脚本 + ~docs/spikes/S15-soak.md~ |
| **C** | 五分钟上手指南（从拿安装包到卸载） | 1 h | 完全独立 | 只加文档 | ~docs/quickstart-desktop.md~ |

**为什么是这三件**：桌面端功能面已收口（~docs/HANDOFF.md~ §7 D 段只剩外部条件项），没有新能力要写；剩下的真实风险是「装完能不能用」「连续跑会不会烂」「新人能不能自己上手」。这三件都**不需要真机、不需要 QQ 账号、不需要 macOS**，是本机今天就能给出结论的。

## 1. 共同约束（三个单元都适用）

### 1.1 环境

- 仓库 ~E:/HBot-Next~，Rust 钉在 **1.100.0**（~rust-toolchain.toml~，别用别的版本跑闸门）。
- 构建缓存：~$env:CARGO_TARGET_DIR="E:/hbot-target/desk"~（**每个单元用自己的目录**，共用会互相等锁；~E:/hbot-target/gate100~ 是主闸门用的，不要占）。
- 每条 pwsh 先 ~[Console]::OutputEncoding=[Text.Encoding]::UTF8~；长命令 ~run_in_background~ + ~job_output({wait:true,timeout_ms:420000})~。
- 日志：~New-Item -ItemType Directory -Force -Path E:/HBot-Next/_recovery | Out-Null~，**只用 ~*~ 重定向写、另用 ~Select-String~ 读**；绝不要 ~Select-String ... *>> 同一个日志~（曾把日志吹到 576 MB）。
- git 不在 PATH，唯一可用：~D:/Visual Studio/Common7/IDE/CommonExtensions/Microsoft/TeamFoundation/Team Explorer/Git/cmd/git.exe~。**子代理不要执行任何 git 命令**（add/commit/push 由 lead 统一做）。

### 1.2 不允许做的事

1. **不许为让检查变绿而改断言、加 ~#[allow(...)]~、放宽阈值**。发现问题就报告问题。
2. **不许装系统级东西**（NDK、驱动、OBS）。本机已有：tauri-cli 2.12.4、Node/pnpm、VS 工具链。
3. **不许改产品代码来迁就脚本**；脚本坏了改脚本，产品坏了先报告再改（改产品要附测试）。
4. **不许把未验证的东西写成「已验证」**：报告里的每个数字必须能追到一条命令 + 退出码。
5. 不许在仓库里留临时文件；临时物一律放 ~_recovery/~（已 gitignore）。

### 1.3 提交约定（只约束 lead；子代理只需把改动留在工作树）

- 每个单元一个 commit，前缀 ~test(desktop):~ 或 ~docs(desktop):~；
- commit 前六道闸门全绿：~cargo fmt --all -- --check~ / ~cargo run -q -p xtask -- gen --check~ / ~cargo run -q -p xtask -- deps~ / ~cargo clippy -j 2 --workspace --all-targets -- -D warnings~ / ~cargo test -j 2 --workspace~（基线 **2038 passed / 126 suites**）/ ~pnpm --dir apps/web run check~（47 文件 / 427 用例）；
- 文档数字变了要同步 ~README.md~ / ~AGENTS.md~ / ~docs/05-quality-gates.md~ / ~docs/HANDOFF.md~。

## 2. 单元 A：桌面端端到端验收

### 2.1 背景（必读）

仓库里**已经有**九支验收脚本，它们在 2026-11-16/17 各自跑过一次，结论写进了 ~docs/spikes/S5-packaging.md~ 与 ~docs/spikes/S6-desktop-ops.md~。此后代码又走了三十多个 commit（~C4.jobs~ / ~C2.session~ / 附件出站 / 只读 sql / 宿主预算 / crash 收尾 …），**这些脚本一次都没复跑过**。所以 A 不是从零写验收，而是**在今天的 HEAD 上复跑并给出结论**。

| 脚本 | 验什么 | 上次结论 |
|---|---|---|
| ~scripts/smoke-nsis-install.ps1~ | 静默装 → 起进程/出窗口 → 关 → 卸 → 查残留（五项） | EXIT=0（S5 §4.5） |
| ~scripts/verify-single-instance.ps1~ | 第二次启动把已有窗口带到前台后 exit 0 | EXIT=0（S6 §2.2） |
| ~scripts/verify-tray-close.ps1~ | 关窗进托盘、进程不退 | EXIT=0（S6 §3.2） |
| ~scripts/verify-tray-actions.ps1~ | 托盘菜单三个 id 的派发 | EXIT=0（S6 §3.3，**不是鼠标点击**） |
| ~scripts/verify-autostart.ps1~ | 开机自启开关、幂等、注册表 | EXIT=0（S6 §4.2） |
| ~scripts/verify-crash-report.ps1~ | 崩两次启动 + ~--logs-tail~ 断言 | EXIT=0（S6 §6.3） |
| ~scripts/startup-probe.ps1~ | 桌面冷启动分段耗时 | 有基线（S2 §3.2） |
| ~scripts/headless-startup-probe.ps1~ | ~hbot serve~ 冷启动到 ready | 有基线（S13 §3.5） |
| ~scripts/mem-probe.ps1~ | 进程树内存采样（工具，不是断言） | — |

### 2.2 步骤

1. **先读** ~docs/spikes/S5-packaging.md~ 与 ~docs/spikes/S6-desktop-ops.md~ 的「实测」段，确认每支脚本的参数与前置条件（例如 ~smoke-nsis-install.ps1~ 要 ~-Installer <setup.exe>~；~verify-autostart.ps1~ 会动 HKCU Run 值）。
2. **拿到安装包**，二选一（先试 ①，失败再 ②）：
   - ① 本机构建：~pnpm --dir apps/web run build~ → ~cargo tauri build --bundles nsis~（tauri-cli 2.12.4 已装；产物在 ~target/release/bundle/nsis/*-setup.exe~，注意 ~CARGO_TARGET_DIR~ 会改变输出位置）；
   - ② 从 CI 下载：最近一次成功 run 的 artifact **~hbot-nsis-x64~**（~actions/upload-artifact@v4~，路径 ~target/release/bundle/nsis/*.exe~）。取 token 与下载手法见 §5。
3. **按顺序跑**（每支脚本单独落一个日志）：
   ~smoke-nsis-install.ps1~ → ~verify-single-instance.ps1~ → ~verify-tray-close.ps1~ → ~verify-tray-actions.ps1~ → ~verify-autostart.ps1~ → ~verify-crash-report.ps1~ → ~startup-probe.ps1~ → ~headless-startup-probe.ps1~。
4. **补一条脚本覆盖不到的链路**：安装后首启 → 新建项目 → 用 ~local~ 平台收一条消息 → run log 里出现回复。做法见 2.3。

### 2.3 首启链路怎么验（没有 GUI 自动化）

桌面壳没有可编程的点击接口，**不要试图模拟鼠标**。用这条等价路径：

1. 用安装后的主 exe 启动，确认进程活着 + ~MainWindowTitle~ 非空（~smoke-nsis-install.ps1~ 已做）；
2. 确认数据目录与库：~hbot_core::paths::data_dir()~ 指向的目录里生成了 ~hbot.db~（**路径以代码为准**，别抄文档）；
3. 项目与 bot 的语义改用**库层等价物**验证：~cargo test -j 2 -p hbot-rpc --test local_runtime~（含「注入一条消息 → 生成的代码执行 → 回复进 run log」）与 ~--test examples_e2e~；这一条钉的正是「新建项目 + 收消息 + 见回复」，只是绕开了 GUI；
4. 在报告里**明确写出**「GUI 点击路径未自动验证」，不要含糊。

### 2.4 判据与产出

- 判据：九支脚本 EXIT=0；首启链路三步有证据；任何失败必须归到「代码 / 脚本 / 环境」之一，并给定位行号。
- 产出：~docs/spikes/S14-desktop-acceptance.md~，结构照 ~docs/spikes/S6-desktop-ops.md~：每支脚本一节（命令、退出码、关键输出、结论），末尾一张总表 + 「没验到的部分」。
- ~README.md~ 的验证段加一行指向 S14。

### 2.5 副作用声明（跑之前先看）

- ~smoke-nsis-install.ps1~：静默装到 ~%LOCALAPPDATA%\hbot-smoke~，结束时卸载 + 清理；失败时加 ~-KeepInstalled~ 会留下安装目录。
- ~verify-autostart.ps1~：会改 **HKCU Run 值**与 ~desktop.json~，脚本自带备份/还原；仍要在报告里写明这一句。
- ~verify-crash-report.ps1~：会故意让进程崩两次，产生崩溃报告文件（在数据目录里），属预期。

## 3. 单元 B：长时稳定性压测

### 3.1 目标

回答一个现有测试回答不了的问题：**连续跑两小时会不会烂**。现有测试全是秒级；桌面端唯一没被覆盖的维度就是时间。

### 3.2 形态（已定，不要改成别的）

- 用 **~hbot serve~（无头）** 而不是 GUI：无头没有 WebView 渲染噪声，内存曲线干净，CI 也能跑。
- 平台用 **~local~**（进程内假适配器，~crates/hbot-rpc/src/app/runtime/local.rs~）：不需要 QQ、不需要网络。
- 负载三件套循环投递：① 普通消息（每轮 1 条）；② ~timer~ 计划（清单里声明一个每 N 秒的计划）；③ ~jobs~ 作业（清单里声明一个能立即完成的类型）。这样三条独立 tick（~tick~ → ~tick_jobs~ → ~tick_waits~）全被压到。

### 3.3 新脚本 ~scripts/soak-serve.ps1~

参数（默认值要能让 ~pwsh -File scripts/soak-serve.ps1~ 直接跑起来）：

| 参数 | 默认 | 含义 |
|---|---|---|
| ~-Minutes~ | ~120~ | 总时长 |
| ~-IntervalSec~ | ~5~ | 每轮注入间隔 |
| ~-SampleSec~ | ~30~ | 内存采样间隔 |
| ~-DataDir~ | ~_recovery/soak/data~ | 临时数据目录（跑完保留，便于事后查库） |
| ~-Bin~ | 自动找 ~hbot.exe~ | serve 可执行文件 |

脚本要做的事：

1. 生成临时项目（~hbot.json~ 声明 ~capabilities~ 的 ~timer~ / ~jobs~ / ~kv~ / ~send~ 与对应的 ~plans~ / ~jobs~）与 ~bots.json~（一个 ~local~ 平台 bot）；
2. 起 serve 子进程，等 ready 行（**照 ~crates/hbot-cli/src/serve.rs~ 的 ready 输出格式**，不要猜）；
3. 循环：每隔 ~-IntervalSec~ 通过 WS 发一条 ~BotInject~（协议见 ~crates/hbot-rpc/src/dto/command.rs~），记录「发出 → run log 出现回复」的延迟；
4. 每 ~-SampleSec~ 调 ~scripts/mem-probe.ps1 -Id <pid> -IncludeChildren -SplitChildren -Json~ 采一次，同时记 ~hbot.db~ 字节数、run log 里 error 计数、~BotSnapshot.queue_depth~（经 RPC 查）；
5. 结束：正常停 serve，把全部采样写成 ~_recovery/soak/summary.json~ + 一张 markdown 表，并按 3.4 的阈值给出 **PASS/FAIL**。

### 3.4 判据（阈值写死，不要临场放宽）

| 维度 | PASS 条件 | 失败说明 |
|---|---|---|
| 内存 | 最后 30 分钟私有内存的线性回归斜率接近 0（每 10 分钟增长 < 10 MB），且峰值 < 首次采样 + 250 MB | 单调爬升 = 泄漏 |
| 处理延迟 | 95% 的注入在 2 秒内见到回复；最大 < 10 秒 | 卡死 / 队列堵 |
| 队列 | ~queue_depth~ 全程回到 0（每轮采样末） | 有滞留 = 丢消息风险 |
| 错误 | run log 里 error 级为 0（**除了压测刻意制造的**，如有必须点名） | 静默失败 |
| 存储 | ~hbot.db~ 增长 < 200 MB；且 ~-wal~ 文件不持续膨胀 | WAL 未 checkpoint |
| 退出 | serve 收到停止信号后 5 秒内退出，退出码 0 | 关不干净 |

**如果 FAIL**：不要调阈值让它变绿。把失败采样序列附在报告里，定位到 crate + 函数，开一个明确的「已知问题」条目。

### 3.5 产出

- ~scripts/soak-serve.ps1~（含 ~.SYNOPSIS~ 注释块，照 ~smoke-nsis-install.ps1~ 的写法）；
- ~docs/spikes/S15-soak.md~：命令、时长、六维表格、结论、发现的缺陷（若有）；
- ~docs/05-quality-gates.md~ 的闸门表下面加一行「长时稳定性（手动，2 h）」并指向 S15。

### 3.6 注意

- **2 小时是默认值，不是必须**：正式跑用 120 分钟；开发调试先 ~-Minutes 5~ 跑通脚本，再放长。
- 脚本必须**可中断**（Ctrl+C 后不留子进程）。收尾里显式杀进程树。
- 跑的时候不要同时 ~cargo test --workspace~，会污染内存曲线（要在报告里写明这一点）。

## 4. 单元 C：五分钟上手指南

### 4.1 目标

给一个**从没见过这个项目**的人写的上手文档：从拿到安装包到看见自己的 bot 回复第一条消息，五分钟内完成，中途不需要读源码。

### 4.2 必须覆盖（顺序即读者路径）

1. **拿到安装包**：CI artifact ~hbot-nsis-x64~ 的下载步骤（GitHub 页面点法），以及没有 GitHub 账号怎么办；
2. **安装**：双击 → 装到哪 → 首次启动可能出现的 SmartScreen 提示怎么处理；
3. **首启界面**：三个 store / 布局一句话带过，重点是「去哪新建项目」；
4. **最小项目**：~hbot.json~ 的最小可用示例（**从 ~crates/hbot-code/src/manifest.rs~ 的字段与 ~examples/~ 里取真实可跑的，不要手写臆造**）+ 一个 ~onMessage~ 回显的 ~src/main.ts~；
5. **接平台**：~bots.json~ 与平台字段；OneBot 需要外部 NapCat（一句话说明 + 指向 ~docs/spikes/S11-napcat-local-fixture.md~），并明确写「没有 QQ 账号也能先用 ~local~ 平台把链路跑通」；
6. **看见效果**：run log 在哪、回复长什么样、~ctx.log~ 的输出落在哪；
7. **日常操作**：关窗进托盘、怎么真正退出、开机自启在哪开、数据目录在哪（~hbot_core::paths::data_dir()~ 的实际值）、怎么备份/清空；
8. **卸载**：控制面板 / 设置里的入口，以及「卸载后数据目录还在不在」（**要实测**，不要猜）；
9. **出问题看哪里**：日志路径、~--logs-tail~、崩溃报告位置。

### 4.3 判据

- **每个路径、每个按钮、每条命令都必须在本机验证过**；写不出来的写「未验证」，不要编。
- 不写产品介绍、不写架构图、不写「为什么这样设计」——那是 ~README.md~ 与 ~docs/02~ 的事。
- 开头一句话说清「这个文档假设你拿到了安装包」。

### 4.4 产出

- ~docs/quickstart-desktop.md~；
- ~README.md~ 顶部「快速开始」指向它（一句话链接）；
- ~docs/HANDOFF.md~ §9「起手三步」加一行指针。

## 5. 环境手册（三单元共用）

### 5.1 取 GitHub token 与下载 artifact

```powershell
$git = "D:/Visual Studio/Common7/IDE/CommonExtensions/Microsoft/TeamFoundation/Team Explorer/Git/cmd/git.exe"
$probe = "protocol=https" + [char]10 + "host=github.com" + [char]10 + [char]10
$out = $probe | & $git credential fill 2>$null
$tok = (($out -split [char]10 | Where-Object { $_ -like "password=*" }) -replace "password=","").Trim()
$env:HTTP_PROXY="http://127.0.0.1:7897"; $env:HTTPS_PROXY="http://127.0.0.1:7897"
# 先查 run 列表，拿到 artifacts_url，再下 zip
curl.exe -sSL --max-time 120 -H "Authorization: Bearer $tok" -H "Accept: application/vnd.github+json" "<ARTIFACT_ZIP_URL>" -o _recovery/hbot-nsis.zip
```

- **必须带 ~-L~**（否则 302 得到 0 字节）；代理恒为 ~http://127.0.0.1:7897~；
- ~Invoke-RestMethod~ 会挂死，只用 ~curl.exe~；
- ~api.github.com~ 偶发 ~schannel: failed to receive handshake~，重试 2–3 次仍失败就等几分钟，不要死磕。

### 5.2 常用查询

```powershell
curl.exe -sS --max-time 60 -H "Authorization: Bearer $tok" -H "Accept: application/vnd.github+json" "https://api.github.com/repos/haijiawe/hbot/actions/runs?per_page=5" -o _recovery/runs.json
```

## 6. 派发与状态

| 单元 | 负责 | 状态 |
|---|---|---|
| A | lead | 进行中 |
| B | 子代理 soak-serve（失败则 lead 接管） | 待派 |
| C | 子代理 quickstart（失败则 lead 接管） | 待派 |

> 执行记录（每完成一步追加一行，带时间与退出码）：

（待填）

