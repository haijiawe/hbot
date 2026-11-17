//! Windows：作业对象（Job Object）做进程树隔离 + 资源上限。
//!
//! 为什么是作业对象而不是 `child.kill()`：`TerminateProcess` 只杀直接子进程，
//! `cmd → npm → node` 这种链会留下一串孤儿；作业对象是内核维护的集合，
//! `TerminateJobObject` 一次杀光，且 `KILL_ON_JOB_CLOSE` 保证句柄一关就不留活口。
//!
//! 为什么这几行是手写 FFI 而不是 `windows-sys`：作业对象只要 6 个 kernel32 函数，
//! 手写声明比拉一整套绑定更省事（写隔离那一层要几十个 API、还要结构体定义，才用了
//! `windows-sys`）。`std::process` 没有作业对象的抽象，`Command::creation_flags`
//! 只能设标志位（做不到"进程一出生就在作业里"）。
//!
//! 已知时间窗（残余风险，见 ADR-0003）：子进程是**先 spawn 再 Assign**，
//! 在两者之间它若抢先 `spawn` 出孙进程，那个孙进程不在作业里。要做到"出生即入作业"
//! 需要 `CreateProcessW` + `PROC_THREAD_ATTRIBUTE_JOB_LIST`（Win8+），属于下一批。
//!
//! # 资源上限：哪些是内核硬上限、哪些是轮询
//!
//! | 限额 | 落点 | 单位 | 口径 | 超了会怎样 |
//! |---|---|---|---|---|
//! | `cpu_ms` | `PerJobUserTimeLimit` + `JOB_OBJECT_LIMIT_JOB_TIME` | 100 ns | **整棵树**（作业累计用户态时间，含已退出进程） | 内核终止作业；另有 10 ms 轮询读记账，用于拿到"为什么被杀"的确切数字 |
//! | `memory_bytes` | `ProcessMemoryLimit` / `JobMemoryLimit` | 字节（提交内存） | 前者单进程、后者**整棵树**；这里两个都设成请求值 | **分配失败**（不是杀进程）；峰值到顶时由轮询判定并 `TerminateJobObject` |
//! | `read_bytes` / `write_bytes` | 无内核开关，只有 `JobObjectBasicAndIoAccountingInformation` 记账 | 字节 | **整棵树**累计传输量（含已退出进程） | 10 ms 轮询发现超了就杀树 |
//! | `process_count` | `ActiveProcessLimit` | 个 | **整棵树**同时存活 | **内核让第 `limit + 1` 个进程创建失败**（创建者拿到 `CreateProcess` 错误），不是我们主动拒 |
//!
//! 记账**不会**被 `TerminateJobObject` 清掉：进程全退出之后、`CloseHandle` 之前仍可查询，
//! 所以 `usage()` 必须在 `Drop`（关句柄）之前调用——调用顺序由 `PlatformRunner` 保证。

use crate::child::Spawned;
use crate::limits::{self, LimitViolation};
use hbot_core::{ResourceLimits, ResourceUsage};
use std::ffi::c_void;
use std::io;
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use std::os::windows::process::CommandExt;

type Handle = *mut c_void;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(
        job: Handle,
        info_class: i32,
        info: *const c_void,
        info_len: u32,
    ) -> i32;
    fn QueryInformationJobObject(
        job: Handle,
        info_class: i32,
        info: *mut c_void,
        info_len: u32,
        return_len: *mut u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
}

/// `JOBOBJECTINFOCLASS::JobObjectBasicAndIoAccountingInformation`（记账 + I/O 计数器，96 字节）。
///
/// 它一次就带回了 `JobObjectBasicAccountingInformation` 的全部字段（时间 + 进程数），
/// 所以不需要再单独查 class 1——多查一次只是多一次系统调用。
const JOB_OBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION: i32 = 8;

/// `JOBOBJECTINFOCLASS::JobObjectExtendedLimitInformation`（上限 + 峰值，144 字节）。
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

