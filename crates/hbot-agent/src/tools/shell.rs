//! `run_command`：把一次工具调用翻译成 [`CommandSpec`]，再交给
//! [`CommandRunner`] 执行（真实实现在 `hbot-command`）。
//!
//! # 各沙箱档的确切行为
//!
//! | 沙箱档 | 结果 |
//! |---|---|
//! | `read-only` | **本文件直接拒**，一个进程都不起（不依赖 runner 报错） |
//! | `workspace-write` | runner 拒：三平台都没有写隔离（`IsolationUnavailable`）→ `denied` |
//! | `danger` | 真跑，但要求进程树约束（Containment）：Windows Job Object / Linux `setsid`+`killpg` / macOS 无 |
//!
//! `workspace-write` 的拒绝是 **fail-closed**：`docs/adr/0003-command-isolation.md` §2.2
//! 规定"建不出这台机器要求的隔离就拒绝"，不许退化成名字白名单。只读档的拒绝放在
//! **调 runner 之前**：网关按 [`Tool::min_sandbox`] 已经拒过一次，但工具可能被直接调用
//! （测试、子代理、未来的批量执行），只有提前拒才能保证"只读档根本没 spawn 进程"。
//!
//! # 程序清单（`allowed_programs`）
//!
//! 清单由**调用方**给：[`RunCommandTool::with_allowed_programs`]。默认空清单 =
//! 一条命令都不许跑（fail-closed），理由见 ADR §5——策略层内置一张"安全命令表"
//! 正是旧项目踩过的坑，而任何**有用**的内置表都必然要写 `cmd`/`sh` 这种解释器。
//!
//! # 时间预算
//!
//! 参数 `timeout_ms` 上限是 [`MAX_COMMAND_TIMEOUT_MS`]（等于 [`CommandSpec::new`] 的默认值）；
//! 超过就 `-32002 invalid`，**不夹紧**——夹紧会让模型以为设了 5 分钟、实际 30 秒被杀。
//! [`Tool::timeout`] 刻意比它多留 [`TOOL_TIMEOUT_GRACE`]，让 runner 自己的超时先到：
//! 那样能拿到 `CommandStatus::TimedOut` 与已收集的部分输出，而不是网关的
//! `tokio::time::timeout` 兜底（那会是 `-32004`，且丢掉部分输出）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hbot_core::command::{CommandDenied, CommandOutput, CommandSpec, CommandStatus};
use hbot_core::{HbotError, SandboxMode};
use serde_json::{Value, json};

use super::{
    MAX_TOOL_OUTPUT_BYTES, Risk, Tool, ToolCtx, ToolOutput, blocking, clamp_output, required_str,
};

/// 命令工具**没有装配执行器**时的统一解释（也用于审计/日志）。
///
/// 刻意不说"平台不支持"：缺 `CommandRunner` 是装配问题（组合根该补），
/// 平台缺隔离才是能力问题（换机器才有用）。两句话混在一起，排障时就会找错方向。
pub const COMMAND_UNAVAILABLE: &str =
    "没有装配命令执行器（CommandRunner 未注入），run_command 当前不可用";

/// 只读档的拒绝理由。
pub const READ_ONLY_DENIED: &str = "当前沙箱档 read-only 不允许执行任何命令（未启动任何进程）";

/// 单次命令的 `timeout_ms` 上限，等于 [`CommandSpec::new`] 的默认超时。
///
/// 不给工具层留"比默认更宽"的旋钮：长任务该拆成多次调用或走后台任务。
pub const MAX_COMMAND_TIMEOUT_MS: u64 = 30_000;

/// 网关侧硬超时相对命令自身超时的余量。
///
/// 两者相等时先到的是 `tokio::time::timeout`（`-32004`，丢部分输出）；多这 10 秒，
/// 正常路径永远是 runner 先返回 `CommandStatus::TimedOut`。
const TOOL_TIMEOUT_GRACE: Duration = Duration::from_secs(10);

/// 交给 runner 的每流字节上限。
///
/// 比 [`MAX_TOOL_OUTPUT_BYTES`] 宽 4 倍（正好等于 [`CommandSpec::new`] 的默认值）：
/// runner 先按内存上限兜住并杀掉进程树，工具层再把**回填给模型**的文本夹到 64 KiB，
/// 并在结果里说清截断到了多少字节。
const MAX_COMMAND_OUTPUT_BYTES: usize = MAX_TOOL_OUTPUT_BYTES * 4;

