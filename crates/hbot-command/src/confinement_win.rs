//! Windows 写隔离：受限令牌 `WRITE_RESTRICTED` + 工作区 write SID + 该目录上的 ACE。
//!
//! 为什么是这三件套（改代码前先读，这几条都是真机踩出来的）：
//!
//! 1. `DISABLE_MAX_PRIVILEGE` **不够**。它只是把令牌里的特权删掉，进程仍然以「用户身份」
//!    通过正常访问检查 —— 用户能写的地方它照样能写。真正的写限制来自 restricting SID：
//!    带 `WRITE_RESTRICTED` 建出来的令牌在做写访问检查时，**除了正常检查之外**还要再用
//!    restricting SID 做一次检查，两次都允许才算允许。所以 restricting SID 必须拿到
//!    「工作区可写」的授权 —— 也就是下面这个由工作区路径派生出来的 write SID。
//! 2. write SID 是**派生**的，不是系统账户：SID 只出现在我们加的 ACE 与令牌的 restricting
//!    列表里，不落地、不需要管理员创建。它必须对同一个工作区稳定（否则每次运行都往 DACL
//!    里塞一条新 ACE），所以用 SHA-256(域分隔串 + 小写化路径) 派生。
//! 3. `CreateProcessWithTokenW` 需要 `SeImpersonatePrivilege`，默认（未提权）管理员令牌里
//!    没有。这里走 `CreateProcessAsUserW`：当受限令牌是「本进程主令牌的受限版本」时，
//!    系统允许免 `SeAssignPrimaryTokenPrivilege` 使用。失败会把 1314 原样报出来，不降级。
//! 4. `lpApplicationName = NULL` 时 CreateProcess **不搜 PATH**，所以可执行文件绝对路径既
//!    传给 `lpApplicationName` 也拼进命令行。
//! 5. 受限令牌下孙进程**建不出新的管道 stdio**（沙箱里的程序自己 spawn 抓输出会 EPERM）。
//!    我们给子进程的 stdin/stdout/stderr 是父进程建好的、继承过去的句柄 —— 继承来的句柄
//!    不再走访问检查，所以命令本身的输出不受影响；受影响的是「命令内部再开管道」，
//!    这一条在 ADR 的残余风险里。
//!
//! 另外：本文件不用 `AddMandatoryAce`（不设完整性标签），所以不涉及「手工拼 ACL 缓冲区少算
//! 4 字节 → 1344」那个坑；DACL 的缓冲区由 `SetEntriesInAclW` 自己分配，`LocalFree` 释放。

use std::ffi::{OsStr, c_void};
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PRIVILEGE_NOT_HELD, ERROR_SUCCESS, GENERIC_READ, GetLastError, HANDLE,
    HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree, SetHandleInformation,
};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GetEffectiveRightsFromAclW, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE,
    SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACL, CreateRestrictedToken, DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE,
    PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
    WRITE_RESTRICTED,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_DELETE_CHILD, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
};

use crate::child::NativeChild;
use crate::confinement::SpawnPlan;

/// 域分隔串：改派生算法时**必须**升级版本号，否则旧工作区上已有的 ACE 会与新 SID 对不上。
const SID_DOMAIN: &[u8] = b"hbot-command/write-sid/v1\0";

/// 工作区里「能写文件、能删文件、能在子目录里删孩子」的权限。`FILE_GENERIC_WRITE` 不含
/// `DELETE` 与 `FILE_DELETE_CHILD`，两个都要显式加：少了前者改不了已有文件，少了后者
/// 删不掉目录里的文件。
const WRITE_MASK: u32 = FILE_GENERIC_WRITE | DELETE | FILE_DELETE_CHILD;

/// `DETACHED_PROCESS`：新进程不挂控制台（stdio 已经全是管道，不需要控制台）。
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// 受限令牌下启动子进程的创建标志。
///
/// **实测（本机 Windows 11，Medium 完整性）**：受限令牌（`WRITE_RESTRICTED`，带不带 restricting
/// SID 都一样）配 `CREATE_NO_WINDOW` 时子进程必定 `0xC0000142 STATUS_DLL_INIT_FAILED`，
/// 连 `cmd.exe /c exit 0` 都起不来；换成 `DETACHED_PROCESS` 或不带控制台相关旗标都正常。
/// 大概率是「建隐藏控制台」要写 `ConDrv` 设备，而 `WRITE_RESTRICTED` 把写访问判据换成了
/// restricting SID 列表 —— 那个列表里没有控制台设备的写权限。所以这一档不要 `CREATE_NO_WINDOW`。
const CONFINED_CREATION_FLAGS: u32 =
    DETACHED_PROCESS | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT;

