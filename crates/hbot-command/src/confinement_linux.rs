//! Linux 写隔离：Landlock LSM（内核 5.13+，即 ABI 1）。
//!
//! 规则集在**父进程**建好（`landlock_create_ruleset` + `landlock_add_rule`），在
//! `pre_exec` 里（fork 之后、`execve` 之前，且在 `containment_linux` 的 `setsid` 之后）
//! 用 `landlock_restrict_self` 装上。装不上就让子进程以 [`landlock::CHILD_FAILURE_EXIT`]
//! 退出并在 stderr 留标记，父进程把它翻译成
//! [`hbot_core::CommandDenied::IsolationUnavailable`]。
//!
//! # fail-closed（本文件最不能退让的一条）
//!
//! **任何"装不上就放行"的路径都是缺陷。** 内核没有 Landlock（`ENOSYS`）、LSM 未启用
//! （`EOPNOTSUPP`）、规则集建不出来、缓存目录属主不对、`pre_exec` 里任何一步失败 ——
//! 全部按"这台机器装不出写隔离"处理，`WorkspaceWrite` 档**继续拒绝执行**。
//! 宁可让这个档位在旧内核上不可用，也不许在没有 Landlock 的情况下把命令跑出去。
//!
//! # 为什么不用 `libc` crate
//!
//! 本 crate 只碰 7 个符号（`syscall` / `prctl` / `open` / `fcntl` / `write` / `_exit` /
//! `getuid`），为它们引入一个覆盖全平台的 crate 属于不必要的依赖面；`containment_linux.rs`
//! 从第一版起就是手写 `extern "C"`。权限位、ABI、错误码常量一律定义在
//! [`crate::landlock`]（纯逻辑，可在任意平台单测），本文件只做"把判据翻译成系统调用"。
//!
//! # 仍然管不到的东西
//!
//! Landlock 只覆盖**文件系统写**。网络（ABI 4 起才可管，本 crate 未申请）、`ioctl` 设备
//! （ABI 5，刻意不申请）、`/proc` 下的伪文件写、`io_uring` 提交的写（内核 5.13–6.x 的
//! 已知缺口，由 `io_uring` 的 `IORING_SETUP_SQPOLL`/注册文件路径绕过路径检查）、以及
//! `ptrace`/ASLR 之类与"写文件"无关的提权面，都不在这一层。ADR 0003 §5 记了这份清单。
//!
//! **本文件只做过交叉编译检查，未在真实 Linux 内核上运行过**（本机没有 WSL 发行版，
//! 见 ADR 0003 §6 的验收记录）。纯判据在 `landlock.rs` 里由跨平台单测覆盖。

use std::ffi::{CString, c_char, c_int, c_long, c_ulong, c_void};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::raw::c_uint;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use hbot_core::SandboxMode;

use crate::landlock::{self, LandlockAbi};

// ---------------------------------------------------------------------------
// 手写的 libc 符号（只声明本文件用到的，够用就好）
// ---------------------------------------------------------------------------

unsafe extern "C" {
    /// 通用系统调用入口。Landlock 没有 C 包装函数，只能走这个。
    fn syscall(number: c_long, ...) -> c_long;
    /// `prctl(2)`：用来设 `PR_SET_NO_NEW_PRIVS`。
    fn prctl(option: c_int, ...) -> c_int;
    /// `open(2)`：用 `O_PATH` 取一个"只用于定位 inode"的 fd 给 Landlock 规则。
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    /// `fcntl(2)`：给规则集 fd 加 `FD_CLOEXEC`。
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    /// `write(2)`：`pre_exec` 里往 stderr 留失败标记（不能分配内存，所以手写）。
    fn write(fd: c_int, buffer: *const c_void, count: usize) -> isize;
    /// `_exit(2)`：`pre_exec` 里失败后立刻退出，**不**跑 atexit、不 flush stdio。
    fn _exit(status: c_int) -> !;
    /// `getuid(2)`：核对私有缓存目录是不是"我们自己的"。
    fn getuid() -> c_uint;
}

