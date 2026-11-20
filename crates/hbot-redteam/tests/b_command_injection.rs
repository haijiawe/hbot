//! B 面：命令注入（目标 `crates/hbot-command` + `crates/hbot-agent/src/tools/shell.rs`）。
//!
//! ## 这一面的核心结论（先读，否则会写错断言）
//!
//! 本仓库的命令执行是**argv 直传**（`std::process::Command::args` / `CreateProcessW` 的
//! 命令行按参数逐个转义），**没有 `sh -c` / `cmd /c` 这一层**。所以
//! `echo hi; rm -rf /` 里的 `;` 只是 `echo` 收到的一个普通字符，**不会**跑起第二个进程。
//! 这不是缺陷，是设计。真正要防的是：
//!
//! 1. 空白名单时必须 fail-closed（否则等于任意命令执行）；
//! 2. 允许清单里混进 `cmd.exe` / `powershell.exe` / `.bat` / `.cmd` 时，隔离当场失效
//!    —— 那条路径等于把 shell 装回来（见 RT-05）。**配置层**已在 RT-05 修复后拒绝这类
//!    清单（`b10` 钉住）；**命令层**仍然只回答"清单里有没有这个名字"（`b03` 钉住）。
//!    两层分工见 `docs/adr/0003-command-isolation.md` §5。
//! 3. 超时必须杀掉**整棵进程树**（只 kill 直接子进程，孙进程会活着继续吃资源）；
//! 4. 输出必须有硬上限（否则一条 `yes` 就能把宿主内存吃满）；
//! 5. 环境变量默认不继承（否则 `APPDATA` / `PATH` / 各种 token 变量直接泄给子进程）；
//! 6. `cwd` 出根必须拒（否则命令在项目外为所欲为）。
//!
//! ## 夹具手法
//!
//! 用**测试二进制自己**当 helper（`std::env::current_exe()` + `--exact b_helper --ignored`），
//! 这样不需要额外构建产物，也不依赖系统里恰好有 `echo`/`ping`。helper 的行为由
//! `HBOT_RT_MODE` 选：`argv` / `env` / `tree` / `tree2` / `flood` / `marker`。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hbot_agent::tools::shell::RunCommandTool;
use hbot_agent::{CancellationToken, Tool, ToolCtx};
use hbot_command::PlatformRunner;
use hbot_core::{CommandDenied, CommandRunner, CommandSpec, CommandStatus, SandboxMode, SessionId};
use hbot_redteam::{TempTree, parse_pid, wait_for_file, wait_gone};
use hbot_rpc::app::agent::config::SandboxConfig;
use hbot_workspace::Workspace;

// ── helper（被 ignore，只有测试自己会按名字把它拉起来）────────────────────────

