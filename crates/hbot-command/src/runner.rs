//! [`PlatformRunner`]：[`hbot_core::ports::CommandRunner`] 的实现（同步、阻塞）。
//!
//! 一次调用的顺序是固定的，**任何一步不满足就拒绝，不跳过**：
//!
//! 1. 校验规格（超时、输出上限、参数规模）；
//! 2. 把 `SandboxMode` 映射成需要的隔离层；`read-only` 直接拒绝；
//! 3. **探测**该层是否可用——不可用就 [`hbot_core::CommandDenied::IsolationUnavailable`]；
//! 4. 解析程序（绝对路径 + 允许清单）与工作目录（必须在项目根内）；
//! 5. 构造环境（清空 + 白名单）；
//! 6. 建隔离范围（+ 写隔离档再建一层写限制）→ spawn → **立刻**把子进程放进范围
//!    （放不进去就杀掉并拒绝）；
//! 7. 有界读输出，轮询等退出；超时/超限 → 杀整棵树；
//! 8. 无论怎么结束都收掉整棵树（不留孤儿），再组装结果。

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hbot_core::{
    CommandDenied, CommandOutput, CommandRunner, CommandSpec, CommandStatus, OutputStream,
    Platform, ResourceLimits, SandboxMode,
};

use crate::child::{Exit, Spawned};
use crate::confinement::WriteConfinement;
use crate::containment::Containment;
use crate::limits::{self, LimitViolation};
use crate::output::BoundedOutput;
use crate::probe::{CapabilityReport, IsolationLevel, IsolationProbe, SystemProbe};
use crate::{env, path, program};

#[cfg(windows)]
use crate::confinement::SpawnPlan;

/// 等退出的轮询间隔。10 ms 是"超时精度"与"空转开销"的折中：
/// 一次 `npm install` 的等待循环也就跑几百次。
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// 杀树之后等读者线程收尾的宽限。超过就返回已读到的部分输出。
const READER_GRACE: Duration = Duration::from_millis(2_000);

/// 超时上限：再长就不该占着线程同步等，应该走后台任务。
const MAX_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// 单流输出上限的硬顶（64 MiB）。超过它说明命令本身不该在同步路径上跑。
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// 真实平台实现。
///
/// 用 [`PlatformRunner::system`] 拿默认探测；测试可用 [`PlatformRunner::with_probe`]
/// 注入一个"本机没有任何隔离"的探测，验证 fail-closed 行为。
pub struct PlatformRunner {
    probe: Arc<dyn IsolationProbe>,
    platform: Platform,
}

impl PlatformRunner {
    /// 用真实平台探测。
    pub fn system() -> Self {
        Self {
            probe: Arc::new(SystemProbe),
            platform: Platform::current(),
        }
    }

    /// 注入自定义探测（只给测试与将来的"能力面板预演"用）。
    pub fn with_probe(probe: Arc<dyn IsolationProbe>) -> Self {
        Self {
            probe,
            platform: Platform::current(),
        }
    }

    /// 当前能力矩阵（三档能不能跑）。
    pub fn capabilities(&self) -> CapabilityReport {
        CapabilityReport::probe_with(self.probe.as_ref(), self.platform)
    }

    /// 这个 runner 认的平台。
    pub fn platform(&self) -> Platform {
        self.platform
    }

    fn require_isolation(&self, spec: &CommandSpec) -> Result<IsolationLevel, CommandDenied> {
        let level = match spec.sandbox {
            // 只读档：命令连"跑起来"都不该发生（AGENTS.md §4.4 的只读档硬拒）。
            SandboxMode::ReadOnly => {
                return Err(CommandDenied::SandboxDenied { mode: spec.sandbox });
            }
            SandboxMode::WorkspaceWrite => IsolationLevel::WriteConfinement,
            SandboxMode::Danger => IsolationLevel::Containment,
        };
        // 平台兑现不了的限额**在探测隔离层之前**就拒绝：静默忽略限额等于骗调用方
        // （调用方会以为"最多 100 ms CPU"已经生效，于是拿它当护栏）。
        // 放在这里而不是 `validate_spec` 里，是因为它依赖"这台机器"而不是"这份规格"：
        // 同一份规格在 Windows 上合法、在 Linux 上要拒（Linux 没有 I/O 上限）。
        if let Some(reason) = limits::unsupported_reason(&spec.limits, self.platform) {
            tracing::debug!(
                program = %spec.program,
                platform = %self.platform,
                %reason,
                "请求了本平台做不到的资源上限，拒绝执行",
            );
            return Err(CommandDenied::IsolationUnavailable {
                platform: self.platform,
                reason,
            });
        }
        if let Err(reason) = self.probe.probe(level) {
            tracing::debug!(
                program = %spec.program,
                level = %level,
                %reason,
                "命令隔离不可用，拒绝执行",
            );
            return Err(CommandDenied::IsolationUnavailable {
                platform: self.platform,
                reason,
            });
        }
        Ok(level)
    }
}

