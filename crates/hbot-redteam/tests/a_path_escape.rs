//! A 面：路径逃逸（目标 `crates/hbot-workspace`）。
//!
//! 统一的判据：**一律 `Err`**，且错误信息里不得出现项目根的绝对路径（信息泄漏也算缺陷）。
//! 少数「按字面名处理、结果仍在根内」的输入（如 `%2e%2e`）不算逃逸，但必须证明落点仍在根内，
//! 并且根外没有产生任何文件 —— 否则「没报错」就等于「写到了外面」。

use std::path::Path;

use hbot_core::HbotError;
use hbot_redteam::{TempTree, assert_no_path_leak, error_chain};
use hbot_workspace::Workspace;

/// 打开一棵临时工作区（根目录已存在）。
fn open_ws(tree: &TempTree) -> Workspace {
    Workspace::open(tree.path().to_path_buf()).expect("临时工作区应当能打开")
}

/// 把 `Result` 的失败渲染成一条消息链；`Ok` 直接判红（本文件所有断言都要求拒绝）。
fn reject<T: std::fmt::Debug>(what: &str, result: Result<T, HbotError>) -> String {
    match result {
        Ok(value) => panic!("【{what}】应当被拒绝，实际却成功了：{value:?}"),
        Err(error) => error_chain(&error),
    }
}

/// 探针：读取项目根的**父目录**里是否出现了不该出现的文件（逃逸成功的最硬证据）。
fn assert_nothing_landed_outside(tree: &TempTree, name: &str) {
    let parent = tree.path().parent().expect("临时目录应当有父目录");
    assert!(
        !parent.join(name).exists(),
        "根外出现了 `{name}`，说明写入真的逃出了工作区"
    );
}

/// 判断解析结果是否仍在根内。`resolve_*` 返回的是 `\\?\C:\…` 形式的规范化路径，
/// 而 `TempTree::path()` 是 `C:\…`，两者要按「规范化后前缀」比较。
fn is_within_tree(resolved: &Path, tree: &TempTree) -> bool {
    let canonical_root =
        std::fs::canonicalize(tree.path()).unwrap_or_else(|_| tree.path().to_path_buf());
    resolved.starts_with(&canonical_root) || resolved.starts_with(tree.path())
}

/// 表驱动地用 `resolve_new` 探一条输入：拒绝就放过，接受就检查落点仍在根内。
///
/// **这里刻意不查「错误信息是否泄漏绝对路径」**：那是另一个独立性质（RT-01），由
/// `a20` 统一覆盖所有编码形态。理由：泄漏是 `Workspace` 的错误**渲染**问题，
/// 与"这条输入有没有逃出根"无关；把两条断言混在表驱动里，会让一个渲染缺陷
/// 同时打红十几条正交用例（信号噪声），而所有编码形态在 `a20` 里一条都不少。
fn assert_contained_or_rejected(tree: &TempTree, ws: &Workspace, raw: &str) {
    match ws.resolve_new(Path::new(raw)) {
        Err(_) => {}
        Ok(resolved) => assert!(
            is_within_tree(&resolved, tree),
            "【{raw}】解析到了根外：{}",
            resolved.display()
        ),
    }
}
// ── 1. `..` 各种拼法 ───────────────────────────────────────────────────────────

/// 防的是：`..` 被当成普通文件名（或只查了首段）从而把子目录层级退回根外。
#[test]
fn a01_路径遍历的各种拼法都必须被拒() {
    let tree = TempTree::new("a01");
    tree.mkdir("sub");
    let ws = open_ws(&tree);

    for raw in [
        "..",
        "../",
        "../secret.txt",
        "sub/../../secret.txt",
        "sub/./../../a.txt",
        "a/../../b",
        "./../x",
    ] {
        let message = reject(raw, ws.resolve_existing(Path::new(raw)));
        assert!(
            message.contains("..") || message.contains("traversal"),
            "【{raw}】的拒绝理由应当点名路径遍历，实际：{message}"
        );
    }
    assert_nothing_landed_outside(&tree, "secret.txt");
}

