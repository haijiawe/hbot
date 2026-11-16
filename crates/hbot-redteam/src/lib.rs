//! 红队用例集（`red-team`）的**夹具库**：只有临时目录、错误链拼接与
//! 少量断言辅助，**没有任何实现**。
//!
//! 存在的理由（`docs/security/red-team-findings.md`）：
//!
//! - 六条攻击面（路径逃逸 / 命令注入 / 提示注入 / 审批绕过 / 资源耗尽 / 密钥泄漏）
//!   的用例分散在 `tests/*.rs`，它们需要同一套"造一个真实临时项目、跑完就删"的夹具；
//! - 夹具如果每个文件各写一份，就会出现"某个文件忘了删临时目录"或"路径拼法不一致"
//!   导致的假绿/假红；
//! - 这个 crate 是**唯一**允许反向依赖实现 crate 的地方（`xtask/src/deps.rs` 的
//!   `ALLOWED` 已登记），所以夹具放这里不会污染实现方向。
//!
//! 依赖方向：本 crate 的 `[dev-dependencies]` 里才有 `hbot-*`，`[dependencies]`
//! 为空 —— 因此它不可能被任何实现 crate 依赖到。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 全局自增序号：给临时目录名去重（进程 id 之外再加一层）。
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 在系统临时目录下造一个唯一路径（**不创建**）。
///
/// 名字里带进程 id 与自增序号：并行测试（`cargo test` 默认多线程）不会互撞，
/// 也不依赖任何第三方 crate（workspace 没有 `tempfile`）。
#[must_use]
pub fn unique_path(tag: &str) -> PathBuf {
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("hbot-redteam-{tag}-{}-{seq}", std::process::id()))
}

/// 一个用完就删的临时目录树。
///
/// `Drop` 里尽力删除：清理失败不许让测试红（Windows 上被杀掉的子进程可能还短暂
/// 持有文件句柄，`remove_dir_all` 会 EPERM/EBUSY）。
#[derive(Debug)]
pub struct TempTree(PathBuf);

impl TempTree {
    /// 造一个空的临时目录。
    #[must_use]
    pub fn new(tag: &str) -> Self {
        let root = unique_path(tag);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时项目根失败");
        Self(root)
    }

    /// 根路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// 拼一个项目内相对路径（**不检查**它是否逃逸：这正是被测代码的职责）。
    #[must_use]
    pub fn join(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }

    /// 写一个文件，自动创建父目录；返回绝对路径。
    pub fn write(&self, relative: &str, body: &str) -> PathBuf {
        self.write_bytes(relative, body.as_bytes())
    }

    /// 写一个字节文件，自动创建父目录；返回绝对路径。
    pub fn write_bytes(&self, relative: &str, body: &[u8]) -> PathBuf {
        let path = self.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("创建父目录失败");
        }
        std::fs::write(&path, body).expect("写夹具文件失败");
        path
    }

    /// 建一个目录树，返回绝对路径。
    pub fn mkdir(&self, relative: &str) -> PathBuf {
        let path = self.join(relative);
        std::fs::create_dir_all(&path).expect("建夹具目录失败");
        path
    }

    /// 项目根内某个相对路径现在的内容（不存在返回 `None`）。
    #[must_use]
    pub fn read(&self, relative: &str) -> Option<String> {
        std::fs::read_to_string(self.join(relative)).ok()
    }

    /// 项目根内某个相对路径是否存在。
    #[must_use]
    pub fn exists(&self, relative: &str) -> bool {
        self.join(relative).exists()
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 把任意错误格式化成一条**可 grep 的**完整错误链。
///
/// 密钥泄漏（F 面）要断言的东西不是 `Display` 一层，而是"调用方把 `{:#}`、
/// `{:?}`、`source()` 逐层拼起来之后"的全貌 —— 只查一层会漏掉被 `thiserror`
/// 包在 `source` 里的上游文案。
#[must_use]
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut out = String::new();
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    let mut depth = 0usize;
    while let Some(item) = current {
        out.push_str(&format!("[{depth}] {item}\n"));
        // `{:?}` 对 thiserror 生成的 Debug 是 `Display` 的超集（含字段名）。
        out.push_str(&format!("[{depth}] {item:?}\n"));
        current = item.source();
        depth += 1;
        if depth > 32 {
            out.push_str("[链过长，已截断]\n");
            break;
        }
    }
    out.push_str(&format!("[alt] {error:#}\n"));
    out
}

