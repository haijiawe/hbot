//! Landlock 的**纯逻辑**层：ABI 版本 → 权限位、档位 → 规则计划、路径 → 是否可写。
//!
//! 为什么单独一个模块：真正的 `syscall` / `pre_exec` 只能在 Linux 上编译，而"按 ABI
//! 收窄权限位""哪些路径该被放行""工作区外的路径必须拒绝"这些**判据**恰好是本实现最容易
//! 写错的地方（申请内核不认识的位会 `EINVAL`；前缀比较写错会放行 `…-outside`）。
//! 把它们抽成不碰内核的纯函数，就能在任意平台（含本机的 Windows）用单测钉住，
//! 而不是拿"交叉编译过了"当证据。
//!
//! 判据的语义（与 ADR 0003 对齐）：
//!
//! - `danger`：**不套** Landlock（[`plan`] 返回 `None`）—— 这一档只要求进程树隔离；
//! - `read_only`：一个写权限都不申请（[`write_permissions`] 返回 `Some(0)`）。
//!   **注意**本 crate 的 runner 仍然直接拒绝只读档（Landlock 给不出"只读视图"，
//!   产品决定是这类档位连命令都不跑）；这里保留形状是为了让"只读 = 零写权限"这条
//!   判据可测、可复用，而不是在只读档里偷偷跑命令；
//! - `workspace-write`：工作区子树 + 私有缓存目录可写，其余一律不可写。
//!
//! 可写范围只有"规则里显式列出的路径"。Landlock 是**只减不增**的：给一个进程本来就没
//! 权限的路径加规则不会让它变得可写。所以"多列了一条路径"不会提权，"少列一条"只会让
//! 命令失败 —— 两个方向的后果都不对称地偏向安全。

#![allow(dead_code)] // 非 Linux 平台只有单测用到；Linux 上才进产品路径

use std::path::{Path, PathBuf};

use hbot_core::SandboxMode;

/// `LANDLOCK_ACCESS_FS_WRITE_FILE`：打开文件写入（含 `O_APPEND`）。
pub(crate) const ACCESS_WRITE_FILE: u64 = 1 << 1;
/// `LANDLOCK_ACCESS_FS_REMOVE_DIR`：`rmdir`。
pub(crate) const ACCESS_REMOVE_DIR: u64 = 1 << 4;
/// `LANDLOCK_ACCESS_FS_REMOVE_FILE`：`unlink`。
pub(crate) const ACCESS_REMOVE_FILE: u64 = 1 << 5;
/// `LANDLOCK_ACCESS_FS_MAKE_CHAR`：`mknod` 字符设备（实际还要 `CAP_MKNOD`）。
pub(crate) const ACCESS_MAKE_CHAR: u64 = 1 << 6;
/// `LANDLOCK_ACCESS_FS_MAKE_DIR`：`mkdir`。
pub(crate) const ACCESS_MAKE_DIR: u64 = 1 << 7;
/// `LANDLOCK_ACCESS_FS_MAKE_REG`：`open(O_CREAT)`。
pub(crate) const ACCESS_MAKE_REG: u64 = 1 << 8;
/// `LANDLOCK_ACCESS_FS_MAKE_SOCK`：`bind` 一个 unix socket 文件。
pub(crate) const ACCESS_MAKE_SOCK: u64 = 1 << 9;
/// `LANDLOCK_ACCESS_FS_MAKE_FIFO`：`mkfifo`。
pub(crate) const ACCESS_MAKE_FIFO: u64 = 1 << 10;
/// `LANDLOCK_ACCESS_FS_MAKE_BLOCK`：`mknod` 块设备。
pub(crate) const ACCESS_MAKE_BLOCK: u64 = 1 << 11;
/// `LANDLOCK_ACCESS_FS_MAKE_SYM`：`symlink`。
pub(crate) const ACCESS_MAKE_SYM: u64 = 1 << 12;
/// `LANDLOCK_ACCESS_FS_REFER`（**ABI 2 起**）：跨目录 `rename`/`link`。
pub(crate) const ACCESS_REFER: u64 = 1 << 13;
/// `LANDLOCK_ACCESS_FS_TRUNCATE`（**ABI 3 起**）：`truncate(2)` / `open(O_TRUNC)`。
pub(crate) const ACCESS_TRUNCATE: u64 = 1 << 14;
/// `LANDLOCK_ACCESS_FS_IOCTL_DEV`（**ABI 5 起**）：设备节点上的 `ioctl`。
///
/// 本 crate **刻意不申请**这一位：申请了就要为 `/dev/null` 之外的一堆设备单独开规则，
/// 而设备 ioctl 不是"写工作区外的文件"，拦它只会让常用命令莫名失败（见 ADR 0003 §5
/// 的残余风险）。常量留在这里是为了让 ABI 对照表是完整的、可被单测钉住的。
pub(crate) const ACCESS_IOCTL_DEV: u64 = 1 << 15;