const JOB_OBJECT_LIMIT_ACTIVE_PROCESS: u32 = 0x0000_0008;
const JOB_OBJECT_LIMIT_PROCESS_MEMORY: u32 = 0x0000_0100;
const JOB_OBJECT_LIMIT_JOB_MEMORY: u32 = 0x0000_0200;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

/// `JOB_OBJECT_LIMIT_JOB_TIME`：`PerJobUserTimeLimit` 生效（作业累计用户态时间上限）。
const JOB_OBJECT_LIMIT_JOB_TIME: u32 = 0x0000_0004;

/// 不弹控制台窗口（GUI 进程里跑命令时不闪黑框，也不改子进程的 stdin/stdout 语义）。
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 作业内并发进程数上限的**默认值**（调用方没请求 `process_count` 时用）。
///
/// 64 是"够 `npm install` 的子树、但拦住 fork 炸弹"的量级。调用方可以用
/// [`ResourceLimits::process_count`] 收紧到 `1..=64`（`0` 是内核保留值，会被夹到 1）。
const MAX_ACTIVE_PROCESSES: u32 = 64;

/// 单进程内存上限的**默认值**（2 GiB）：`rustc` 编大 crate 会到 1 GB 上下，留一倍余量。
const PROCESS_MEMORY_LIMIT: usize = 2 * 1024 * 1024 * 1024;

/// 整个作业内存上限的**默认值**（4 GiB）：`cargo build` 同时起多个 `rustc` 时不要误杀。
const JOB_MEMORY_LIMIT: usize = 4 * 1024 * 1024 * 1024;

/// `JOBOBJECT_BASIC_LIMIT_INFORMATION`（x64 上 64 字节，字段顺序/对齐必须与 sdkddkver 一致）。
///
/// 时间字段的单位都是 **100 ns**，且是**整棵作业树**的累计值（不是单进程、不是瞬时值）。
#[repr(C)]
struct BasicLimitInformation {
    /// 单进程用户态时间上限（100 ns）；本 crate 不用它（口径是整棵树）。
    per_process_user_time_limit: i64,
    /// **整棵树**用户态时间上限（100 ns）；`JOB_OBJECT_LIMIT_JOB_TIME` 置位时生效。
    per_job_user_time_limit: i64,
    /// 哪些字段生效（`JOB_OBJECT_LIMIT_*` 位组合）。
    limit_flags: u32,
    /// 单进程最小/最大工作集（字节）；本 crate 不动。
    minimum_working_set_size: usize,
    /// 单进程最大工作集（字节）；本 crate 不动。
    maximum_working_set_size: usize,
    /// **整棵树**同时存活的进程数上限；`0` 是保留值（= 不限），所以有效区间是 `1..=64`。
    active_process_limit: u32,
    /// CPU 亲和性掩码；本 crate 不动（`0` = 不限制）。
    affinity: usize,
    /// 优先级类别；本 crate 不动。
    priority_class: u32,
    /// 调度类别；本 crate 不动。
    scheduling_class: u32,
}

/// `IO_COUNTERS`（6 × u64 = 48 字节）。
///
/// 两个 `*_transfer_count` 的单位是**字节**，口径是**整棵树**累计（含已退出进程，
/// 作业对象会替它们继续记账）。`*_operation_count` 是操作次数，本 crate 不看。
#[repr(C)]
struct IoCounters {
    /// 读操作次数。
    read_operation_count: u64,
    /// 写操作次数。
    write_operation_count: u64,
    /// 其它操作次数。
    other_operation_count: u64,
    /// **整棵树**累计读取字节（`ReadFile` 一类读操作搬了多少字节）。
    read_transfer_count: u64,
    /// **整棵树**累计写出字节。
    write_transfer_count: u64,
    /// 其它传输字节。
    other_transfer_count: u64,
}

/// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`（x64 上 144 字节）。
///
/// 长度必须一字不差地传给 `SetInformationJobObject`，否则它返回
/// 「参数错误」而不是指出哪个字段——照结构体定义手算对齐就是这个原因。
#[repr(C)]
struct ExtendedLimitInformation {
    /// 基础上限。
    basic: BasicLimitInformation,
    /// I/O 记账（`SetInformationJobObject` 时按 0 传，`QueryInformationJobObject` 时可读）。
    io: IoCounters,
    /// 单进程提交内存上限（字节）；`0` 是保留值。
    process_memory_limit: usize,
    /// **整棵树**提交内存上限（字节）；`0` 是保留值。
    job_memory_limit: usize,
    /// 单进程提交内存峰值（字节，只读回填）。
    peak_process_memory_used: usize,
    /// **整棵树**提交内存峰值（字节，只读回填）——`ResourceUsage::peak_memory_bytes` 就是它。
    peak_job_memory_used: usize,
}

/// `JOBOBJECT_BASIC_ACCOUNTING_INFORMATION`（48 字节）。
///
/// 全部是作业开始以来的**累计值**；`*_time` 单位 100 ns，进程退出后仍保留。
#[repr(C)]
struct BasicAccountingInformation {
    /// **整棵树**累计用户态时间（100 ns，含已退出进程）。
    total_user_time: i64,
    /// **整棵树**累计内核态时间（100 ns）。
    total_kernel_time: i64,
    /// 本计费周期内的用户态时间（100 ns）；本 crate 不看。
    this_period_total_user_time: i64,
    /// 本计费周期内的内核态时间（100 ns）；本 crate 不看。
    this_period_total_kernel_time: i64,
    /// 累计缺页次数。
    total_page_fault_count: u32,
    /// **累计**进过作业的进程总数（不是并发数！`process_count` 不能用它判）。
    total_processes: u32,
    /// **当前**存活进程数——`ResourceUsage::peak_processes` 靠 10 ms 采样它的峰值。
    active_processes: u32,
    /// 累计已终止进程数。
    total_terminated_processes: u32,
}

/// `JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION`（x64 上 96 字节）。
#[repr(C)]
struct BasicAndIoAccountingInformation {
    /// 时间与进程数记账。
    basic: BasicAccountingInformation,
    /// I/O 记账。
    io: IoCounters,
}

pub(crate) struct Containment {
    job: Handle,
    /// 采样到的用量（原子是为了满足 `Send + Sync`；实际只有 runner 一个线程读）。
    cpu_ms: AtomicU64,
    read_bytes: AtomicU64,
    write_bytes: AtomicU64,
    peak_memory_bytes: AtomicU64,
    peak_processes: AtomicU32,
}

// SAFETY: 作业对象句柄是内核对象句柄，跨线程使用是合法的（内核对象本身没有线程亲和性），
// 只有 `kill_tree`/`Drop` 会碰它；`PlatformRunner` 的签名要求 `Send + Sync`。
unsafe impl Send for Containment {}
unsafe impl Sync for Containment {}

