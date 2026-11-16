# ADR-0003：命令执行隔离（`run_command` 的准入判据）

- 状态：**提议（Proposed）** —— 端口与原型已落地，`run_command` 仍返回 denied，等接入后转 Accepted
- 日期：2026-11-16
- 相关：`docs/02-architecture.md` §11.2、ADR-0002 §10.2 第 4 条（命令工具平台矩阵）
- 代码：`crates/hbot-core/src/command.rs`（契约）、`crates/hbot-core/src/ports.rs`（端口）、`crates/hbot-command/`（实现）

---

## 1. 问题

M0–M3 之后 `run_command` 是一个**恒返回 denied 的占位**（`crates/hbot-agent/src/tools/shell.rs` 里
`HbotError::denied("命令隔离尚未接入")`）。这既不是隔离也不是功能：模型看得见工具、点了必然失败，
而"什么时候才可以跑"这个问题没有任何可执行的判据。

本 ADR 要定的不是"怎么把命令跑起来"，而是：

> **在什么条件下允许启动一个子进程，以及条件不满足时如何拒绝。**

产品负责人已接受的原则：**装不上隔离就拒绝执行**（fail-closed），不接受"先跑起来再说"，
也不接受"用名字白名单假装隔离"（旧项目第 190 批已把那张表删掉，见 `AGENTS.md` §5.9）。

### 1.1 威胁与假设

| 假设 | 含义 |
|---|---|
| Agent 会被注入 | 群消息、网页、日志、生成代码都可能写入"帮我执行 `curl … \| sh`"这类指令 |
| 模型没有恶意，但有错 | 参数写错、拼错路径、把 `..` 当普通字符，比"主动越狱"更常见 |
| 用户会点确认 | 审批卡只证明"用户知道要跑命令"，不证明命令本身无害 |
| 本项目不假设 OS 可信度更高 | 隔离必须由内核原语提供，不能只靠我们自己的字符串检查 |

因此本层要回答的是：**命令本体（不是一个字符串，而是一个可执行文件 + 参数数组）能不能被启动，
启动之后它最多能碰到什么。**

---

## 2. 决策

### 2.1 一句话

**端口放 `hbot-core`（`CommandRunner`），实现在 `hbot-command`；实现必须 fail-closed：
按 `SandboxMode` 推导所需隔离层级 → 探测该层级是否可用 → 不可用就返回
`CommandDenied::IsolationUnavailable`，绝不降级为"直接跑"。**

### 2.2 拆成两层隔离，不混为一谈

旧项目的教训是"隔离"被当成一个布尔量，于是"能杀进程树"被误读成"能限制写盘"。本 ADR 显式分成两层：

| 层级 | 含义 | 能被什么满足 |
|---|---|---|
| `Containment` | 进程**树**可控：能一起杀、有进程数与内存上限 | Windows Job Object；Linux `setsid` + `killpg` |
| `WriteConfinement` | 进程**写不了**工作区以外的文件 | Windows 受限令牌 + write SID + 低完整性；Linux Landlock/seccomp/bwrap；macOS 无（`sandbox-exec` 已弃用） |

`SandboxMode` → 所需层级的映射（`hbot-command::CapabilityReport::required_level`）：

| 档位 | 所需层级 | 现状 |
|---|---|---|
| `read-only` | —— 直接拒（`CommandDenied::SandboxDenied`） | 只读档根本不允许跑命令 |
| `workspace-write` | `WriteConfinement` | **三平台都没有** → 一律 `IsolationUnavailable` |
| `danger` | `Containment` | Windows/Linux 可用；macOS 不可用 |

这条映射是本次最重要的设计决定：**它让"功能缺失"变得可观测且诚实**——
`workspace-write` 档下命令被拒，理由是"这台机器没有写隔离"，而不是"命令隔离尚未接入"。

### 2.3 契约（`hbot-core`）

`CommandSpec` 有意**不提供**"命令行字符串"这个字段：只有 `program` + `args: Vec<String>`，
拼 shell 字符串这件事在类型上就做不到。字段还包括 `cwd`、`workspace_root`、`env`（显式键值）、
`inherit_env`（**白名单**，按名取值）、`timeout`、`max_output_bytes`、`sandbox`、`allowed_programs`
（空 = 全拒）。