impl Default for PlatformRunner {
    fn default() -> Self {
        Self::system()
    }
}

impl std::fmt::Debug for PlatformRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformRunner")
            .field("platform", &self.platform)
            .finish_non_exhaustive()
    }
}

impl CommandRunner for PlatformRunner {
    fn run(&self, spec: CommandSpec) -> Result<CommandOutput, CommandDenied> {
        validate_spec(&spec)?;
        let level = self.require_isolation(&spec)?;
        let program_path = program::resolve_program(&spec)?;
        let cwd = path::resolve_cwd(&spec.workspace_root, &spec.cwd)?;
        let env_pairs = env::build_env(&spec)?;
        // `workspace-write` 档在 Linux 上只放行工作区 + 私有缓存根。包管理器（npm / pnpm /
        // corepack / yarn）没有可写缓存目录就起不来，而失败的样子很像"隔离坏了"——
        // 于是把它们的缓存变量默认指到 `cache::root()`：与 Landlock 规则用的是**同一个**
        // 路径，不可能漂移。显式指定的值仍然优先（见 `cache::apply_write_cache_defaults`）。
        #[cfg(target_os = "linux")]
        let env_pairs = {
            let mut pairs = env_pairs;
            if level == IsolationLevel::WriteConfinement {
                crate::cache::apply_write_cache_defaults(&mut pairs, &crate::cache::root());
            }
            pairs
        };

        // 先建隔离范围：建不出来就不 spawn——绝不允许"先跑起来再想办法"。
        // 资源上限也在这一步落进内核（Windows 写进作业对象、Linux 记下来在子进程里 setrlimit）：
        // 限额装不上和隔离装不上同罪，都按 `IsolationUnavailable` 拒绝。
        let guard = Containment::create(&spec.limits).map_err(|reason| {
            CommandDenied::IsolationUnavailable {
                platform: self.platform,
                reason,
            }
        })?;

        // 写隔离层：只有 `workspace-write` 档需要它。**建不出来就拒绝**——探测说可用
        // 但真建时失败（权限被收回、路径消失）同样不能放行，所以这里再失败一次。
        let confinement = match level {
            IsolationLevel::WriteConfinement => Some(
                WriteConfinement::create(&spec.workspace_root).map_err(|reason| {
                    tracing::debug!(
                        workspace = %spec.workspace_root.display(),
                        %reason,
                        "写隔离建不起来，拒绝执行",
                    );
                    CommandDenied::IsolationUnavailable {
                        platform: self.platform,
                        reason,
                    }
                })?,
            ),
            IsolationLevel::Containment => None,
        };

        let started = Instant::now();
        let mut child = start(
            &program_path,
            &spec,
            &cwd,
            &env_pairs,
            &guard,
            confinement.as_ref(),
        )?;
        tracing::debug!(
            pid = child.id(),
            program = %program_path.display(),
            level = %level,
            "命令已启动",
        );

        // 进不了隔离范围 = 没有隔离：杀掉刚起的进程并拒绝（fail-closed 的最后一米）。
        if let Err(err) = guard.attach(&child) {
            tracing::warn!(
                program = %program_path.display(),
                level = %level,
                error = %err,
                "无法把子进程放进隔离范围，已终止并拒绝执行",
            );
            let _ = guard.kill_tree(&mut child);
            let _ = child.wait();
            return Err(CommandDenied::IsolationUnavailable {
                platform: self.platform,
                reason: format!("无法把子进程加入隔离范围（{err}），已终止该进程并拒绝执行"),
            });
        }

        let stdout = child.take_stdout();
        let stderr = child.take_stderr();
        let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
            let _ = guard.kill_tree(&mut child);
            let _ = child.wait();
            return Err(CommandDenied::IoFailed {
                context: "接管子进程输出管道".to_owned(),
                reason: "spawn 之后拿不到管道（标准输出/错误未按 piped 建立）".to_owned(),
            });
        };

