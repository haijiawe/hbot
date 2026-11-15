//! Windows：作业对象（Job Object）做进程树隔离 + 资源上限。
//!
//! 为什么是作业对象而不是 `child.kill()`：`TerminateProcess` 只杀直接子进程，
//! `cmd → npm → node` 这种链会留下一串孤儿；作业对象是内核维护的集合，
//! `TerminateJobObject` 一次杀光，且 `KILL_ON_JOB_CLOSE` 保证句柄一关就不留活口。
//!
//! 为什么手写 FFI 而不是 `windows-sys`：本 crate 只用 5 个 kernel32 函数，
//! 为它引一个第三方依赖（以及一次供应链/版本审查）不划算；`AGENTS.md` 也要求
//! 新增依赖先获批。`std::process` 没有作业对象的抽象，`Command::creation_flags`
//! 只能设标志位（做不到"进程一出生就在作业里"）。
//!
//! 已知时间窗（残余风险，见 ADR-0003）：子进程是**先 spawn 再 Assign**，
//! 在两者之间它若抢先 `spawn` 出孙进程，那个孙进程不在作业里。要做到"出生即入作业"
//! 需要 `CreateProcessW` + `PROC_THREAD_ATTRIBUTE_JOB_LIST`（Win8+），属于下一批。

use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::process::{Child, Command};
use std::ptr;

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
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
}

/// `JOBOBJECTINFOCLASS::JobObjectExtendedLimitInformation`。
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

const JOB_OBJECT_LIMIT_ACTIVE_PROCESS: u32 = 0x0000_0008;
const JOB_OBJECT_LIMIT_PROCESS_MEMORY: u32 = 0x0000_0100;
const JOB_OBJECT_LIMIT_JOB_MEMORY: u32 = 0x0000_0200;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

/// 不弹控制台窗口（GUI 进程里跑命令时不闪黑框，也不改子进程的 stdin/stdout 语义）。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 作业内并发进程数上限。
///
/// 64 是"够 `npm install` 的子树、但拦住 fork 炸弹"的量级；真机接线时应改成
/// 可由设置页调整的配置，本原型先写死（ADR-0003 的"重估信号"之一）。
const MAX_ACTIVE_PROCESSES: u32 = 64;

/// 单进程内存上限（2 GiB）：`rustc` 编大 crate 会到 1 GB 上下，留一倍余量。
const PROCESS_MEMORY_LIMIT: usize = 2 * 1024 * 1024 * 1024;

/// 整个作业内存上限（4 GiB）：`cargo build` 同时起多个 `rustc` 时不要误杀。
const JOB_MEMORY_LIMIT: usize = 4 * 1024 * 1024 * 1024;

/// `JOBOBJECT_BASIC_LIMIT_INFORMATION`（x64 上 64 字节，字段顺序/对齐必须与 sdkddkver 一致）。
#[repr(C)]
struct BasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

/// `IO_COUNTERS`（6 × u64 = 48 字节）。
#[repr(C)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

/// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`（x64 上 144 字节）。
///
/// 长度必须一字不差地传给 `SetInformationJobObject`，否则它返回
/// 「参数错误」而不是指出哪个字段——照结构体定义手算对齐就是这个原因。
#[repr(C)]
struct ExtendedLimitInformation {
    basic: BasicLimitInformation,
    io: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

pub(crate) struct Containment {
    job: Handle,
}

// SAFETY: 作业对象句柄是内核对象句柄，跨线程使用是合法的（内核对象本身没有线程亲和性），
// 只有 `kill_tree`/`Drop` 会碰它；`PlatformRunner` 的签名要求 `Send + Sync`。
unsafe impl Send for Containment {}
unsafe impl Sync for Containment {}

impl Containment {
    pub(crate) fn create() -> Result<Self, String> {
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
        info.basic.active_process_limit = MAX_ACTIVE_PROCESSES;
        info.process_memory_limit = PROCESS_MEMORY_LIMIT;
        info.job_memory_limit = JOB_MEMORY_LIMIT;

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
        Ok(Self { job })
    }

    pub(crate) fn configure(&self, command: &mut Command) {
        command.creation_flags(CREATE_NO_WINDOW);
    }

    pub(crate) fn attach(&self, child: &Child) -> io::Result<()> {
        // `Child` 在 Windows 上持有的就是 `CreateProcessW` 返回的 hProcess（全权限），
        // 满足 AssignProcessToJobObject 需要的 PROCESS_SET_QUOTA | PROCESS_TERMINATE。
        let process = child.as_raw_handle() as Handle;
        // SAFETY: 两个句柄都由 Rust 侧持有且有效（job 存活于 self，process 存活于 child）。
        let ok = unsafe { AssignProcessToJobObject(self.job, process) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn kill_tree(&self, child: &mut Child) -> io::Result<()> {
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
    Containment::create()
        .map(drop)
        .map_err(|reason| format!("作业对象不可用（{reason}）"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn job_object_can_be_created_and_probed() {
        // 真实探测：本机能不能建作业对象（CI 上若在受限容器里跑，这条会红，
        // 那时要看的正是"探测如实报告了不可用"）。
        let probe_result = probe();
        assert!(probe_result.is_ok(), "作业对象应可用：{probe_result:?}");
    }
}
