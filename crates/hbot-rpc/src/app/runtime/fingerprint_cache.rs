//! 项目内容指纹的缓存：把“每条消息都读全项目文件”降成“代码可能变了才读内容”。
//!
//! ## 为什么需要它
//!
//! 生成代码热重载的判据是“项目内容变了没有”。最直白的实现是每条消息都
//! [`project_fingerprint`]——遍历 `src/**` 里所有 `.ts/.js/.mjs` 并 blake3 一遍字节。
//! 1000 msg/min、20 个文件的项目就是每分钟几万次文件读，**热路径上的文件系统工作**
//! 正是本项目明确拒绝的那类缺陷。
//!
//! 这里改成两层判据：
//!
//! 1. **元数据签名**（路径 + mtime + 长度）：只有 [`FingerprintCache`] 的
//!    `metadata_ttl`（默认 2s）到期才 `stat` 一遍。元数据在内核缓存里，且**不读内容**。
//! 2. **内容地板**（`content_floor`，默认 5s）：即使元数据看着没变，距上次读内容超过地板
//!    也强制重读一遍内容。这是对“元数据看不出的改动”的兜底。
//!
//! 于是稳态成本 = 每 `metadata_ttl` 一次 `read_dir + stat`，加每 `content_floor` 一次内容
//! 哈希；**两条消息之间不读任何文件内容**。
//!
//! ## 刻意采用备选方案（没有用 `notify` 文件监听）
//!
//! 设计稿允许两种做法。这里选元数据签名，理由是：`notify` 要新增外部依赖、每个 bot 一个
//! 监听线程与事件队列，而文件系统事件在单元测试里不可靠（Windows 上尤其容易 flaky），
//! 会成为新的测试不稳定源。
//!
//! **它到底会漏哪一种改动**：在**同一个 mtime 时间刻度内**、**大小不变**地改写文件
//! （例如把 `if (a)` 改成 `if (b)`，两字节）；此时元数据签名完全不变。
//! **兜底就是内容地板**：最坏情况下这个改动会被推迟到 `content_floor` 到期后的下一次检查
//! ——即陈旧窗口有上界（默认 5s），不会永久漏掉。编辑器保存通常还会改长度或 mtime；
//! 真漏了，代价也只是“延迟几秒生效”，不是“永远不生效”。
//!
//! ## 清单（`hbot.json`）也参与指纹
//!
//! 指纹决定“要不要换 isolate”，而 isolate 出生时按**清单**装配设备能力
//! （`SandboxHandler::worker` 的 `device_apps`）与状态能力（`source.handles(root)`）。
//! 所以只把 `src/**` 算进指纹会留下一个洞：**改清单不改源码 → 指纹不变 → isolate 不换 →
//! 新声明永远不生效**（`capabilities.state.kv` 从 false 改成 true 也一样）。
//! 于是清单文件本身也进指纹（[`MANIFEST_FILE`]），清单里的 `capabilities` / `permissions`
//! 才能像源码一样“保存即生效”。
//!
//! ## 测试缝
//!
//! 时间从参数注入（[`FingerprintCache::current_at`]），内容读数用
//! [`FingerprintCache::content_reads`] 暴露，测试才能断言“没变就不读内容”而不 sleep。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use hbot_core::HbotError;

/// 元数据签名的默认刷新间隔。
///
/// 2s：比人眼可感知的“保存后多久生效”短得多，同时把 `stat` 频率压到每 2s 一次。
pub const DEFAULT_METADATA_TTL: Duration = Duration::from_secs(2);

/// 内容哈希的默认地板：无论元数据怎么说，最多信它这么久。
///
/// 5s：这是“同一 mtime 刻度内同长改写”这一盲区的最坏陈旧窗口上界。
pub const DEFAULT_CONTENT_FLOOR: Duration = Duration::from_secs(5);

/// 参与指纹的文件扩展名（`docs/02-architecture.md` §6.1 的可执行源码范围）。
const SOURCE_EXTENSIONS: [&str; 3] = ["ts", "js", "mjs"];

/// 逐文件元数据：相对路径、mtime（纳秒）、长度。
type MetaSignature = Vec<(String, u64, u64)>;

#[derive(Default)]
struct State {
    /// 上次算出的元数据签名。
    signature: Option<MetaSignature>,
    /// 元数据签名的计算时刻。
    signature_at: Option<Instant>,
    /// 上次算出的内容指纹。
    fingerprint: Option<String>,
    /// 内容指纹的计算时刻（地板判据用它）。
    content_at: Option<Instant>,
    /// 指纹**实际发生变化**的时刻（热重载 300ms 去抖从这里起算）。
    changed_at: Option<Instant>,
}