/// 把 `canonicalize()` 出来的逐字（verbatim）路径还原成普通路径。
///
/// **实测**：`\\?\C:\Users\...` 这类逐字路径交给子进程当当前目录时，`cmd.exe` 会打印
/// `UNC paths are not supported. Defaulting to Windows directory.` 然后跑到 `C:\Windows` 里，
/// 于是工作区内的相对路径写盘全部失败（还会试图写系统目录）。写隔离只作用于绝对路径语义，
/// 这里只需去掉前缀，目录对象是同一个。
fn plain_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

/// 由路径派生的 write SID（二进制形式给 API，字符串形式给日志/报告）。
struct WriteSid {
    bytes: Vec<u8>,
    text: String,
}

impl WriteSid {
    fn from_path(path: &Path) -> Self {
        let mut material = SID_DOMAIN.to_vec();
        // NTFS 默认大小写不敏感：`C:\Work` 与 `c:\work` 是同一个目录，SID 必须相同，
        // 否则同一工作区会因为命令大小写不同而拿到两个 SID、两份 ACE。
        material.extend(
            path.to_string_lossy()
                .to_lowercase()
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        Self::from_material(&material)
    }

    /// 固定材料的 write SID（探测用：不依赖任何真实目录）。
    fn from_label(label: &str) -> Self {
        let mut material = SID_DOMAIN.to_vec();
        material.push(0xff);
        material.extend(label.as_bytes());
        Self::from_material(&material)
    }

    fn from_material(material: &[u8]) -> Self {
        let digest = crate::sha256::digest(material);
        let part = |offset: usize| {
            u32::from_be_bytes([
                digest[offset],
                digest[offset + 1],
                digest[offset + 2],
                digest[offset + 3],
            ])
        };
        let (a, b, c) = (part(0), part(4), part(8));
        // RID 取低 31 位再加 1：保证非零（0 是保留值），也让 RID 落在普通账户区间内。
        let rid = (part(12) & 0x7fff_ffff) + 1;

        // SID 二进制布局：revision(1) + 子权威个数(1) + 6 字节大端 authority + 每个子权威 4 字节小端。
        let mut bytes = Vec::with_capacity(8 + 16);
        bytes.push(1); // SID_REVISION
        bytes.push(4); // 4 个子权威
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 5]); // SECURITY_NT_AUTHORITY
        for value in [a, b, c, rid] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }

        Self {
            bytes,
            text: format!("S-1-5-{a}-{b}-{c}-{rid}"),
        }
    }

    fn as_ptr(&self) -> *mut c_void {
        self.bytes.as_ptr() as *mut c_void
    }
}

/// 把 `path` 上的写权限授予 `sid`（`SET_ACCESS` 是合并语义：保留已有 ACE，只改同 SID 的那条）。
fn apply_write_ace(path: &Path, sid: &WriteSid) -> Result<(), String> {
    let wide = wide_cstr(&path.to_string_lossy())
        .map_err(|error| format!("工作区路径无法转成 UTF-16（{error}）：{}", path.display()))?;

    let mut old_descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let mut old_dacl: *mut ACL = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut old_dacl,
            ptr::null_mut(),
            &mut old_descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "读取 {} 的 DACL 失败：{}",
            path.display(),
            os_error(status)
        ));
    }

    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: WRITE_MASK,
        grfAccessMode: SET_ACCESS,
        // 只写在目录上不够：命令在下面新建的文件/子目录要继承这条 ACE 才写得了。
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: trustee(sid),
    };
    let mut new_dacl: *mut ACL = ptr::null_mut();
    let status = unsafe { SetEntriesInAclW(1, &access, old_dacl, &mut new_dacl) };
    if status != ERROR_SUCCESS {
        unsafe {
            LocalFree(old_descriptor);
        }
        return Err(format!(
            "为 write SID 生成新 DACL 失败：{}",
            os_error(status)
        ));
    }

    let status = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            new_dacl,
            ptr::null_mut(),
        )
    };

    unsafe {
        LocalFree(new_dacl as *mut c_void);
        LocalFree(old_descriptor);
    }
    if status != ERROR_SUCCESS {
        return Err(format!(
            "把 DACL 写回 {} 失败：{}",
            path.display(),
            os_error(status)
        ));
    }
    Ok(())
}