/// `landlock_create_ruleset(2)` 的 syscall 号。
///
/// Landlock 的三个调用在**所有架构**上都编到 444/445/446（内核 5.13 引入时就统一了，
/// 没走 `arch/*/syscalls` 的差异分配），所以这里不需要按架构分叉。
const SYS_LANDLOCK_CREATE_RULESET: c_long = 444;
/// `landlock_add_rule(2)`。
const SYS_LANDLOCK_ADD_RULE: c_long = 445;
/// `landlock_restrict_self(2)`。
const SYS_LANDLOCK_RESTRICT_SELF: c_long = 446;

/// `landlock_create_ruleset` 的 flags：只读地查询 ABI 版本（不建规则集）。
const CREATE_RULESET_VERSION: c_long = 1;
/// `landlock_add_rule` 的规则类型：`LANDLOCK_RULE_PATH_BENEATH`。
const RULE_PATH_BENEATH: c_long = 1;

/// `PR_SET_NO_NEW_PRIVS`：`landlock_restrict_self` 的前置条件（必须有它才能降权）。
const PR_SET_NO_NEW_PRIVS: c_int = 38;

/// `O_PATH`：只定位 inode，不要读写权限（Landlock 规则只要一个"指向目录"的 fd）。
const O_PATH: c_int = 0o10000000;
/// `O_CLOEXEC`：不把这个 fd 泄漏给 `execve` 之后的程序。
const O_CLOEXEC: c_int = 0o2000000;
/// `F_SETFD`。
const F_SETFD: c_int = 2;
/// `FD_CLOEXEC`。
const FD_CLOEXEC: c_int = 1;

/// `ENOSYS`：内核里没有这个 syscall（Landlock 未编入 / 内核 < 5.13）。
const ENOSYS: i32 = 38;
/// `EOPNOTSUPP`：内核有 Landlock 但 LSM 未启用 / 被策略关掉。
const EOPNOTSUPP: i32 = 95;

/// `landlock_create_ruleset` 的入参（`struct landlock_ruleset_attr`，目前只有这一个字段）。
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

/// `landlock_path_beneath_attr`：内核要求 **12 字节紧凑布局**。
///
/// `packed` 不能省：不加的话 `parent_fd` 前面会多 4 字节填充（24 → 28 或 16 → 24），
/// `landlock_add_rule` 会返回 `EINVAL` —— 而 `EINVAL` 看起来跟"位申请错了"完全一样，
/// 极易被误判成"内核不支持"。
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Landlock 写隔离实例：持有已建好的规则集 fd，spawn 时在 `pre_exec` 里装。
pub(crate) struct WriteConfinement {
    ruleset: OwnedFd,
}