/// helper 子进程的入口：按 `HBOT_RT_MODE` 扮演不同角色。
///
/// 它自己**不是**被测对象，是"被沙箱跑起来的那个程序"。
#[test]
#[ignore = "helper：只由本文件的用例以子进程方式拉起，不单独运行"]
fn b_helper() {
    let mode = std::env::var("HBOT_RT_MODE").unwrap_or_default();
    let dir = PathBuf::from(std::env::var("HBOT_RT_DIR").unwrap_or_default());
    match mode.as_str() {
        "argv" => {
            // 逐条打印真实 argv：若实现偷偷套了 shell，这些字面量会被重新解释（看不到它们）。
            for arg in std::env::args().skip(1) {
                println!("ARGVITEM={arg}");
            }
            println!("DONE=argv");
        }
        "env" => {
            for name in [
                "PATH",
                "APPDATA",
                "USERPROFILE",
                "SystemRoot",
                "HBOT_RT_SECRET",
            ] {
                if std::env::var_os(name).is_some() {
                    println!("ENV_PRESENT={name}");
                } else {
                    println!("ENV_ABSENT={name}");
                }
            }
            println!("DONE=env");
        }
        "marker" => {
            let _ = std::fs::write(dir.join("marker.txt"), "helper-ran");
            println!("DONE=marker");
        }
        "tree" => {
            // 先报告自己的 pid，再拉起孙进程，最后一直活着等超时。
            let _ = std::fs::write(
                dir.join("child.pid"),
                format!("CHILD={}", std::process::id()),
            );
            let exe = std::env::current_exe().expect("helper 的 current_exe");
            let spawned = std::process::Command::new(exe)
                .args(["--exact", "b_helper", "--ignored", "--nocapture"])
                .env("HBOT_RT_MODE", "tree2")
                .env("HBOT_RT_DIR", dir.display().to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match spawned {
                Ok(child) => {
                    let _ = std::fs::write(
                        dir.join("child.spawned"),
                        format!("GRANDCHILD_SPAWNED={}", child.id()),
                    );
                }
                Err(error) => {
                    let _ = std::fs::write(
                        dir.join("child.spawned"),
                        format!("GRANDCHILD_SPAWN_FAILED={error}"),
                    );
                }
            }
            sleep_forever();
        }
        "tree2" => {
            let _ = std::fs::write(
                dir.join("grandchild.pid"),
                format!("GRAND={}", std::process::id()),
            );
            sleep_forever();
        }
        "flood" => {
            use std::io::Write;
            let chunk = vec![b'x'; 8 * 1024];
            let mut stdout = std::io::stdout();
            loop {
                if stdout.write_all(&chunk).is_err() {
                    // 宿主判定超限后关管道，这里退出也是正常路径。
                    std::process::exit(0);
                }
                let _ = stdout.flush();
            }
        }
        other => {
            println!("DONE=unknown-{other}");
        }
    }
}

fn sleep_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── 规格构造 ─────────────────────────────────────────────────────────────────

/// 当前测试二进制（拿它当被沙箱执行的程序）。
fn helper_exe() -> PathBuf {
    std::env::current_exe().expect("current_exe 应可用")
}

/// 一条 `danger` 档的 helper 规格：允许清单里**只有**测试二进制自己。
///
/// `danger` 只要求进程树隔离（不限制写盘），所以 helper 能往临时目录写 pid 文件。
fn helper_spec(root: &Path, mode: &str, dir: &Path) -> CommandSpec {
    let exe = helper_exe();
    let program = exe.display().to_string();
    let mut spec = CommandSpec::new(program.clone(), root);
    spec.allowed_programs = vec![program];
    spec.sandbox = SandboxMode::Danger;
    spec.args = vec![
        "--exact".to_owned(),
        "b_helper".to_owned(),
        "--ignored".to_owned(),
        "--nocapture".to_owned(),
    ];
    spec.env.push(("HBOT_RT_MODE".to_owned(), mode.to_owned()));
    spec.env
        .push(("HBOT_RT_DIR".to_owned(), dir.display().to_string()));
    spec.timeout = Duration::from_secs(15);
    spec.max_output_bytes = 256 * 1024;
    spec
}

fn runner() -> PlatformRunner {
    PlatformRunner::system()
}

/// 本平台上的「解释器」程序，以及让它执行一段命令所需的参数。
///
/// `b03` 钉的是**命令层不判断名字危不危险**这个事实，与解释器叫什么无关：Windows 上是
/// `cmd /c <脚本>`，POSIX 上是 `sh -c <脚本>`。把 `cmd` 写死会让这条用例在 Linux 上先撞
/// `ProgramNotFound`（`cmd` 不存在），于是它测的就不再是"命令层不兜底"而是"这台机器有没有
/// `cmd`" —— 判据跑偏，红得也看不懂（2026-11-20 CI 实测）。
fn platform_shell() -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        (
            "cmd",
            vec!["/c".to_owned(), "echo PWNED>marker.txt".to_owned()],
        )
    } else {
        (
            "sh",
            vec!["-c".to_owned(), "echo PWNED>marker.txt".to_owned()],
        )
    }
}

/// 同 [`platform_shell`]，但脚本只打印一行（不需要产生副作用时用）。
fn platform_shell_echo() -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        ("cmd", vec!["/c".to_owned(), "echo hi".to_owned()])
    } else {
        ("sh", vec!["-c".to_owned(), "echo hi".to_owned()])
    }
}

// ── 1. 空白名单 = fail-closed ────────────────────────────────────────────────

