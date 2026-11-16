//! Linux 写隔离：Landlock LSM（内核 5.13+）。
//!
//! 规则集在父进程建好、在 `pre_exec` 里（fork 之后、exec 之前）用
//! `landlock_restrict_self` 装上 —— 装不上就让 spawn 失败，绝不静默放行。
//!
//! **本文件只做过交叉编译检查，未在 Linux 上运行。** 验收命令是
//! `cargo check --target x86_64-unknown-linux-gnu -p hbot-command`（见 ADR 0003 §6）。
//!
//! 设计取舍：
//! - 只处理「写」类访问位（`handled_access_fs` 不含 `READ_FILE` / `EXECUTE`），
//!   读与执行不受影响 —— 与 Windows 侧「只做第二道写检查」等价。
//! - 不处理 `IOCTL_DEV`：处理了就要为 `/dev/null` 等设备单独开规则，而设备上的
//!   ioctl 不是「写文件」，拦它只会让常用命令莫名失败。
//! - `REFER`（ABI ≥ 2）与 `TRUNCATE`（ABI ≥ 3）按内核版本按需加进 handled 集：
//!   把内核不认识的位置 1 会得到 `EINVAL`，那不是「不支持写隔离」而是「参数错了」。
//! - 内核没有 Landlock（`ENOSYS`）或未启用（`EOPNOTSUPP`）→ 探测失败 → `WorkspaceWrite`
//!   档继续拒绝。**不降级**：没有 Landlock 就不跑命令。

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

/// `LANDLOCK_ACCESS_FS_WRITE_FILE`
const ACCESS_WRITE_FILE: u64 = 1 << 1;
/// `LANDLOCK_ACCESS_FS_REMOVE_DIR`
const ACCESS_REMOVE_DIR: u64 = 1 << 4;
/// `LANDLOCK_ACCESS_FS_REMOVE_FILE`
const ACCESS_REMOVE_FILE: u64 = 1 << 5;
/// `LANDLOCK_ACCESS_FS_MAKE_CHAR`
const ACCESS_MAKE_CHAR: u64 = 1 << 6;
/// `LANDLOCK_ACCESS_FS_MAKE_DIR`
const ACCESS_MAKE_DIR: u64 = 1 << 7;
/// `LANDLOCK_ACCESS_FS_MAKE_REG`
const ACCESS_MAKE_REG: u64 = 1 << 8;
/// `LANDLOCK_ACCESS_FS_MAKE_SOCK`
const ACCESS_MAKE_SOCK: u64 = 1 << 9;
/// `LANDLOCK_ACCESS_FS_MAKE_FIFO`
const ACCESS_MAKE_FIFO: u64 = 1 << 10;
/// `LANDLOCK_ACCESS_FS_MAKE_BLOCK`
const ACCESS_MAKE_BLOCK: u64 = 1 << 11;
/// `LANDLOCK_ACCESS_FS_MAKE_SYM`
const ACCESS_MAKE_SYM: u64 = 1 << 12;
/// `LANDLOCK_ACCESS_FS_REFER`（ABI 2 起）
const ACCESS_REFER: u64 = 1 << 13;
/// `LANDLOCK_ACCESS_FS_TRUNCATE`（ABI 3 起）
const ACCESS_TRUNCATE: u64 = 1 << 14;

/// `landlock_create_ruleset` 的 flags：只读地查询 ABI 版本。
const CREATE_RULESET_VERSION: libc::c_long = 1;
/// `landlock_add_rule` 的规则类型。
const RULE_PATH_BENEATH: libc::c_long = 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

/// `landlock_path_beneath_attr`：内核要求 12 字节紧凑布局（`packed` 不能省，
/// 否则字段之间会多出 4 字节填充，`add_rule` 返回 `EINVAL`）。
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Landlock 写隔离实例：持有规则集 fd，spawn 时交给子进程。
pub(crate) struct WriteConfinement {
    ruleset: OwnedFd,
}

impl WriteConfinement {
    pub(crate) fn create(workspace_root: &Path) -> Result<Self, String> {
        let abi = landlock_abi()?;
        let handled = handled_access(abi);
        let ruleset = create_ruleset(handled)?;

        let workspace = std::fs::canonicalize(workspace_root)
            .map_err(|error| format!("规范化工作区路径失败：{error}"))?;
        // 工作区内：所有被处理的写访问都允许。
        add_path_rule(&ruleset, &workspace, handled)?;
        // 命令的 `2>/dev/null` 之类要能写设备节点；设备写不是文件写，单独允许。
        // 加不上不算削弱边界（只会让某些命令失败），所以不当作致命错误。
        for device in ["/dev/null", "/dev/zero", "/dev/tty"] {
            let _ = add_path_rule(&ruleset, Path::new(device), ACCESS_WRITE_FILE);
        }

        tracing::debug!(
            workspace = %workspace.display(),
            abi,
            handled = format!("0x{handled:x}"),
            "Linux 写隔离已建立（Landlock）"
        );
        Ok(Self { ruleset })
    }