/// 在项目内执行命令的工具。
///
/// `allowed_programs` 是**实例字段**而不是全局常量：清单是装配处的决定，同一进程里
/// 不同会话（例如"只许 git"的子代理）可以有不同清单。
#[derive(Clone, Debug)]
pub struct RunCommandTool {
    /// 允许跑的程序：裸文件名（按 PATH 解析）或绝对路径；空 = 全拒。
    allowed_programs: Arc<[String]>,
}

impl Default for RunCommandTool {
    fn default() -> Self {
        Self::new()
    }
}

impl RunCommandTool {
    /// 空清单：任何命令都会被 `ProgramNotAllowed` 拒掉。
    ///
    /// 这是 [`crate::tools::ToolRegistry::default_tools`] 用的构造：默认不许跑命令，
    /// 要跑必须由装配处显式给清单。
    pub fn new() -> Self {
        Self {
            allowed_programs: Vec::new().into(),
        }
    }

    /// 带程序清单构造。
    ///
    /// 条目可以是裸文件名（`"git"`，按子进程的 PATH 解析）或绝对路径。空清单是允许的，
    /// 但语义是"一条都不许跑"——不提供"空 = 不限"这种危险默认。
    pub fn with_allowed_programs<I, S>(programs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            allowed_programs: programs
                .into_iter()
                .map(Into::into)
                .collect::<Vec<_>>()
                .into(),
        }
    }

    /// 当前程序清单（空切片 = 全拒）。
    pub fn allowed_programs(&self) -> &[String] {
        &self.allowed_programs
    }
}

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        "在项目内以 argv 形式执行一条允许清单内的命令（不经 shell）。\
         只读档直接拒；workspace-write 档当前一律拒绝（缺写隔离）；\
         danger 档在进程树约束下真跑。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "可执行文件名，如 cargo"
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "参数数组（不经 shell）"
                },
                "cwd": {
                    "type": "string",
                    "description": "工作区相对工作目录，默认根"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "超时毫秒数，默认 30000，上限也是 30000"
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn risk(&self) -> Risk {
        Risk::Command
    }

    /// 比命令自身超时多留余量，让 runner 的超时先到（见模块注释）。
    fn timeout(&self) -> Duration {
        Duration::from_millis(MAX_COMMAND_TIMEOUT_MS) + TOOL_TIMEOUT_GRACE
    }

    async fn run(&self, args: Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, HbotError> {
        let program = required_str("run_command", &args, "command")?
            .trim()
            .to_owned();
        if program.is_empty() {
            return Err(HbotError::invalid(
                "工具 `run_command` 的参数 `command` 不能为空",
            ));
        }
        let spec_args = parse_args(&args)?;
        let timeout = parse_timeout(&args)?;
        let cwd = parse_cwd(&args, ctx.workspace.root())?;

        // 只读档：**调 runner 之前**就拒。网关的 min_sandbox 判据已经拒过一次，
        // 这里再拒是为了"工具被直接调用"的路径（测试 / 子代理 / 批量执行）。
        if ctx.sandbox == SandboxMode::ReadOnly {
            return Err(HbotError::denied(READ_ONLY_DENIED));
        }

        // 装配缺失也是拒绝，理由与"平台不支持"分开（见 COMMAND_UNAVAILABLE）。
        let Some(runner) = ctx.command.clone() else {
            return Err(HbotError::denied(COMMAND_UNAVAILABLE));
        };

        let spec = self.build_spec(program, spec_args, cwd, timeout, ctx);

        // runner 是同步阻塞实现（`CommandRunner::run`），必须搬出 async 执行器。
        let outcome = blocking(move || Ok::<_, HbotError>(runner.run(spec))).await?;
        match outcome {
            Ok(output) => Ok(render_output(output, ctx.sandbox, timeout)),
            Err(denied) => denied_to_result(denied, ctx.sandbox),
        }
    }
}

impl RunCommandTool {
    /// 把工具参数翻译成 [`CommandSpec`]。
    ///
    /// `cwd` 一律挂在项目根下再交给 runner：绝对路径会覆盖根（runner 的 cwd 守卫
    /// 随后按项目根判越界），`..` 逃逸同样被那道守卫拒掉——工具层不做自己的路径判据，
    /// 免得两处规则漂移。
    fn build_spec(
        &self,
        program: String,
        args: Vec<String>,
        cwd: PathBuf,
        timeout: Duration,
        ctx: &ToolCtx<'_>,
    ) -> CommandSpec {
        let mut spec = CommandSpec::new(program, ctx.workspace.root().to_path_buf());
        spec.args = args;
        spec.cwd = cwd;
        spec.timeout = timeout;
        spec.max_output_bytes = MAX_COMMAND_OUTPUT_BYTES;
        spec.sandbox = ctx.sandbox;
        spec.allowed_programs = self.allowed_programs.to_vec();
        // 子进程要能按 PATH 找到程序、要 SystemRoot 才起得来（仍是白名单继承，
        // 不是 `inherit_entire_env`——那一项 runner 直接拒）。
        spec.inherit_system_env();
        spec
    }
}

/// 解析 `cwd` 参数：相对路径挂在项目根下，缺省是项目根。
///
/// **越界交给 runner 的 cwd 守卫判**（绝对路径覆盖根、`..` 逃逸都过不去），
/// 工具层不再写第二份 is_within 判据——两处规则必然漂移。
fn parse_cwd(args: &Value, root: &Path) -> Result<PathBuf, HbotError> {
    match args.get("cwd") {
        None | Some(Value::Null) => Ok(root.to_path_buf()),
        Some(Value::String(raw)) if raw.trim().is_empty() => Ok(root.to_path_buf()),
        Some(Value::String(raw)) => Ok(root.join(raw)),
        Some(_) => Err(HbotError::invalid(
            "工具 `run_command` 的参数 `cwd` 必须是字符串",
        )),
    }
}

/// 解析 `args`：必须是字符串数组；缺省为空。
fn parse_args(args: &Value) -> Result<Vec<String>, HbotError> {
    let invalid = || HbotError::invalid("工具 `run_command` 的参数 `args` 必须是字符串数组");
    match args.get("args") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or_else(invalid))
            .collect(),
        Some(_) => Err(invalid()),
    }
}