/// 防的是：**空白名单被当成"不限制"**。那样只要沙箱档是 `danger`，任何命令都能跑，
/// 白名单形同虚设，等于把宿主机的命令行交给模型。
#[test]
fn b01_空白名单必须拒绝一切命令() {
    let tree = TempTree::new("b01");
    let mut spec = helper_spec(tree.path(), "marker", tree.path());
    spec.allowed_programs.clear();
    spec.args = vec![
        "--exact".to_owned(),
        "b_helper".to_owned(),
        "--ignored".to_owned(),
    ];

    match runner().run(spec) {
        Err(CommandDenied::ProgramNotAllowed { program }) => {
            assert!(!program.is_empty(), "拒绝信息应当点名被拒的程序");
        }
        Err(other) => panic!("期望 ProgramNotAllowed，实际：{other:?}"),
        Ok(output) => panic!("空白名单竟然跑起来了：{output:?}"),
    }
    assert!(
        !tree.exists("marker.txt"),
        "空白名单下 helper 仍然执行了（marker.txt 出现）"
    );
}

// ── 2. 元字符只是普通字符（按实际实现判定）──────────────────────────────────

/// 防的是：实现里其实套了一层 shell（`sh -c` / `cmd /c`），于是参数里的 `;`、`&&`、
/// `|`、`>`、`$(...)`、反引号全部变成"再跑一条命令"。
///
/// 实测结论：本实现是 **argv 直传**，元字符原样到达程序。判据是双向的——
/// ① helper 收到的 argv 里能看到那串字面量；② 工作区里没有凭空出现 `marker.txt`。
#[test]
fn b02_参数里的元字符必须原样传递而不是被shell解释() {
    let tree = TempTree::new("b02");
    let payloads = [
        "echo hi; rm -rf /",
        "echo $(whoami)",
        "echo `id`",
        "echo hi && calc",
        "a | b",
        "; echo PWNED > marker.txt",
    ];
    let mut spec = helper_spec(tree.path(), "argv", tree.path());
    for payload in payloads {
        spec.args.push(payload.to_owned());
    }

    let output = runner().run(spec).expect("helper 应当正常退出");
    assert_eq!(
        output.status,
        CommandStatus::Exited {
            code: Some(0),
            signal: None
        },
        "helper 没正常退出：{output:?}"
    );
    let stdout = output.stdout.as_str();
    for payload in payloads {
        assert!(
            stdout.contains(&format!("ARGVITEM={payload}")),
            "参数 `{payload}` 没有原样到达程序（被 shell 吃掉或改写了）：\n{stdout}"
        );
    }
    assert!(
        !tree.exists("marker.txt"),
        "工作区出现了 marker.txt —— 参数里的 `>` 被当成重定向执行了"
    );
}

/// 防的是：允许清单把 `cmd.exe` / `powershell.exe` 这类**解释器**放进来。
///
/// 这条用例**故意断言"能跑"**，钉的是**命令层的事实**：`hbot-command` 只回答"这份清单里有没有
/// 这个名字"，不会替你判断哪些名字危险（ADR-0003 §5）。所以判据必须在**配置层** ——
/// RT-05 的修法与对应用例是 `b10`（`hbot-rpc` 的 `SandboxConfig::resolve_with`），
/// 本用例不随配置层收紧而改变：它证明的是"命令层不会兜底"。
#[test]
fn b03_允许cmd等于把shell装回来自证() {
    let tree = TempTree::new("b03");
    let (shell, args) = platform_shell();
    let mut spec = CommandSpec::new(shell, tree.path());
    spec.allowed_programs = vec![shell.to_owned()];
    spec.sandbox = SandboxMode::Danger;
    spec.args = args;
    spec.timeout = Duration::from_secs(15);

    let output = runner()
        .run(spec)
        .expect("解释器在允许清单里时应当能跑（这正是风险所在）");
    let _ = output;
    assert!(
        tree.exists("marker.txt"),
        "允许解释器却没有执行重定向 —— 若实现改成拒绝解释器，本用例连同 RT-05 一起更新"
    );
}

// ── 3. 超时必须杀整棵进程树 ─────────────────────────────────────────────────

