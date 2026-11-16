//! 子进程句柄抽象。
//!
//! 为什么需要它：Windows 的写隔离必须自己调 `CreateProcessAsUserW`（`std::process::Command`
//! 没有「用哪个令牌」的接口，也没有 `STARTUPINFOEX` 句柄白名单的钩子），于是运行器要同时
//! 处理两种子进程：std 的 `Child` 与受限令牌建出来的原生进程。把所有 `cfg` 收在这一个文件
//! 里，`runner` 就只剩一条逻辑。

use std::io::{self, Read};

/// 子进程退出结果。原生进程拿不到 `std::process::ExitStatus`（那是 std 自己造的句柄包装），
/// 所以单独一支。
#[derive(Debug, Clone, Copy)]
pub(crate) enum Exit {
    /// 由 `std::process` 创建的子进程。
    Std(std::process::ExitStatus),
    /// 由受限令牌原生创建的子进程（Windows）。
    #[cfg(windows)]
    Code(i32),
}

impl Exit {
    pub(crate) fn code(self) -> Option<i32> {
        match self {
            Self::Std(status) => status.code(),
            #[cfg(windows)]
            Self::Code(code) => Some(code),
        }
    }

    #[cfg(unix)]
    pub(crate) fn signal(self) -> Option<i32> {
        use std::os::unix::process::ExitStatusExt;

        match self {
            Self::Std(status) => status.signal(),
            #[cfg(windows)]
            Self::Code(_) => None,
        }
    }

    #[cfg(not(unix))]
    pub(crate) fn signal(self) -> Option<i32> {
        let _ = self;
        None
    }
}

/// 一个已启动的子进程。
pub(crate) struct Spawned {
    inner: Inner,
}

enum Inner {
    Std(std::process::Child),
    #[cfg(windows)]
    Native(NativeChild),
}

impl Spawned {
    pub(crate) fn std(child: std::process::Child) -> Self {
        Self {
            inner: Inner::Std(child),
        }
    }

    #[cfg(windows)]
    pub(crate) fn native(child: NativeChild) -> Self {
        Self {
            inner: Inner::Native(child),
        }
    }

    pub(crate) fn id(&self) -> u32 {
        match &self.inner {
            Inner::Std(child) => child.id(),
            #[cfg(windows)]
            Inner::Native(child) => child.pid,
        }
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<Exit>> {
        match &mut self.inner {
            Inner::Std(child) => child.try_wait().map(|status| status.map(Exit::Std)),
            #[cfg(windows)]
            Inner::Native(child) => child.try_wait(),
        }
    }

    pub(crate) fn wait(&mut self) -> io::Result<Exit> {
        match &mut self.inner {
            Inner::Std(child) => child.wait().map(Exit::Std),
            #[cfg(windows)]
            Inner::Native(child) => child.wait(),
        }
    }

    /// 尽力杀掉子进程本身（进程树由 `Containment` 负责）。
    pub(crate) fn kill(&mut self) -> io::Result<()> {
        match &mut self.inner {
            Inner::Std(child) => child.kill(),
            #[cfg(windows)]
            Inner::Native(child) => child.kill(),
        }
    }

    pub(crate) fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        match &mut self.inner {
            Inner::Std(child) => child
                .stdout
                .take()
                .map(|stream| Box::new(stream) as Box<dyn Read + Send>),
            #[cfg(windows)]
            Inner::Native(child) => child.take_stdout(),
        }
    }

    pub(crate) fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        match &mut self.inner {
            Inner::Std(child) => child
                .stderr
                .take()
                .map(|stream| Box::new(stream) as Box<dyn Read + Send>),
            #[cfg(windows)]
            Inner::Native(child) => child.take_stderr(),
        }
    }

    /// 进程句柄，给作业对象（`Containment`）用。
    #[cfg(windows)]
    pub(crate) fn process_handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        use std::os::windows::io::AsRawHandle;

        match &self.inner {
            Inner::Std(child) => child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            Inner::Native(child) => child.process,
        }
    }
}

/// 受限令牌创建出来的原生子进程（Windows）。
#[cfg(windows)]
pub(crate) struct NativeChild {
    process: windows_sys::Win32::Foundation::HANDLE,
    pid: u32,
    stdout: Option<windows_sys::Win32::Foundation::HANDLE>,
    stderr: Option<windows_sys::Win32::Foundation::HANDLE>,
    exit: Option<i32>,
}

// 句柄本身是内核对象标识，跨线程移动是安全的（与 `std::process::Child` 同理）。
#[cfg(windows)]
unsafe impl Send for NativeChild {}

#[cfg(windows)]
impl NativeChild {
    pub(crate) fn new(
        process: windows_sys::Win32::Foundation::HANDLE,
        pid: u32,
        stdout: windows_sys::Win32::Foundation::HANDLE,
        stderr: windows_sys::Win32::Foundation::HANDLE,
    ) -> Self {
        Self {
            process,
            pid,
            stdout: Some(stdout),
            stderr: Some(stderr),
            exit: None,
        }
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<Exit>> {
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

        if let Some(code) = self.exit {
            return Ok(Some(Exit::Code(code)));
        }
        let waited = unsafe { WaitForSingleObject(self.process, 0) };
        if waited == WAIT_TIMEOUT {
            return Ok(None);
        }
        if waited != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        let mut code: u32 = 0;
        if unsafe { GetExitCodeProcess(self.process, &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let code = code as i32;
        self.exit = Some(code);
        Ok(Some(Exit::Code(code)))
    }

    pub(crate) fn wait(&mut self) -> io::Result<Exit> {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, INFINITE, WaitForSingleObject,
        };

        if let Some(code) = self.exit {
            return Ok(Exit::Code(code));
        }
        let waited = unsafe { WaitForSingleObject(self.process, INFINITE) };
        if waited != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        let mut code: u32 = 0;
        if unsafe { GetExitCodeProcess(self.process, &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let code = code as i32;
        self.exit = Some(code);
        Ok(Exit::Code(code))
    }

    pub(crate) fn kill(&mut self) -> io::Result<()> {
        use windows_sys::Win32::System::Threading::TerminateProcess;

        if unsafe { TerminateProcess(self.process, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stdout.take().map(file_from_handle)
    }

    pub(crate) fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stderr.take().map(file_from_handle)
    }
}

#[cfg(windows)]
impl Drop for NativeChild {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        unsafe {
            if !self.process.is_null() {
                CloseHandle(self.process);
            }
            for handle in [self.stdout.take(), self.stderr.take()]
                .into_iter()
                .flatten()
            {
                CloseHandle(handle);
            }
        }
    }
}

#[cfg(windows)]
fn file_from_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> Box<dyn Read + Send> {
    use std::os::windows::io::{FromRawHandle, RawHandle};

    Box::new(unsafe { std::fs::File::from_raw_handle(handle as RawHandle) })
}