/// 防的是：`..` 与真实符号链接叠加（先跳进链接再 `..`）把解析点落到根外。
#[test]
fn a02_子目录里先进入再回退两层也必须被拒() {
    let tree = TempTree::new("a02");
    tree.mkdir("sub");
    tree.mkdir("sub/deeper");
    let ws = open_ws(&tree);

    for raw in [
        "sub/deeper/../../../x.txt",
        "sub/deeper/../../sub/../../x.txt",
    ] {
        let message = reject(raw, ws.resolve_existing(Path::new(raw)));
        assert!(message.contains(".."), "应当点名 `..`，实际：{message}");
    }
    assert_nothing_landed_outside(&tree, "x.txt");
}

// ── 2. 编码绕过 ───────────────────────────────────────────────────────────────

/// 防的是：把 `%2e%2e` 当成「已经被上层解码过的 `..`」而漏判，或反过来解码后再拼路径逃逸。
///
/// 实测结论：本实现**不做百分号解码**，`%2e%2e` 是字面文件名，因此不逃逸。
/// 判定为安全，但必须证明「要么拒绝、要么落点仍在根内」。
#[test]
fn a03_百分号编码的点号不得逃出根() {
    let tree = TempTree::new("a03");
    let ws = open_ws(&tree);

    for raw in [
        "%2e%2e/secret.txt",
        "%2e%2e%2fsecret.txt",
        "%2e%2e%5csecret.txt",
        "..%2fsecret.txt",
        "%252e%252e/secret.txt",
    ] {
        assert_contained_or_rejected(&tree, &ws, raw);
    }
    assert_nothing_landed_outside(&tree, "secret.txt");
}

/// 防的是：`....//`、`....\` 这类「四个点 + 分隔符」被某些解码器折叠成 `../`。
#[test]
fn a04_四点与混合分隔符的变体必须被拒() {
    let tree = TempTree::new("a04");
    let ws = open_ws(&tree);

    for raw in [
        "....//....//secret.txt",
        "....\\\\secret.txt",
        "....//secret.txt",
        "..\\..\\secret.txt",
        "..%2e/secret.txt",
    ] {
        reject(raw, ws.resolve_new(Path::new(raw)));
    }
    assert_nothing_landed_outside(&tree, "secret.txt");
}

/// 防的是：Unicode 全角句点 `．．`（U+FF0E）被 NFKC 归一化后当成 `..`，而守卫只认 ASCII。
#[test]
fn a05_全角句点不得当作上跳目录() {
    let tree = TempTree::new("a05");
    let ws = open_ws(&tree);

    for raw in ["．．/secret.txt", "．．．/secret.txt", "。．/secret.txt"] {
        assert_contained_or_rejected(&tree, &ws, raw);
    }
    assert_nothing_landed_outside(&tree, "secret.txt");
}

// ── 3. 绝对路径 / 盘符 / UNC / 设备前缀 ────────────────────────────────────────

/// 防的是：绝对路径被 `root.join(absolute)` 的语义吞掉（Rust 里 join 绝对路径会整体替换），
/// 于是「项目内相对路径」直接变成读遍全盘。
#[test]
fn a06_绝对路径必须被拒() {
    let tree = TempTree::new("a06");
    let ws = open_ws(&tree);

    for raw in [
        "/etc/passwd",
        "/",
        "C:\\Windows\\win.ini",
        "c:/windows/win.ini",
        "\\Windows\\win.ini",
    ] {
        let message = reject(raw, ws.resolve_existing(Path::new(raw)));
        assert!(
            message.contains("absolute") || message.contains("绝对"),
            "【{raw}】应当因「绝对路径」被拒，实际：{message}"
        );
    }
}