/// 解析 `timeout_ms`：缺省 30000；上限 [`MAX_COMMAND_TIMEOUT_MS`]；`0`/负数/非整数都拒。
fn parse_timeout(args: &Value) -> Result<Duration, HbotError> {
    let Some(raw) = args.get("timeout_ms").filter(|value| !value.is_null()) else {
        return Ok(Duration::from_millis(MAX_COMMAND_TIMEOUT_MS));
    };
    let Some(ms) = as_millis(raw) else {
        return Err(HbotError::invalid(
            "工具 `run_command` 的参数 `timeout_ms` 必须是非负整数毫秒",
        ));
    };
    if ms == 0 {
        return Err(HbotError::invalid(
            "工具 `run_command` 的参数 `timeout_ms` 必须大于 0",
        ));
    }
    if ms > MAX_COMMAND_TIMEOUT_MS {
        return Err(HbotError::invalid(format!(
            "工具 `run_command` 的参数 `timeout_ms` 不能超过 {MAX_COMMAND_TIMEOUT_MS} ms（长任务请拆成多次调用或走后台任务）"
        )));
    }
    Ok(Duration::from_millis(ms))
}

/// 接受整数与"整数值的浮点"（参数闸门允许 `10.0` 这种写法，这里不能两套判据）。
fn as_millis(value: &Value) -> Option<u64> {
    if let Some(ms) = value.as_u64() {
        return Some(ms);
    }
    let float = value.as_f64()?;
    if float.is_finite() && float >= 0.0 && float.fract() == 0.0 && float <= u64::MAX as f64 {
        Some(float as u64)
    } else {
        None
    }
}

/// `CommandDenied` → 工具结果/错误的唯一映射。
///
/// 分工：
/// - **能力/策略拒绝** → `denied`（-32005）：清单、档位、路径、隔离。重试同一个动作没意义。
/// - **规格不合法** → `invalid`（-32002）：模型改参数就能重来。
/// - **环境故障** → `internal`（-32006）：拉不起进程、回收失败，不是模型能修的。
/// - **输出超限** → `Ok(ToolOutput::error)`：命令**真的跑了**并因超限被杀，记成
///   "denied" 会是假话（审计里会污染审批率），记成 internal 又会误导排障。
fn denied_to_result(denied: CommandDenied, sandbox: SandboxMode) -> Result<ToolOutput, HbotError> {
    let message = denied.to_string();
    match denied {
        // 超限：不返回半截输出（runner 不提供），只告诉上限与怎么改。
        CommandDenied::TooLong { limit_bytes, .. } => Ok(ToolOutput::error(format!(
            "命令输出超过 {limit_bytes} 字节上限，已终止整棵进程树并丢弃本次输出；\
             请减小输出量（加 --quiet、只打印摘要、或重定向到项目内文件）后重试。"
        ))),
        // 资源限额：与 `TooLong` 同一分类 —— 命令**真的跑了**并因超限被杀，
        // 记成 denied 会是假话（污染审批率），记成 internal 又会误导排障。
        // 模型能做的是"少用一点资源"，所以把上限与真实用量都告诉它。
        CommandDenied::CpuLimit { .. }
        | CommandDenied::MemoryLimit { .. }
        | CommandDenied::IoLimit { .. }
        | CommandDenied::ProcessLimit { .. } => Ok(ToolOutput::error(format!(
            "命令触到资源上限，已终止整棵进程树并丢弃本次输出：{message}\
             请减少资源占用（缩小输入、分批处理、别起常驻子进程）后重试。"
        ))),
        CommandDenied::IsolationUnavailable { .. } => Err(HbotError::denied(format!(
            "命令未执行：当前沙箱档 {} 需要{}，但这台机器建立不了。{message}",
            sandbox.as_str(),
            required_isolation_zh(sandbox),
        ))),
        CommandDenied::ProgramNotAllowed { .. }
        | CommandDenied::SandboxDenied { .. }
        | CommandDenied::OutsideWorkspace { .. }
        | CommandDenied::InheritEnvRejected { .. } => Err(HbotError::denied(message)),
        CommandDenied::InvalidSpec { .. } | CommandDenied::ProgramNotFound { .. } => {
            Err(HbotError::invalid(message))
        }
        CommandDenied::SpawnFailed { .. } | CommandDenied::IoFailed { .. } => {
            Err(HbotError::internal(message))
        }
    }
}