impl WriteConfinement {
    /// 建规则集：工作区子树 + 私有缓存目录可写，其余不可写。
    ///
    /// 失败一律返回 `Err`（调用方据此拒绝执行），**不会**退化成"不装隔离直接跑"。
    pub(crate) fn create(workspace_root: &Path) -> Result<Self, String> {
        let abi = landlock_abi()?;
        let granted = landlock::write_permissions(SandboxMode::WorkspaceWrite, abi)
            .ok_or_else(|| "内部错误：workspace-write 档应当申请写权限位".to_owned())?;
        let workspace = std::fs::canonicalize(workspace_root)
            .map_err(|error| format!("规范化工作区路径失败：{error}"))?;
        let cache = prepare_cache_root()?;
        let plan = landlock::plan(
            SandboxMode::WorkspaceWrite,
            abi,
            &workspace,
            Some(cache.as_path()),
        )
        .ok_or_else(|| "内部错误：workspace-write 档应当有规则计划".to_owned())?;

        // `handled` 是"内核要检查哪些写位"（与放行无关），`granted` 是"放行哪些"。
        // 两者在 workspace-write 档恰好相等，但对只读档就不等了 —— 别合并成一个变量。
        let ruleset = create_ruleset(plan.handled())?;
        let own_uid = unsafe { getuid() };
        for rule in plan.rules() {
            // 缓存目录是我们自己建的、又在 tmp 这种世界可写的地方，必须核对属主：
            // 别的本地用户抢先建一个同名目录并指向他处，就等于把可写范围挪出了边界。
            // 打开 fd 与核对属主、挂规则用的是**同一个** inode，没有 TOCTOU 窗口。
            let require_owner = if rule.path() == cache.as_path() {
                Some(own_uid)
            } else {
                None
            };
            add_path_rule(&ruleset, rule.path(), rule.access(), require_owner)?;
        }
        // `2>/dev/null` 这类重定向是"写一个文件"，Landlock 一样会检查。设备节点只给
        // `WRITE_FILE`（一个 `MAKE_*` 都没给，创建不了新节点），加不上不算削弱边界
        // —— 只会让某些命令失败 —— 所以不当作致命错误。
        //
        // 只在**真有放行**时才加：只读档（granted == 0）给设备开 WRITE_FILE 就等于
        // 凭空多出一类可写目标，与"一位都不放行"矛盾。
        if granted != 0 {
            for device in landlock::DEVICE_WRITE_PATHS {
                if let Err(error) = add_path_rule(
                    &ruleset,
                    Path::new(*device),
                    landlock::ACCESS_WRITE_FILE,
                    None,
                ) {
                    tracing::debug!(device, %error, "设备节点写规则未加上（不影响边界）");
                }
            }
        }

        tracing::debug!(
            workspace = %workspace.display(),
            cache = %cache.display(),
            abi = abi.raw(),
            handled = %landlock::permission_names(plan.handled()),
            granted = %landlock::permission_names(granted),
            "Linux 写隔离已建立（Landlock）"
        );
        Ok(Self { ruleset })
    }

