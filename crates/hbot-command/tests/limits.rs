//! 资源上限（`CommandSpec::limits`）的证据测试：**真跑子进程**，不 mock 内核行为。
//!
//! 这里证的六件事：
//! - `cpu_limit_terminates_a_busy_loop`：`cpu_ms = 200` 的死循环在约 1 秒内被杀，
//!   且拒绝原因是 **CPU**（`HBOT_COMMAND_CPU_LIMIT`），不是超时；
//! - `memory_limit_stops_a_greedy_allocator`：`memory_bytes` 到顶后程序**跑不完**
//!   （要么被我们判超限，要么分配失败自己退出）；
//! - `process_limit_refuses_an_extra_child`：`process_count = 1` 时再 spawn 子进程，
//!   内核拒绝创建（**真实表现是子进程创建失败，不是我们主动拒**）；
//! - `io_limit_is_enforced_or_refused`：`read_bytes` 在 Windows 上由 10 ms 轮询兑现，
//!   在 Linux 上**必须** `IsolationUnavailable`（fail-closed，绝不静默放行）；
//! - `usage_reports_measured_numbers`：`CommandOutput::usage` 真的带回了用量；
//! - `unsupported_limits_are_never_silently_ignored`：能力矩阵说做不到的限额，一律拒绝。
//!
//! 子进程用**当前测试二进制自己**（`current_exe` + `--exact helper --ignored`），
//! helper 按 `HBOT_HELPER_KIND` 分派行为。

mod common;

use std::time::{Duration, Instant};

use hbot_command::PlatformRunner;
use hbot_core::{
    CommandDenied, CommandOutput, CommandRunner, CommandSpec, ResourceLimits, SandboxMode,
};

/// 本机必须支持进程树隔离，否则这不是"实现不足"而是"平台不支持"。
fn require_containment() {
    let caps = PlatformRunner::system().capabilities();
    assert!(
        caps.supports(SandboxMode::Danger),
        "本机无法建立进程树隔离，本测试无法验证（{}）",
        caps.summary_zh()
    );
}

/// 跑一个 helper 子进程，返回（结果，墙钟耗时）。
fn run_helper(
    tag: &str,
    kind: &str,
    limits: ResourceLimits,
) -> (Result<CommandOutput, CommandDenied>, Duration) {
    let root = common::test_root(tag);
    let mut spec: CommandSpec = common::helper_spec(&root, kind);
    spec.limits = limits;
    // 超时留足：被限额杀掉的命令**不该**走到超时那条路（否则就测不出限额生效）。
    spec.timeout = Duration::from_secs(30);
    let started = Instant::now();
    let asked = limits;
    let outcome = PlatformRunner::system().run(spec);
    let elapsed = started.elapsed();
    // 实机证据：`--nocapture` 时看得见"请求了什么、真实结果是什么、用了多久"。
    eprintln!(
        "[limits] kind={kind} limits={asked:?} elapsed={elapsed:?} -> {}",
        message_of(&outcome)
    );
    (outcome, elapsed)
}

fn limits_of(f: impl FnOnce(&mut ResourceLimits)) -> ResourceLimits {
    let mut limits = ResourceLimits::default();
    f(&mut limits);
    limits
}

fn message_of(outcome: &Result<CommandOutput, CommandDenied>) -> String {
    match outcome {
        Ok(output) => format!(
            "stdout={:?} stderr={:?} status={:?}",
            output.stdout, output.stderr, output.status
        ),
        Err(denied) => format!("{denied}（code={}）", denied.code()),
    }
}

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

#[test]
fn cpu_limit_terminates_a_busy_loop() {
    require_containment();
    let (outcome, elapsed) =
        run_helper("cpu", "spin", limits_of(|limits| limits.cpu_ms = Some(200)));
    let denied = match &outcome {
        Ok(_) => panic!(
            "死循环必须被 CPU 上限杀掉，却正常结束：{}",
            message_of(&outcome)
        ),
        Err(denied) => denied,
    };
    assert!(
        matches!(denied, CommandDenied::CpuLimit { .. }),
        "拒绝原因必须是 CPU 上限（不是超时、不是别的）：{}",
        message_of(&outcome)
    );
    assert_eq!(denied.code(), "HBOT_COMMAND_CPU_LIMIT");
    if let CommandDenied::CpuLimit { used_ms, limit_ms } = denied {
        assert_eq!(*limit_ms, 200, "拒绝里必须带回调用方请求的上限");
        assert!(*used_ms >= 200, "实测用量必须不小于上限，实测 {used_ms} ms");
    }
    // Linux 的 RLIMIT_CPU 是秒级向上取整（200 ms → 1 s），所以给到 5 秒。
    assert!(
        elapsed < Duration::from_secs(5),
        "CPU 上限生效太慢：耗时 {elapsed:?}（期望约 1 秒内）"
    );
}