/// 带缓存的项目内容指纹。
///
/// 一个项目一个实例，由 [`crate::app::runtime::sandbox_handler::SandboxHandler`] 持有。
pub struct FingerprintCache {
    root: PathBuf,
    metadata_ttl: Duration,
    content_floor: Duration,
    state: Mutex<State>,
    /// 累计读过的文件内容次数（测试断言热路径零读）。
    content_reads: AtomicU64,
}

impl std::fmt::Debug for FingerprintCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FingerprintCache")
            .field("root", &self.root)
            .field("metadata_ttl", &self.metadata_ttl)
            .field("content_floor", &self.content_floor)
            .field("content_reads", &self.content_reads())
            .finish()
    }
}

impl FingerprintCache {
    /// 用默认 TTL 建缓存（生产路径）。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_ttls(root, DEFAULT_METADATA_TTL, DEFAULT_CONTENT_FLOOR)
    }

    /// 覆盖两个 TTL（测试用；`content_floor` 设为 [`Duration::ZERO`] 等于每轮都重读内容）。
    pub fn with_ttls(
        root: impl Into<PathBuf>,
        metadata_ttl: Duration,
        content_floor: Duration,
    ) -> Self {
        Self {
            root: root.into(),
            metadata_ttl,
            content_floor,
            state: Mutex::new(State::default()),
            content_reads: AtomicU64::new(0),
        }
    }

    /// 项目根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 累计读取文件内容的次数（每次读一个文件算一次）。
    ///
    /// 这是给测试用的观测点：稳态下 N 条消息之间它**不应该增长**。
    pub fn content_reads(&self) -> u64 {
        self.content_reads.load(Ordering::Relaxed)
    }

    /// 指纹最近一次实际变化的时刻（`None` = 自创建以来没变过）。
    pub fn changed_at(&self) -> Option<Instant> {
        self.lock().changed_at
    }

    /// 当前项目内容指纹（用 `Instant::now()` 判定 TTL）。
    pub fn current(&self) -> Result<String, HbotError> {
        self.current_at(Instant::now())
    }

    /// 当前项目内容指纹（显式传入“现在”，测试可注入）。
    ///
    /// 返回值是**内容哈希**：只有内容真变了它才变，所以调用方可以直接拿它和上次编译的
    /// 指纹比较，决定要不要重编译。
    pub fn current_at(&self, now: Instant) -> Result<String, HbotError> {
        // 第 1 步：决定要不要刷元数据（`stat`，不读内容）。
        let refresh_signature = {
            let state = self.lock();
            match (state.signature_at, state.fingerprint.is_some()) {
                (Some(at), true) => now.saturating_duration_since(at) >= self.metadata_ttl,
                _ => true,
            }
        };
        // 第 2 步：算元数据（锁外，要碰文件系统）。
        let signature = if refresh_signature {
            Some(self.metadata_signature()?)
        } else {
            None
        };

        // 第 3 步：决定要不要重读内容。
        let need_content = {
            let state = self.lock();
            let floor_due = match (state.content_at, state.fingerprint.is_some()) {
                (Some(at), true) => now.saturating_duration_since(at) >= self.content_floor,
                _ => true,
            };
            let metadata_changed = match (&signature, &state.signature) {
                (Some(new), Some(old)) => new != old,
                // 刷新了元数据但原先没有签名：必然要重算内容。
                (Some(_), None) => true,
                // 本轮没刷新元数据：没有证据说变了。
                (None, _) => false,
            };
            floor_due || metadata_changed
        };

        {
            let mut state = self.lock();
            if let Some(signature) = signature {
                state.signature = Some(signature);
                state.signature_at = Some(now);
            }
        }

        if !need_content {
            // 稳态命中：一个字节的内容都没读。
            if let Some(fingerprint) = self.lock().fingerprint.clone() {
                return Ok(fingerprint);
            }
        }

        // 第 4 步：真读内容（锁外，大项目要几十毫秒）。
        let fingerprint = self.content_fingerprint()?;

        let mut state = self.lock();
        let changed = state.fingerprint.as_deref() != Some(fingerprint.as_str());
        if changed {
            state.changed_at = Some(now);
        }
        state.fingerprint = Some(fingerprint.clone());
        state.content_at = Some(now);
        Ok(fingerprint)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // 临界区不 panic：poison 只会来自内部 bug，退化成内层数据总比整个 bot 卡死好。
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 只读元数据（`read_dir` + `metadata`），**不打开任何文件**。
    fn metadata_signature(&self) -> Result<MetaSignature, HbotError> {
        let files = source_files(&self.root)?;
        let mut signature = Vec::with_capacity(files.len());
        for (relative, path) in files {
            let metadata = std::fs::metadata(&path).map_err(|error| {
                HbotError::internal(format!(
                    "读取生成代码元数据失败 {}：{error}",
                    path.display()
                ))
            })?;
            let nanos = metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|since_epoch| u64::try_from(since_epoch.as_nanos()).unwrap_or(u64::MAX))
                .unwrap_or(0);
            signature.push((relative, nanos, metadata.len()));
        }
        Ok(signature)
    }

    /// 真读内容并哈希（缓存内的 [`project_fingerprint`]）。
    fn content_fingerprint(&self) -> Result<String, HbotError> {
        let files = source_files(&self.root)?;
        fingerprint_files(&self.root, &files, &self.content_reads)
    }
}

