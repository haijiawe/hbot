//! 隔离测试的公共夹具：真子进程、真进程存活判定、临时项目根。
//!
//! 这里刻意**不 mock**任何东西：这五条测试要证的正是"内核层真的把树杀了"，
//! 用假进程就什么都没证。

#![allow(dead_code)] // 每个测试文件只用到其中一部分

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hbot_command::PlatformRunner;
use hbot_core::{CommandSpec, SandboxMode};

/// 当前测试二进制（用它自己当 helper 子进程：不需要额外构建产物）。
pub fn test_exe() -> PathBuf {
    std::env::current_exe().expect("current_exe 应可用")
}

/// 建一个干净的临时项目根。
///
/// 不用 `TestDir`（`hbot-workspace` 的）是为了让本 crate 的测试只依赖 `hbot-core`，
/// 与 `Cargo.toml` 里的依赖约束一致。
pub fn test_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("hbot-command-it-{tag}-{}", std::process::id()));
    if root.exists() {
        let _ = std::fs::remove_dir_all(&root);
    }
    std::fs::create_dir_all(&root).expect("建测试项目根");
    std::fs::canonicalize(&root).expect("规范化测试项目根")
}

/// 测试用的基准规格：`danger` 档（只要求进程树隔离）+ 允许清单里只有测试二进制自己。
pub fn danger_spec(root: &Path) -> CommandSpec {
    let exe = test_exe();
    let mut spec = CommandSpec::new(exe.display().to_string(), root);
    spec.allowed_programs = vec![exe.display().to_string()];
    spec.sandbox = SandboxMode::Danger;
    spec.timeout = Duration::from_secs(10);
    spec.max_output_bytes = 64 * 1024;
    spec
}

/// 真探测得到的 runner（本机实际能力）。测试自己会先断言所需能力可用，
/// 免得"隔离不可用"被误读成"实现有 bug"。
pub fn system_runner() -> PlatformRunner {
    PlatformRunner::system()
}

/// 跑命令用的完整规格：把 helper 种类与它要的子进程参数都塞进显式环境。
///
/// 程序是**测试二进制自己**，参数让它只跑那个被 ignore 的 `helper` 测试。
pub fn helper_spec(root: &Path, kind: &str) -> CommandSpec {
    let mut spec = danger_spec(root);
    spec.args = vec![
        "--exact".to_owned(),
        "helper".to_owned(),
        "--ignored".to_owned(),
        "--nocapture".to_owned(),
    ];
    spec.env
        .push(("HBOT_HELPER_KIND".to_owned(), kind.to_owned()));
    spec
}

/// 从 helper 的 stdout 里取 `KEY=<pid>`。
pub fn parse_pid(stdout: &str, key: &str) -> Option<u32> {
    stdout.lines().find_map(|line| {
        line.strip_prefix(key)?
            .strip_prefix('=')?
            .trim()
            .parse()
            .ok()
    })
}

/// 进程是否还活着（不是 zombie：已退出但未回收也算"没了"）。
#[cfg(windows)]
pub fn process_exists(pid: u32) -> bool {
    use std::ffi::c_void;

    const SYNCHRONIZE: u32 = 0x0010_0000;
    const WAIT_TIMEOUT: u32 = 0x0000_0102;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    // SAFETY: 三个都是纯查询调用；句柄在这里创建、在这里关闭，不跨线程共享。
    unsafe {
        let handle = OpenProcess(SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            // 打不开通常就是进程已经没了（或 PID 已被回收成别的进程，概率可忽略）。
            return false;
        }
        let wait = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        wait == WAIT_TIMEOUT
    }
}

/// 进程是否还活着（`/proc/<pid>` 存在即可）。
#[cfg(target_os = "linux")]
pub fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// 其它平台没有本文件对应的实现（测试会先跳过）。
#[cfg(not(any(windows, target_os = "linux")))]
pub fn process_exists(_pid: u32) -> bool {
    false
}

/// 等到进程消失（或超时）。
pub fn wait_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_exists(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !process_exists(pid)
}

/// 等文件出现（helper 用它把 pid 写给父进程）。
pub fn wait_for_file(path: &Path, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            let text = text.trim().to_owned();
            if !text.is_empty() {
                return Some(text);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}