#[test]
fn cpu_time_is_reported_in_usage_even_without_a_limit() {
    require_containment();
    let (outcome, _) = run_helper("burn", "burn", ResourceLimits::default());
    let output = outcome.expect("烧 200 ms CPU 的命令应当正常退出");
    assert_eq!(
        output.status,
        hbot_core::CommandStatus::Exited {
            code: Some(0),
            signal: None
        },
        "helper 应当正常收尾"
    );
    assert!(
        output.usage.cpu_ms >= 100,
        "usage.cpu_ms 必须真的测到用量（0 只表示平台没测），实测 {} ms",
        output.usage.cpu_ms
    );
}

// ---------------------------------------------------------------------------
// 内存
// ---------------------------------------------------------------------------

#[test]
fn memory_limit_stops_a_greedy_allocator() {
    require_containment();
    let (outcome, _) = run_helper(
        "memory",
        "hog",
        limits_of(|limits| limits.memory_bytes = Some(64 * 1024 * 1024)),
    );
    match &outcome {
        // 我们判了内存超限。
        Err(denied) => assert!(
            matches!(denied, CommandDenied::MemoryLimit { .. }),
            "只可能是内存上限：{}",
            message_of(&outcome)
        ),
        // 内核让分配失败，程序自己报错退出 —— 这正是 Windows job 内存上限的真实表现。
        Ok(output) => {
            assert!(
                !output.stdout.contains("HOG=done"),
                "内存上限没生效：分配器一路跑到结束（{}）",
                message_of(&outcome)
            );
            assert_ne!(
                output.status,
                hbot_core::CommandStatus::Exited {
                    code: Some(0),
                    signal: None
                },
                "内存到顶时程序不该干净退出（{}）",
                message_of(&outcome)
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 并发进程数
// ---------------------------------------------------------------------------

#[test]
fn process_limit_refuses_an_extra_child() {
    require_containment();
    let caps = PlatformRunner::system().capabilities();
    let (outcome, elapsed) = run_helper(
        "process",
        "spawner",
        limits_of(|limits| limits.process_count = Some(1)),
    );
    if !caps.process_limit {
        // 平台做不到就**必须**拒绝（fail-closed），这是 Linux 上的预期。
        match &outcome {
            Ok(_) => panic!(
                "平台不支持进程数上限时必须拒绝执行，实际却跑了：{}",
                message_of(&outcome)
            ),
            Err(denied) => assert!(
                matches!(denied, CommandDenied::IsolationUnavailable { .. }),
                "必须是 IsolationUnavailable：{}",
                message_of(&outcome)
            ),
        }
        return;
    }

    match &outcome {
        // **本机的真实表现**：预算 1 连"根进程 + conhost.exe"都装不下，
        // 10 ms 轮询在第一个采样点就看到 2 个活跃进程，直接 `TerminateJobObject`
        // 并以 `ProcessLimit` 报错（实测约 25 ms，子进程往往还没起来）。
        Err(denied) => assert!(
            matches!(denied, CommandDenied::ProcessLimit { .. }),
            "只可能是进程数上限：{}",
            message_of(&outcome)
        ),
        // 另一条可能的真实路径（慢机器 / 采样点错开时）：内核在**创建第 N+1 个进程时**
        // 让它失败，helper 只能把失败打印出来 —— 这时不是我们主动拒，是内核拒。
        Ok(output) => {
            assert!(
                !output.stdout.contains("SPAWN=ok"),
                "进程数上限没生效：子进程居然起来了（{}）",
                message_of(&outcome)
            );
            assert!(
                output.stdout.contains("SPAWN=fail") || !output.stderr.is_empty(),
                "helper 应当报出子进程创建失败（{}）",
                message_of(&outcome)
            );
            assert_ne!(
                output.status,
                hbot_core::CommandStatus::Exited {
                    code: Some(0),
                    signal: None
                },
                "子进程被拒后 helper 不该当作成功（{}）",
                message_of(&outcome)
            );
        }
    }
    assert!(
        elapsed < Duration::from_secs(20),
        "进程数上限不该拖到超时：耗时 {elapsed:?}"
    );
}

/// 预算**够用**时必须放行：这条与上一条是一对。
///
/// 上一条证"超出上限被挡住"，这一条证"没超不被误杀"——少一条的话，把判据从 `>` 写成
/// `>=`、"无论多少都杀"也照样能让上一条通过。
///
/// **Windows 的树里有一个不速之客：`conhost.exe`。** 命令是用 `CREATE_NO_WINDOW`
/// 起的，内核仍会给它配一个（隐藏的）控制台宿主，而 `conhost.exe` 是根进程的子进程、
/// 因此也在作业里。所以"根进程 + 1 个子进程"实际占 **3** 个名额，预算得给 3 而不是 2。
/// 这不是本 crate 的记账把戏，是 Windows 控制台的真实形状（`Get-CimInstance Win32_Process`
/// 能看到 `limits-*.exe → conhost.exe` 这条父子关系）。
#[test]
fn process_limit_allows_a_child_when_the_budget_covers_it() {
    require_containment();
    let caps = PlatformRunner::system().capabilities();
    if !caps.process_limit {
        return; // 平台做不到，上一条已经断言了 fail-closed。
    }
    let root = common::test_root("process-ok");
    let mut spec: CommandSpec = common::helper_spec(&root, "spawner");
    spec.env
        .push(("HBOT_SPAWN_SLEEP_MS".to_owned(), "300".to_owned()));
    // 根进程 + conhost.exe + PING.EXE = 3。
    spec.limits = limits_of(|limits| limits.process_count = Some(3));
    spec.timeout = Duration::from_secs(30);

    let outcome = PlatformRunner::system().run(spec);
    let output = match outcome {
        Ok(output) => output,
        Err(denied) => panic!(
            "这棵树正好 3 个进程（根 + conhost + 子进程），等于上限 3，不该被杀：{denied}（code={}）",
            denied.code()
        ),
    };
    assert!(
        output.stdout.contains("SPAWN=ok"),
        "预算够时子进程应当创建成功：{:?}",
        output.stdout
    );
    assert_eq!(
        output.status,
        hbot_core::CommandStatus::Exited {
            code: Some(0),
            signal: None
        }
    );
}

// ---------------------------------------------------------------------------
// I/O 字节
// ---------------------------------------------------------------------------

#[test]
fn io_limit_is_enforced_or_refused() {
    require_containment();
    let caps = PlatformRunner::system().capabilities();
    let root = common::test_root("io");
    // 1 MiB 的源文件：远超 1 KiB 上限。
    let payload = root.join("payload.bin");
    std::fs::write(&payload, vec![b'z'; 1024 * 1024]).expect("写测试载荷");

    let mut spec: CommandSpec = common::helper_spec(&root, "reader");
    spec.env
        .push(("HBOT_LIMITS_FILE".to_owned(), payload.display().to_string()));
    spec.limits = limits_of(|limits| limits.read_bytes = Some(1024));
    spec.timeout = Duration::from_secs(30);

    let outcome = PlatformRunner::system().run(spec);
    if !caps.io_limit {
        match &outcome {
            Ok(_) => panic!(
                "平台不支持 I/O 上限时必须拒绝执行，不能静默放行：{}",
                message_of(&outcome)
            ),
            Err(denied) => {
                assert!(
                    matches!(denied, CommandDenied::IsolationUnavailable { .. }),
                    "必须是 IsolationUnavailable：{}",
                    message_of(&outcome)
                );
                assert_eq!(denied.code(), "HBOT_COMMAND_ISOLATION_UNAVAILABLE");
            }
        }
        return;
    }
    // Windows：轮询读到 `ReadTransferCount` 超限就杀树。
    // 注意作业对象的 I/O 记账连**加载镜像/DLL** 的读都算，所以 1 KiB 这种上限
    // 往往在 helper 还没读到自己的文件时就已经到顶了 —— 这仍然是"上限生效"。
    match &outcome {
        Ok(_) => panic!(
            "读 1 MiB 必须触到 1 KiB 的读取上限：{}",
            message_of(&outcome)
        ),
        Err(denied) => {
            assert!(
                matches!(
                    denied,
                    CommandDenied::IoLimit {
                        direction: hbot_core::IoDirection::Read,
                        ..
                    }
                ),
                "必须是读取方向的 I/O 上限：{}",
                message_of(&outcome)
            );
            assert_eq!(denied.code(), "HBOT_COMMAND_IO_LIMIT");
        }
    }
}

#[test]
fn write_limit_is_enforced_or_refused() {
    require_containment();
    let caps = PlatformRunner::system().capabilities();
    let root = common::test_root("write");
    let target = root.join("out.bin");
    let mut spec: CommandSpec = common::helper_spec(&root, "writer");
    spec.env
        .push(("HBOT_LIMITS_FILE".to_owned(), target.display().to_string()));
    spec.limits = limits_of(|limits| limits.write_bytes = Some(1024));
    spec.timeout = Duration::from_secs(30);

    let outcome = PlatformRunner::system().run(spec);
    if !caps.io_limit {
        match &outcome {
            Ok(_) => panic!(
                "平台不支持 I/O 上限时必须拒绝执行，不能静默放行：{}",
                message_of(&outcome)
            ),
            Err(denied) => assert!(
                matches!(denied, CommandDenied::IsolationUnavailable { .. }),
                "必须是 IsolationUnavailable：{}",
                message_of(&outcome)
            ),
        }
        return;
    }
    match &outcome {
        Ok(_) => panic!(
            "写 1 MiB 必须触到 1 KiB 的写出上限：{}",
            message_of(&outcome)
        ),
        Err(denied) => assert!(
            matches!(
                denied,
                CommandDenied::IoLimit {
                    direction: hbot_core::IoDirection::Write,
                    ..
                }
            ),
            "必须是写出方向的 I/O 上限：{}",
            message_of(&outcome)
        ),
    }
}

// ---------------------------------------------------------------------------
// usage 与能力矩阵
// ---------------------------------------------------------------------------

#[test]
fn usage_reports_measured_numbers() {
    require_containment();
    let (outcome, _) = run_helper("usage", "burn", ResourceLimits::default());
    let output = outcome.expect("无限制的 helper 应当正常退出");
    // CPU 时间是两个平台都能测的（Windows 读作业记账，Linux 读 getrusage）。
    assert!(
        output.usage.cpu_ms > 0,
        "usage.cpu_ms 必须被测量：{output:?}"
    );
    // 峰值内存只有 Windows 有整棵树口径；Linux 的 ru_maxrss 是上界，也应当非 0。
    assert!(
        output.usage.peak_memory_bytes > 0,
        "usage.peak_memory_bytes 必须被测量：{output:?}"
    );
}

#[test]
fn unsupported_limits_are_never_silently_ignored() {
    require_containment();
    let caps = PlatformRunner::system().capabilities();
    // 逐个探测：能力矩阵说"不支持"的，请求它就必须被拒绝（绝不静默忽略）。
    let probes: [(&str, ResourceLimits); 4] = [
        ("cpu", limits_of(|limits| limits.cpu_ms = Some(500))),
        (
            "memory",
            limits_of(|limits| limits.memory_bytes = Some(64 * 1024 * 1024)),
        ),
        ("io", limits_of(|limits| limits.read_bytes = Some(1024))),
        (
            "process",
            limits_of(|limits| limits.process_count = Some(2)),
        ),
    ];
    let supported = [
        caps.cpu_limit,
        caps.memory_limit,
        caps.io_limit,
        caps.process_limit,
    ];
    for ((kind, limits), supported) in probes.into_iter().zip(supported) {
        let (outcome, _) = run_helper("supported", "noop", limits);
        if supported {
            assert!(
                outcome.is_ok(),
                "能力矩阵说支持 {kind} 上限，实际却被拒（面板在骗人）：{}",
                message_of(&outcome)
            );
        } else {
            match &outcome {
                Ok(_) => panic!(
                    "能力矩阵说 {kind} 上限做不到，请求它就必须拒绝执行：{}",
                    message_of(&outcome)
                ),
                Err(denied) => {
                    assert!(
                        matches!(denied, CommandDenied::IsolationUnavailable { .. }),
                        "{kind} 的拒绝必须是 IsolationUnavailable：{}",
                        message_of(&outcome)
                    );
                    assert!(
                        !denied.to_string().is_empty(),
                        "拒绝理由不能为空（要告诉调用方为什么做不到）"
                    );
                }
            }
        }
    }
}

#[test]
fn null_limits_keep_the_old_behaviour() {
    require_containment();
    let (outcome, _) = run_helper("null", "noop", ResourceLimits::default());
    let output = outcome.expect("默认（不限）必须与旧行为一致：命令正常跑完");
    assert_eq!(
        output.status,
        hbot_core::CommandStatus::Exited {
            code: Some(0),
            signal: None
        }
    );
    assert!(output.stdout.contains("NOOP=ok"));
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
        // 死循环：纯算术，不分配、不读盘，只烧 CPU。
        "spin" => {
            let mut acc: u64 = 0;
            loop {
                acc = acc.wrapping_mul(6364136223846793005).wrapping_add(1);
                std::hint::black_box(acc);
            }
        }
        // 烧约 200 ms CPU 后干净退出（测 usage 回填）。
        "burn" => {
            let deadline = Instant::now() + Duration::from_millis(200);
            let mut acc: u64 = 0;
            while Instant::now() < deadline {
                for _ in 0..100_000 {
                    acc = acc.wrapping_mul(6364136223846793005).wrapping_add(1);
                }
                std::hint::black_box(acc);
            }
            println!("BURN=done");
            let _ = std::io::stdout().flush();
        }
        // 持续分配、且**真的碰到**每一页（只 reserve 不写不会提交）。
        "hog" => {
            let mut held: Vec<Vec<u8>> = Vec::new();
            let chunk = 8 * 1024 * 1024;
            let mut failed = false;
            for _ in 0..256 {
                let mut block: Vec<u8> = Vec::new();
                if block.try_reserve_exact(chunk).is_err() {
                    failed = true;
                    break;
                }
                block.resize(chunk, 1);
                held.push(block);
            }
            if failed {
                println!("HOG=fail");
                let _ = std::io::stdout().flush();
                std::process::exit(3);
            }
            println!("HOG=done");
            let _ = std::io::stdout().flush();
        }
        // 起一个子进程：作业对象的进程数上限会在这里拒绝。
        "spawner" => {
            match spawn_child() {
                Ok(pid) => println!("SPAWN=ok({pid})"),
                Err(error) => println!("SPAWN=fail({error})"),
            }
            let _ = std::io::stdout().flush();
            // 默认睡 5 秒（给上限留出观测窗口）；"上限够用"的用例把它调小。
            let sleep_ms = std::env::var("HBOT_SPAWN_SLEEP_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(5_000);
            std::thread::sleep(Duration::from_millis(sleep_ms));
        }
        // 读一个大文件：作业对象的 I/O 记账会先到顶。
        "reader" => {
            let path = std::env::var("HBOT_LIMITS_FILE").expect("helper 需要 HBOT_LIMITS_FILE");
            match std::fs::read(&path) {
                Ok(bytes) => println!("BYTES={}", bytes.len()),
                Err(error) => println!("READ=fail({error})"),
            }
            let _ = std::io::stdout().flush();
        }
        // 写一个大文件：写出方向的 I/O 记账会先到顶。
        "writer" => {
            let path = std::env::var("HBOT_LIMITS_FILE").expect("helper 需要 HBOT_LIMITS_FILE");
            match std::fs::write(&path, vec![b'w'; 1024 * 1024]) {
                Ok(()) => println!("WROTE=ok"),
                Err(error) => println!("WRITE=fail({error})"),
            }
            let _ = std::io::stdout().flush();
        }
        "noop" => {
            println!("NOOP=ok");
            let _ = std::io::stdout().flush();
        }
        other => panic!("未知 helper 种类：{other:?}"),
    }
}

#[cfg(windows)]
fn spawn_child() -> std::io::Result<u32> {
    use std::process::{Command, Stdio};

    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
    let ping = std::path::Path::new(&system_root)
        .join("System32")
        .join("PING.EXE");
    Command::new(ping)
        .args(["-n", "120", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|child| child.id())
}

#[cfg(target_os = "linux")]
fn spawn_child() -> std::io::Result<u32> {
    use std::process::{Command, Stdio};

    Command::new("/bin/sleep")
        .arg("120")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|child| child.id())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn spawn_child() -> std::io::Result<u32> {
    Err(std::io::Error::other("本平台未实现子进程夹具"))
}