`CommandDenied` 是可区分的枚举（不是 `String`）：`IsolationUnavailable{platform,reason}`、
`ProgramNotAllowed`、`ProgramNotFound`、`OutsideWorkspace`、`TooLong{stream,limit_bytes}`、
`InheritEnvRejected`、`SandboxDenied`、`InvalidSpec`、`SpawnFailed`、`IoFailed`。
每个变体的 `Display` 都是**一句可操作的中文**，可以直接贴给用户。`IsolationUnavailable` 的实际文案是：

> 这台机器上无法建立命令隔离（平台：Windows；原因：Windows 的受限令牌 + write SID + 低完整性标签尚未接入（本原型只做了作业对象）），已拒绝执行。

Linux 上 `reason` 会区分两种情形（`probe.rs::linux_write_confinement_reason`）：
「内核未启用 landlock（`/sys/kernel/security/lsm` 里没有它），无法建立写隔离」与
「内核已启用 landlock，但本 crate 尚未接入写限制层」——前者用户换台机器也没用，后者是产品没做完。

### 2.4 实现里的六道闸（顺序即防线）

1. `validate_spec`：超时 ∈ (0, 30min]、输出上限 ∈ (1, 64 MiB]、参数无 NUL 且总长受限；
2. `require_isolation`：按 `sandbox` 推导层级 → 探测 → 不可用即拒（**先于任何程序名解析**，
   所以"程序不存在"不会掩盖"隔离不可用"）；
3. 程序解析：绝对路径 + canonicalize；裸名只搜 `PATH`（不搜当前目录），Windows 只补
   `.exe`/`.com` 并**拒绝 `.bat`/`.cmd`**（否则等于把 `cmd.exe /c` 请回来）；
4. 白名单比对：空清单 = 全拒，按 canonicalize 之后的绝对路径比较；
5. 工作目录：`cwd` 相对化到 `workspace_root` 后 `canonicalize`，再**逐组件**比较
   （不是字符串前缀比较，`...-cwd` 与 `...-cwd-outside` 必须分得开）；
6. 环境：`env_clear()` + 白名单 + 显式键值；`inherit_entire_env = true` 直接拒
   （`InheritEnvRejected`），并额外拒绝 `LD_PRELOAD`/`LD_LIBRARY_PATH`/`LD_AUDIT`/
   `DYLD_INSERT_LIBRARIES` 这类加载器注入变量。

跑起来之后：`stdin = null`、`stdout/stderr = piped`、两个读者线程按上限收集（超限即停读并置标志，
**内存上限与程序输出量无关**）；主线程 10 ms 轮询判定退出/超时/超限；**不论结局都杀整棵树**，
再 `wait()` 回收，再给读者线程一个 2 s 宽限期。

### 2.5 平台实现

| 平台 | `Containment` | `WriteConfinement` | 拒绝时的返回 |
|---|---|---|---|
| Windows | **有**：Job Object，`KILL_ON_JOB_CLOSE` + `ACTIVE_PROCESS=64` + 单进程 2 GiB / 作业 4 GiB；超时用 `TerminateJobObject` | **无**（受限令牌未接） | `IsolationUnavailable{platform: Windows, reason: "Windows 的受限令牌 + write SID + 低完整性标签尚未接入（本原型只做了作业对象）"}` |
| Linux | **有**：`pre_exec` 里 `setsid()`（失败退 `setpgid(0,0)`），超时 `kill(-pid, SIGKILL)` | **无**（Landlock 未接） | `IsolationUnavailable{reason: "内核未启用 landlock…" 或 "内核已启用 landlock，但本 crate 尚未接入写限制层"}` |
| macOS | **无** | **无** | `IsolationUnavailable{reason: "macOS 上 sandbox-exec 已弃用，本 crate 不提供命令隔离实现"}` |
| 其它 | 无 | 无 | `IsolationUnavailable{reason: "平台 Other 未提供命令隔离实现"}` |