/// `danger` 档之外、`workspace-write` 档申请的写权限位（ABI 1 的基数部分）。
const BASE_WRITE_PERMISSIONS: u64 = ACCESS_WRITE_FILE
    | ACCESS_REMOVE_DIR
    | ACCESS_REMOVE_FILE
    | ACCESS_MAKE_CHAR
    | ACCESS_MAKE_DIR
    | ACCESS_MAKE_REG
    | ACCESS_MAKE_SOCK
    | ACCESS_MAKE_FIFO
    | ACCESS_MAKE_BLOCK
    | ACCESS_MAKE_SYM;

/// 写隔离档允许写设备节点的白名单（申请 `WRITE_FILE`）。
///
/// 为什么需要：`cmd 2>/dev/null` 这类重定向是**写一个文件**，Landlock 一样会检查。
/// 不给设备节点开口子，一堆命令会因为"写不了 /dev/null"而失败 —— 失败方向是安全
/// （不会放行），但会让人误判成"隔离坏了"。给设备开 `WRITE_FILE` 不影响边界：
/// 设备写不是"写工作区外的文件"，而 `MAKE_*` 一个都没给，创建不了新节点。
pub(crate) const DEVICE_WRITE_PATHS: &[&str] = &["/dev/null", "/dev/zero", "/dev/full", "/dev/tty"];

/// Landlock ABI 版本（`landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`
/// 的返回值）。
///
/// 版本号决定**能申请哪些权限位**：申请内核不认识的位会 `EINVAL`，而 `EINVAL` 看起来
/// 和"参数写错了"一模一样，所以必须按版本收窄，不能"一次性申请全部再祈祷"。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LandlockAbi(u32);

impl LandlockAbi {
    /// 从内核返回的原始版本号构造。
    pub(crate) const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// 原始版本号（日志与能力矩阵里显示用）。
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }

    /// ABI 2：`LANDLOCK_ACCESS_FS_REFER`（跨目录 rename/link）。
    pub(crate) const fn supports_refer(self) -> bool {
        self.0 >= 2
    }

    /// ABI 3：`LANDLOCK_ACCESS_FS_TRUNCATE`。
    pub(crate) const fn supports_truncate(self) -> bool {
        self.0 >= 3
    }

    /// ABI 4：网络（`LANDLOCK_ACCESS_NET_*`，规则集里是另一个字段）。
    pub(crate) const fn supports_network(self) -> bool {
        self.0 >= 4
    }

    /// ABI 5：`LANDLOCK_ACCESS_FS_IOCTL_DEV`。
    pub(crate) const fn supports_ioctl_dev(self) -> bool {
        self.0 >= 5
    }
}