/// 防的是：盘符相对路径 `C:x`（Windows 语义 = C 盘当前目录）被当成普通相对名，
/// 从而解析到**另一个盘**上。
#[test]
fn a07_盘符相对路径必须被拒() {
    let tree = TempTree::new("a07");
    let ws = open_ws(&tree);

    for raw in ["C:x", "c:secret.txt", "C:..\\x", "Z:file.txt"] {
        let message = reject(raw, ws.resolve_existing(Path::new(raw)));
        assert!(
            message.contains("drive") || message.contains("盘"),
            "【{raw}】应当因「盘符路径」被拒，实际：{message}"
        );
    }
}

/// 防的是：UNC / `\\?\` / 设备命名空间把路径重定向到网络共享或原始设备。
#[test]
fn a08_unc与设备命名空间必须被拒() {
    let tree = TempTree::new("a08");
    let ws = open_ws(&tree);

    for raw in [
        "\\\\server\\share\\x",
        "//server/share/x",
        "\\\\?\\C:\\Windows\\win.ini",
        "\\\\?\\UNC\\server\\share\\x",
        "\\\\.\\PhysicalDrive0",
        "\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy1\\x",
    ] {
        reject(raw, ws.resolve_existing(Path::new(raw)));
    }
}

// ── 4. NTFS 特性 ──────────────────────────────────────────────────────────────

/// 防的是：NTFS 备用数据流（ADS）让 `a.txt:hidden` 读写到与 `a.txt` **不同**的数据，
/// 于是「只允许写 a.txt」被绕过成任意隐藏数据。
#[test]
fn a09_ntfs备用数据流必须被拒() {
    let tree = TempTree::new("a09");
    tree.write("file.txt", "visible");
    let ws = open_ws(&tree);

    for raw in [
        "file.txt:stream",
        "file.txt::$DATA",
        "file.txt:ads",
        "a.txt:",
    ] {
        reject(raw, ws.resolve_existing(Path::new(raw)));
    }
}

/// 防的是：Windows 会把尾随点/空格**静默剥掉**，于是 `a.` 与 `a` 落到同一个文件，
/// 白名单按字面判就漏了（`CON.` 之类同样生效）。
#[test]
fn a10_尾随点或空格必须被拒() {
    let tree = TempTree::new("a10");
    tree.mkdir("sub");
    let ws = open_ws(&tree);

    for raw in ["a.", "a ", "a.txt.", "a.txt ", "sub./x", "sub /x", "sub/a."] {
        let message = reject(raw, ws.resolve_new(Path::new(raw)));
        assert!(
            message.contains("dot")
                || message.contains("space")
                || message.contains("点")
                || message.contains("空格"),
            "【{raw}】应当因「尾随点/空格」被拒，实际：{message}"
        );
    }
}

/// 防的是：Windows 保留设备名（`CON`/`NUL`/`COM1`…）在**任意目录**都指向设备本身，
/// 写进去等于直接向控制台/空设备/串口写入；带扩展名的 `CON.txt` 同样被当作设备。
#[test]
fn a11_windows保留设备名必须被拒() {
    let tree = TempTree::new("a11");
    tree.mkdir("sub");
    let ws = open_ws(&tree);

    for raw in [
        "CON",
        "con",
        "CON.txt",
        "NUL",
        "nul.txt",
        "NUL.tar.gz",
        "COM1",
        "COM1.log",
        "LPT9.log",
        "a/nul.txt",
        "sub/COM1",
        "PRN",
        "AUX.txt",
    ] {
        let message = reject(raw, ws.resolve_new(Path::new(raw)));
        assert!(
            message.contains("reserved") || message.contains("device") || message.contains("设备"),
            "【{raw}】应当因「保留设备名」被拒，实际：{message}"
        );
    }
}

// ── 5. 符号链接 ───────────────────────────────────────────────────────────────

/// 建一个符号链接；本机没有权限时返回 `false`（测试据实跳过，不算通过）。
fn try_symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (target, link);
        false
    }
}

fn try_symlink_dir(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (target, link);
        false
    }
}