Windows 侧用**手写 FFI**（`#[link(name = "kernel32")]`）而不是 `windows-sys`：
本次硬约束是"不许新增第三方依赖"，而用到的只有 5 个函数 + 1 个 `#[repr(C)]` 结构体
（`JOBOBJECT_EXTENDED_LIMIT_INFORMATION`，x64 为 144 字节，已用单测钉死尺寸）。
Linux 侧同理只声明 `setsid`/`setpgid`/`kill` 三个符号，不引入 `libc`。

---

## 3. 候选方案与代价

| # | 方案 | 代价 | 残余风险 | 判定 |
|---|---|---|---|---|
| ① | **完全不做**，删掉 `run_command` | 零成本，工具表少一行 | Agent 无法运行测试/构建，"Agent-first IDE"这个产品前提不成立 | 否 |
| ② | **程序名字白名单**（旧项目第 190 批之前的路子） | 便宜、无平台差异 | 白名单本身是判据，而"`node` 能做什么"由参数决定：`node -e "..."`、`git config alias`、`npm exec` 都能把白名单变成万能入口。这是**假装隔离** | 否（旧项目已证伪） |
| ③ | **Job Object / 进程组 + 资源上限**（本 ADR 采用） | Windows 手写 FFI ~120 行；Linux 一个 `pre_exec`；跨平台行为有差异 | 只保证"能一起杀 + 有上限"，**不限制写盘、不限制网络**；Windows 上 `spawn → AssignProcessToJobObject` 之间有窗口，孙进程可能逃出（正解是 `PROC_THREAD_ATTRIBUTE_JOB_LIST`，需 `STARTUPINFOEX`，本批未做）；Linux 上父进程被 `SIGKILL` 时已 `setsid` 的子孙会存活（无 PID namespace） | **采用**（作为 `Containment` 层） |
| ④ | **受限令牌 + 低完整性标签**（Win）/ **Landlock/seccomp**（Linux） | 高：旧项目实测每条命令多 ~1.5 s（`powershell.exe` 地板价 1 s）；受限令牌下**孙进程用管道 stdio 会 `EPERM`**；DLL/策略版本号、SACL 手工拼缓冲区等一串坑（`E:\H Bot 制造\...\acl\confine.ts` 顶部注释） | 无写隔离时：命令能以当前用户身份写 `%USERPROFILE%\.ssh`、改 `PATH` 之外的任何地方、读全部用户文件 | **延后**（下一批；本 ADR 把它的缺失写成"拒绝"） |
| ⑤ | **容器**（Docker/Podman） | 需要用户机器上有可用运行时 + 镜像拉取；冷启动数秒；文件系统映射与项目路径要双向对齐 | 运行时缺失时整条链路不可用；桌面产品不能假设用户装了 Docker | 否（可作为未来的可选后端） |
| ⑥ | **虚拟机 / WSL** | 最强隔离；需要虚拟化支持、镜像、跨边界文件同步；Windows 家庭版 WSL 可能不可用 | 复杂度与启动成本远超"跑一条 `cargo test`"的收益 | 否（明确留作逃生出口） |

### 3.1 为什么不是 tokio

端口是同步 `fn run`（与 `Clock`/`IdGenerator`/`EventSink` 同一风格：core 里全部同步，异步边界在实现方）。
命令本来就是阻塞操作，实现里用 `std::process` + 线程读者 + 10 ms 轮询，调用方需要的话自己
`spawn_blocking`。这样 `hbot-command` 不需要 tokio 运行时，端口也不必引入 `async-trait`。

---

## 4. fail-closed 的理由

1. **失败模式不对称**：误拒一条命令，用户看到一句中文原因，重试或改用别的方式；
   误放一条命令，代价是不可逆的（写盘、外发数据、起常驻进程）。两种错误的代价差若干数量级。
2. **"部分隔离"没有意义**：如果装不上写隔离却照跑，那么 `workspace-write` 档在语义上等于 `danger` 档，
   而用户以为自己处在更安全的档位 —— 这是**安全属性的静默降级**，比直接不可用更危险。