/// 某一档**放行**的写权限位。
///
/// `None` = **不套** Landlock（`danger` 档）；`Some(0)` = 一位都不放行（只读档）。
///
/// 按 ABI 收窄是这个函数唯一的职责：ABI 1 申请 `REFER`/`TRUNCATE` 会 `EINVAL`，
/// 而 `EINVAL` 在 `landlock_create_ruleset` 上就是"建不出来" —— 那会让一台本来支持
/// Landlock 的机器白白报 `IsolationUnavailable`。
///
/// **注意别把"放行"与"handle"混成一件事**：只读档放行 0 位，但规则集仍然要 handle
/// 全部写位（见 [`handled_write_bits`]）。Landlock 只检查被 handle 的位，
/// `handled = 0` 的含义是"什么都不管"，等于根本没套隔离。
pub(crate) fn write_permissions(tier: SandboxMode, abi: LandlockAbi) -> Option<u64> {
    match tier {
        SandboxMode::Danger => None,
        SandboxMode::ReadOnly => Some(0),
        SandboxMode::WorkspaceWrite => Some(handled_write_bits(abi)),
    }
}

/// 规则集要 **handle**（接受检查）的写权限位 —— 与档位无关，只看 ABI。
///
/// Landlock 的语义是"只检查被 handle 的位"：想拦住某类写，必须先把那一位放进
/// `handled_access_fs`，再**不给**任何规则放行它。所以 `read_only` 档的 `handled`
/// **不是 0** —— 0 意味着"什么都不管"。
///
/// 刻意不含 `IOCTL_DEV`（ABI 5）：设备 ioctl 不是"写工作区外的文件"，handle 了它却
/// 不给设备规则放行，只会让常用命令莫名失败。
pub(crate) fn handled_write_bits(abi: LandlockAbi) -> u64 {
    let mut mask = BASE_WRITE_PERMISSIONS;
    if abi.supports_refer() {
        mask |= ACCESS_REFER;
    }
    if abi.supports_truncate() {
        mask |= ACCESS_TRUNCATE;
    }
    mask
}

/// 一条路径规则：`path` 之下允许 `access` 里的访问。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PathRule {
    path: PathBuf,
    access: u64,
}

impl PathRule {
    /// 被放行的路径（已规范化）。
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// 这条规则允许的访问位。
    pub(crate) fn access(&self) -> u64 {
        self.access
    }
}

/// 一个规则集的完整计划：申请哪些位（`handled`）+ 放行哪些路径。
///
/// 只有 `handled` 里的位会被内核检查；没被 handle 的访问（读、执行）一律不受影响 ——
/// 这正是"写隔离"而不是"文件系统 jail"的含义。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RulePlan {
    handled: u64,
    rules: Vec<PathRule>,
}

impl RulePlan {
    /// 规则集要 handle 的访问位。
    pub(crate) fn handled(&self) -> u64 {
        self.handled
    }

    /// 规则列表（工作区、缓存目录……）。
    pub(crate) fn rules(&self) -> &[PathRule] {
        &self.rules
    }

    /// 这条路径上 Landlock 会放行多少写权限（`0` = 一点都写不了）。
    ///
    /// 逐组件比较，不是字符串前缀比较：`/proj` 与 `/proj-outside` 必须分得开
    /// （`crates/hbot-command/src/path.rs` 的 cwd 守卫踩过同一个坑）。
    pub(crate) fn write_access(&self, candidate: &Path) -> u64 {
        let mut access = 0;
        for rule in &self.rules {
            if is_within(rule.path(), candidate) {
                access |= rule.access();
            }
        }
        access
    }

    /// 这条路径在规则集之下能不能写（至少有一个写类访问位被放行）。
    pub(crate) fn is_writable(&self, candidate: &Path) -> bool {
        self.write_access(candidate) != 0
    }
}