/// 该路径的 DACL 给 `sid` 的有效权限（探针用它确认「ACE 真的生效了」）。
fn effective_rights(path: &Path, sid: &WriteSid) -> Result<u32, String> {
    let wide = wide_cstr(&path.to_string_lossy())
        .map_err(|error| format!("路径无法转成 UTF-16（{error}）：{}", path.display()))?;
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "读取 {} 的 DACL 失败：{}",
            path.display(),
            os_error(status)
        ));
    }

    let mut rights: u32 = 0;
    let status = unsafe { GetEffectiveRightsFromAclW(dacl, &trustee(sid), &mut rights) };
    unsafe {
        LocalFree(descriptor);
    }
    if status != ERROR_SUCCESS {
        return Err(format!(
            "计算 {} 的有效权限失败：{}",
            path.display(),
            os_error(status)
        ));
    }
    Ok(rights)
}

fn trustee(sid: &WriteSid) -> TRUSTEE_W {
    TRUSTEE_W {
        pMultipleTrustee: ptr::null_mut(),
        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: sid.as_ptr() as *mut u16,
    }
}

/// 受限令牌（restricting SID = 工作区 write SID）。
struct Confiner {
    token: HANDLE,
}

// 令牌句柄可以在线程间移动（与其它内核句柄一样），只是不能同时被两个线程用。
unsafe impl Send for Confiner {}

impl Drop for Confiner {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.token);
        }
    }
}

impl Confiner {
    fn new(sid: &WriteSid) -> Result<Self, String> {
        Self::create_token(Some(sid))
    }

    fn create_token(sid: Option<&WriteSid>) -> Result<Self, String> {
        let mut base: HANDLE = ptr::null_mut();
        let opened = unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
                &mut base,
            )
        };
        if opened == 0 {
            return Err(format!(
                "打开本进程令牌失败：{}",
                io::Error::last_os_error()
            ));
        }

        let restricted_sid = SID_AND_ATTRIBUTES {
            Sid: sid.map_or(ptr::null_mut(), WriteSid::as_ptr),
            Attributes: 0,
        };
        let count = u32::from(sid.is_some());
        let mut restricted: HANDLE = ptr::null_mut();
        let created = unsafe {
            CreateRestrictedToken(
                base,
                WRITE_RESTRICTED | DISABLE_MAX_PRIVILEGE,
                0,
                ptr::null(),
                0,
                ptr::null(),
                count,
                if count == 0 {
                    ptr::null()
                } else {
                    &restricted_sid
                },
                &mut restricted,
            )
        };
        let error = io::Error::last_os_error();
        unsafe {
            CloseHandle(base);
        }
        if created == 0 {
            return Err(format!("CreateRestrictedToken 失败：{error}"));
        }
        Ok(Self { token: restricted })
    }

    /// 用受限令牌启动进程。stdio 全走管道，句柄白名单保证只继承这三个句柄。
    fn spawn(&self, plan: &SpawnPlan) -> io::Result<NativeChild> {
        let mut application = wide(&plan.application.to_string_lossy());
        if application.contains(&0) {
            return Err(invalid_input("可执行文件路径含 NUL"));
        }
        // `lpApplicationName` 必须单独给一份带结尾 NUL 的缓冲区。
        application.push(0);

        let mut command_line: Vec<u16> = Vec::new();
        command_line.extend(quote_arg(&wide(&plan.application.to_string_lossy())));
        for argument in &plan.args {
            if argument.contains('\0') {
                return Err(invalid_input("命令行参数含 NUL"));
            }
            command_line.push(u16::from(b' '));
            command_line.extend(quote_arg(&wide(argument)));
        }
        command_line.push(0);

        let mut current_directory = wide(&plain_path(&plan.cwd).to_string_lossy());
        if current_directory.contains(&0) {
            return Err(invalid_input("工作目录含 NUL"));
        }
        current_directory.push(0);

        let environment = environment_block(&plan.env)?;

        // 继承属性：`bInheritHandle = TRUE`，否则子进程拿不到这三个句柄。
        let inheritable = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };

        let (mut stdout_read, stdout_write) = create_pipe(&inheritable)?;
        let (mut stderr_read, stderr_write) = create_pipe(&inheritable)?;
        // stdin：`NUL` 设备（不是管道）—— `null` 语义，且不占管道缓冲区。
        let stdin = open_nul(&inheritable)?;

        // 句柄白名单：`bInheritHandles = TRUE` 会继承所有可继承句柄，加白名单后只剩这三个。
        let handles = [stdin.0, stdout_write.0, stderr_write.0];
        let attribute_list = AttributeList::new(&handles)?;

        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = stdin.0;
        startup.StartupInfo.hStdOutput = stdout_write.0;
        startup.StartupInfo.hStdError = stderr_write.0;
        startup.lpAttributeList = attribute_list.raw();

        let flags = CONFINED_CREATION_FLAGS;
        let mut process = PROCESS_INFORMATION {
            hProcess: ptr::null_mut(),
            hThread: ptr::null_mut(),
            dwProcessId: 0,
            dwThreadId: 0,
        };
        let created = unsafe {
            CreateProcessAsUserW(
                self.token,
                application.as_ptr(),
                command_line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                1,
                flags,
                environment.as_ptr() as *const c_void,
                current_directory.as_ptr(),
                &startup.StartupInfo,
                &mut process,
            )
        };
        // 属性列表必须在 CreateProcess 返回之后才能删。
        drop(attribute_list);

        if created == 0 {
            let code = unsafe { GetLastError() };
            return Err(process_error(code));
        }
        unsafe {
            CloseHandle(process.hThread);
        }
        // 父进程这边把写端/读端交出去：写端随 `OwnedHandle` 的 Drop 关掉（不然读端等不到 EOF）。
        Ok(NativeChild::new(
            process.hProcess,
            process.dwProcessId,
            stdout_read.take(),
            stderr_read.take(),
        ))
    }
}