3. **能力探测必须是运行时的**：Landlock 依内核配置、受限令牌依 PowerShell/系统策略，
   编译期 `cfg` 判断不了。所以判据只能是"运行时探测 + 不可用即拒"。
4. **拒绝原因必须可显示**：`IsolationUnavailable{platform, reason}` 的 `Display` 直接是一句可操作中文，
   UI 不需要再翻译一次（也避免"拒绝但没有理由"这种最难排查的形态）。
5. **探测先于程序解析**（有单测钉住）：否则"程序不存在"会掩盖"隔离不可用"，
   用户会以为只是名字写错了。

---

## 5. 这层挡住了什么、挡不住什么

**挡住**（有测试或内核语义支撑）：

- 命令以 shell 字符串形式注入 —— 类型上没有这个字段，Windows 上还拒绝 `.bat/.cmd` 与相对路径；
- 命令从 `cwd` 逃出项目根（`..`、根外绝对路径、共享字符串前缀的兄弟目录都在测试里）；
- 父进程环境变量（含密钥）泄漏进子进程 —— 默认 `env_clear()`，只认显式白名单；
- 命令被改名/替换成别的程序 —— 白名单比对的是 canonicalize 之后的绝对路径；
- 超时后留下整棵进程树 —— Windows 用 `TerminateJobObject`，Linux 用 `kill(-pgid)`；
- 输出无限增长吃满内存 —— 上限之外读者停读，返回 `TooLong` 并杀树；
- 单个命令 fork 出 1000 个进程或吃 8 GiB 内存 —— Job Object 的 `ACTIVE_PROCESS` / 内存上限。

**挡不住**（必须承认，别当已解决）：

- **写盘**：`workspace-write` 档当前是**拒绝**而不是"受限运行"。一旦接入受限令牌，也只是"挡住写入"，
  不挡读取（除低完整性外）。
- **读文件**：`danger` 档下命令能读当前用户可读的一切（含 `~/.ssh`、keyring 之外的凭据文件）。
- **网络**：没有防火墙规则，命令可以外发数据、访问内网。
- **Windows 上的逃逸窗口**：`spawn` 之后、`AssignProcessToJobObject` 之前起的孙进程不在 job 里
  （`KILL_ON_JOB_CLOSE` 杀不到它）。
- **Linux 上的孤儿**：本进程被 `SIGKILL` 时，已 `setsid` 的子孙不会被连带杀掉
  （Windows 侧因为 `KILL_ON_JOB_CLOSE` 反而更稳）。
- **程序清单是调用方的责任**：`hbot-command` 实现只保证"清单里的绝对路径才放行"，**不判断哪些名字
  危险**（分层取舍，不是遗漏）。解释器准入判据放在**配置解析层**
  （`crates/hbot-rpc/src/app/agent/config.rs` 的 `SandboxConfig::resolve_with`）：默认拒绝
  `INTERPRETER_PROGRAMS`（`cmd`/`powershell`/`python`/`node` 等）里的名字并返回 `-32002`；
  显式 `HBOT_ALLOW_SHELL=1`（只认 `1`/`true`/`yes`/`on`）才放行并 `warn`（每进程每条清单一次）。
  绕开配置层直接构造 `CommandSpec`（如红队 `b03`）仍然能跑 `cmd` —— 这正是"命令层不兜底、
  判据必须在配置层"的可执行证据（RT-05，2026-11-16）。
- **半写文件**：超时是 `SIGKILL`/`TerminateProcess` 语义，不给程序清理机会。
- **CPU/IO 抢占**：只限内存与进程数，没限 CPU 时间与 IO 带宽。

---

## 6. 证据

`crates/hbot-command` 内的测试**真跑子进程**（子进程是测试二进制自己，`current_exe --exact helper`）：

