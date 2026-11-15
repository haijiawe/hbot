//! [`PlatformRunner`]：[`hbot_core::ports::CommandRunner`] 的实现（同步、阻塞）。
//!
//! 一次调用的顺序是固定的，**任何一步不满足就拒绝，不跳过**：
//!
//! 1. 校验规格（超时、输出上限、参数规模）；
//! 2. 把 `SandboxMode` 映射成需要的隔离层；`read-only` 直接拒绝；
//! 3. **探测**该层是否可用——不可用就 [`hbot_core::CommandDenied::IsolationUnavailable`]；
//! 4. 解析程序（绝对路径 + 允许清单）与工作目录（必须在项目根内）；
//! 5. 构造环境（清空 + 白名单）；
//! 6. 建隔离范围 → spawn → **立刻**把子进程放进范围（放不进去就杀掉并拒绝）；
//! 7. 有界读输出，轮询等退出；超时/超限 → 杀整棵树；
//! 8. 无论怎么结束都收掉整棵树（不留孤儿），再组装结果。

use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hbot_core::{
    CommandDenied, CommandOutput, CommandRunner, CommandSpec, CommandStatus, OutputStream,
    Platform, SandboxMode,
};

use crate::containment::Containment;
use crate::output::BoundedOutput;
use crate::probe::{CapabilityReport, IsolationLevel, IsolationProbe, SystemProbe};
use crate::{env, path, program};

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

        // 先建隔离范围：建不出来就不 spawn——绝不允许"先跑起来再想办法"。
        let guard =
            Containment::create().map_err(|reason| CommandDenied::IsolationUnavailable {
                platform: self.platform,
                reason,
            })?;

        let mut command = Command::new(&program_path);
        command
            .args(&spec.args)
            .current_dir(&cwd)
            .env_clear()
            .envs(env_pairs.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        guard.configure(&mut command);

        let started = Instant::now();
        let mut child = command.spawn().map_err(|err| CommandDenied::SpawnFailed {
            program: program_path.display().to_string(),
            reason: err.to_string(),
        })?;

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

        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = guard.kill_tree(&mut child);
            let _ = child.wait();
            return Err(CommandDenied::IoFailed {
                context: "接管子进程输出管道".to_owned(),
                reason: "spawn 之后拿不到管道（标准输出/错误未按 piped 建立）".to_owned(),
            });
        };

        let limit = spec.max_output_bytes;
        let output = BoundedOutput::spawn(stdout, stderr, limit);
        let outcome = wait_for_finish(&mut child, &guard, &output, spec.timeout)?;

        // 不论正常退出、超时还是超限，都把整棵树收掉：命令返回就不该留下后代进程。
        let _ = guard.kill_tree(&mut child);
        let _ = child.wait();
        output.wait_closed(READER_GRACE);

        let duration = started.elapsed();
        let (stdout, stderr, late_overflow) = output.into_parts();

        match outcome {
            WaitOutcome::Overflow(stream) => Err(CommandDenied::TooLong {
                stream,
                limit_bytes: limit,
            }),
            WaitOutcome::TimedOut => Ok(CommandOutput {
                status: CommandStatus::TimedOut,
                stdout,
                stderr,
                duration,
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
                Ok(CommandOutput {
                    status: map_status(status),
                    stdout,
                    stderr,
                    duration,
                })
            }
        }
    }
}

/// 等待结束的三种情形。
enum WaitOutcome {
    Exited(ExitStatus),
    TimedOut,
    Overflow(OutputStream),
}

fn wait_for_finish(
    child: &mut Child,
    guard: &Containment,
    output: &BoundedOutput,
    timeout: Duration,
) -> Result<WaitOutcome, CommandDenied> {
    let started = Instant::now();
    loop {
        if let Some(stream) = output.overflow_stream() {
            return Ok(WaitOutcome::Overflow(stream));
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
    program::validate_args(&spec.args)
}

fn map_status(status: ExitStatus) -> CommandStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        CommandStatus::Exited {
            code: status.code(),
            signal: status.signal(),
        }
    }
    #[cfg(not(unix))]
    {
        CommandStatus::Exited {
            code: status.code(),
            signal: None,
        }
    }
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
}