    pub(crate) fn configure(&self, command: &mut Command) {
        let ruleset = self.ruleset.as_raw_fd();
        // SAFETY: 闭包里只做 async-signal-safe 的事（`prctl` / `syscall`），不分配内存、
        // 不加锁、不碰 stdio。`ruleset` 是 Copy 的 fd 号，父进程保活到 spawn 结束。
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(
                    libc::PR_SET_NO_NEW_PRIVS,
                    1 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                ) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::syscall(
                    libc::SYS_landlock_restrict_self,
                    ruleset as libc::c_long,
                    0 as libc::c_long,
                ) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

/// 这台机器能不能装出写隔离（Landlock 可用且规则集建得出来）。结果缓存：每条命令都探测
/// 一次没有意义，Landlock 的可用性在内核生命周期内不变。
pub(crate) fn probe() -> Result<(), String> {
    static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
    PROBE.get_or_init(probe_once).clone()
}

fn probe_once() -> Result<(), String> {
    let abi = landlock_abi()?;
    // 建一个空规则集：ABI 有了但这一步失败（例如被 seccomp/LSM 策略挡住）同样算不可用。
    let ruleset = create_ruleset(handled_access(abi))?;
    drop(ruleset);
    Ok(())
}

fn landlock_abi() -> Result<u32, String> {
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0 as libc::c_long,
            CREATE_RULESET_VERSION,
        )
    };
    if abi < 0 {
        let error = io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::ENOSYS) => "内核不支持 Landlock（ENOSYS）：需要 5.13 及以上内核".to_string(),
            Some(libc::EOPNOTSUPP) => {
                "内核未启用 Landlock（EOPNOTSUPP）：需在 LSM 列表里启用".to_string()
            }
            _ => format!("Landlock 探测失败：{error}"),
        });
    }
    Ok(abi as u32)
}

/// 该 ABI 支持的处理位集合。
fn handled_access(abi: u32) -> u64 {
    let mut access = ACCESS_WRITE_FILE
        | ACCESS_REMOVE_DIR
        | ACCESS_REMOVE_FILE
        | ACCESS_MAKE_CHAR
        | ACCESS_MAKE_DIR
        | ACCESS_MAKE_REG
        | ACCESS_MAKE_SOCK
        | ACCESS_MAKE_FIFO
        | ACCESS_MAKE_BLOCK
        | ACCESS_MAKE_SYM;
    if abi >= 2 {
        access |= ACCESS_REFER;
    }
    if abi >= 3 {
        access |= ACCESS_TRUNCATE;
    }
    access
}

fn create_ruleset(handled_access_fs: u64) -> Result<OwnedFd, String> {
    let attr = RulesetAttr { handled_access_fs };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>() as libc::c_long,
            0 as libc::c_long,
        )
    };
    if fd < 0 {
        return Err(format!(
            "landlock_create_ruleset 失败：{}",
            io::Error::last_os_error()
        ));
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    // fd 不该漏给 exec 后的程序：restrict_self 在 pre_exec 里（exec 之前）已经用过它。
    unsafe {
        libc::fcntl(owned.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    Ok(owned)
}

fn add_path_rule(ruleset: &OwnedFd, path: &Path, allowed_access: u64) -> Result<(), String> {
    // `O_PATH`：只要能定位到路径即可，不要求读权限；`O_CLOEXEC` 让 fd 不泄漏给 exec。
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("路径含 NUL：{}", path.display()))?;
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(format!(
            "打开 {} 失败：{}",
            path.display(),
            io::Error::last_os_error()
        ));
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let attr = PathBeneathAttr {
        allowed_access,
        parent_fd: owned.as_raw_fd(),
    };
    let result = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd() as libc::c_long,
            RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0 as libc::c_long,
        )
    };
    if result != 0 {
        return Err(format!(
            "为 {} 添加 Landlock 规则失败：{}",
            path.display(),
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ACCESS_REFER, ACCESS_TRUNCATE, ACCESS_WRITE_FILE, handled_access};

    #[test]
    fn handled_bits_follow_the_abi() {
        assert_eq!(handled_access(1) & ACCESS_WRITE_FILE, ACCESS_WRITE_FILE);
        assert_eq!(handled_access(1) & ACCESS_REFER, 0);
        assert_eq!(handled_access(1) & ACCESS_TRUNCATE, 0);
        assert_ne!(handled_access(2) & ACCESS_REFER, 0);
        assert_eq!(handled_access(2) & ACCESS_TRUNCATE, 0);
        assert_ne!(handled_access(3) & ACCESS_TRUNCATE, 0);
    }
}
