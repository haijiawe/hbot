//! 命令隔离的证据测试：全部**真跑子进程**，不 mock 内核行为。
//!
//! 五条必测：
//! - `timeout_kills_the_whole_process_tree`
//! - `isolation_unavailable_is_denied_not_allowed`
//! - `output_is_bounded`
//! - `environment_is_not_inherited_by_default`
//! - `working_directory_outside_the_workspace_is_rejected`
//!
//! 子进程用**当前测试二进制自己**（`current_exe` + `--exact helper`），
//! 因此不需要额外的构建产物；helper 按 `HBOT_HELPER_KIND` 分派行为。

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use hbot_command::{IsolationLevel, IsolationProbe, PlatformRunner};
use hbot_core::{
    CommandDenied, CommandOutput, CommandRunner, CommandSpec, CommandStatus, OutputStream,
    Platform, SandboxMode,
};

const ENV_KEY: &str = "HBOT_TEST_SECRET";

/// 探测注入：任何隔离层级都"装不上"。
struct DenyAllProbe;

impl IsolationProbe for DenyAllProbe {
    fn probe(&self, level: IsolationLevel) -> Result<(), String> {
        Err(format!("测试注入：{level} 不可用"))
    }
}

/// 本机必须支持进程树隔离，否则这不是"实现不足"而是"平台不支持"，
/// 报错里要能一眼看出来是哪一种。
fn require_containment() {
    let caps = PlatformRunner::system().capabilities();
    assert!(
        caps.supports(SandboxMode::Danger),
        "本机无法建立进程树隔离，本测试无法验证（{}）",
        caps.summary_zh()
    );
}

fn expect_exited(output: &CommandOutput) -> i32 {
    match output.status {
        CommandStatus::Exited {
            code: Some(code), ..
        } => code,
        ref other => panic!(
            "期望正常退出，实际是 {other:?}；stdout={:?} stderr={:?}",
            output.stdout, output.stderr
        ),
    }
}

// ---------------------------------------------------------------------------
// 1. 超时杀掉整棵进程树（含孙进程）
// ---------------------------------------------------------------------------

#[test]
fn timeout_kills_the_whole_process_tree() {
    require_containment();

    let root = common::test_root("tree");
    let mut spec = common::helper_spec(&root, "tree");
    spec.timeout = Duration::from_secs(3);

    let started = std::time::Instant::now();
    let output = common::system_runner()
        .run(spec)
        .expect("命令应跑到超时并正常返回结果");
    let elapsed = started.elapsed();

    assert_eq!(
        output.status,
        CommandStatus::TimedOut,
        "超时必须如实报 TimedOut"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "超时后必须尽快返回，实际花了 {elapsed:?}"
    );

    let helper_pid = common::parse_pid(&output.stdout, "HELPER_PID")
        .unwrap_or_else(|| panic!("helper 没报 pid；stdout={:?}", output.stdout));
    let grandchild_pid = common::parse_pid(&output.stdout, "GRANDCHILD_PID")
        .unwrap_or_else(|| panic!("helper 没报孙进程 pid；stdout={:?}", output.stdout));

    // 子进程自己先没：这条如果红，说明连直接子进程都没被杀。
    assert!(
        common::wait_gone(helper_pid, Duration::from_secs(5)),
        "超时后 helper 进程 {helper_pid} 仍在运行"
    );
    // 关键的一条：孙进程也必须没了 —— 证明杀的是**整棵树**，不是 `child.kill()`。
    assert!(
        common::wait_gone(grandchild_pid, Duration::from_secs(5)),
        "超时后孙进程 {grandchild_pid} 仍在运行（只杀了直接子进程）"
    );
}

// ---------------------------------------------------------------------------
// 2. 拿不到隔离 ⇒ 拒绝，而不是放行
// ---------------------------------------------------------------------------