/// 防的是：超时只 `kill()` 直接子进程，孙进程继续活着（编译链、`pnpm`、`node` 都会这样），
/// 于是"超时"变成"留下一个没人管的进程继续吃 CPU/内存/写盘"。
#[test]
fn b04_超时必须杀掉整棵进程树() {
    let tree = TempTree::new("b04");
    let dir = tempfile_dir(&tree);
    let mut spec = helper_spec(tree.path(), "tree", &dir);
    spec.timeout = Duration::from_millis(1_500);

    let output = runner()
        .run(spec)
        .expect("超时不是 Err，而是带 TimedOut 的输出");
    assert_eq!(
        output.status,
        CommandStatus::TimedOut,
        "期望超时状态，实际：{output:?}"
    );

    let child_text = wait_for_file(&dir.join("child.pid"), Duration::from_secs(5))
        .expect("helper 应当在被杀之前写下自己的 pid");
    let child_pid = parse_pid(&child_text, "CHILD").expect("解析子进程 pid");

    let grand_text = wait_for_file(&dir.join("grandchild.pid"), Duration::from_secs(5))
        .expect("孙进程应当被拉起并写下自己的 pid");
    let grand_pid = parse_pid(&grand_text, "GRAND").expect("解析孙进程 pid");

    assert!(
        wait_gone(child_pid, Duration::from_secs(10)),
        "直接子进程 {child_pid} 在超时后仍然活着"
    );
    assert!(
        wait_gone(grand_pid, Duration::from_secs(10)),
        "孙进程 {grand_pid} 在超时后仍然活着 —— 只杀了直接子进程，没杀整棵树"
    );
}

/// 在临时工作区里开一个 helper 用来写文件的目录（`danger` 档不限制写盘位置）。
fn tempfile_dir(tree: &TempTree) -> PathBuf {
    let dir = tree.path().to_path_buf();
    std::fs::create_dir_all(&dir).expect("helper 输出目录");
    dir
}

// ── 4. 输出上限 ─────────────────────────────────────────────────────────────

/// 防的是：一条无限输出的命令（`yes`、死循环 `echo`）把宿主内存吃满，
/// 或让判定一直等不到进程退出。
#[test]
fn b05_无限输出必须被硬上限截断且及时返回() {
    let tree = TempTree::new("b05");
    let mut spec = helper_spec(tree.path(), "flood", tree.path());
    spec.max_output_bytes = 64 * 1024;
    spec.timeout = Duration::from_secs(30);

    let started = Instant::now();
    let outcome = runner().run(spec);
    let elapsed = started.elapsed();

    match outcome {
        Err(CommandDenied::TooLong {
            stream,
            limit_bytes,
        }) => {
            assert_eq!(limit_bytes, 64 * 1024, "上限应当就是配置的那个值");
            assert!(
                elapsed < Duration::from_secs(25),
                "超限判定来得太晚（{elapsed:?}），期间内存可能已经涨过一轮"
            );
            let _ = stream;
        }
        Err(other) => panic!("期望 TooLong，实际：{other:?}"),
        Ok(output) => panic!(
            "无限输出没有被截断（返回了 {} 字节）：status={:?}",
            output.stdout.len(),
            output.status
        ),
    }
}

// ── 5. 环境变量默认不继承 ───────────────────────────────────────────────────

/// 防的是：子进程继承宿主的 `APPDATA` / `PATH` / token 变量，于是"跑一条命令"
/// 顺手把用户凭据和环境侦察结果交给了可能不可信的程序。
#[test]
fn b06_环境变量默认不继承() {
    let tree = TempTree::new("b06");
    let spec = helper_spec(tree.path(), "env", tree.path());

    let output = runner().run(spec).expect("helper 应当正常退出");
    let stdout = output.stdout.as_str();

    for name in ["APPDATA", "USERPROFILE", "PATH", "HBOT_RT_SECRET"] {
        assert!(
            !stdout.contains(&format!("ENV_PRESENT={name}")),
            "子进程继承了父进程的 `{name}`：\n{stdout}"
        );
    }
    assert!(
        stdout.contains("DONE=env"),
        "helper 没跑到结尾，输出不可信：\n{stdout}"
    );
}

// ── 6. 工作目录守卫 ─────────────────────────────────────────────────────────