/// 工作区写隔离。
pub(crate) struct WriteConfinement {
    confiner: Confiner,
}

impl WriteConfinement {
    pub(crate) fn create(workspace_root: &Path) -> Result<Self, String> {
        let workspace = std::fs::canonicalize(workspace_root)
            .map_err(|error| format!("规范化工作区路径失败：{error}"))?;
        let sid = WriteSid::from_path(&workspace);
        apply_write_ace(&workspace, &sid)?;
        tracing::debug!(
            workspace = %workspace.display(),
            write_sid = sid.text,
            "Windows 写隔离已建立（受限令牌 + write SID + ACE）"
        );
        let confiner = Confiner::new(&sid)?;
        Ok(Self { confiner })
    }

    pub(crate) fn spawn(&self, plan: &SpawnPlan) -> io::Result<NativeChild> {
        self.confiner.spawn(plan)
    }
}

/// 这台机器能不能装出写隔离。缓存结果：探测要建令牌 + 写一条 ACE + 起一个随即退出的
/// 系统进程，跑不起每条命令都做一遍。
pub(crate) fn probe() -> Result<(), String> {
    static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
    PROBE.get_or_init(probe_once).clone()
}

fn probe_once() -> Result<(), String> {
    let sid = WriteSid::from_label("probe");

    // 1. 令牌：`WRITE_RESTRICTED` + 自建 restricting SID 建不建得出来。
    let confiner = Confiner::new(&sid)?;

    // 2. DACL 往返：在私有临时目录上真的加一条 ACE，再读回有效权限。
    //    只验证 API 返回值不够 —— 目录属主不对、卷不支持 DACL 时返回成功但权限没变。
    let scratch = std::env::temp_dir().join(format!(
        "hbot-command-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&scratch)
        .map_err(|error| format!("无法在临时目录建探测目录 {}：{error}", scratch.display()))?;
    let acl_result = (|| {
        apply_write_ace(&scratch, &sid)?;
        let rights = effective_rights(&scratch, &sid)?;
        let missing = WRITE_MASK & !rights;
        if missing != 0 {
            return Err(format!(
                "ACE 写入后有效权限仍缺位（缺 0x{missing:x}），说明这台机器的目录不支持写隔离"
            ));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    acl_result?;

    // 3. 端到端：用受限令牌真的起一个进程。`CreateProcessAsUserW` 在「受限令牌不是本进程
    //    主令牌的受限版本」时要 `SeAssignPrimaryTokenPrivilege`（默认令牌没有），而这一点
    //    只有真起进程才知道。跑的是一个随即退出的系统程序，不碰用户数据。
    let command = system_command()?;
    let mut child = confiner
        .spawn(&SpawnPlan {
            application: command,
            args: vec!["/c".to_string(), "exit".to_string(), "0".to_string()],
            cwd: std::env::temp_dir(),
            env: probe_environment(),
        })
        .map_err(|error| error.to_string())?;
    let mut stderr = String::new();
    if let Some(mut stream) = child.take_stderr() {
        let _ = stream.read_to_string(&mut stderr);
    }
    let exit = child.wait().map_err(|error| error.to_string())?;
    match exit.code() {
        Some(0) => Ok(()),
        other => Err(format!(
            "受限令牌下起进程失败（退出码 {other:?}）：{stderr}"
        )),
    }
}

/// 探测用的系统命令解释器（`/c exit 0` 只起进程、不写盘）。
fn system_command() -> Result<PathBuf, String> {
    if let Some(comspec) = std::env::var_os("ComSpec") {
        let path = PathBuf::from(comspec);
        if path.is_file() {
            return Ok(path);
        }
    }
    for candidate in [
        r"C:\Windows\System32\cmd.exe",
        r"C:\Windows\SysWOW64\cmd.exe",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("找不到 cmd.exe，无法验证受限令牌能否启动进程".to_string())
}

fn probe_environment() -> Vec<(String, String)> {
    let mut environment = Vec::new();
    for name in [
        "SystemRoot",
        "SystemDrive",
        "windir",
        "PATH",
        "PATHEXT",
        "ComSpec",
    ] {
        if let Some(value) = std::env::var_os(name) {
            environment.push((name.to_string(), value.to_string_lossy().into_owned()));
        }
    }
    environment
}

/// 一次 `CreateProcess` 用的进程/线程句柄对 + 属性列表。
struct AttributeList {
    /// 只用来保命：属性列表就摆在这块内存里，移动它会让上面的指针悬空。
    _buffer: Vec<usize>,
    raw: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl AttributeList {
    fn new(handles: &[HANDLE]) -> io::Result<Self> {
        let mut size: usize = 0;
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size);
        }
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        // `Vec<u8>` 的对齐只有 1，而属性列表要求指针对齐 —— 用 `usize` 缓冲。
        let mut buffer = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        let raw = buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if unsafe { InitializeProcThreadAttributeList(raw, 1, 0, &mut size) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let list = Self {
            _buffer: buffer,
            raw,
        };
        let bytes = std::mem::size_of_val(handles);
        if unsafe {
            UpdateProcThreadAttribute(
                list.raw,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr() as *const c_void,
                bytes,
                ptr::null_mut(),
                ptr::null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(list)
    }

    fn raw(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.raw
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.raw);
        }
    }
}

/// RAII 句柄：中途任何一步失败都不把句柄漏在父进程里。
struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn take(&mut self) -> HANDLE {
        std::mem::replace(&mut self.0, ptr::null_mut())
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn create_pipe(inheritable: &SECURITY_ATTRIBUTES) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read: HANDLE = ptr::null_mut();
    let mut write: HANDLE = ptr::null_mut();
    if unsafe { CreatePipe(&mut read, &mut write, inheritable, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let read = OwnedHandle(read);
    let write = OwnedHandle(write);
    // 读端留在父进程：不能被继承，否则子进程退出后读端等不到 EOF。
    if unsafe { SetHandleInformation(read.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((read, write))
}

fn open_nul(inheritable: &SECURITY_ATTRIBUTES) -> io::Result<OwnedHandle> {
    let path = wide_cstr(r"\\.\NUL")
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            inheritable,
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(handle))
}

/// MSVC 运行库的引号规则：反斜杠只在引号前加倍；参数以反斜杠结尾时再加倍。
fn quote_arg(argument: &[u16]) -> Vec<u16> {
    let quote = u16::from(b'"');
    let backslash = u16::from(b'\\');
    let space = u16::from(b' ');
    let tab = u16::from(b'\t');

    if !argument.is_empty()
        && !argument
            .iter()
            .any(|&unit| unit == space || unit == tab || unit == quote)
    {
        return argument.to_vec();
    }

    let mut quoted = vec![quote];
    let mut backslashes = 0usize;
    for &unit in argument {
        if unit == backslash {
            backslashes += 1;
            quoted.push(unit);
        } else if unit == quote {
            quoted.extend(std::iter::repeat_n(backslash, backslashes + 1));
            quoted.push(unit);
            backslashes = 0;
        } else {
            backslashes = 0;
            quoted.push(unit);
        }
    }
    quoted.extend(std::iter::repeat_n(backslash, backslashes));
    quoted.push(quote);
    quoted
}

/// Windows 要求环境块按名字排序（大小写不敏感），整体以双 NUL 结束。
fn environment_block(environment: &[(String, String)]) -> io::Result<Vec<u16>> {
    let mut entries: Vec<(String, String)> = Vec::with_capacity(environment.len());
    for (name, value) in environment {
        if name.contains('\0') || value.contains('\0') || name.contains('=') {
            return Err(invalid_input("环境变量名/值非法"));
        }
        entries.push((name.clone(), value.clone()));
    }
    entries.sort_by_key(|entry| entry.0.to_lowercase());

    let mut block = Vec::new();
    if entries.is_empty() {
        block.push(0);
    }
    for (name, value) in entries {
        block.extend(OsStr::new(&format!("{name}={value}")).encode_wide());
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn wide_cstr(text: &str) -> Result<Vec<u16>, String> {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    if wide.contains(&0) {
        return Err("字符串含 NUL".to_string());
    }
    wide.push(0);
    Ok(wide)
}

fn os_error(code: u32) -> String {
    format!("{}（{code}）", io::Error::from_raw_os_error(code as i32))
}

fn invalid_input(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason.to_string())
}

fn process_error(code: u32) -> io::Error {
    if code == ERROR_PRIVILEGE_NOT_HELD {
        return io::Error::new(
            io::ErrorKind::PermissionDenied,
            "CreateProcessAsUserW 被拒（1314 ERROR_PRIVILEGE_NOT_HELD）：受限令牌必须是本进程\
             主令牌的受限版本，或调用方拥有 SeAssignPrimaryTokenPrivilege。本机装不出写隔离。"
                .to_string(),
        );
    }
    io::Error::other(format!("CreateProcessAsUserW 失败：{}", os_error(code)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn scratch_dir(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("hbot-command-wconf-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("创建测试目录");
        std::fs::canonicalize(&path).expect("规范化测试目录")
    }

    fn comspec() -> PathBuf {
        system_command().expect("本机应能拿到 cmd.exe")
    }

    /// 用受限令牌跑一条 cmd 命令，返回（退出码，合并输出）。
    fn run(workspace: &Path, args: &[&str]) -> (i32, String) {
        let confinement = super::WriteConfinement::create(workspace).expect("建写隔离");
        let mut child = confinement
            .spawn(&plan(workspace, args))
            .expect("启动受限进程");
        let mut output = String::new();
        if let Some(mut stream) = child.take_stderr() {
            let _ = stream.read_to_string(&mut output);
        }
        if let Some(mut stream) = child.take_stdout() {
            let _ = stream.read_to_string(&mut output);
        }
        let exit = child.wait().expect("等待退出");
        (exit.code().unwrap_or(-1), output)
    }

    fn scratch_workspace(tag: &str) -> PathBuf {
        let path = scratch_dir(tag);
        std::fs::create_dir_all(&path).expect("建工作区");
        path
    }

    fn plan(workspace: &Path, args: &[&str]) -> SpawnPlan {
        SpawnPlan {
            application: comspec(),
            args: args.iter().map(|argument| argument.to_string()).collect(),
            cwd: workspace.to_path_buf(),
            env: probe_environment(),
        }
    }

    #[test]
    fn write_sid_is_stable_and_case_insensitive() {
        let upper = WriteSid::from_path(Path::new(r"C:\Work\Repo"));
        let lower = WriteSid::from_path(Path::new(r"c:\work\repo"));
        assert_eq!(upper.text, lower.text);
        assert_eq!(upper.bytes, lower.bytes);
        assert_ne!(
            upper.text,
            WriteSid::from_path(Path::new(r"C:\Work\Other")).text
        );
        assert!(upper.text.starts_with("S-1-5-"));
        assert_eq!(upper.bytes[0], 1);
        assert_eq!(upper.bytes[1], 4);
        assert_eq!(upper.bytes.len(), 8 + 4 * 4);
    }

    #[test]
    fn probe_reports_this_machine() {
        match super::probe() {
            Ok(()) => {}
            Err(reason) => panic!("本机应能装出写隔离，实际失败：{reason}"),
        }
    }

    #[test]
    fn workspace_ace_grants_write() {
        let workspace = scratch_dir("ace");
        let sid = WriteSid::from_path(&workspace);
        apply_write_ace(&workspace, &sid).expect("加 ACE");
        let rights = effective_rights(&workspace, &sid).expect("读取有效权限");
        assert_eq!(
            rights & WRITE_MASK,
            WRITE_MASK,
            "write SID 应拿到全部写权限（实测 0x{rights:x}）"
        );
        // `SET_ACCESS` 是合并语义：别人的权限不能被这条 ACE 削掉，否则我们会把工作区
        // 从自己的用户手里锁死。
        let other = WriteSid::from_label("someone-else");
        let other_rights = effective_rights(&workspace, &other).expect("读取有效权限");
        assert_eq!(other_rights & WRITE_MASK, 0, "别的 SID 不该拿到写权限");
    }

    #[test]
    fn restricted_token_can_write_inside_and_not_outside() {
        let workspace = scratch_workspace("write");
        let outside = std::env::temp_dir().join(format!(
            "hbot-command-wconf-outside-{}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&outside);

        let (code, output) = run(&workspace, &["/c", "echo", "inside", ">", "inside.txt"]);
        assert_eq!(code, 0, "工作区内写应成功：{output}");
        let written = workspace.join("inside.txt");
        assert!(written.is_file(), "工作区内的文件应存在");
        assert!(
            std::fs::read_to_string(&written)
                .expect("读取")
                .contains("inside")
        );

        let command = format!("echo escaped> \"{}\"", outside.display());
        let (code, output) = run(&workspace, &["/c", &command]);
        assert_ne!(code, 0, "工作区外写应失败：{output}");
        assert!(
            !outside.exists(),
            "工作区外的文件不该存在：{}",
            outside.display()
        );

        // 相对路径逃逸也一样（`..\` 在 cmd 里按当前目录解析）。
        let (code, output) = run(
            &workspace,
            &["/c", "echo", "escaped", ">", r"..\escaped.txt"],
        );
        assert_ne!(code, 0, "相对路径逃逸应失败：{output}");
        let escaped = workspace.parent().expect("父目录").join("escaped.txt");
        assert!(!escaped.exists(), "逃逸目标不该存在：{}", escaped.display());

        // 删工作区里的文件是允许的（`FILE_DELETE_CHILD`），否则命令清理不了自己的产物。
        let (code, output) = run(&workspace, &["/c", "del", "inside.txt"]);
        assert_eq!(code, 0, "窗口内的删除应成功：{output}");
        assert!(!written.exists(), "文件应已被删除");
    }

    #[test]
    fn confined_process_can_still_read_outside() {
        // 写限制只做「第二道写检查」，读不该被拦；否则命令连可执行文件都加载不了。
        let workspace = scratch_workspace("read");
        let file = workspace.join("readable.txt");
        std::fs::File::create(&file)
            .and_then(|mut handle| handle.write_all(b"hello"))
            .expect("准备文件");
        let (code, output) = run(&workspace, &["/c", "type", "readable.txt"]);
        assert_eq!(code, 0, "读应成功：{output}");
        assert!(output.contains("hello"), "应读到内容：{output}");
    }

    /// 钉住创建标志：实测受限令牌配 `CREATE_NO_WINDOW` 必定 `0xC0000142`，命令根本跑不起来。
    /// 谁要是把它加回来，这条测试会先红，而不是等到真机上「命令全都起不来」才发现。
    #[test]
    fn confined_flags_do_not_ask_for_a_console() {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        assert_eq!(super::CONFINED_CREATION_FLAGS & CREATE_NO_WINDOW, 0);
        assert_eq!(
            super::CONFINED_CREATION_FLAGS & super::DETACHED_PROCESS,
            super::DETACHED_PROCESS
        );
    }

    #[test]
    fn plain_path_strips_verbatim_prefix() {
        let verbatim = Path::new(r"\\?\C:\Users\me\workspace");
        assert_eq!(
            super::plain_path(verbatim),
            Path::new(r"C:\Users\me\workspace")
        );
        assert_eq!(
            super::plain_path(Path::new(r"\\?\UNC\srv\share\ws")),
            Path::new(r"\\srv\share\ws")
        );
        // 普通路径原样返回，别把相对路径也改写掉。
        assert_eq!(
            super::plain_path(Path::new(r"C:\plain")),
            Path::new(r"C:\plain")
        );
    }
}