/// 该沙箱档要求哪一级隔离（只在错误消息里用）。
fn required_isolation_zh(sandbox: SandboxMode) -> &'static str {
    match sandbox {
        SandboxMode::ReadOnly => "「不执行命令」（只读档不跑任何进程）",
        SandboxMode::WorkspaceWrite => "「写隔离」（WriteConfinement）",
        SandboxMode::Danger => "「进程树约束」（Containment）",
    }
}

/// `CommandOutput` → 工具输出。
///
/// 超时**不**映射成 `-32004`：命令确实跑了、进程树确实被杀了，这和"网关等不到工具返回"
/// 是两件事；模型需要看到超时上限与已收集的部分输出，才能决定缩短命令还是拆开跑。
fn render_output(output: CommandOutput, sandbox: SandboxMode, timeout: Duration) -> ToolOutput {
    let mut content = String::new();
    content.push_str(&format!("沙箱档：{}\n", sandbox.as_str()));
    // 只有"正常退出且退出码 0"才算成功；超时、被信号杀、无退出码都算工具失败。
    let is_error = !matches!(output.status, CommandStatus::Exited { code: Some(0), .. });
    match output.status {
        CommandStatus::TimedOut => {
            content.push_str(&format!(
                "结束：**超时**（上限 {} ms），整棵进程树已被终止；下面是已收集到的部分输出\n",
                timeout.as_millis()
            ));
        }
        CommandStatus::Exited { code, signal } => {
            content.push_str(&format!("耗时：{} ms\n", output.duration.as_millis()));
            match (code, signal) {
                (Some(code), _) => content.push_str(&format!("结束：退出码 {code}\n")),
                (None, Some(signal)) => {
                    content.push_str(&format!("结束：被信号 {signal} 终止（无退出码）\n"));
                }
                (None, None) => content.push_str("结束：无退出码（进程可能被外部终止）\n"),
            }
        }
    }
    content.push_str(&format!(
        "--- stdout（{} 字节）---\n{}\n",
        output.stdout.len(),
        output.stdout
    ));
    content.push_str(&format!(
        "--- stderr（{} 字节）---\n{}",
        output.stderr.len(),
        output.stderr
    ));

    let (mut content, truncated) = clamp_output(content);
    if truncated {
        // clamp_output 只加"已截断"，这里补上确切字节数：模型据此决定怎么缩小输出。
        content.push_str(&format!(
            "\n（回填内容已截断到 {MAX_TOOL_OUTPUT_BYTES} 字节；完整输出未保留，请减小输出量后重试）"
        ));
    }
    ToolOutput {
        content,
        truncated,
        is_error,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    // 只在 Windows 那条 `timeout_kills_the_whole_process_tree` 里用：`Instant` 放在模块级
    // 会让 Linux 的 `-D unused-imports` 红（clippy 在 CI 的 ubuntu 上就是这么抓到的）。
    #[cfg(windows)]
    use std::time::Instant;

    use hbot_command::{CapabilityReport, IsolationLevel, IsolationProbe, PlatformRunner};
    use hbot_core::{CommandDenied, CommandRunner, CommandSpec, CommandStatus, OutputStream};
    use serde_json::json;

    use super::*;
    use crate::cancel::CancellationToken;
    use crate::test_support::{TestDir, session_id};

    /// 假执行器：记录调用次数与最后一次 `CommandSpec`，返回预置结果。
    struct FakeRunner {
        calls: Arc<AtomicUsize>,
        last_spec: Arc<Mutex<Option<CommandSpec>>>,
        outcome: Result<CommandOutput, CommandDenied>,
    }

    impl FakeRunner {
        fn new(outcome: Result<CommandOutput, CommandDenied>) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                last_spec: Arc::new(Mutex::new(None)),
                outcome,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn last_spec(&self) -> CommandSpec {
            self.last_spec
                .lock()
                .expect("spec lock")
                .clone()
                .expect("runner 应当收到 spec")
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, spec: CommandSpec) -> Result<CommandOutput, CommandDenied> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut guard) = self.last_spec.lock() {
                *guard = Some(spec);
            }
            self.outcome.clone()
        }
    }

    fn exited(code: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            status: CommandStatus::Exited {
                code: Some(code),
                signal: None,
            },
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            duration: Duration::from_millis(7),
            usage: Default::default(),
        }
    }

    /// 跑一次工具；`dir` 的根就是工作区根。
    async fn run_with(
        dir: &TestDir,
        sandbox: SandboxMode,
        runner: Option<Arc<dyn CommandRunner>>,
        args: Value,
    ) -> Result<ToolOutput, HbotError> {
        run_tool(&RunCommandTool::new(), dir, sandbox, runner, args).await
    }

    async fn run_tool(
        tool: &RunCommandTool,
        dir: &TestDir,
        sandbox: SandboxMode,
        runner: Option<Arc<dyn CommandRunner>>,
        args: Value,
    ) -> Result<ToolOutput, HbotError> {
        let cancel = CancellationToken::new();
        let ctx = ToolCtx {
            workspace: dir.workspace(),
            sandbox,
            cancel: &cancel,
            session_id: session_id(),
            call_id: "call",
            edits: None,
            command: runner,
            device: None,
        };
        tool.run(args, &ctx).await
    }

    #[tokio::test]
    async fn run_command_denies_and_never_pretends_success() {
        let dir = TestDir::new("run-command");
        // 即使最宽的沙箱档，没有装配执行器也不许伪造成功。
        let error = run_with(
            &dir,
            SandboxMode::Danger,
            None,
            json!({ "command": "cargo" }),
        )
        .await
        .expect_err("must deny");
        assert!(matches!(error, HbotError::Denied { .. }));
        assert!(error.to_string().contains(COMMAND_UNAVAILABLE));
    }

    #[test]
    fn run_command_schema_is_declared() {
        let tool = RunCommandTool::new();
        let schema = tool.schema();
        assert_eq!(schema["required"][0], "command");
        assert_eq!(tool.risk(), Risk::Command);
        assert_eq!(tool.min_sandbox(), SandboxMode::WorkspaceWrite);
        assert!(tool.allowed_programs().is_empty(), "默认必须是没有清单");
    }

    #[tokio::test]
    async fn missing_runner_says_not_assembled_rather_than_unsupported() {
        let dir = TestDir::new("run-command-missing");
        let error = run_with(
            &dir,
            SandboxMode::Danger,
            None,
            json!({ "command": "cargo" }),
        )
        .await
        .expect_err("没有执行器必须拒");
        assert_eq!(error.code(), -32005);
        let message = error.to_string();
        assert!(
            message.contains("未装配") || message.contains("没有装配"),
            "{message}"
        );
        assert!(
            !message.contains("平台不支持"),
            "不能把装配缺失说成平台不支持：{message}"
        );
    }

    #[tokio::test]
    async fn read_only_is_denied_before_the_runner_is_called() {
        let dir = TestDir::new("run-command-read-only");
        let runner = Arc::new(FakeRunner::new(Ok(exited(0, "should not run", ""))));
        let dynamic: Arc<dyn CommandRunner> = runner.clone();
        let error = run_with(
            &dir,
            SandboxMode::ReadOnly,
            Some(dynamic),
            json!({ "command": "cargo", "args": ["test"] }),
        )
        .await
        .expect_err("只读档必须拒");
        assert!(matches!(error, HbotError::Denied { .. }));
        assert_eq!(error.code(), -32005);
        assert_eq!(runner.calls(), 0, "只读档不许碰执行器（等于没 spawn 进程）");
        assert!(error.to_string().contains("read-only"));
    }

    /// workspace-write 的真实行为**由这台机器装不装得出写隔离决定**，不能钉死成"一律拒"。
    ///
    /// 2026-11-16 之前 `hbot-command` 只有进程树 Containment、没有写隔离，于是这一档
    /// 恒报 `IsolationUnavailable`；`WriteConfinement` 落地后（Windows：受限令牌 +
    /// 工作区 write SID + ACE）这条通路会**真的往下走**，于是同样的输入变成"拒在允许
    /// 清单上"（本用例用的是空清单）。所以断言跟随能力矩阵分叉：
    /// - 装得出写隔离 → 必须不报 `IsolationUnavailable`，且拒在允许清单（证明它没被
    ///   隔离闸挡住，隔离那一层真的通了）；
    /// - 装不出 → 仍然 `IsolationUnavailable` + 「命令未执行」（fail-closed 没松）。
    #[tokio::test]
    async fn workspace_write_follows_this_machines_write_isolation_capability() {
        let dir = TestDir::new("run-command-workspace-write");
        let confinement_available =
            CapabilityReport::system().supports(SandboxMode::WorkspaceWrite);
        let runner: Arc<dyn CommandRunner> = Arc::new(PlatformRunner::system());
        let error = run_with(
            &dir,
            SandboxMode::WorkspaceWrite,
            Some(runner),
            json!({ "command": "cargo", "args": ["--version"] }),
        )
        .await
        .expect_err("空允许清单下没有任何命令能跑");
        assert!(matches!(error, HbotError::Denied { .. }));
        assert_eq!(error.code(), -32005);
        let message = error.to_string();
        if confinement_available {
            assert!(
                !message.contains("建立不了"),
                "这台机器装得出写隔离，不该报 IsolationUnavailable：{message}"
            );
            assert!(
                message.contains("不在本次允许清单内"),
                "写隔离可用时应当拒在允许清单上（隔离层已放行），实际：{message}"
            );
        } else {
            assert!(message.contains("写隔离"), "{message}");
            assert!(message.contains("命令未执行"), "{message}");
        }
    }

    /// 探测失败必须 fail-closed，而且必须**先拒在隔离闸上**。
    ///
    /// 注入一个恒失败的 `IsolationProbe`，再用一个根本不存在的程序名：如果实现是"先解析
    /// 程序、再看隔离"，这里会变成 -32002「找不到可执行文件」（`invalid`，模型会以为换个
    /// 程序名就能重来）；只有隔离闸在前，才会是 -32005「命令未执行」。
    #[tokio::test]
    async fn failing_write_isolation_probe_denies_before_looking_at_the_program() {
        struct DenyAllProbe;

        impl IsolationProbe for DenyAllProbe {
            fn probe(&self, _level: IsolationLevel) -> Result<(), String> {
                Err("注入的探测失败".to_string())
            }
        }

        let dir = TestDir::new("run-command-workspace-write-probe");
        let runner: Arc<dyn CommandRunner> =
            Arc::new(PlatformRunner::with_probe(Arc::new(DenyAllProbe)));
        let error = run_with(
            &dir,
            SandboxMode::WorkspaceWrite,
            Some(runner),
            json!({ "command": "hbot-definitely-not-an-installed-program" }),
        )
        .await
        .expect_err("探测失败必须拒");
        assert!(matches!(error, HbotError::Denied { .. }));
        assert_eq!(error.code(), -32005, "必须是 denied 而不是 invalid");
        let message = error.to_string();
        assert!(message.contains("写隔离"), "{message}");
        assert!(message.contains("命令未执行"), "{message}");
        assert!(message.contains("注入的探测失败"), "{message}");
        assert!(
            !message.contains("找不到"),
            "隔离闸必须在解析程序之前：{message}"
        );
    }

    #[tokio::test]
    async fn timeout_budget_above_the_cap_is_invalid_and_never_runs() {
        let dir = TestDir::new("run-command-timeout-cap");
        let runner = Arc::new(FakeRunner::new(Ok(exited(0, "", ""))));
        let dynamic: Arc<dyn CommandRunner> = runner.clone();
        let error = run_with(
            &dir,
            SandboxMode::Danger,
            Some(dynamic),
            json!({ "command": "cargo", "timeout_ms": MAX_COMMAND_TIMEOUT_MS + 1 }),
        )
        .await
        .expect_err("超上限必须拒");
        assert_eq!(error.code(), -32002);
        assert_eq!(runner.calls(), 0);
    }

    #[tokio::test]
    async fn spec_is_built_from_tool_arguments() {
        let dir = TestDir::new("run-command-spec");
        // 工作区根会被 `Workspace::open` 规范化（Windows 上是 `\\?\C:\...` 长名），
        // 所以期望值从同一个入口取，别用 `dir.path()` 比字面量。
        let root = dir.workspace().root().to_path_buf();
        let runner = Arc::new(FakeRunner::new(Ok(exited(0, "ok", ""))));
        let dynamic: Arc<dyn CommandRunner> = runner.clone();
        let tool = RunCommandTool::with_allowed_programs(["cargo"]);
        run_tool(
            &tool,
            &dir,
            SandboxMode::Danger,
            Some(dynamic),
            json!({
                "command": "cargo",
                "args": ["test", "--all"],
                "cwd": "sub",
                "timeout_ms": 5000
            }),
        )
        .await
        .expect("假执行器必定成功");

        let spec = runner.last_spec();
        assert_eq!(spec.program, "cargo");
        assert_eq!(spec.args, vec!["test".to_string(), "--all".to_string()]);
        assert_eq!(spec.cwd, root.join("sub"));
        assert_eq!(spec.workspace_root, root);
        assert_eq!(spec.timeout, Duration::from_millis(5000));
        assert_eq!(spec.sandbox, SandboxMode::Danger);
        assert_eq!(spec.allowed_programs, vec!["cargo".to_string()]);
        assert_eq!(spec.max_output_bytes, MAX_COMMAND_OUTPUT_BYTES);
        assert!(!spec.inherit_entire_env, "不许整份环境继承");
        assert!(
            spec.inherit_env
                .iter()
                .any(|name| name.eq_ignore_ascii_case("PATH")),
            "子进程要能按 PATH 找程序"
        );
    }

    #[tokio::test]
    async fn output_over_the_tool_limit_is_truncated_with_the_byte_count() {
        let dir = TestDir::new("run-command-truncated");
        let huge = "x".repeat(100 * 1024);
        let runner = Arc::new(FakeRunner::new(Ok(exited(0, &huge, ""))));
        let dynamic: Arc<dyn CommandRunner> = runner.clone();
        let output = run_with(
            &dir,
            SandboxMode::Danger,
            Some(dynamic),
            json!({ "command": "cargo" }),
        )
        .await
        .expect("截断不是错误");
        assert!(output.truncated);
        assert!(!output.is_error);
        assert!(output.content.contains(&MAX_TOOL_OUTPUT_BYTES.to_string()));
        assert!(output.content.len() <= MAX_TOOL_OUTPUT_BYTES + 512);
    }

    #[tokio::test]
    async fn too_long_is_a_tool_error_that_names_the_byte_limit() {
        let dir = TestDir::new("run-command-too-long");
        let runner = Arc::new(FakeRunner::new(Err(CommandDenied::TooLong {
            stream: OutputStream::Stdout,
            limit_bytes: MAX_COMMAND_OUTPUT_BYTES,
        })));
        let dynamic: Arc<dyn CommandRunner> = runner.clone();
        let output = run_with(
            &dir,
            SandboxMode::Danger,
            Some(dynamic),
            json!({ "command": "cargo" }),
        )
        .await
        .expect("超限是工具结果，不是网关错误");
        assert!(output.is_error, "超限要显式告诉模型");
        assert!(
            output
                .content
                .contains(&MAX_COMMAND_OUTPUT_BYTES.to_string())
        );
        assert!(output.content.contains("进程树"));
    }

    #[tokio::test]
    async fn command_denials_map_to_the_documented_error_codes() {
        let dir = TestDir::new("run-command-mapping");
        let cases: Vec<(CommandDenied, i32)> = vec![
            (
                CommandDenied::ProgramNotAllowed {
                    program: "rm".to_string(),
                },
                -32005,
            ),
            (
                CommandDenied::SandboxDenied {
                    mode: SandboxMode::ReadOnly,
                },
                -32005,
            ),
            (
                CommandDenied::OutsideWorkspace {
                    path: "../etc".to_string(),
                },
                -32005,
            ),
            (
                CommandDenied::InheritEnvRejected {
                    vars: "LD_PRELOAD".to_string(),
                },
                -32005,
            ),
            (
                CommandDenied::InvalidSpec {
                    reason: "args 太多".to_string(),
                },
                -32002,
            ),
            (
                CommandDenied::ProgramNotFound {
                    program: "nope".to_string(),
                },
                -32002,
            ),
            (
                CommandDenied::SpawnFailed {
                    program: "cargo".to_string(),
                    reason: "权限不足".to_string(),
                },
                -32006,
            ),
            (
                CommandDenied::IoFailed {
                    context: "回收进程树".to_string(),
                    reason: "句柄已关闭".to_string(),
                },
                -32006,
            ),
        ];
        for (denied, expected) in cases {
            let runner = Arc::new(FakeRunner::new(Err(denied.clone())));
            let dynamic: Arc<dyn CommandRunner> = runner.clone();
            let error = run_with(
                &dir,
                SandboxMode::Danger,
                Some(dynamic),
                json!({ "command": "cargo" }),
            )
            .await
            .expect_err("这些变体都要变成错误");
            assert_eq!(error.code(), expected, "{denied:?}");
        }
    }

    #[tokio::test]
    async fn non_zero_exit_is_reported_as_tool_error_without_gateway_error() {
        let dir = TestDir::new("run-command-exit-code");
        let runner = Arc::new(FakeRunner::new(Ok(exited(3, "out", "boom"))));
        let dynamic: Arc<dyn CommandRunner> = runner.clone();
        let output = run_with(
            &dir,
            SandboxMode::Danger,
            Some(dynamic),
            json!({ "command": "cargo" }),
        )
        .await
        .expect("非零退出不是网关错误");
        assert!(output.is_error);
        assert!(output.content.contains("退出码 3"));
        assert!(output.content.contains("boom"));
    }

    #[tokio::test]
    async fn timeout_is_reported_in_the_tool_output_with_partial_output() {
        let dir = TestDir::new("run-command-timeout-output");
        let runner = Arc::new(FakeRunner::new(Ok(CommandOutput {
            status: CommandStatus::TimedOut,
            stdout: "half".to_string(),
            stderr: String::new(),
            duration: Duration::from_millis(30_000),
            usage: Default::default(),
        })));
        let dynamic: Arc<dyn CommandRunner> = runner.clone();
        let output = run_with(
            &dir,
            SandboxMode::Danger,
            Some(dynamic),
            json!({ "command": "cargo" }),
        )
        .await
        .expect("超时是工具结果");
        assert!(output.is_error);
        assert!(output.content.contains("超时"));
        assert!(output.content.contains("进程树"));
        assert!(output.content.contains("half"), "部分输出要回填");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn danger_mode_really_runs_a_process() {
        let dir = TestDir::new("run-command-danger");
        let runner: Arc<dyn CommandRunner> = Arc::new(PlatformRunner::system());
        let tool = RunCommandTool::with_allowed_programs(["cmd"]);
        let output = run_tool(
            &tool,
            &dir,
            SandboxMode::Danger,
            Some(runner),
            json!({ "command": "cmd", "args": ["/c", "echo hi"] }),
        )
        .await
        .expect("装配了执行器 + danger 档应当真跑");
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("hi"), "{}", output.content);
        assert!(output.content.contains("退出码 0"), "{}", output.content);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn empty_allow_list_denies_every_program() {
        let dir = TestDir::new("run-command-empty-list");
        let runner: Arc<dyn CommandRunner> = Arc::new(PlatformRunner::system());
        let error = run_with(
            &dir,
            SandboxMode::Danger,
            Some(runner),
            json!({ "command": "cmd", "args": ["/c", "echo hi"] }),
        )
        .await
        .expect_err("空清单必须全拒（fail-closed）");
        assert_eq!(error.code(), -32005);
        assert!(error.to_string().contains("不在本次允许清单内"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cwd_outside_the_workspace_is_denied() {
        let dir = TestDir::new("run-command-cwd-escape");
        let runner: Arc<dyn CommandRunner> = Arc::new(PlatformRunner::system());
        let tool = RunCommandTool::with_allowed_programs(["cmd"]);
        let error = run_tool(
            &tool,
            &dir,
            SandboxMode::Danger,
            Some(runner),
            json!({ "command": "cmd", "args": ["/c", "echo hi"], "cwd": ".." }),
        )
        .await
        .expect_err("越界 cwd 必须拒");
        assert_eq!(error.code(), -32005);
        assert!(error.to_string().contains("不在项目根内"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn timeout_kills_the_whole_process_tree() {
        let dir = TestDir::new("run-command-timeout-tree");
        let runner: Arc<dyn CommandRunner> = Arc::new(PlatformRunner::system());
        let tool = RunCommandTool::with_allowed_programs(["cmd"]);
        let started = Instant::now();
        // ping 是 cmd 的**子进程**：只杀 cmd 会留下 ping，进程树约束必须连它一起收掉。
        let output = run_tool(
            &tool,
            &dir,
            SandboxMode::Danger,
            Some(runner),
            json!({ "command": "cmd", "args": ["/c", "ping -n 6 127.0.0.1"], "timeout_ms": 400 }),
        )
        .await
        .expect("超时是工具结果");
        let elapsed = started.elapsed();
        assert!(output.is_error);
        assert!(output.content.contains("超时"), "{}", output.content);
        assert!(
            elapsed < Duration::from_secs(10),
            "超时后必须立刻返回，实际 {elapsed:?}"
        );
    }
}