/// 防的是：项目内的符号链接指向根外，于是「项目内路径」实际读写的是根外文件。
#[test]
fn a12_指向根外的符号链接必须被拒() {
    let tree = TempTree::new("a12");
    let outside = TempTree::new("a12-outside");
    outside.write("secret.txt", "TOP-SECRET-OUTSIDE");
    let ws = open_ws(&tree);

    if !try_symlink_file(&outside.join("secret.txt"), &tree.join("link.txt")) {
        eprintln!(
            "跳过 a12：本机无权创建符号链接（需要 SeCreateSymbolicLinkPrivilege 或开发者模式）"
        );
        return;
    }

    for raw in ["link.txt", "./link.txt"] {
        let message = reject(raw, ws.resolve_existing(Path::new(raw)));
        assert!(
            message.contains("escapes") || message.contains("逃"),
            "【{raw}】应当因「逃出工作区根」被拒，实际：{message}"
        );
    }
    // 也不能通过写入工具把内容种到根外。
    let message = reject(
        "write link.txt",
        ws.write_file(Path::new("link.txt"), "OVERWRITTEN", None),
    );
    let _ = message;
    assert_eq!(
        outside.read("secret.txt").as_deref(),
        Some("TOP-SECRET-OUTSIDE"),
        "根外文件被改写了，说明链接被跟随写入"
    );
}

/// 防的是：链接位于**最后一段**——父目录都是真实目录，只有最后一段是链接时被漏判。
#[test]
fn a13_最后一段是符号链接时不得写穿() {
    let tree = TempTree::new("a13");
    tree.mkdir("sub");
    let outside = TempTree::new("a13-outside");
    outside.write("target.txt", "ORIGINAL-OUTSIDE");
    let ws = open_ws(&tree);

    if !try_symlink_file(&outside.join("target.txt"), &tree.join("sub/last.txt")) {
        eprintln!("跳过 a13：本机无权创建符号链接");
        return;
    }

    reject(
        "write through last-segment symlink",
        ws.write_file(Path::new("sub/last.txt"), "PWNED", None),
    );
    assert_eq!(
        outside.read("target.txt").as_deref(),
        Some("ORIGINAL-OUTSIDE"),
        "最后一段链接被跟随写入，根外文件被改"
    );
}

/// 防的是：先 `cd` 进一个**目录**链接，再用 `..` 回到根外的父目录（链接 + 上跳组合拳）。
#[test]
fn a14_目录链接后再上跳必须被拒() {
    let tree = TempTree::new("a14");
    let outside = TempTree::new("a14-outside");
    outside.write("secret.txt", "OUTSIDE-DIR-SECRET");
    let ws = open_ws(&tree);

    if !try_symlink_dir(outside.path(), &tree.join("dirlink")) {
        eprintln!("跳过 a14：本机无权创建目录符号链接");
        return;
    }

    // 纯 `..`：守卫在字面阶段就拒。
    reject(
        "dirlink/../secret.txt",
        ws.resolve_existing(Path::new("dirlink/../secret.txt")),
    );

    // 直接进链接：必须因「逃出根」被拒。
    let message = reject(
        "dirlink/secret.txt",
        ws.resolve_existing(Path::new("dirlink/secret.txt")),
    );
    assert!(
        message.contains("escapes") || message.contains("逃"),
        "应当因「逃出工作区根」被拒，实际：{message}"
    );
}

/// 正对照：指向根内的符号链接**必须可用**，否则「全拒」也能让上面几条假绿。
/// 防的是：守卫被写成"见到链接就拒"，于是项目里正常的 `ln -s` 布局（monorepo 常见）全废，
/// 而维护者为了能用又会去放宽判据 —— 那才是真正的逃逸入口。
#[test]
fn a15_指向根内的符号链接允许使用() {
    let tree = TempTree::new("a15");
    tree.write("real.txt", "INSIDE");
    let ws = open_ws(&tree);

    if !try_symlink_file(&tree.join("real.txt"), &tree.join("alias.txt")) {
        eprintln!("跳过 a15：本机无权创建符号链接");
        return;
    }

    let content = ws
        .read_file(Path::new("alias.txt"), 1024)
        .expect("指向根内的链接应当可读");
    assert_eq!(content.content, "INSIDE");
}