/// 为某一档构造规则计划。
///
/// `None` = 这一档不套 Landlock（`danger`）；`read_only` 得到零写位的空计划。
/// `cache` 是给包管理器用的私有缓存根：它通常在工作区之外（`<tmpdir>/…`），
/// 所以必须单独放行，否则 `npm`/`pnpm`/`cargo` 在写隔离档下一个都跑不起来。
pub(crate) fn plan(
    tier: SandboxMode,
    abi: LandlockAbi,
    workspace_root: &Path,
    cache: Option<&Path>,
) -> Option<RulePlan> {
    // `handled` 与"放不放行"是两件事：只读档也 handle 全部写位，只是一条规则都不给，
    // 于是内核把它全拒掉。`handled = 0` 等于没套 Landlock，绝不能出现。
    let handled = handled_write_bits(abi);
    let granted = write_permissions(tier, abi)?;
    let mut rules = Vec::new();
    if granted != 0 {
        rules.push(PathRule {
            path: workspace_root.to_path_buf(),
            access: granted,
        });
        // 缓存目录已经在工作区里时不必单独列（重复规则无害，但会让日志与审计难读）。
        // 用 `filter` 而不是嵌套 `if`：本 crate 的 `rust-version` 是 1.85，
        // let-chains（`if let Some(x) = y && cond`）要 1.88 起才能编译。
        if let Some(cache) = cache.filter(|cache| !is_within(workspace_root, cache)) {
            rules.push(PathRule {
                path: cache.to_path_buf(),
                access: granted,
            });
        }
    }
    Some(RulePlan { handled, rules })
}

/// `candidate` 是否在 `root` 之下（含 `root` 自身）。
///
/// 两边都必须**带根**（Unix 上以 `/` 打头，Windows 上以盘符或 `\` 打头）：
/// 没有根的相对路径压根不存在"在不在工作区里"这个判断，而 Landlock 的规则也是按
/// 绝对路径（`O_PATH` fd）建立的。调用方必须先 `canonicalize`。
///
/// 用 `has_root()` 而不是 `is_absolute()`：Windows 上 `\proj` 有根但 `is_absolute()`
/// 返回 false（它缺盘符前缀），那会让同一份判据在两个平台上给出不同结论。
pub(crate) fn is_within(root: &Path, candidate: &Path) -> bool {
    if !root.has_root() || !candidate.has_root() {
        return false;
    }
    let mut root_components = root.components();
    let mut candidate_components = candidate.components();
    loop {
        match root_components.next() {
            None => return true,
            Some(expected) => match candidate_components.next() {
                Some(actual) if actual == expected => {}
                _ => return false,
            },
        }
    }
}

/// 把权限位渲染成人能读的短名（日志用；顺序固定，便于 grep）。
pub(crate) fn permission_names(mask: u64) -> String {
    const NAMES: &[(u64, &str)] = &[
        (ACCESS_WRITE_FILE, "write_file"),
        (ACCESS_REMOVE_DIR, "remove_dir"),
        (ACCESS_REMOVE_FILE, "remove_file"),
        (ACCESS_MAKE_CHAR, "make_char"),
        (ACCESS_MAKE_DIR, "make_dir"),
        (ACCESS_MAKE_REG, "make_reg"),
        (ACCESS_MAKE_SOCK, "make_sock"),
        (ACCESS_MAKE_FIFO, "make_fifo"),
        (ACCESS_MAKE_BLOCK, "make_block"),
        (ACCESS_MAKE_SYM, "make_sym"),
        (ACCESS_REFER, "refer"),
        (ACCESS_TRUNCATE, "truncate"),
        (ACCESS_IOCTL_DEV, "ioctl_dev"),
    ];
    if mask == 0 {
        return "none".to_owned();
    }
    NAMES
        .iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join("|")
}

/// 子进程装载写隔离失败时**必须**用的退出码。
///
/// 为什么不用"让 `pre_exec` 返回 `Err`"：那样 `Command::spawn` 会在**父进程**里返回
/// 一个 `io::Error`，被 `runner` 归到 `SpawnFailed`（"命令自己没起来"），用户看到的是
/// 程序名/权限之类的原因，而不是"这台机器装不上写隔离"。用哨兵退出码 + stderr 标记，
/// 父进程就能把它翻译成 [`hbot_core::CommandDenied::IsolationUnavailable`]。
///
/// `127` 是 shell 的"命令没找到"惯用码，语义接近"命令根本没被执行"。
pub(crate) const CHILD_FAILURE_EXIT: i32 = 127;