        let limit = spec.max_output_bytes;
        let output = BoundedOutput::spawn(stdout, stderr, limit);
        let outcome = wait_for_finish(&mut child, &guard, &output, spec.timeout, &spec.limits)?;

        // 不论正常退出、超时还是超限，都把整棵树收掉：命令返回就不该留下后代进程。
        let _ = guard.kill_tree(&mut child);
        let _ = child.wait();
        output.wait_closed(READER_GRACE);

        // 用量必须在这里读：Windows 上作业对象的累计记账随 `CloseHandle` 消失（`guard` 在
        // 函数末尾才 drop），Linux 上 `RUSAGE_CHILDREN` 只有在 `wait` 回收了子进程之后才有数。
        // 超时分支也带它：调用方要看到"超时那一刻已经用了多少"。
        let usage = guard.usage();

        let duration = started.elapsed();
        let (stdout, stderr, late_overflow) = output.into_parts();

        match outcome {
            WaitOutcome::Overflow(stream) => Err(CommandDenied::TooLong {
                stream,
                limit_bytes: limit,
            }),
            // 轮询判定超限：与超时同一处理路径（杀树已经在上面做了），但错误原因不同——
            // "被限额杀掉"和"跑太久"对调用方是两件事。
            WaitOutcome::Limited(violation) => Err(violation.into_denied()),
            WaitOutcome::TimedOut => Ok(CommandOutput {
                status: CommandStatus::TimedOut,
                stdout,
                stderr,
                duration,
                usage,
            }),
            WaitOutcome::Exited(status) => {
                // 子进程刚好在超限那一刻退出：字节确实超了，照样按 TooLong 报，
                // 不能因为"它自己退了"就把超限当成功。
                if let Some(stream) = late_overflow {
                    return Err(CommandDenied::TooLong {
                        stream,
                        limit_bytes: limit,
                    });
                }
                // 写隔离是在子进程的 `pre_exec` 里装的（fork 之后、exec 之前）。装不上时
                // 子进程以哨兵码 127 退出并在 stderr 留一行标记 —— 那不是"命令自己失败了"，
                // 而是"这台机器没装上隔离"。必须翻译成 `IsolationUnavailable`（fail-closed）：
                // 绝不能把它当成一次普通的非零退出交给上层，否则调用方会以为"命令跑过了"。
                #[cfg(not(windows))]
                if confinement.is_some() {
                    if let Some(reason) =
                        crate::landlock::isolation_failure_reason(status.code(), &stderr)
                    {
                        tracing::warn!(
                            program = %program_path.display(),
                            %reason,
                            "子进程未能装载写隔离，按隔离不可用拒绝",
                        );
                        return Err(CommandDenied::IsolationUnavailable {
                            platform: self.platform,
                            reason,
                        });
                    }
                }
                // Linux 的 CPU 上限由 `RLIMIT_CPU` 直接生效：内核到点发 SIGXCPU，进程就此
                // 退出。**必须**把 `Exited { signal: 24 }` 翻译回 `CpuLimit`，否则调用方只看到
                // "被信号 24 杀了"，得自己去查信号表才知道撞的是我们设的上限。
                #[cfg(target_os = "linux")]
                if let Some(limit_ms) = guard.cpu_kill_hint(status.signal()) {
                    tracing::warn!(
                        program = %program_path.display(),
                        used_ms = usage.cpu_ms,
                        limit_ms,
                        "子进程被 RLIMIT_CPU 杀掉（SIGXCPU），按 CPU 上限拒绝",
                    );
                    return Err(CommandDenied::CpuLimit {
                        used_ms: usage.cpu_ms,
                        limit_ms,
                    });
                }
                // 收尾再判一次：轮询最后一次采样之后到进程真正退出之间，用量可能刚好越线
                // （尤其是 I/O 累计量与 Windows 的 CPU 记账）。漏了这一下会把"超限了但它自己
                // 先退了"当成成功——那是最不该给调用方的假话。
                if let Some(violation) = guard.violation(&spec.limits) {
                    return Err(violation.into_denied());
                }
                Ok(CommandOutput {
                    status: map_status(status),
                    stdout,
                    stderr,
                    duration,
                    usage,
                })
            }
        }
    }
}