#[test]
fn isolation_unavailable_is_denied_not_allowed() {
    let root = common::test_root("deny");
    let runner = PlatformRunner::with_probe(Arc::new(DenyAllProbe));

    // 故意给一个**不存在**的程序：如果实现顺序对了，拒绝原因必须是"隔离不可用"，
    // 而不是"程序找不到"——这条同时钉住了"先探测隔离，再解析程序"。
    let mut spec = CommandSpec::new("hbot-definitely-not-a-real-program", &root);
    spec.sandbox = SandboxMode::Danger;
    spec.allowed_programs = vec!["hbot-definitely-not-a-real-program".to_owned()];

    match runner.run(spec) {
        Err(CommandDenied::IsolationUnavailable { platform, reason }) => {
            assert_eq!(platform, Platform::current());
            assert!(
                reason.contains("测试注入"),
                "拒绝原因应保留探测细节，实际：{reason}"
            );
            let shown = CommandDenied::IsolationUnavailable { platform, reason }.to_string();
            assert!(
                shown.contains("已拒绝执行"),
                "给用户看的拒绝文案要能直接展示，实际：{shown}"
            );
        }
        other => panic!("隔离不可用时必须返回 IsolationUnavailable，实际：{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 3. 输出有上限：截断 + 杀进程，不把内存吃满
// ---------------------------------------------------------------------------

#[test]
fn output_is_bounded() {
    require_containment();

    let root = common::test_root("flood");
    let pid_file = root.join("flood.pid");
    let limit = 8 * 1024;

    let mut spec = common::helper_spec(&root, "flood");
    spec.max_output_bytes = limit;
    spec.timeout = Duration::from_secs(20);
    spec.env.push((
        "HBOT_HELPER_PID_FILE".to_owned(),
        pid_file.display().to_string(),
    ));

    let denied = common::system_runner()
        .run(spec)
        .expect_err("无限输出必须被判 TooLong，而不是把内存吃满");
    match denied {
        CommandDenied::TooLong {
            stream,
            limit_bytes,
        } => {
            assert_eq!(stream, OutputStream::Stdout);
            assert_eq!(limit_bytes, limit);
        }
        other => panic!("期望 TooLong，实际：{other:?}"),
    }

    // 判超限之后必须把进程杀掉，不能留一个还在写管道的孤儿。
    let pid: u32 = common::wait_for_file(&pid_file, Duration::from_secs(5))
        .expect("helper 应写出自己的 pid")
        .parse()
        .expect("pid 文件内容应是数字");
    assert!(
        common::wait_gone(pid, Duration::from_secs(5)),
        "输出超限后 helper 进程 {pid} 仍在运行"
    );
}

// ---------------------------------------------------------------------------
// 4. 环境变量默认不继承（只认显式白名单）
// ---------------------------------------------------------------------------

#[test]
fn environment_is_not_inherited_by_default() {
    require_containment();

    let root = common::test_root("env");
    // SAFETY: 这是本测试进程自己的环境；只用本测试独占的变量名，
    // 同进程里的其它测试不读它（`cargo test` 同进程并行，故变量名必须独占）。
    unsafe { std::env::set_var(ENV_KEY, "should-not-leak") };

    // 不写白名单：父进程的变量一个都不该出现在子进程里。
    let plain = common::system_runner()
        .run(common::helper_spec(&root, "env"))
        .expect("env helper 应正常退出");
    assert_eq!(expect_exited(&plain), 0, "stderr={:?}", plain.stderr);
    assert!(
        plain.stdout.contains("SECRET=MISSING"),
        "父进程的 SECRET 泄漏进子进程了；stdout={:?}",
        plain.stdout
    );
    assert!(
        plain.stdout.contains("PATH=MISSING"),
        "未显式声明的 PATH 也泄漏了；stdout={:?}",
        plain.stdout
    );

    // 显式白名单：只点名的那一个必须传进去。
    let mut spec = common::helper_spec(&root, "env");
    spec.inherit_env = vec![ENV_KEY.to_owned()];
    let named = common::system_runner()
        .run(spec)
        .expect("env helper 应正常退出");
    assert_eq!(expect_exited(&named), 0, "stderr={:?}", named.stderr);
    assert!(
        named.stdout.contains("SECRET=should-not-leak"),
        "显式白名单的变量没有传进去；stdout={:?}",
        named.stdout
    );
}

// ---------------------------------------------------------------------------
// 5. 工作目录必须在项目根内（canonicalize + 逐组件比较）
// ---------------------------------------------------------------------------

#[test]
fn working_directory_outside_the_workspace_is_rejected() {
    require_containment();

    let root = common::test_root("cwd");
    // 与项目根平级的兄弟目录：字符串前缀很像（`...-cwd` vs `...-cwd-outside`），
    // 只有逐组件比较才分得开。
    let outside = root.with_file_name(format!(
        "{}-outside",
        root.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::create_dir_all(&outside).expect("建根外目录");

    // 绝对路径指向根外。
    let mut spec = common::helper_spec(&root, "env");
    spec.cwd = outside.clone();
    match common::system_runner().run(spec) {
        Err(CommandDenied::OutsideWorkspace { .. }) => {}
        other => panic!("根外 cwd 必须被拒（OutsideWorkspace），实际：{other:?}"),
    }

    // 相对路径 `..` 逃逸。
    let mut spec = common::helper_spec(&root, "env");
    spec.cwd = Path::new("..").to_path_buf();
    match common::system_runner().run(spec) {
        Err(CommandDenied::OutsideWorkspace { .. }) => {}
        other => panic!("`..` 逃逸必须被拒（OutsideWorkspace），实际：{other:?}"),
    }

    let _ = std::fs::remove_dir_all(&outside);
}

// ---------------------------------------------------------------------------
// 附加：程序清单为空 = 全拒（fail-closed 的另一半）
// ---------------------------------------------------------------------------

#[test]
fn empty_program_allowlist_allows_nothing() {
    let root = common::test_root("allowlist");
    let mut spec = common::helper_spec(&root, "env");
    spec.allowed_programs.clear();

    match common::system_runner().run(spec) {
        Err(CommandDenied::ProgramNotAllowed { .. }) => {}
        other => panic!("空清单必须全拒，实际：{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// helper：被上面的测试当作子进程启动
// ---------------------------------------------------------------------------

#[test]
#[ignore = "helper：由本文件里的测试当子进程启动，不单独跑"]
fn helper() {
    use std::io::Write;

    match std::env::var("HBOT_HELPER_KIND")
        .unwrap_or_default()
        .as_str()
    {
        "tree" => {
            let grandchild = spawn_grandchild();
            println!("HELPER_PID={}", std::process::id());
            println!("GRANDCHILD_PID={grandchild}");
            let _ = std::io::stdout().flush();
            // 不主动退出：等父进程超时杀树。
            std::thread::sleep(Duration::from_secs(120));
        }
        "env" => {
            let read = |key: &str| match std::env::var(key) {
                Ok(value) => value,
                Err(_) => "MISSING".to_owned(),
            };
            println!("SECRET={}", read(ENV_KEY));
            println!("PATH={}", read("PATH"));
            let _ = std::io::stdout().flush();
        }
        "flood" => {
            let pid_file = std::env::var("HBOT_HELPER_PID_FILE").expect("helper 需要 pid 文件路径");
            std::fs::write(&pid_file, std::process::id().to_string()).expect("写 pid 文件");
            let chunk = vec![b'x'; 8 * 1024];
            let mut out = std::io::stdout();
            loop {
                if out.write_all(&chunk).is_err() {
                    break; // 父进程不读了（判了超限）——正常收尾
                }
                if out.flush().is_err() {
                    break;
                }
            }
        }
        other => panic!("未知 helper 种类：{other:?}"),
    }
}

#[cfg(windows)]
fn spawn_grandchild() -> u32 {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
    let ping = Path::new(&system_root).join("System32").join("PING.EXE");
    Command::new(ping)
        .args(["-n", "120", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("起孙进程")
        .id()
}

#[cfg(target_os = "linux")]
fn spawn_grandchild() -> u32 {
    Command::new("/bin/sleep")
        .arg("120")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("起孙进程")
        .id()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn spawn_grandchild() -> u32 {
    panic!("本平台未实现孙进程夹具")
}

// helper 子进程的启动参数在 `common::helper_spec` 里设置：
// `--exact helper --ignored --nocapture`（只跑 `helper` 这一个被 ignore 的测试，
// 且不吞掉它的 stdout——测试要靠它的 println 取 pid）。