| 测试 | 证什么 |
|---|---|
| `timeout_kills_the_whole_process_tree` | helper 起一个孙进程（`PING.EXE`/`/bin/sleep`），3 s 超时后断言**两个 pid 都没了**；并用"只杀直接子进程"做过反证：此时孙进程仍存活（`grandchild_alive_after_child_only_kill=True`），说明断言不是空转 |
| `isolation_unavailable_is_denied_not_allowed` | 注入 `DenyAllProbe`，断言返回 `IsolationUnavailable`（带着探测原因与 `Platform::current()`），且**不是**放行；同时钉住"先探测后解析程序" |
| `output_is_bounded` | helper 无限写 stdout，8 KiB 上限下判 `TooLong{stream: Stdout}`，且 helper 进程被杀 |
| `environment_is_not_inherited_by_default` | 父进程设 `HBOT_TEST_SECRET`，子进程看到 `SECRET=MISSING`、`PATH=MISSING`；显式加入 `inherit_env` 后才看得到 |
| `working_directory_outside_the_workspace_is_rejected` | 根外绝对路径与 `..` 都返回 `OutsideWorkspace` |
| `empty_program_allowlist_allows_nothing` | 空清单 = 全拒（fail-closed 的另一半） |

注：Linux 分支**只编译、未运行**（本机是 Windows，`cfg` 不生效）；macOS 分支同理只编译。

---

## 7. 可逆性

- 端口在 `hbot-core::ports::CommandRunner`，只有 1 个方法；`hbot-command` 只是**一个实现**。
  换成容器后端、WSL 后端、或"受限令牌 + Landlock 都接上"的加强版，都只是新写一个
  `impl CommandRunner`，`hbot-agent` 侧一行不用改（它只认 trait）。
- 拒绝判据集中在 `CapabilityReport`（`probe` + `required_level` + `supports`）：
  当某平台接入写隔离后，只需改探测实现，**不需要改调用方**。
- 反向可逆：若将来产品决定放弃命令执行，删除 `hbot-command` 并从 `ALLOWED` 表移除一行即可；
  `hbot-core` 里的契约类型没有下游耦合（只有 `hbot-command` 用它们）。

---

## 8. 重估信号

任一条成立，就该重开这个 ADR：

1. 用户在 `workspace-write` 档需要跑命令（当前被拒）→ 必须接入受限令牌 / Landlock，
   否则"Agent 跑测试"这条主链路永远走 `danger` 档。
2. 实测出现**逃出 job / 进程组**的孤儿进程（`ACTIVE_PROCESS` 计数对不上）→ 上
   `PROC_THREAD_ATTRIBUTE_JOB_LIST`。
3. 命令平均耗时被隔离开销推高 > 2 s（旧项目受限令牌实测 ~1.5 s/条）→ 评估常驻 worker
   或容器后端。
4. 目标 Linux 发行版里 Landlock 可用率 < 80% → 改用 `bwrap`/容器作为等价层。
5. 需要跑 MSVC/Windows SDK 这类"必须有真机"的构建 → 评估 WSL/容器作为**可选**后端
   （本 ADR 明确不做，但不否决未来接入）。
6. 出现"命令读到了不该读的文件"的实测事故 → 读隔离进入必做项（当前只谈写）。

---

## 9. 对既有文档的影响

- `docs/02-architecture.md` §11.2 的"命令隔离"三条 bullet 已按本 ADR 改写为**现状 + 能力矩阵**，
  不再写"Windows 受限令牌 + Job Object"当作已完成。
- `docs/03-directory.md` 的 crate 清单需要加 `hbot-command`（依赖只有 `hbot-core`）。
- `xtask/src/deps.rs` 的 `ALLOWED` 已加 `("hbot-command", &["hbot-core"])`。
- `run_command` **仍然返回 denied**：本 ADR 只交端口 + 实现 + 证据，接线是下一批的事。
- **2026-11-16 补记（RT-05）**：§5 的"程序清单是调用方的责任"现在由**配置解析层**执行 ——
  `crates/hbot-rpc/src/app/agent/config.rs` 的 `SandboxConfig::resolve_with` 默认拒绝解释器/外壳类
  白名单（`-32002`），`HBOT_ALLOW_SHELL=1` 是显式逃生阀（放行 + 每进程一次 `warn`）。
  **§2.4 的六道闸顺序与 `hbot-command` 的职责不变**：那一层仍然只认"清单里有没有这个名字"，
  危险名字的解释权在配置层。详见 `docs/security/red-team-findings.md` RT-05。