/// 防的是：`cwd` 指到项目外（`..`、绝对路径、符号链接），命令于是在项目外删文件/改配置，
/// 而"沙箱只保护项目根"这条前提当场失效。
#[test]
fn b07_工作目录在项目外必须被拒() {
    let tree = TempTree::new("b07");
    let outside = TempTree::new("b07-outside");

    // ① runner 层：绝对路径指向根外。
    let mut spec = helper_spec(tree.path(), "marker", tree.path());
    spec.cwd = outside.path().to_path_buf();
    match runner().run(spec) {
        Err(CommandDenied::OutsideWorkspace { .. }) => {}
        Err(other) => panic!("期望 OutsideWorkspace，实际：{other:?}"),
        Ok(output) => panic!("cwd 指到根外竟然跑起来了：{output:?}"),
    }

    // ② runner 层：`..` 相对逃逸。
    let mut spec = helper_spec(tree.path(), "marker", tree.path());
    spec.cwd = PathBuf::from("..");
    match runner().run(spec) {
        Err(CommandDenied::OutsideWorkspace { .. }) => {}
        Err(other) => panic!("期望 OutsideWorkspace，实际：{other:?}"),
        Ok(output) => panic!("`..` 作 cwd 竟然跑起来了：{output:?}"),
    }

    // ③ 工具层：`run_command` 的 `cwd: ".."` 必须是拒绝，而不是"跑起来但落在根外"。
    //    这里必须先把 `cmd` 放进允许清单，否则会先撞上"程序不在清单内"而测不到 cwd 守卫。
    let (shell, args) = platform_shell_echo();
    let ctx = tool_ctx(&tree);
    let tool = RunCommandTool::with_allowed_programs([shell]);
    let result = block_on(tool.run(
        serde_json::json!({"command": shell, "args": args, "cwd": ".."}),
        &ctx,
    ));
    let error = result.expect_err("根外 cwd 必须是 Err（不是可执行的 error 结果）");
    let message = format!("{error}");
    assert!(
        message.contains("不在项目根内") || message.contains("cwd"),
        "拒绝理由应当说明 cwd 出界：\n{message}"
    );
}

// ── 7. 非法超时预算 ─────────────────────────────────────────────────────────

/// 防的是：`timeout_ms` 为 0 / 负数 / 超过上限时被"修正成默认值"然后照常执行，
/// 于是调用方以为会 1 秒被杀，实际按 30 秒跑。
#[test]
fn b08_非法超时预算必须在执行前被拒() {
    let tree = TempTree::new("b08");
    let ctx = tool_ctx(&tree);
    let tool = RunCommandTool::new();

    for budget in [
        serde_json::json!(0),
        serde_json::json!(30_001),
        serde_json::json!(-1),
    ] {
        let result = block_on(tool.run(
            serde_json::json!({"command": "cmd", "args": ["/c", "echo hi"], "timeout_ms": budget}),
            &ctx,
        ));
        let error = result.expect_err("非法 timeout_ms 必须在执行前被拒");
        let message = format!("{error}");
        assert!(
            message.contains("timeout_ms") || message.contains("超时"),
            "拒绝理由应当点名超时预算：\n{message}"
        );
    }

    // runner 层：零超时必须 InvalidSpec（不是"立刻超时"）。
    let mut spec = helper_spec(tree.path(), "marker", tree.path());
    spec.timeout = Duration::ZERO;
    match runner().run(spec) {
        Err(CommandDenied::InvalidSpec { .. }) => {}
        Err(other) => panic!("期望 InvalidSpec，实际：{other:?}"),
        Ok(output) => panic!("零超时竟然跑起来了：{output:?}"),
    }
}

// ── 8. 脚本解释器不能当"程序" ───────────────────────────────────────────────

/// 防的是：把 `.bat` / `.cmd` 当成可直接执行的文件（Windows 必须经 `cmd.exe /c` 解释），
/// 那等于替调用方把 shell 装回来：引号解析、变量展开、重定向全部回来了。
#[test]
fn b09_bat与cmd脚本不能作为可执行程序() {
    let tree = TempTree::new("b09");
    let script = tree.path().join("evil.cmd");
    std::fs::write(&script, "@echo off\r\necho PWNED>marker.txt\r\n").expect("写测试脚本");

    let program = script.display().to_string();
    let mut spec = CommandSpec::new(program.clone(), tree.path());
    spec.allowed_programs = vec![program.clone()];
    spec.sandbox = SandboxMode::Danger;
    spec.timeout = Duration::from_secs(10);

    match runner().run(spec) {
        Err(CommandDenied::ProgramNotAllowed { .. })
        | Err(CommandDenied::ProgramNotFound { .. }) => {}
        Err(other) => panic!("期望被拒，实际：{other:?}"),
        Ok(output) => panic!("`.cmd` 竟然被直接执行了：{output:?}"),
    }
    assert!(
        !tree.exists("marker.txt"),
        "`.cmd` 被执行了（marker.txt 出现）"
    );
}

// ── 9. RT-05：配置解析层的解释器准入判据 ───────────────────────────────────