// ── 6. 保留目录与大小写 ───────────────────────────────────────────────────────

/// 防的是：在大小写不敏感的文件系统上 `.GIT/config` 与 `.git/config` 是同一个文件，
/// 守卫若只比小写字面就漏判，等于允许改写版本库元数据（植入 hook / 改 HEAD）。
#[test]
fn a16_保留目录的大小写变体必须被拒() {
    let tree = TempTree::new("a16");
    let ws = open_ws(&tree);

    for raw in [
        ".git/config",
        ".GIT/config",
        ".Git/config",
        ".git/hooks/pre-commit",
        ".hbot/state.json",
        ".HBOT/state.json",
        ".Hbot/x",
    ] {
        let message = reject(raw, ws.resolve_new(Path::new(raw)));
        assert!(
            message.contains(".git") || message.contains(".hbot"),
            "【{raw}】应当因「保留目录」被拒，实际：{message}"
        );
    }
}

/// 防的是：大小写不敏感文件系统上 `Src/../OUT` 与 `src/../out` 等价，
/// 守卫若按大小写敏感比较「是否仍在根内」，就能被绕过。
#[test]
fn a17_大小写变体的上跳仍必须被拒() {
    let tree = TempTree::new("a17");
    tree.mkdir("Src");
    let ws = open_ws(&tree);

    for raw in ["Src/../OUT/a.txt", "SRC/../OUT/a.txt", "src/../out/a.txt"] {
        let message = reject(raw, ws.resolve_new(Path::new(raw)));
        assert!(message.contains(".."), "应当点名 `..`，实际：{message}");
    }
}

// ── 7. 写入口本身也过守卫 ─────────────────────────────────────────────────────

/// 防的是：守卫只贴在「解析」函数上，写/读/建目录的入口却能拿原始字符串直接落盘。
#[test]
fn a18_写入入口本身也过路径守卫() {
    let tree = TempTree::new("a18");
    tree.mkdir("sub");
    let ws = open_ws(&tree);

    for raw in [
        "../escape.txt",
        "C:escape.txt",
        "\\\\server\\share\\x.txt",
        ".git/config",
    ] {
        reject(raw, ws.write_file(Path::new(raw), "PWNED", None));
    }
    assert!(!tree.path().parent().unwrap().join("escape.txt").exists());
    assert!(!tree.join(".git/config").exists());

    // 正对照：根内普通写入必须成功，否则上面「全拒」毫无意义。
    let written = ws
        .write_file(Path::new("sub/ok.txt"), "hello", None)
        .expect("根内写入应当成功");
    assert_eq!(written.hash.len(), 64, "应当返回 blake3 十六进制哈希");
    assert_eq!(tree.read("sub/ok.txt").as_deref(), Some("hello"));
}

/// 防的是：写时校验（TOCTOU）缺失 —— 但本用例只证明 `rename` / `delete_to_trash` 也过守卫。
#[test]
fn a19_重命名与删除也过路径守卫() {
    let tree = TempTree::new("a19");
    tree.write("a.txt", "x");
    tree.mkdir("sub");
    let ws = open_ws(&tree);

    reject(
        "rename to ../renamed.txt",
        ws.rename(Path::new("a.txt"), Path::new("../renamed.txt")),
    );

    let message = reject("delete ../nope", ws.delete_to_trash(Path::new("../nope")));
    assert_no_path_leak(&message, tree.path());
    assert!(tree.exists("a.txt"), "失败的 rename 不该动源文件");
}

// ── 8. 信息泄漏（RT-01，已修）────────────────────────────────────────────────