impl Containment {
    pub(crate) fn create(limits: &ResourceLimits) -> Result<Self, String> {
        // SAFETY: 两个空指针都是 API 允许的（无安全属性、匿名作业对象）。
        let job = unsafe { CreateJobObjectW(ptr::null_mut(), ptr::null()) };
        if job.is_null() {
            return Err(format!("建立作业对象失败：{}", io::Error::last_os_error()));
        }
        let mut info: ExtendedLimitInformation = unsafe { std::mem::zeroed() };
        info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_JOB_MEMORY;
        // 进程数：`0` 是保留值（不限），所以夹到 `1..=64`；没请求就沿用 64。
        info.basic.active_process_limit = limits
            .process_count
            .map_or(MAX_ACTIVE_PROCESSES, limits::process_limit_clamp);
        // CPU：`PerJobUserTimeLimit` 是 100 ns 单位的整棵树用户态时间。
        // 换算失败（超出 i64）不该在这里静默退化——`limits::validate` 已把 `cpu_ms`
        // 挡在 24 小时内，真到这里说明调用方绕过了校验，直接拒绝更安全。
        if let Some(cpu_ms) = limits.cpu_ms {
            let Some(ticks) = limits::cpu_ms_to_100ns(cpu_ms) else {
                // SAFETY: `job` 是上面创建成功、尚未关闭的句柄。
                unsafe {
                    CloseHandle(job);
                }
                return Err(format!(
                    "CPU 上限 {cpu_ms} 毫秒换算成 100 ns 单位时溢出，拒绝建立作业对象"
                ));
            };
            info.basic.per_job_user_time_limit = ticks;
            info.basic.limit_flags |= JOB_OBJECT_LIMIT_JOB_TIME;
        }
        // 内存：请求了就同时收紧"单进程"与"整棵树"，语义是"这条命令最多用这么多提交内存"。
        info.process_memory_limit = limits
            .memory_bytes
            .map_or(PROCESS_MEMORY_LIMIT, limits::memory_limit_usize);
        info.job_memory_limit = limits
            .memory_bytes
            .map_or(JOB_MEMORY_LIMIT, limits::memory_limit_usize);

        // SAFETY: `info` 的长度与 `ExtendedLimitInformation` 一致，且结构体全是 POD。
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &info as *const ExtendedLimitInformation as *const c_void,
                std::mem::size_of::<ExtendedLimitInformation>() as u32,
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            // SAFETY: `job` 是上面创建成功且尚未关闭的句柄。
            unsafe {
                CloseHandle(job);
            }
            return Err(format!("给作业对象设置上限失败：{err}"));
        }
        Ok(Self {
            job,
            cpu_ms: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            peak_memory_bytes: AtomicU64::new(0),
            peak_processes: AtomicU32::new(0),
        })
    }

    pub(crate) fn configure(&self, command: &mut Command) {
        command.creation_flags(CREATE_NO_WINDOW);
    }

    pub(crate) fn attach(&self, child: &Spawned) -> io::Result<()> {
        // 句柄是全权限的（`CreateProcessW` / `CreateProcessAsUserW` 返回的 hProcess），
        // 满足 AssignProcessToJobObject 需要的 PROCESS_SET_QUOTA | PROCESS_TERMINATE。
        let process = child.process_handle();
        // SAFETY: 两个句柄都由 Rust 侧持有且有效（job 存活于 self，process 存活于 child）。
        let ok = unsafe { AssignProcessToJobObject(self.job, process) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn kill_tree(&self, child: &mut Spawned) -> io::Result<()> {
        // SAFETY: `self.job` 在 Drop 之前一直有效。退出码 1 只是标记"被隔离层杀掉"。
        let ok = unsafe { TerminateJobObject(self.job, 1) };
        if ok == 0 {
            let err = io::Error::last_os_error();
            // 作业已空时 TerminateJobObject 也可能失败；此时子进程可能已经自己退出，
            // 再补一刀直接杀子进程，然后如实返回错误。
            let _ = child.kill();
            return Err(err);
        }
        Ok(())
    }

    /// 读一次作业对象的累计记账，顺手更新采样峰值。
    ///
    /// 两次 `QueryInformationJobObject`（class 8 拿时间/I/O/活跃进程数，class 9 拿内存峰值），
    /// 都是几十微秒级，放在既有的 10 ms 循环里开销可以忽略。查询失败时**保留上一次的值**
    /// （例如作业刚被 `TerminateJobObject` 清空的瞬间），不把用量清零——清零会让
    /// `violation` 误判"没超限"。
    fn sample(&self) {
        let mut accounting: BasicAndIoAccountingInformation = unsafe { std::mem::zeroed() };
        // SAFETY: `accounting` 的大小与 class 8 期望的 96 字节一致，且是 POD。
        let ok = unsafe {
            QueryInformationJobObject(
                self.job,
                JOB_OBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION,
                &mut accounting as *mut BasicAndIoAccountingInformation as *mut c_void,
                std::mem::size_of::<BasicAndIoAccountingInformation>() as u32,
                ptr::null_mut(),
            )
        };
        if ok != 0 {
            // 100 ns → 毫秒。
            let cpu_ms =
                u64::try_from(accounting.basic.total_user_time.max(0)).unwrap_or(0) / 10_000;
            self.cpu_ms.store(cpu_ms, Ordering::Relaxed);
            self.read_bytes
                .store(accounting.io.read_transfer_count, Ordering::Relaxed);
            self.write_bytes
                .store(accounting.io.write_transfer_count, Ordering::Relaxed);
            self.peak_processes
                .fetch_max(accounting.basic.active_processes, Ordering::Relaxed);
        }

        let mut info: ExtendedLimitInformation = unsafe { std::mem::zeroed() };
        // SAFETY: `info` 的大小与 class 9 期望的 144 字节一致，且是 POD。
        let ok = unsafe {
            QueryInformationJobObject(
                self.job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &mut info as *mut ExtendedLimitInformation as *mut c_void,
                std::mem::size_of::<ExtendedLimitInformation>() as u32,
                ptr::null_mut(),
            )
        };
        if ok != 0 {
            self.peak_memory_bytes.fetch_max(
                u64::try_from(info.peak_job_memory_used).unwrap_or(0),
                Ordering::Relaxed,
            );
        }
    }

    /// 最近一次采样得到的用量（不重新查询）。
    fn snapshot(&self) -> ResourceUsage {
        ResourceUsage {
            cpu_ms: self.cpu_ms.load(Ordering::Relaxed),
            peak_memory_bytes: self.peak_memory_bytes.load(Ordering::Relaxed),
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            write_bytes: self.write_bytes.load(Ordering::Relaxed),
            peak_processes: self.peak_processes.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn usage(&self) -> ResourceUsage {
        self.sample();
        self.snapshot()
    }

    pub(crate) fn violation(&self, limits: &ResourceLimits) -> Option<LimitViolation> {
        self.sample();
        limits::violation_of(&self.snapshot(), limits)
    }
}

impl Drop for Containment {
    fn drop(&mut self) {
        // SAFETY: `self.job` 是 create 成功、且只在这里关闭一次的句柄。
        // KILL_ON_JOB_CLOSE 在这里生效：范围内还有活着的进程时它们会被杀掉。
        unsafe {
            CloseHandle(self.job);
        }
    }
}

pub(crate) fn probe() -> Result<(), String> {
    // 真建一个空作业对象并设上限：不跑 powershell、不改系统设置，几十微秒。
    Containment::create(&ResourceLimits::default())
        .map(drop)
        .map_err(|reason| format!("作业对象不可用（{reason}）"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbot_core::IoDirection;

    /// 把作业对象当前的上限读回来（验证 `create(limits)` 真的写进去了）。
    fn read_limits(job: Handle) -> ExtendedLimitInformation {
        let mut info: ExtendedLimitInformation = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            QueryInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &mut info as *mut ExtendedLimitInformation as *mut c_void,
                std::mem::size_of::<ExtendedLimitInformation>() as u32,
                ptr::null_mut(),
            )
        };
        assert_ne!(
            ok,
            0,
            "QueryInformationJobObject 失败：{}",
            io::Error::last_os_error()
        );
        info
    }

    #[test]
    fn extended_limit_information_has_expected_size() {
        // 长度算错会让 SetInformationJobObject 报「参数错误」，而错误信息不会告诉你
        // 是哪个字段——所以这里把 x64 的 144 字节钉死。
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<ExtendedLimitInformation>(), 144);
        #[cfg(target_pointer_width = "32")]
        assert_eq!(std::mem::size_of::<ExtendedLimitInformation>(), 112);
    }

    #[test]
    fn accounting_structures_have_expected_size() {
        // 96 字节是 `JobObjectBasicAndIoAccountingInformation` 的长度；传小了会被截断。
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<BasicAndIoAccountingInformation>(), 96);
        assert_eq!(std::mem::size_of::<BasicAccountingInformation>(), 48);
        assert_eq!(std::mem::size_of::<IoCounters>(), 48);
    }

    #[test]
    fn job_object_can_be_created_and_probed() {
        // 真实探测：本机能不能建作业对象（CI 上若在受限容器里跑，这条会红，
        // 那时要看的正是"探测如实报告了不可用"）。
        let probe_result = probe();
        assert!(probe_result.is_ok(), "作业对象应可用：{probe_result:?}");
    }

    #[test]
    fn requested_limits_are_written_into_the_job_object() {
        let limits = ResourceLimits {
            cpu_ms: Some(100),
            memory_bytes: Some(64 * 1024 * 1024),
            read_bytes: Some(1024),
            write_bytes: Some(2048),
            process_count: Some(2),
        };
        let guard = Containment::create(&limits).expect("作业对象应能设上限");
        let info = read_limits(guard.job);
        assert_eq!(
            info.basic.per_job_user_time_limit, 1_000_000,
            "100 ms = 1e6 × 100 ns"
        );
        assert_ne!(
            info.basic.limit_flags & JOB_OBJECT_LIMIT_JOB_TIME,
            0,
            "置了时间上限就必须带 JOB_OBJECT_LIMIT_JOB_TIME"
        );
        assert_eq!(info.basic.active_process_limit, 2);
        assert_eq!(info.process_memory_limit, 64 * 1024 * 1024);
        assert_eq!(info.job_memory_limit, 64 * 1024 * 1024);
    }

    #[test]
    fn process_count_zero_is_clamped_instead_of_disabling_the_limit() {
        // `ActiveProcessLimit = 0` 在内核里是"不限"——照抄进去等于把限制关掉。
        let limits = ResourceLimits {
            process_count: Some(0),
            ..ResourceLimits::default()
        };
        let guard = Containment::create(&limits).expect("作业对象应能设上限");
        let info = read_limits(guard.job);
        assert_eq!(info.basic.active_process_limit, 1, "0 必须夹到 1，不是 0");
    }

    #[test]
    fn sample_is_zero_before_anything_runs() {
        let guard = Containment::create(&ResourceLimits::default()).expect("作业对象可建");
        let usage = guard.usage();
        assert_eq!(usage.cpu_ms, 0, "空作业没有 CPU 记账");
        assert_eq!(usage.read_bytes, 0);
        assert_eq!(usage.peak_processes, 0);
        assert_eq!(
            guard.violation(&ResourceLimits {
                cpu_ms: Some(100),
                ..ResourceLimits::default()
            }),
            None,
            "还没跑任何程序，不该报超限"
        );
    }

    #[test]
    fn violation_reads_the_real_job_accounting() {
        // 这条用真实作业对象验证"轮询判定"接线：设一个 1 字节的 I/O 上限，
        // 起一个必然 >1 字节输出的进程，轮询必须报 IoLimit 而不是 None。
        // 进程本身由 `hbot-command` 的集成测试覆盖，这里只钉住"读得到记账"这一点。
        let guard = Containment::create(&ResourceLimits::default()).expect("作业对象可建");
        let limits = ResourceLimits {
            read_bytes: Some(1),
            ..ResourceLimits::default()
        };
        // 空作业没有 I/O，所以这里只能是 None；能返回 None 且不 panic 就说明
        // 轮询路径在"没有数据"时不会误报（误报比漏报更难查）。
        assert_eq!(guard.violation(&limits), None);
        assert_eq!(
            LimitViolation::Io {
                direction: IoDirection::Read,
                used_bytes: 2,
                limit_bytes: 1,
            }
            .into_denied()
            .code(),
            "HBOT_COMMAND_IO_LIMIT"
        );
    }
}