/// 一次性计算项目内容指纹（**每次调用都读全部内容**，只给测试与冷启动用）。
///
/// 热路径请用 [`FingerprintCache`]：这个函数的成本是 O(文件数 × 文件大小)。
pub fn project_fingerprint(root: &Path) -> Result<String, HbotError> {
    let counter = AtomicU64::new(0);
    let files = source_files(root)?;
    fingerprint_files(root, &files, &counter)
}

/// 枚举参与指纹的源文件，按相对路径排序（顺序稳定，否则指纹会随目录遍历顺序抖动）。
///
/// 清单（`hbot.json`）也在这里：manifest 里的 `capabilities` / `permissions` 决定 isolate
/// 出生时装哪些端口，所以它必须能单独触发一次 isolate 更换（见模块文档「清单也参与指纹」）。
/// 清单不存在时不入列（项目可能还没写清单，`manifest::load` 会退回"全不声明"）。
fn source_files(root: &Path) -> Result<Vec<(String, PathBuf)>, HbotError> {
    let mut files = Vec::new();
    collect_sources(&root.join("src"), root, &mut files)?;
    let manifest = root.join(hbot_code::manifest::MANIFEST_FILE);
    if manifest.is_file() {
        files.push((hbot_code::manifest::MANIFEST_FILE.to_owned(), manifest));
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

/// 把 `(相对路径, 绝对路径)` 列表折算成内容哈希。
fn fingerprint_files(
    root: &Path,
    files: &[(String, PathBuf)],
    counter: &AtomicU64,
) -> Result<String, HbotError> {
    let mut buffer = root.to_string_lossy().as_bytes().to_vec();
    buffer.push(0);
    for (relative, path) in files {
        let bytes = std::fs::read(path).map_err(|error| {
            HbotError::internal(format!("读取生成代码失败 {}：{error}", path.display()))
        })?;
        counter.fetch_add(1, Ordering::Relaxed);
        buffer.extend_from_slice(relative.as_bytes());
        buffer.push(0);
        buffer.extend_from_slice(&bytes);
        buffer.push(0);
    }
    Ok(hbot_workspace::hash_bytes(&buffer))
}

/// 递归收集 `.ts/.js/.mjs`（跳过 `.d.ts`：它只是类型声明，不影响运行代码）。
fn collect_sources(
    directory: &Path,
    root: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), HbotError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        // 没有 `src/` 的项目 = 空指纹；让编译阶段去报“缺入口/缺文件”的真实错误，
        // 而不是在这里抛一个难懂的 IO 错误。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(HbotError::internal(format!(
                "读取生成代码目录失败 {}：{error}",
                directory.display()
            )));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            HbotError::internal(format!(
                "读取生成代码目录项失败 {}：{error}",
                directory.display()
            ))
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            HbotError::internal(format!(
                "读取生成代码文件类型失败 {}：{error}",
                path.display()
            ))
        })?;
        if file_type.is_dir() {
            collect_sources(&path, root, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name.ends_with(".d.ts") {
            continue;
        }
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            continue;
        };
        if !SOURCE_EXTENSIONS.contains(&extension) {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        out.push((relative, path));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::app::test_support::TempDir;

    use super::{
        DEFAULT_CONTENT_FLOOR, DEFAULT_METADATA_TTL, FingerprintCache, project_fingerprint,
    };

    /// 建一个带 `src/` 的项目，写入 `files` 里的（相对路径, 内容）。
    fn project(label: &str, files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new(label);
        for (relative, content) in files {
            let path = dir.path().join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create dir");
            }
            std::fs::write(&path, content).expect("write source");
        }
        dir
    }

    /// D1 的核心断言：代码没变时，连续 N 次取指纹**零次内容读**。
    ///
    /// 时间从参数注入，所以不用 sleep 也能跨过 TTL/地板；`content_reads` 是暴露给测试的
    /// 计数器缝。
    #[test]
    fn steady_state_reads_no_file_contents() {
        let dir = project(
            "fp-steady",
            &[
                ("src/main.ts", "export default {};"),
                ("src/handlers/a.ts", "export const a = 1;"),
                ("src/lib/b.mjs", "export const b = 2;"),
            ],
        );
        let cache = FingerprintCache::with_ttls(
            dir.to_path_buf(),
            DEFAULT_METADATA_TTL,
            DEFAULT_CONTENT_FLOOR,
        );
        let start = Instant::now();

        let first = cache.current_at(start).expect("第一次指纹");
        let reads_after_first = cache.content_reads();
        assert_eq!(reads_after_first, 3, "首轮读三个源文件");
        assert!(
            cache.changed_at().is_some(),
            "首个指纹视为一次变化（从无到有）"
        );

        // 30 条消息 = 30 次取指纹，全部在 3s 内（跨过一次元数据刷新，但没到内容地板）。
        for step in 1..=30_u32 {
            let now = start + Duration::from_millis(u64::from(step) * 100);
            let fingerprint = cache.current_at(now).expect("稳态指纹");
            assert_eq!(fingerprint, first, "内容没变，指纹必须稳定");
        }
        assert_eq!(
            cache.content_reads(),
            reads_after_first,
            "代码没变时不许再读文件内容（热路径零文件读）"
        );
    }

    /// 元数据变了（长度变化）就在下一次元数据刷新时被发现，不等内容地板。
    #[test]
    fn metadata_change_is_picked_up_without_waiting_for_the_floor() {
        let dir = project("fp-meta", &[("src/main.ts", "export default {};")]);
        let cache = FingerprintCache::with_ttls(
            dir.to_path_buf(),
            Duration::from_millis(50),
            Duration::from_secs(60),
        );
        let start = Instant::now();
        let first = cache.current_at(start).expect("首次指纹");
        assert_eq!(cache.content_reads(), 1);

        std::fs::write(dir.path().join("src/main.ts"), "export default 1 + 1;").expect("保存");
        let second = cache
            .current_at(start + Duration::from_millis(100))
            .expect("改动后指纹");
        assert_ne!(second, first, "内容变了指纹必须变");
        assert_eq!(cache.content_reads(), 2, "只重读一次（不是每轮读）");
    }

    /// 内容地板：即使元数据完全没刷新（TTL 设得极大），到期也会重读内容。
    ///
    /// 这演示的正是“同一 mtime 刻度内同长改写”这个盲区的兜底路径。
    #[test]
    fn content_floor_rereads_even_when_metadata_is_never_refreshed() {
        let dir = project("fp-floor", &[("src/main.ts", "aaa")]);
        let cache = FingerprintCache::with_ttls(
            dir.to_path_buf(),
            Duration::from_secs(3_600),
            Duration::from_millis(50),
        );
        let start = Instant::now();
        let first = cache.current_at(start).expect("首次指纹");
        assert_eq!(cache.content_reads(), 1);

        // 同长度改写：元数据层面（长度一致，若 mtime 也没变）看不出差别。
        std::fs::write(dir.path().join("src/main.ts"), "bbb").expect("保存");
        let second = cache
            .current_at(start + Duration::from_millis(100))
            .expect("地板到期后的指纹");
        assert_ne!(second, first, "地板必须兜住元数据看不出的改动");
        assert_eq!(cache.content_reads(), 2, "地板到期才重读一次");
    }

    /// 地板到期但内容真没变：重读一遍，指纹与 `changed_at` 都不许变（避免误触发热重载）。
    #[test]
    fn floor_reread_without_change_keeps_fingerprint_and_changed_at() {
        let dir = project("fp-floor-stable", &[("src/main.ts", "export default {};")]);
        let cache = FingerprintCache::with_ttls(
            dir.to_path_buf(),
            Duration::from_millis(50),
            Duration::from_millis(200),
        );
        let start = Instant::now();
        let first = cache.current_at(start).expect("首次指纹");
        let changed_at = cache.changed_at();

        let again = cache
            .current_at(start + Duration::from_millis(500))
            .expect("地板到期后重算");
        assert_eq!(again, first);
        assert_eq!(cache.content_reads(), 2, "地板到期重读了一次");
        assert_eq!(
            cache.changed_at(),
            changed_at,
            "内容没变就不许刷新 changed_at（否则每 5s 触发一次重编译）"
        );
    }

    /// 新增文件也进指纹（新建模块必须触发重编译）。
    #[test]
    fn added_file_changes_the_fingerprint() {
        let dir = project("fp-added", &[("src/main.ts", "export default {};")]);
        let cache = FingerprintCache::with_ttls(
            dir.to_path_buf(),
            Duration::from_millis(10),
            Duration::from_secs(60),
        );
        let start = Instant::now();
        let first = cache.current_at(start).expect("首次指纹");

        std::fs::write(dir.path().join("src/lib.ts"), "export const extra = 1;").expect("新增文件");
        let second = cache
            .current_at(start + Duration::from_millis(50))
            .expect("新增文件后的指纹");
        assert_ne!(second, first);
        assert_eq!(cache.content_reads(), 3, "首轮 1 次 + 新增后 2 次");
    }

    /// 没有 `src/` 的项目：空指纹，不报 IO 错误（让编译阶段报真实错误）。
    #[test]
    fn missing_src_directory_is_an_empty_project() {
        let dir = TempDir::new("fp-empty");
        let fingerprint = project_fingerprint(dir.path()).expect("空项目指纹");
        assert!(!fingerprint.is_empty());
        assert_eq!(
            fingerprint,
            project_fingerprint(dir.path()).expect("重复计算")
        );
    }

    /// `project_fingerprint` 是纯函数：同内容同结果，改内容就改结果。
    #[test]
    fn project_fingerprint_tracks_content() {
        let dir = project("fp-pure", &[("src/main.ts", "export default {};")]);
        let before = project_fingerprint(dir.path()).expect("首次");
        assert_eq!(before, project_fingerprint(dir.path()).expect("稳定"));

        std::fs::write(dir.path().join("src/main.ts"), "export default 42;").expect("改写");
        let after = project_fingerprint(dir.path()).expect("改写后");
        assert_ne!(before, after);
    }

    /// `.d.ts` 不参与指纹（纯类型声明，改了也不影响运行代码）。
    #[test]
    fn type_declarations_are_ignored() {
        let dir = project(
            "fp-dts",
            &[
                ("src/main.ts", "export default {};"),
                ("hbot.d.ts", "declare const x: 1;"),
            ],
        );
        let before = project_fingerprint(dir.path()).expect("首次");
        std::fs::write(dir.path().join("hbot.d.ts"), "declare const x: 2;").expect("改写声明");
        assert_eq!(before, project_fingerprint(dir.path()).expect("改写声明后"));
    }

    /// 清单（`hbot.json`）单独进指纹：只改清单、一个源码字节都不动，也必须换指纹。
    ///
    /// 这是「把 `capabilities.state.kv` 从 `false` 改成 `true` 却不改任何 `.ts`」那条路径的
    /// 最小证据：isolate 只在指纹变化时重建，而设备/状态能力是**重建时**按清单装配的。
    /// 不把清单算进指纹，用户改了声明就得等下一次改源码才生效。
    #[test]
    fn manifest_change_alone_changes_the_fingerprint() {
        let dir = project("fp-manifest", &[("src/main.ts", "export default {};")]);
        let manifest = dir.path().join(hbot_code::manifest::MANIFEST_FILE);
        std::fs::write(&manifest, r#"{"name":"d","platform":{"kind":"local"}}"#).expect("写清单");
        let cache = FingerprintCache::with_ttls(
            dir.to_path_buf(),
            Duration::from_millis(10),
            Duration::from_secs(60),
        );
        let start = Instant::now();
        let before = cache.current_at(start).expect("首次指纹");

        // 只改清单里的能力声明，`src/main.ts` 一个字节都不动。
        std::fs::write(
            &manifest,
            r#"{"name":"d","platform":{"kind":"local"},"capabilities":{"state":{"kv":true}}}"#,
        )
        .expect("改写清单");
        let after = cache
            .current_at(start + Duration::from_millis(50))
            .expect("改清单后的指纹");
        assert_ne!(after, before, "只改清单也必须换指纹，否则新声明永远不生效");
    }
}