/// 子进程在 stderr 上留下的标记前缀（父进程据此区分"哨兵退出"与"命令自己退了 127"）。
pub(crate) const CHILD_FAILURE_MARKER: &str = "hbot-landlock:";

/// 把"子进程以哨兵码退出"翻译成给用户看的拒绝原因。
///
/// 三个条件同时满足才算"隔离没装上"：退出码是 [`CHILD_FAILURE_EXIT`]、stderr 里有
/// [`CHILD_FAILURE_MARKER`]、且能取到那一行。**返回 `None` 时不许当成成功**——
/// 这个函数只回答"要不要改判成 IsolationUnavailable"，不参与放行判断。
pub(crate) fn isolation_failure_reason(exit_code: Option<i32>, stderr: &str) -> Option<String> {
    if exit_code != Some(CHILD_FAILURE_EXIT) {
        return None;
    }
    let line = stderr
        .lines()
        .find(|line| line.contains(CHILD_FAILURE_MARKER))?
        .trim();
    Some(format!(
        "子进程没能装载 Landlock 写隔离（{line}），命令未执行 —— 已按「隔离装不上」拒绝，绝不降级放行"
    ))
}

/// 把"子进程装载失败"那一行写进调用方给的栈上缓冲区，返回长度。
///
/// 为什么不用 `format!`：这个函数唯一的调用点在 `pre_exec` 里（fork 之后、exec 之前），
/// 那里**不能分配内存** —— 子进程只持有父进程地址空间的一份拷贝，而父进程可能正持有
/// 堆分配器的锁，一分配就可能死锁。所以用固定缓冲区 + 手写数字转换。
///
/// 输出形如 `hbot-landlock: landlock_restrict_self (errno=13)\n`，缓冲区不够就截断
/// （父进程只靠前缀 `hbot-landlock:` 识别，截断不影响判定）。
pub(crate) fn format_child_failure(buffer: &mut [u8], what: &[u8], errno: i32) -> usize {
    fn push(buffer: &mut [u8], len: &mut usize, bytes: &[u8]) {
        for &byte in bytes {
            if *len < buffer.len() {
                buffer[*len] = byte;
                *len += 1;
            }
        }
    }

    let mut len = 0usize;
    push(buffer, &mut len, CHILD_FAILURE_MARKER.as_bytes());
    push(buffer, &mut len, b" ");
    push(buffer, &mut len, what);
    push(buffer, &mut len, b" (errno=");
    // 十进制转换（`errno` 实际恒正，但仍按有符号处理，免得 `i32::MIN` 溢出）。
    let mut digits = [0u8; 10];
    let mut count = 0usize;
    let mut value = errno.unsigned_abs();
    if value == 0 {
        digits[0] = b'0';
        count = 1;
    }
    while value > 0 {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
    }
    if errno < 0 {
        push(buffer, &mut len, b"-");
    }
    while count > 0 {
        count -= 1;
        push(buffer, &mut len, &digits[count..=count]);
    }
    push(buffer, &mut len, b")\n");
    len
}