/// 等待结束的几种情形。
enum WaitOutcome {
    /// 自己退出了（可能是正常退出，也可能是被内核按上限杀掉——看退出码/信号）。
    Exited(Exit),
    /// 等到了墙钟超时。
    TimedOut,
    /// 输出超过 `max_output_bytes`。
    Overflow(OutputStream),
    /// 轮询发现突破了资源上限（Windows 的 CPU / 内存 / I/O）。
    Limited(LimitViolation),
}

fn wait_for_finish(
    child: &mut Spawned,
    guard: &Containment,
    output: &BoundedOutput,
    timeout: Duration,
    limits: &ResourceLimits,
) -> Result<WaitOutcome, CommandDenied> {
    let started = Instant::now();
    loop {
        if let Some(stream) = output.overflow_stream() {
            return Ok(WaitOutcome::Overflow(stream));
        }
        // 限额判定和等待共用这一个 10 ms 循环（不另开线程）：多一次 `QueryInformationJobObject`
        // 是几十微秒，比一次 `try_wait` 还便宜。判定放在 `try_wait` **之前**，是为了让
        // "刚好撞上限又刚好退出"的命令报 CpuLimit 而不是 Exited。
        if let Some(violation) = guard.violation(limits) {
            return Ok(WaitOutcome::Limited(violation));
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(WaitOutcome::Exited(status)),
            Ok(None) => {}
            Err(err) => {
                let _ = guard.kill_tree(child);
                return Err(CommandDenied::IoFailed {
                    context: "等待子进程结束".to_owned(),
                    reason: err.to_string(),
                });
            }
        }
        if started.elapsed() >= timeout {
            return Ok(WaitOutcome::TimedOut);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn validate_spec(spec: &CommandSpec) -> Result<(), CommandDenied> {
    if spec.timeout.is_zero() {
        return Err(CommandDenied::InvalidSpec {
            reason: "超时必须大于 0（0 会让命令立刻被判超时）".to_owned(),
        });
    }
    if spec.timeout > MAX_TIMEOUT {
        return Err(CommandDenied::InvalidSpec {
            reason: format!("超时不能超过 {} 分钟", MAX_TIMEOUT.as_secs() / 60),
        });
    }
    if spec.max_output_bytes == 0 {
        return Err(CommandDenied::InvalidSpec {
            reason: "输出上限必须大于 0".to_owned(),
        });
    }
    if spec.max_output_bytes > MAX_OUTPUT_BYTES {
        return Err(CommandDenied::InvalidSpec {
            reason: format!("输出上限不能超过 {} MiB", MAX_OUTPUT_BYTES / (1024 * 1024)),
        });
    }
    // 限额的**合法性**（`Some(0)` 之类）在这里判；平台**支持性**在 `require_isolation` 里判
    // （那条依赖这台机器，不依赖规格本身）。
    limits::validate(&spec.limits)?;
    program::validate_args(&spec.args)
}

fn map_status(exit: Exit) -> CommandStatus {
    CommandStatus::Exited {
        code: exit.code(),
        signal: exit.signal(),
    }
}

/// 起子进程：写隔离档走原生 `CreateProcessAsUserW`（要挂受限令牌），其余走
/// [`std::process::Command`]。
///
/// 两条路的**外部行为必须一致**：`current_dir`、清空环境后重设、stdin=/dev/null、
/// stdout/stderr 管道。差异只在"用谁的令牌、继承哪些句柄"，以及控制台的处理——
/// 受限令牌那一档不能要控制台（实测 `CREATE_NO_WINDOW` 会让子进程 `0xC0000142`，
/// 见 `confinement_win.rs` 的 `CONFINED_CREATION_FLAGS`），所以两边旗标本来就不该共用。
fn start(
    program_path: &Path,
    spec: &CommandSpec,
    cwd: &Path,
    env_pairs: &[(String, String)],
    guard: &Containment,
    confinement: Option<&WriteConfinement>,
) -> Result<Spawned, CommandDenied> {
    #[cfg(windows)]
    if let Some(confinement) = confinement {
        // 受限令牌下 `std::process::Command` 无计可施（没有设置令牌/属性表的接口），
        // 所以这里自己拼 `CreateProcessAsUserW` 的全部参数。
        let plan = SpawnPlan {
            application: program_path.to_path_buf(),
            args: spec.args.clone(),
            cwd: cwd.to_path_buf(),
            env: env_pairs.to_vec(),
        };
        return confinement
            .spawn(&plan)
            .map(Spawned::native)
            .map_err(|err| CommandDenied::SpawnFailed {
                program: program_path.display().to_string(),
                reason: err.to_string(),
            });
    }

    let mut command = Command::new(program_path);
    command
        .args(&spec.args)
        .current_dir(cwd)
        .env_clear()
        .envs(env_pairs.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    guard.configure(&mut command);
    #[cfg(not(windows))]
    if let Some(confinement) = confinement {
        // Linux：Landlock 规则集在 `pre_exec` 里装上（fork 后、exec 前）。
        confinement.configure(&mut command);
    }
    command
        .spawn()
        .map(Spawned::std)
        .map_err(|err| CommandDenied::SpawnFailed {
            program: program_path.display().to_string(),
            reason: err.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DenyAll;

    impl IsolationProbe for DenyAll {
        fn probe(&self, level: IsolationLevel) -> Result<(), String> {
            Err(format!("{level} 在测试里被注入为不可用"))
        }
    }

    fn danger_spec() -> CommandSpec {
        let mut spec = CommandSpec::new("definitely-not-a-real-program", std::env::temp_dir());
        spec.sandbox = SandboxMode::Danger;
        spec.timeout = Duration::from_secs(1);
        spec
    }

    #[test]
    fn injected_unavailable_isolation_denies_before_looking_at_the_program() {
        let runner = PlatformRunner::with_probe(Arc::new(DenyAll));
        // 程序名故意不存在：如果实现先去解析程序，这里会拿到 ProgramNotFound 而不是隔离拒绝。
        let denied = runner.run(danger_spec()).expect_err("必须拒绝");
        assert!(
            matches!(denied, CommandDenied::IsolationUnavailable { .. }),
            "{denied:?}"
        );
    }

    #[test]
    fn read_only_sandbox_is_denied_immediately() {
        let runner = PlatformRunner::with_probe(Arc::new(DenyAll));
        let mut spec = danger_spec();
        spec.sandbox = SandboxMode::ReadOnly;
        assert!(matches!(
            runner.run(spec),
            Err(CommandDenied::SandboxDenied { .. })
        ));
    }

    #[test]
    fn zero_timeout_is_invalid() {
        let mut spec = CommandSpec::new("cargo", std::env::temp_dir());
        spec.timeout = Duration::ZERO;
        assert!(matches!(
            validate_spec(&spec),
            Err(CommandDenied::InvalidSpec { .. })
        ));
    }

    #[test]
    fn zero_output_limit_is_invalid() {
        let mut spec = CommandSpec::new("cargo", std::env::temp_dir());
        spec.max_output_bytes = 0;
        assert!(matches!(
            validate_spec(&spec),
            Err(CommandDenied::InvalidSpec { .. })
        ));
    }

    #[test]
    fn invalid_limits_are_rejected_before_isolation_is_consulted() {
        // 顺序契约：规格校验在隔离探测之前。`process_count = 0` 是作业对象的保留值
        // （表示"不限"），照抄进去等于把限制关掉，所以必须报 `InvalidSpec`，
        // 而不是被注入的 DenyAll 探测先拒成 `IsolationUnavailable`。
        let runner = PlatformRunner::with_probe(Arc::new(DenyAll));
        let mut spec = danger_spec();
        spec.limits.process_count = Some(0);
        let denied = runner.run(spec).expect_err("必须拒绝");
        assert!(
            matches!(denied, CommandDenied::InvalidSpec { .. }),
            "{denied:?}"
        );
        assert_eq!(denied.code(), "HBOT_COMMAND_INVALID_SPEC");
    }

    #[test]
    fn default_limits_pass_spec_validation() {
        // 不请求限额时必须与改动前完全一致（既有行为不变）。
        assert_eq!(validate_spec(&danger_spec()), Ok(()));
    }
}