/// 防的是：错误信息把项目根的**绝对路径**（含用户名、目录布局）回显给远端调用方，
/// 于是「一个被拒的路径」变成一次免费的环境侦察。
///
/// RT-01（已修）：`resolve_existing` / `read_file` 的 `NotFound` 曾经直接把
/// `\\?\C:\Users\<用户>\…\<根>\<字面名>` 写进错误文案，现在只回显相对项目根的路径。
/// 见 `docs/security/red-team-findings.md` 的 RT-01。
#[test]
fn a20_拒绝信息不得泄漏项目根绝对路径() {
    let tree = TempTree::new("a20");
    tree.mkdir("sub");
    tree.write("sub/exists.txt", "ok");
    let ws = open_ws(&tree);

    // 覆盖 a01–a11 里出现过的**全部编码形态**：任何一条被拒时都不许回显绝对路径。
    // （同一个缺陷在一条用例里表驱动覆盖，避免一个渲染问题打红十几条正交用例。）
    for raw in [
        // 不存在的合法相对路径（父目录还在 → 走 `resolve_entry` 的 NotFound 分支）
        "missing.txt",
        "sub/missing.txt",
        // 父目录不存在（走 `canonical_within` 的 NotFound 分支）
        "missing/deep/file.txt",
        // `..` 与百分号编码
        "../secret.txt",
        "%2e%2e/secret.txt",
        "%2e%2e%2fsecret.txt",
        "%2e%2e%5csecret.txt",
        "%252e%252e/secret.txt",
        // 四点 / 混合分隔符
        "....//....//secret.txt",
        "....\\\\secret.txt",
        "..%2e/secret.txt",
        // 全角句点（被当作字面名 → NotFound 分支）
        "．．/secret.txt",
        "。．/secret.txt",
        // 绝对路径 / UNC / 设备命名空间 / 盘符相对
        "/etc/passwd",
        r"C:\Windows\win.ini",
        r"\\server\share\x",
        r"\\?\C:\Windows\win.ini",
        "C:secret.txt",
        // 保留目录（错误里带的是用户给的 raw，本身不含根路径；作为回归项保留）
        ".git/config",
        ".hbot/state.json",
        // 保留设备名 / 尾随点 / 备用数据流（字面名 → NotFound 或 invalid 分支）
        "CON.txt",
        "a ",
        "file.txt:stream",
    ] {
        let message = reject(raw, ws.resolve_existing(Path::new(raw)));
        assert_no_path_leak(&message, tree.path());
        // `resolve_new` 是创建路径走的入口：拒绝时同样不许泄漏；接受时只要求落点在根内
        // （某条输入"该不该被拒"由 a01–a11 负责，本用例只负责 RT-01 的渲染问题）。
        match ws.resolve_new(Path::new(raw)) {
            Err(error) => {
                let message = error_chain(&error);
                assert_no_path_leak(&message, tree.path());
            }
            Ok(resolved) => assert!(
                is_within_tree(&resolved, &tree),
                "【{raw}】解析到了根外：{}",
                resolved.display()
            ),
        }
    }

    // 各个**真实写入口**也走同一套解析：读、写、建目录、列举、改名、删除。
    let message = reject("read missing", ws.read_file(Path::new("missing.txt"), 1024));
    assert_no_path_leak(&message, tree.path());
    // `write_file` 会按需创建缺失的父目录（`create_dir_all` 语义），所以"父目录不存在"
    // 不走到错误分支；这里取同一条守卫上**真正会失败**的那条：路径里那个"目录"其实是文件。
    let message = reject(
        "write through a file",
        ws.write_file(Path::new("sub/exists.txt/child.txt"), "x", None),
    );
    assert_no_path_leak(&message, tree.path());
    let message = reject(
        "create dir through a file",
        ws.create_dir(Path::new("sub/exists.txt/deep")),
    );
    assert_no_path_leak(&message, tree.path());
    let message = reject("list missing", ws.list_dir(Path::new("missing/sub")));
    assert_no_path_leak(&message, tree.path());
    let message = reject(
        "rename missing",
        ws.rename(Path::new("sub/exists.txt"), Path::new("missing/deep/x.txt")),
    );
    assert_no_path_leak(&message, tree.path());
    let message = reject("delete missing", ws.delete_to_trash(Path::new("nope.txt")));
    assert_no_path_leak(&message, tree.path());
}
