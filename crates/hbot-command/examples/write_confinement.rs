//! 演示/取证：同一个程序、同一组目标路径，只换沙箱档位，写盘结果就不一样。
//!
//! ```text
//! cargo run -p hbot-command --example write_confinement
//! ```
//!
//! 父进程用 [`PlatformRunner`] 把**本示例自己**当子进程拉起两次
//! （`workspace-write` 一次、`danger` 一次），子进程按固定的三处路径试写：
//!
//! - `INSIDE`：工作区里的相对路径 —— 两种档位都该成功；
//! - `OUTSIDE`：工作区外的绝对路径（容器目录里）—— 只有 `danger` 成功；
//! - `ESCAPE`：`..` 相对逃逸 —— 同样只有 `danger` 成功。
//!
//! 这是给「工作区外写盘被拒」那条测试做反证的：如果关掉隔离后同一个子进程也写不出去，
//! 那条测试就是恒真的，什么也证明不了。

use std::path::{Path, PathBuf};
use std::time::Duration;

use hbot_command::{CommandSpec, PlatformRunner};
use hbot_core::{CommandRunner, CommandStatus, SandboxMode};

/// 子进程看到的「工作区外」绝对路径。
const OUTSIDE_ENV: &str = "HBOT_EXAMPLE_OUTSIDE";

/// `..` 逃逸（退出工作区一级）。
#[cfg(windows)]
const ESCAPE_RELATIVE: &str = r"..\escaped.txt";
#[cfg(not(windows))]
const ESCAPE_RELATIVE: &str = "../escaped.txt";

fn main() {
    if std::env::args().any(|argument| argument == "--child") {
        child();
        return;
    }

    let runner = PlatformRunner::system();
    let caps = runner.capabilities();
    println!("平台：{}", caps.platform);
    println!("{}", caps.summary_zh());
    println!();

    let (container, workspace) = setup();
    let outside = container.join("outside.txt");

    for mode in [SandboxMode::WorkspaceWrite, SandboxMode::Danger] {
        let spec = child_spec(&workspace, &outside, mode);
        println!("=== 档位：{mode:?} ===");
        match runner.run(spec) {
            Ok(output) => {
                println!("退出：{:?}", status_text(&output.status));
                print!("{}", output.stdout);
                if !output.stderr.trim().is_empty() {
                    println!("stderr：{}", output.stderr.trim());
                }
                println!(
                    "盘上：inside={} outside={} escaped={}",
                    flag(workspace.join("inside.txt")),
                    flag(outside.clone()),
                    flag(container.join("escaped.txt")),
                );
            }
            Err(denied) => println!("被拒：{denied}"),
        }
        println!();
    }

    let _ = std::fs::remove_dir_all(&container);
}

/// 子进程：按三种路径各写一个文件，把结果打印成 `LABEL=ok/fail(...)`。
fn child() {
    attempt("INSIDE", Path::new("inside.txt"));
    match std::env::var(OUTSIDE_ENV) {
        Ok(outside) => attempt("OUTSIDE", Path::new(&outside)),
        Err(_) => println!("OUTSIDE=miss(没有 {OUTSIDE_ENV})"),
    }
    attempt("ESCAPE", Path::new(ESCAPE_RELATIVE));
}

fn attempt(label: &str, path: &Path) {
    let result = match std::fs::write(path, b"hbot") {
        Ok(()) => "ok".to_owned(),
        Err(error) => format!("fail({error})"),
    };
    println!("{label}={result}");
}

/// 容器目录 + 工作区子目录：`..` 逃逸落在容器里（工作区外，但仍是本示例独占的位置）。
fn setup() -> (PathBuf, PathBuf) {
    let container =
        std::env::temp_dir().join(format!("hbot-command-example-wconf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&container);
    let workspace = container.join("workspace");
    std::fs::create_dir_all(&workspace).expect("建工作区");
    (
        std::fs::canonicalize(&container).expect("规范化容器目录"),
        std::fs::canonicalize(&workspace).expect("规范化工作区"),
    )
}

fn child_spec(workspace: &Path, outside: &Path, mode: SandboxMode) -> CommandSpec {
    let exe = std::env::current_exe().expect("current_exe 应可用");
    let mut spec = CommandSpec::new(exe.display().to_string(), workspace);
    spec.args = vec!["--child".to_owned()];
    spec.allowed_programs = vec![exe.display().to_string()];
    spec.sandbox = mode;
    spec.timeout = Duration::from_secs(30);
    spec.max_output_bytes = 64 * 1024;
    spec.inherit_system_env();
    spec.env
        .push((OUTSIDE_ENV.to_owned(), outside.display().to_string()));
    spec
}

fn status_text(status: &CommandStatus) -> String {
    match status {
        CommandStatus::Exited { code, signal } => {
            format!("Exited(code={code:?}, signal={signal:?})")
        }
        CommandStatus::TimedOut => "TimedOut".to_owned(),
    }
}

fn flag(path: PathBuf) -> &'static str {
    if path.is_file() {
        "存在"
    } else {
        "不存在"
    }
}