    /// 把 Landlock 挂到子进程上：`pre_exec` 里 `fork` 之后、`execve` 之前。
    ///
    /// 闭包里**只能**做 async-signal-safe 的事：`prctl` / `syscall` / `write` / `_exit`。
    /// 不分配内存、不加锁、不碰 `std::io` —— fork 之后子进程只持有一份地址空间拷贝，
    /// 在这里做任何分配都可能撞上父进程当时持有的锁而死锁。
    pub(crate) fn configure(&self, command: &mut Command) {
        let ruleset = self.ruleset.as_raw_fd();
        // SAFETY: 闭包只调用 async-signal-safe 的系统调用；`ruleset` 是 Copy 的 fd 号，
        // 父进程在 `spawn` 返回前一直持有 `self`，fd 不会被关闭。
        unsafe {
            command.pre_exec(move || {
                if prctl(
                    PR_SET_NO_NEW_PRIVS,
                    1 as c_ulong,
                    0 as c_ulong,
                    0 as c_ulong,
                    0 as c_ulong,
                ) != 0
                {
                    child_fail(b"prctl(PR_SET_NO_NEW_PRIVS)", io::Error::last_os_error());
                }
                if syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset as c_long, 0 as c_long) != 0 {
                    child_fail(b"landlock_restrict_self", io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

/// 子进程装载失败：往 stderr 写一行标记，然后以哨兵码退出。
///
/// **绝不 `return Ok(())`。** 这条路径一旦静默通过，命令就在没有写隔离的情况下跑出去了，
/// 而这正是本模块存在的唯一理由。
fn child_fail(what: &[u8], error: io::Error) -> ! {
    let errno = error.raw_os_error().unwrap_or(-1);
    let mut buffer = [0u8; 192];
    let len = landlock::format_child_failure(&mut buffer, what, errno);
    // SAFETY: `buffer[..len]` 是刚写好的栈上字节；`write(2)` 是 async-signal-safe 的。
    // 部分写/被信号打断都无所谓 —— 父进程只要看到标记行就够。
    unsafe {
        write(2, buffer.as_ptr() as *const c_void, len);
        _exit(landlock::CHILD_FAILURE_EXIT);
    }
}

/// 私有缓存目录：建好、收紧权限、核对属主，返回规范化后的路径。
///
/// 为什么必须在 `plan` 之前做：缓存路径要作为**单独一条 Landlock 规则**放行，而
/// `landlock::plan` 的去重/越界判断都按绝对路径比较 —— 先 `canonicalize` 才能保证
/// "放行的路径"与"环境变量指向的路径"是同一个 inode（`/tmp` 在有些发行版上是符号链接）。
fn prepare_cache_root() -> Result<PathBuf, String> {
    let root = crate::cache::root();
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("创建私有缓存目录 {} 失败：{error}", root.display()))?;
    // 0700 是尽力而为：目录已经存在（别的用户建的）时 `set_permissions` 会失败，
    // 那种情况下面核对属主会把它拦下来，不需要在这里提前报错。
    let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
    let canonical = std::fs::canonicalize(&root)
        .map_err(|error| format!("规范化私有缓存目录 {} 失败：{error}", root.display()))?;
    Ok(canonical)
}

/// 建规则集。`handled_access_fs` 里的位才会被内核检查，其余访问（读、执行）不受影响。
fn create_ruleset(handled_access_fs: u64) -> Result<OwnedFd, String> {
    let attr = RulesetAttr { handled_access_fs };
    // SAFETY: `attr` 是 `#[repr(C)]` 且大小与内核的 `struct landlock_ruleset_attr` 一致；
    // 指针只在本次调用期间有效。
    let fd = unsafe {
        syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>() as c_long,
            0 as c_long,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(format!("landlock_create_ruleset 失败：{error}"));
    }
    // SAFETY: 成功分支返回的是内核刚给我们的新 fd，独占所有权。
    let owned = unsafe { OwnedFd::from_raw_fd(fd as c_int) };
    // 规则集 fd 不该落到 `execve` 之后的程序里：`restrict_self` 在 exec 之前就用完了。
    // SAFETY: 只是给一个有效 fd 设标志位，不会解引用。
    let _ = unsafe { fcntl(owned.as_raw_fd(), F_SETFD, FD_CLOEXEC) };
    Ok(owned)
}

/// 给规则集加一条 "`path` 之下允许 `allowed_access`" 的规则。
///
/// `require_owner = Some(uid)` 时还会核对这个 inode 的属主与类型 —— 只有私有缓存目录
/// 需要（它在世界可写的 tmp 下）。核对用的是**规则要挂上去的那个 fd**，所以中间没有
/// 可以被替换的窗口。
fn add_path_rule(
    ruleset: &OwnedFd,
    path: &Path,
    allowed_access: u64,
    require_owner: Option<c_uint>,
) -> Result<(), String> {
    let file = open_path_fd(path)?;
    if let Some(uid) = require_owner {
        let metadata = file
            .metadata()
            .map_err(|error| format!("读取 {} 的元数据失败：{error}", path.display()))?;
        if metadata.uid() != uid {
            return Err(format!(
                "私有缓存目录 {} 的属主是 uid {}，不是当前用户 uid {uid} —— \
                 拒绝把它纳入可写范围",
                path.display(),
                metadata.uid()
            ));
        }
        if !metadata.is_dir() {
            return Err(format!(
                "私有缓存路径 {} 不是目录 —— 拒绝把它纳入可写范围",
                path.display()
            ));
        }
    }
    let attr = PathBeneathAttr {
        allowed_access,
        parent_fd: file.as_raw_fd(),
    };
    // SAFETY: `attr` 是 `#[repr(C, packed)]` 的 12 字节布局（与内核结构一致）；
    // `parent_fd` 是上面 `O_PATH` 打开的有效 fd，本次调用期间不会关闭。
    let result = unsafe {
        syscall(
            SYS_LANDLOCK_ADD_RULE,
            ruleset.as_raw_fd() as c_long,
            RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0 as c_long,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        return Err(format!(
            "为 {} 添加 Landlock 规则失败：{error}",
            path.display()
        ));
    }
    Ok(())
}

/// 用 `O_PATH | O_CLOEXEC` 打开一个路径，得到一个"只用于定位 inode"的 fd。
///
/// `O_PATH` 不要求读权限（否则工作区里没有读位的子目录就打不开），也永远不会返回
/// 一个能读写的 fd —— 让规则引用它不会有副作用。
fn open_path_fd(path: &Path) -> Result<File, String> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("路径里含 NUL 字节：{}", path.display()))?;
    // SAFETY: `c_path` 是以 NUL 结尾的有效 C 字符串；`O_PATH` 不需要 mode 参数。
    let fd = unsafe { open(c_path.as_ptr(), O_PATH | O_CLOEXEC) };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(format!("打开 {} 失败：{error}", path.display()));
    }
    // SAFETY: 成功分支的 fd 是新拿到的，独占所有权，交给 `File` 负责关闭。
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// 这台机器能不能装出写隔离（Landlock 可用且三个 syscall 都走得通）。
///
/// 结果缓存：Landlock 的可用性在内核生命周期内不变，每条命令探测一次没有意义 —
/// 但探测失败也缓存，因为失败原因同样是稳定的。
pub(crate) fn probe() -> Result<(), String> {
    static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
    PROBE.get_or_init(probe_once).clone()
}

fn probe_once() -> Result<(), String> {
    let abi = landlock_abi()?;
    // handle 位看 ABI，放行位看档位 —— 两件事，别共用一个变量（见 `landlock::plan` 的注释）。
    let handled = landlock::handled_write_bits(abi);
    let granted = landlock::write_permissions(SandboxMode::WorkspaceWrite, abi)
        .ok_or_else(|| "内部错误：workspace-write 档应当申请写权限位".to_owned())?;
    // 建一个真规则集并加一条真规则：ABI 号有了但这两步被 seccomp/LSM 挡住的情况
    // 一样算"装不上"，不能只看版本号就宣称可用。
    let ruleset = create_ruleset(handled)?;
    let temp = std::env::temp_dir();
    add_path_rule(&ruleset, &temp, granted, None)
        .map_err(|error| format!("Landlock 规则集自检失败（{}）：{error}", temp.display()))?;
    Ok(())
}

/// 能力矩阵里那句话（ABI 版本 + 可写范围）。拿不到 ABI 时返回 `None`
/// （那时 `probe` 已经在报不可用，不需要再给一段说明）。
pub(crate) fn describe() -> Option<String> {
    let abi = landlock_abi().ok()?;
    Some(landlock::describe_zh(abi, &crate::cache::root()))
}

/// 查询 Landlock ABI 版本。
///
/// `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` 在 flags 里带
/// `VERSION` 时**不建规则集**，只返回版本号；失败时把真实 errno 文本带进原因字符串，
/// 让人一眼看出是"内核太旧"还是"LSM 没启用"。
fn landlock_abi() -> Result<LandlockAbi, String> {
    static ABI: OnceLock<Result<LandlockAbi, String>> = OnceLock::new();
    ABI.get_or_init(landlock_abi_once).clone()
}

fn landlock_abi_once() -> Result<LandlockAbi, String> {
    // SAFETY: 传空指针 + 长度 0 + `VERSION` 标志是文档规定的"只查版本"用法，内核不会解引用。
    let raw = unsafe {
        syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0 as c_long,
            CREATE_RULESET_VERSION,
        )
    };
    if raw < 0 {
        let error = io::Error::last_os_error();
        let errno = error.raw_os_error().unwrap_or(-1);
        return Err(match errno {
            ENOSYS => format!(
                "内核不支持 Landlock（ENOSYS，errno={ENOSYS}）：需要 5.13 及以上内核；{error}"
            ),
            EOPNOTSUPP => format!(
                "内核未启用 Landlock（EOPNOTSUPP，errno={EOPNOTSUPP}）：需在内核 LSM 列表里启用；{error}"
            ),
            _ => format!("Landlock 探测失败（errno={errno}）：{error}"),
        });
    }
    Ok(LandlockAbi::from_raw(raw as u32))
}