/// 防的是：`allowedPrograms` 里出现 `cmd` / `powershell` / `python` 这类解释器时，
/// 配置层原样放行 —— `run_command` 的 argv 保证对它们无效（`cmd /c …` 之后就是任意命令）。
///
/// 这条钉的是**配置解析层**（`hbot-rpc` 的 `SandboxConfig::resolve_with`）：判据必须在那里，
/// 因为 `hbot-command` 永远只回答"清单里有没有这个名字"（ADR-0003 §5）。`b03` 钉的是另一半：
/// 命令层不会替你兜底。
#[test]
fn b10_配置层必须拒绝解释器类允许清单() {
    let tree = TempTree::new("b10");
    let config_path = tree.path().join("agent.json");
    let none = |_name: &str| None::<String>;

    // ① 全路径 + 大小写 + 扩展名混写：都必须在解析层被识别（判据 = basename/小写/去扩展名）。
    std::fs::write(
        &config_path,
        r#"{"sandbox":"danger","allowedPrograms":["C:/Windows/System32/WindowsPowerShell/v1.0/POWERSHELL.EXE","cmd"]}"#,
    )
    .expect("写 agent.json");
    let error = SandboxConfig::resolve_with(tree.path(), &none).expect_err("解释器清单必须被拒");
    assert_eq!(error.code(), -32002, "拒绝走 -32002：{error}");
    let message = error.to_string();
    assert!(
        message.contains("POWERSHELL.EXE"),
        "要回显用户写的那一行，否则不知道删哪个：{message}"
    );
    assert!(message.contains("`cmd`"), "要列出被拒的名字：{message}");
    assert!(
        message.contains("HBOT_ALLOW_SHELL"),
        "要说清逃生阀（默认拒绝，显式才放行）：{message}"
    );

    // ② 具体程序不误杀：`ssh` / `shellcheck` 只含 "sh" 子串，不是解释器。
    std::fs::write(
        &config_path,
        r#"{"sandbox":"danger","allowedPrograms":["git","ssh","shellcheck"]}"#,
    )
    .expect("写 agent.json");
    let config = SandboxConfig::resolve_with(tree.path(), &none).expect("具体程序必须可用");
    assert_eq!(config.allowed_programs(), ["git", "ssh", "shellcheck"]);

    // ③ 逃生阀：显式打开时放行，而且名字原样保留（放行不是"顺手删掉"，否则命令层拿到空清单）。
    //    告警的"每进程每条清单一次"由 `hbot-rpc` 的 `tests/config_warnings.rs` 契约测试钉住。
    std::fs::write(
        &config_path,
        r#"{"sandbox":"danger","allowedPrograms":["cmd"]}"#,
    )
    .expect("写 agent.json");
    let hatch = |name: &str| match name {
        "HBOT_ALLOW_SHELL" => Some("1".to_string()),
        _ => None,
    };
    let config = SandboxConfig::resolve_with(tree.path(), &hatch).expect("逃生阀打开后应放行");
    assert_eq!(
        config.allowed_programs(),
        ["cmd"],
        "放行时清单必须原样交给命令层"
    );
}

// ── 工具层夹具 ─────────────────────────────────────────────────────────────

fn tool_ctx<'a>(tree: &TempTree) -> ToolCtx<'a> {
    // `ToolCtx` 的所有字段都是 pub（`hbot-agent/src/tools/mod.rs`），可以直接构造。
    let workspace = Arc::new(Workspace::open(tree.path().to_path_buf()).expect("打开临时工作区"));
    let cancel: &'a CancellationToken = Box::leak(Box::new(CancellationToken::new()));
    let runner: Arc<dyn CommandRunner> = Arc::new(PlatformRunner::system());
    ToolCtx {
        workspace,
        sandbox: SandboxMode::WorkspaceWrite,
        cancel,
        session_id: SessionId::from_uuid(uuid::Uuid::from_u128(0x52)),
        call_id: "red-team-b",
        edits: None,
        command: Some(runner),
        // 这组红队用例打的是命令注入，不碰设备能力；`None` 让 `read_screen`
        // 走"没有装配设备后端"的诚实拒绝（与生产装配缺失时同一条路径）。
        device: None,
    }
}

/// 极简 `block_on`：只跑一个 future，不引入 `#[tokio::test]` 的多线程运行时
/// （工具内部用 `spawn_blocking`，需要一个能跑阻塞任务的运行时）。
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .expect("建 tokio 运行时")
        .block_on(future)
}