/// 断言一段错误文本**不含**某个绝对路径（A 面的信息泄漏判据）。
///
/// 为什么信息泄漏也算缺陷：错误文案会经 `ApiError.message` 直达 UI 与日志，
/// 而项目根是用户机器上的绝对路径（含用户名）——它把"这台机器上谁在哪个目录
/// 跑这个 bot"写进了可外发的地方。
///
/// # Panics
///
/// 文本里出现任意一种根路径写法（原样 / `\\?\` 前缀 / `/` 分隔）时 panic。
pub fn assert_no_path_leak(text: &str, root: &Path) {
    let mut forms: Vec<String> = Vec::new();
    push_path_forms(&mut forms, root);
    // `%TEMP%` 在本机是 **8.3 短名**（`C:\Users\ADMINI~1\...`），而工作区内部规范化出来的
    // 是长名（`\\?\C:\Users\Administrator\...`）：两者是**不同的字符串**，只比较其中一个
    // 会让断言静默失效（假绿）。所以短名、长名、以及目录**叶子名**都算泄漏形态。
    if let Ok(canonical) = std::fs::canonicalize(root) {
        push_path_forms(&mut forms, &canonical);
    }
    // 叶子名是唯一且与短名/长名无关的：它只可能出现在路径里，不可能出现在相对路径中。
    if let Some(name) = root.file_name().and_then(|name| name.to_str()) {
        if name.len() >= 8 {
            forms.push(name.to_owned());
        }
    }
    for form in forms {
        if form.is_empty() {
            continue;
        }
        assert!(
            !text.contains(&form),
            "错误信息泄漏了项目根的绝对路径 `{form}`：\n{text}"
        );
        let slashed = form.replace('\\', "/");
        assert!(
            !text.contains(&slashed),
            "错误信息泄漏了项目根的绝对路径 `{slashed}`：\n{text}"
        );
    }
}

/// 把一个路径的几种等价写法推入 `forms`（原样 / 正斜杠 / `\\?\` 前缀）。
fn push_path_forms(forms: &mut Vec<String>, path: &Path) {
    let display = path.display().to_string();
    if display.is_empty() {
        return;
    }
    forms.push(display.clone());
    forms.push(display.replace('\\', "/"));
    if let Some(raw) = path.to_str() {
        if !raw.starts_with(r"\\?\") {
            forms.push(format!(r"\\?\{raw}"));
        }
    }
}

// ── 子进程存活判定（B 面「超时必须杀掉整棵进程树」要用）──────────────────────

/// 进程是否还活着（Windows：`OpenProcess` + 0 超时 `WaitForSingleObject`）。
///
/// 打不开句柄通常意味着进程已经没了（或 PID 已被回收，概率可忽略）。
#[cfg(windows)]
#[must_use]
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
            return false;
        }
        let wait = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        wait == WAIT_TIMEOUT
    }
}

/// 进程是否还活着（`/proc/<pid>` 存在即可）。
#[cfg(not(windows))]
#[must_use]
pub fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// 等到进程消失，返回「是否真的消失了」。
pub fn wait_gone(pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !process_exists(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    !process_exists(pid)
}

/// 等文件出现并返回其内容（helper 用它把 pid 交给父进程）。
pub fn wait_for_file(path: &Path, timeout: std::time::Duration) -> Option<String> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            let text = text.trim().to_owned();
            if !text.is_empty() {
                return Some(text);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    None
}

/// 把 helper 写进文件的一行 `KEY=1234` 解析成 pid。
#[must_use]
pub fn parse_pid(text: &str, key: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        line.strip_prefix(key)?
            .strip_prefix('=')
            .map(str::trim)?
            .parse()
            .ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_tree_creates_and_removes_its_root() {
        let root;
        {
            let tree = TempTree::new("selftest");
            root = tree.path().to_path_buf();
            assert!(root.is_dir(), "根目录必须真的建出来");
            tree.write("src/main.ts", "export const x = 1;");
            assert_eq!(
                tree.read("src/main.ts").as_deref(),
                Some("export const x = 1;")
            );
            tree.mkdir("a/b");
            assert!(tree.exists("a/b"));
        }
        assert!(!root.exists(), "Drop 之后临时目录必须被删掉");
    }

    #[test]
    fn unique_path_never_repeats() {
        let first = unique_path("dup");
        let second = unique_path("dup");
        assert_ne!(first, second);
    }

    #[test]
    fn error_chain_walks_every_source() {
        #[derive(Debug)]
        struct Outer(Inner);
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "外层")
            }
        }
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "内层")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        impl std::error::Error for Inner {}

        let chain = error_chain(&Outer(Inner));
        assert!(chain.contains("外层"));
        assert!(chain.contains("内层"), "source 链必须被走完：\n{chain}");
    }

    #[test]
    fn path_leak_detection_covers_both_separators() {
        let root = Path::new(r"C:\Users\tester\Projects\bot");
        assert_no_path_leak("路径 `../../etc/passwd` 不允许", root);
        let leaked = format!("写入失败：{}", root.display());
        let outcome = std::panic::catch_unwind(|| assert_no_path_leak(&leaked, root));
        assert!(outcome.is_err(), "泄漏必须被抓住");
    }

    #[test]
    fn path_leak_detection_matches_the_canonical_root_too() {
        // 回归守卫：`%TEMP%` 在本机是 8.3 短名（`C:\Users\ADMINI~1\…`），而工作区错误里出现的是
        // `fs::canonicalize` 出来的长名（`\\?\C:\Users\Administrator\…`）。只比较短名会让
        // 「不得泄漏绝对路径」的断言**永远为真**（假绿）——这个坑真踩过。
        let tree = TempTree::new("leak-canonical");
        let canonical = std::fs::canonicalize(tree.path()).expect("规范化临时目录");
        let leaked = format!(
            "not found: path `{}`",
            canonical.join("missing.txt").display()
        );
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_no_path_leak(&leaked, tree.path());
        }));
        assert!(
            outcome.is_err(),
            "规范化后的绝对路径也必须被判为泄漏（短名/长名不一致会让断言假绿）"
        );
    }
}