/// 能力矩阵里的一句话说明（含 ABI 与可写范围）。
///
/// 刻意**不带**"可用"两个字：这句话是塞进 `summary_zh` 的括号里的，外面已经写了
/// "写隔离可用（…）"，再写一遍就变成"可用（…可用…）"。
pub(crate) fn describe_zh(abi: LandlockAbi, cache_root: &Path) -> String {
    format!(
        "Landlock（ABI {}）；可写范围 = 工作区 + 私有缓存目录（{}）",
        abi.raw(),
        cache_root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABI1: LandlockAbi = LandlockAbi::from_raw(1);
    const ABI2: LandlockAbi = LandlockAbi::from_raw(2);
    const ABI3: LandlockAbi = LandlockAbi::from_raw(3);
    const ABI5: LandlockAbi = LandlockAbi::from_raw(5);

    fn workspace() -> PathBuf {
        // 用统一的绝对形状（`/` 打头的组件在 Windows 上也能按组件比较），
        // 这样这条测试在三平台跑的都是同一份判据。
        PathBuf::from("/proj/workspace")
    }

    fn cache() -> PathBuf {
        PathBuf::from("/tmp/hbot-command-write-cache")
    }

    fn workspace_plan(abi: LandlockAbi) -> RulePlan {
        plan(
            SandboxMode::WorkspaceWrite,
            abi,
            &workspace(),
            Some(&cache()),
        )
        .expect("workspace-write 档必须有规则计划")
    }

    // ---- ABI → 权限位 ----------------------------------------------------

    #[test]
    fn abi_one_has_neither_refer_nor_truncate() {
        let mask = write_permissions(SandboxMode::WorkspaceWrite, ABI1).expect("有权限位");
        assert_eq!(mask & ACCESS_REFER, 0, "ABI 1 没有 REFER，申请了会 EINVAL");
        assert_eq!(
            mask & ACCESS_TRUNCATE,
            0,
            "ABI 1 没有 TRUNCATE，申请了会 EINVAL"
        );
        // ABI 1 的基数位一个都不能少，否则写隔离形同虚设。
        assert_eq!(mask & BASE_WRITE_PERMISSIONS, BASE_WRITE_PERMISSIONS);
        assert_eq!(mask & ACCESS_IOCTL_DEV, 0);
    }

    #[test]
    fn abi_two_adds_refer_and_still_no_truncate() {
        let mask = write_permissions(SandboxMode::WorkspaceWrite, ABI2).expect("有权限位");
        assert_ne!(mask & ACCESS_REFER, 0, "ABI 2 起才有 REFER");
        assert_eq!(mask & ACCESS_TRUNCATE, 0, "TRUNCATE 要 ABI 3");
        assert!(!ABI1.supports_refer() && ABI2.supports_refer() && !ABI2.supports_truncate());
    }

    #[test]
    fn abi_three_adds_truncate_and_never_ioctl_dev() {
        let mask = write_permissions(SandboxMode::WorkspaceWrite, ABI3).expect("有权限位");
        assert_ne!(mask & ACCESS_TRUNCATE, 0, "ABI 3 起才有 TRUNCATE");
        assert!(ABI3.supports_truncate());
        // 设备 ioctl 是 ABI 5 的事，而且本 crate 刻意不申请：设备 ioctl 不是"写工作区外文件"。
        assert_eq!(mask & ACCESS_IOCTL_DEV, 0);
        assert!(!ABI3.supports_ioctl_dev() && ABI5.supports_ioctl_dev());
        assert!(!ABI3.supports_network() && LandlockAbi::from_raw(4).supports_network());
    }

    // ---- 档位 → 权限位 ---------------------------------------------------

    #[test]
    fn read_only_requests_no_write_permission_at_all() {
        let mask = write_permissions(SandboxMode::ReadOnly, ABI3).expect("只读档有（空的）形状");
        assert_eq!(mask, 0, "只读档一个写位都不放行");
        let plan = plan(SandboxMode::ReadOnly, ABI3, &workspace(), Some(&cache()))
            .expect("只读档也有计划（空计划）");
        // 关键：handled 不是 0。`handled = 0` 意味着"内核什么都不检查"，等于没套 Landlock；
        // 只读档要的是"检查所有写位，但一条规则都不放行"。
        assert_eq!(
            plan.handled(),
            handled_write_bits(ABI3),
            "只读档必须 handle 全部写位，否则等于没装隔离"
        );
        assert_ne!(plan.handled(), 0);
        assert!(plan.rules().is_empty(), "只读档没有任何可写路径");
        assert!(
            !plan.is_writable(&workspace().join("a.txt")),
            "只读档下工作区内的文件也不可写"
        );
        assert!(!plan.is_writable(Path::new("/proj/workspace")));
    }

    #[test]
    fn handled_bits_follow_the_abi_for_every_tier() {
        // ABI 收窄只影响"能不能申请"，不影响"是不是要检查"：三档都用同一套写位。
        for abi in [ABI1, ABI2, ABI3, ABI5] {
            let handled = handled_write_bits(abi);
            assert_ne!(handled & ACCESS_WRITE_FILE, 0, "ABI 1 起就有 WRITE_FILE");
            assert_eq!(
                handled & ACCESS_REFER != 0,
                abi.supports_refer(),
                "REFER 只在 ABI 2 起申请"
            );
            assert_eq!(
                handled & ACCESS_TRUNCATE != 0,
                abi.supports_truncate(),
                "TRUNCATE 只在 ABI 3 起申请"
            );
            assert_eq!(
                handled & ACCESS_IOCTL_DEV,
                0,
                "本 crate 刻意不 handle 设备 ioctl"
            );
            // 只读档放行 0 位，但 handle 的位一位不少。
            assert_eq!(write_permissions(SandboxMode::ReadOnly, abi), Some(0));
            let read_only = plan(SandboxMode::ReadOnly, abi, &workspace(), None).expect("有计划");
            assert_eq!(read_only.handled(), handled);
            assert!(read_only.rules().is_empty());
        }
    }

    #[test]
    fn danger_does_not_install_landlock() {
        assert_eq!(write_permissions(SandboxMode::Danger, ABI3), None);
        assert_eq!(plan(SandboxMode::Danger, ABI3, &workspace(), None), None);
    }

    // ---- 路径 → 规则 -----------------------------------------------------

    #[test]
    fn workspace_subtree_is_writable() {
        let plan = workspace_plan(ABI3);
        assert_eq!(plan.rules().len(), 2, "工作区 + 缓存目录");
        assert!(plan.is_writable(&workspace()), "工作区根本身可写");
        assert!(plan.is_writable(&workspace().join("inside.txt")));
        assert!(
            plan.is_writable(&workspace().join("a/b/c/d.txt")),
            "整棵子树都可写，不只是第一层"
        );
        assert_eq!(
            plan.write_access(&workspace().join("inside.txt")),
            plan.handled(),
            "工作区内放行的是整套写位"
        );
    }

    #[test]
    fn cache_directory_is_writable_and_separate_from_the_workspace() {
        let plan = workspace_plan(ABI3);
        assert!(plan.is_writable(&cache()), "缓存根自身可写");
        assert!(
            plan.is_writable(&cache().join("_cacache/tmp/0001")),
            "包管理器要在缓存里建子目录"
        );
        let rule_paths: Vec<&Path> = plan.rules().iter().map(PathRule::path).collect();
        assert_eq!(rule_paths, vec![workspace().as_path(), cache().as_path()]);
    }

    #[test]
    fn paths_outside_the_workspace_and_the_cache_are_not_writable() {
        let plan = workspace_plan(ABI3);
        for outside in [
            "/etc/passwd",
            "/home/me/.ssh/authorized_keys",
            // 字符串前缀很像，但不是同一棵子树：逐组件比较才分得开。
            "/proj/workspace-outside/x",
            // 父目录也不行：`..` 逃逸的落点就在父目录。
            "/proj",
            "/tmp",
            "/tmp/other-cache",
        ] {
            assert!(!plan.is_writable(Path::new(outside)), "{outside} 不该可写");
        }
        // 相对路径没有参照物，恒不可写（调用方必须先 canonicalize）。
        assert!(!plan.is_writable(Path::new("inside.txt")));
        assert!(!plan.is_writable(Path::new("../escaped.txt")));
    }

    #[test]
    fn cache_inside_the_workspace_is_not_duplicated() {
        let inside = workspace().join("cache");
        let plan = plan(
            SandboxMode::WorkspaceWrite,
            ABI3,
            &workspace(),
            Some(&inside),
        )
        .expect("有规则计划");
        assert_eq!(plan.rules().len(), 1, "缓存已在工作区里，不重复列一条规则");
        assert!(plan.is_writable(&inside.join("x")));
    }

    #[test]
    fn device_paths_are_not_workspace_writes() {
        // 设备白名单是**具体路径**级别、只给 WRITE_FILE，且不进工作区规则。
        let plan = workspace_plan(ABI3);
        for device in DEVICE_WRITE_PATHS {
            assert_eq!(
                plan.write_access(Path::new(device)),
                0,
                "{device} 不该被当成工作区里的可写路径"
            );
            assert!(!is_within(&workspace(), Path::new(device)));
        }
    }

    // ---- 子进程失败 → 拒绝原因 -------------------------------------------

    #[test]
    fn child_failure_exit_is_translated_into_isolation_unavailable() {
        let stderr = format!("{CHILD_FAILURE_MARKER} landlock_restrict_self (errno=1)\n");
        let reason = isolation_failure_reason(Some(CHILD_FAILURE_EXIT), &stderr)
            .expect("哨兵退出 + 标记 = 隔离没装上");
        assert!(reason.contains("Landlock"), "{reason}");
        assert!(reason.contains("绝不降级放行"), "{reason}");

        // 命令自己退 127（没有标记）：不许误判成隔离失败。
        assert_eq!(
            isolation_failure_reason(Some(CHILD_FAILURE_EXIT), "boom"),
            None
        );
        assert_eq!(isolation_failure_reason(Some(127), ""), None);
        // 有标记但退出码不是哨兵码：也不改判。
        assert_eq!(
            isolation_failure_reason(Some(1), &format!("{CHILD_FAILURE_MARKER} x")),
            None
        );
        // 被信号杀掉（没有退出码）：同样不改判。
        assert_eq!(
            isolation_failure_reason(None, &format!("{CHILD_FAILURE_MARKER} x")),
            None
        );
    }

    // ---- 其它 -----------------------------------------------------------

    #[test]
    fn child_failure_line_is_exact_and_truncates_safely() {
        let mut buffer = [0u8; 96];
        let len = format_child_failure(&mut buffer, b"landlock_restrict_self", 13);
        let line = std::str::from_utf8(&buffer[..len]).expect("ASCII");
        assert_eq!(line, "hbot-landlock: landlock_restrict_self (errno=13)\n");
        // 父进程用的正是这条判定：写出来的行必须能被 `isolation_failure_reason` 认出来。
        assert!(isolation_failure_reason(Some(CHILD_FAILURE_EXIT), line).is_some());

        // 缓冲区很小也不能 panic、不能写越界（`pre_exec` 里 panic = 子进程带着锁退出）。
        let mut tiny = [0u8; 8];
        let len = format_child_failure(&mut tiny, b"truncate-everything", 1);
        assert_eq!(len, tiny.len());
        assert_eq!(&tiny[..], b"hbot-lan".as_slice());

        // errno 为 0 时也要给出一个数字，不能出现空字段。
        let mut zero = [0u8; 64];
        let len = format_child_failure(&mut zero, b"x", 0);
        assert_eq!(&zero[..len], b"hbot-landlock: x (errno=0)\n".as_slice());
    }

    #[test]
    fn permission_names_are_stable_and_readable() {
        assert_eq!(permission_names(0), "none");
        let abi1 = permission_names(write_permissions(SandboxMode::WorkspaceWrite, ABI1).unwrap());
        assert!(
            !abi1.contains("refer") && !abi1.contains("truncate"),
            "{abi1}"
        );
        assert!(abi1.starts_with("write_file|remove_dir"));
        let abi3 = permission_names(write_permissions(SandboxMode::WorkspaceWrite, ABI3).unwrap());
        assert!(abi3.ends_with("refer|truncate"), "{abi3}");
        assert!(!abi3.contains("ioctl_dev"));
    }

    #[test]
    fn describe_mentions_the_abi_and_the_writable_scope() {
        let text = describe_zh(ABI3, &cache());
        assert!(text.contains("ABI 3"), "{text}");
        assert!(text.contains("工作区"), "{text}");
        assert!(text.contains("私有缓存目录"), "{text}");
    }
}
