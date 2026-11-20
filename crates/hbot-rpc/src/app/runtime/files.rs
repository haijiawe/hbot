//! ctx.files 的宿主后端：一个把路径钉在项目根里的 std::fs 实现（C3.file）。
//!
//! 三条不变量：
//! 1. 用户给的路径已经被 `normalize_path` 归一过（相对、/ 分隔、没有 .. 与盘符），
//!    本模块只负责把它接到项目根上；
//! 2. 接上之后还要再验一次真实位置 —— 符号链接可以把 a/b 指到项目外，
//!    归一化只能挡住字面上的 ..，挡不住链接；
//! 3. 文本读（`read`）要求内容真的是 UTF-8；二进制走 `readBytes`，原样字节不校验编码。
//! 4. 递归删除前先扫一遍整棵树：符号链接一律拒绝（不跟进），条目数超上限报
//!    `TooMany`，扫完再删 —— 扫和删分开，才不会有「删到一半遇到链接停下」的半残状态。

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hbot_core::{
    BotId, Clock, FileEntry, FileError, FileStore, LineRange, MAX_FILE_REMOVE_ENTRIES, ReadBytes,
    ReadFile, check_file_quota, validate_file_list, validate_file_read, validate_file_write,
    validate_file_write_range,
};
use hbot_store::Store;

/// 以项目根为唯一可写目录的文件后端。
pub struct ProjectFiles {
    root: PathBuf,
    /// 磁盘配额账本；`None` = 不记账（测试与不关心配额的宿主）。
    quota: Option<FileQuota>,
}

/// 磁盘配额的三个输入：给谁记账、账本在哪、上限多少。
///
/// ## 为什么账本在库里而不是扫树
///
/// 配额判据在**每一次写**之前都要问一次「还放得下吗」。扫树是 O(项目) 的磁盘操作，
/// 而热路径上只允许 O(1)（`docs/pitfalls.md` 坑 23）。代价是账本可能与真实磁盘有偏差
/// （用户在 IDE 里手工改文件不经过这里），这是刻意的取舍：配额挡的是「生成代码把盘写满」，
/// 不是安全边界（路径已经钉在项目根里了）。
struct FileQuota {
    bot_id: BotId,
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
    /// 上限（字节）；0 = 不限（与 `limits.quotaMb = 0` 同口径）。
    limit: u64,
}

impl FileQuota {
    /// 预留 `incoming` 字节；放不下时按 `HBOT_FILE_QUOTA` 报错。
    ///
    /// 判据在**一条 SQL** 里（`repositories/files_usage.rs` 的 `reserve`）：同一 bot 的不同
    /// 会话是并行的，「先读占用、再判够不够、最后加」三步会让两个写都看到同一个旧值。
    fn reserve(&self, path: &str, incoming: u64) -> Result<(), FileError> {
        if incoming == 0 {
            return Ok(());
        }
        let now = self.clock.now().unix_millis();
        let reserved = self
            .store
            .reserve_file_usage(&self.bot_id, incoming, self.limit, now)
            .map_err(|error| FileError::Failed {
                message: format!("配额账本写入失败：{error}"),
            })?;
        if reserved.is_some() {
            return Ok(());
        }
        // 账本说放不下：用 core 的判据造错误（错误形状只有一份，别在 rpc 再写一遍）。
        let used = self.store.file_usage(&self.bot_id).unwrap_or(u64::MAX);
        let verdict = check_file_quota(path, used, incoming, self.limit);
        debug_assert!(
            verdict.is_err(),
            "账本说放不下（used={used} incoming={incoming} limit={}），core 判据却说放得下：两处判据已经不一致",
            self.limit
        );
        verdict?;
        Ok(())
    }

    /// 退掉 `amount` 字节（删除文件、覆盖写变短、写失败回滚）。
    fn release(&self, amount: u64) -> Result<(), FileError> {
        if amount == 0 {
            return Ok(());
        }
        let now = self.clock.now().unix_millis();
        self.store
            .release_file_usage(&self.bot_id, amount, now)
            .map(|_remaining| ())
            .map_err(|error| FileError::Failed {
                message: format!("配额账本回退失败：{error}"),
            })
    }
}

/// 一次写的配额调整结果，用来在写失败时原样撤销。
///
/// 预留与退账都可能发生（覆盖写变短就是退账），所以撤销不是单向的：预留过的要退回去，
/// 退过的要补回来。两者互斥。
#[derive(Debug, Default, Clone, Copy)]
struct QuotaAdjust {
    reserved: u64,
    released: u64,
}

impl std::fmt::Debug for ProjectFiles {
    /// 手写而不是 derive：`Arc<dyn Clock>` 没有 `Debug`，而把账本整个打出来也没有意义。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectFiles")
            .field("root", &self.root)
            .field("quota_limit", &self.quota.as_ref().map(|quota| quota.limit))
            .finish()
    }
}

impl ProjectFiles {
    /// 用项目根建一个后端；根必须已经存在（调用方在拿到项目根时才建它）。
    ///
    /// 不带配额：不记账也就不会因为累计体积拒绝写入。生产装配走 [`Self::with_quota`]。
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = fs::canonicalize(root.into())?;
        Ok(Self { root, quota: None })
    }

    /// 接上磁盘配额（`limit == 0` = 不限，但仍然记账 —— 账本本身对排障有用）。
    #[must_use]
    pub fn with_quota(
        mut self,
        bot_id: BotId,
        store: Arc<Store>,
        clock: Arc<dyn Clock>,
        limit: u64,
    ) -> Self {
        self.quota = Some(FileQuota {
            bot_id,
            store,
            clock,
            limit,
        });
        self
    }

    /// 已规范化的项目根（绝对路径）。
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 按「写前 / 写后」的实际大小记账：涨了预留差值，跌了退回差值。
    ///
    /// 与 [`Self::reserve_quota`] 的分工：这里的 `after` 是**已知**的最终大小（整文件
    /// 覆盖写），所以不必写完再对账；`write_range` 的最终大小写前算不出来，走另一条路。
    fn adjust_quota(&self, path: &str, before: u64, after: u64) -> Result<QuotaAdjust, FileError> {
        let Some(quota) = &self.quota else {
            return Ok(QuotaAdjust::default());
        };
        if after > before {
            let incoming = after - before;
            quota.reserve(path, incoming)?;
            return Ok(QuotaAdjust {
                reserved: incoming,
                released: 0,
            });
        }
        if before > after {
            let shrinking = before - after;
            quota.release(shrinking)?;
            return Ok(QuotaAdjust {
                reserved: 0,
                released: shrinking,
            });
        }
        Ok(QuotaAdjust::default())
    }

    /// 撤销一次 [`Self::adjust_quota`]：写失败了，磁盘上的内容还是原来那份。
    fn undo_quota(&self, adjust: &QuotaAdjust, cause: &FileError) {
        let Some(quota) = &self.quota else {
            return;
        };
        let undo = if adjust.reserved > 0 {
            quota.release(adjust.reserved)
        } else if adjust.released > 0 {
            // 回滚预留时路径只进错误文案，给个能看懂的占位而不是原路径（原路径这时
            // 可能已经被写成半截，写进错误里只会误导）。
            quota.reserve("<回滚>", adjust.released)
        } else {
            return;
        };
        if let Err(error) = undo {
            tracing::warn!(
                reserved = adjust.reserved,
                released = adjust.released,
                %error,
                original = %cause,
                "文件写失败之后配额回滚也失败：这个 bot 的账本会偏离实际，重启后可用 set_file_usage 对账"
            );
        }
    }

    /// 写完把账本对齐到实际大小。
    ///
    /// `reserved` 是写前预留的量（上界），`before` / `actual` 是写前写后的真实大小。
    /// 需要补的净变化是 `actual - before`，已经记了 `reserved`，所以只处理差额：
    /// 差额为负就退回去（预留多了、或文件变短了），为正说明上界算错 —— 记 error 留证据，
    /// **不**为了对齐去动一个已经写完的文件。
    fn settle_quota(&self, path: &str, before: u64, reserved: u64, actual: u64) {
        let Some(quota) = &self.quota else {
            return;
        };
        let target = i128::from(actual) - i128::from(before);
        let applied = i128::from(reserved);
        let diff = target - applied;
        if diff < 0 {
            let refund = u64::try_from(-diff).unwrap_or(u64::MAX);
            if let Err(error) = quota.release(refund) {
                tracing::warn!(%error, path, refund, "配额对账退账失败：账本会偏高");
            }
        } else if diff > 0 {
            tracing::error!(
                path,
                before,
                reserved,
                actual,
                "按上界预留的配额被实际增长超过：上界算错了，账本会偏低"
            );
        }
    }

    /// 现有文件的字节数；不存在按 0（新建文件不占旧空间）。
    fn existing_bytes(&self, path: &Path) -> Result<u64, FileError> {
        match fs::metadata(path) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    return Err(FileError::Invalid {
                        message: format!(
                            "{} 是目录，不是文件：写文件请给一个文件路径",
                            self.display(path)
                        ),
                    });
                }
                Ok(metadata.len())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(self.failed(path, error)),
        }
    }

    /// 删完之后退账：文件按自身大小，目录按整棵树的文件大小之和。
    ///
    /// 账本回退失败只记 warn、不报错：文件**真的已经删掉了**，这时返回错误会让调用方
    /// 以为删除失败、进而去重试一个不存在的路径。账本偏高是保守方向（少放行，不会多占盘），
    /// 需要精确值时可走 `set_file_usage` 对账。
    fn release_after_remove(&self, path: &str, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let Some(quota) = &self.quota else {
            return;
        };
        if let Err(error) = quota.release(bytes) {
            tracing::warn!(bytes, path, %error, "文件已删除，但配额账本回退失败：账本会偏高");
        }
    }

    /// 把一条已归一的相对路径接到根上，并验证真实位置仍在根内。
    ///
    /// 最深的已存在祖先做 canonicalize 后必须仍以根开头；剩余（不存在的）段
    /// 只能是普通相对段 —— 它们来自 `normalize_path`，不会含 .. 。
    fn resolve(&self, relative: &str) -> Result<PathBuf, FileError> {
        let mut resolved = self.root.clone();
        let mut missing: Vec<&str> = Vec::new();
        let mut parts = relative
            .split('/')
            .filter(|part| !part.is_empty())
            .peekable();
        while let Some(part) = parts.next() {
            let candidate = resolved.join(part);
            if candidate.exists() {
                resolved =
                    fs::canonicalize(&candidate).map_err(|error| self.failed(&candidate, error))?;
                if !resolved.starts_with(&self.root) {
                    return Err(FileError::Denied {
                        path: relative.to_owned(),
                    });
                }
            } else {
                missing.push(part);
                missing.extend(parts);
                break;
            }
        }
        for part in missing {
            if Path::new(part)
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err(FileError::Denied {
                    path: relative.to_owned(),
                });
            }
            resolved.push(part);
        }
        Ok(resolved)
    }

    fn failed(&self, path: &Path, error: io::Error) -> FileError {
        FileError::Failed {
            message: format!("{}: {error}", self.display(path)),
        }
    }

    /// 只暴露相对项目根的路径：绝对路径进日志等于把用户的目录结构写进排障记录。
    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn symlink_denied(&self, path: &Path) -> FileError {
        FileError::Denied {
            path: format!(
                "{}（是符号链接：递归删除不跟进链接，请先手动处理）",
                self.display(path)
            ),
        }
    }

    /// 数一棵树有多少条（含根）、文件一共多少字节，顺带拒绝符号链接与超上限。
    ///
    /// 目录本身不占配额，只有文件占 —— 账本记的是「这个 bot 写了多少字节」。
    fn count_tree(&self, path: &Path, count: &mut u32, bytes: &mut u64) -> Result<(), FileError> {
        *count += 1;
        if *count as usize > MAX_FILE_REMOVE_ENTRIES {
            return Err(FileError::TooMany {
                path: self.display(path),
                limit: MAX_FILE_REMOVE_ENTRIES,
            });
        }
        let metadata = fs::symlink_metadata(path).map_err(|error| self.failed(path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(self.symlink_denied(path));
        }
        if metadata.is_dir() {
            let reader = fs::read_dir(path).map_err(|error| self.failed(path, error))?;
            for entry in reader {
                let entry = entry.map_err(|error| self.failed(path, error))?;
                self.count_tree(&entry.path(), count, bytes)?;
            }
        } else {
            *bytes = (*bytes).saturating_add(metadata.len());
        }
        Ok(())
    }

    /// 自底向上删一棵已经数过的树。`remove_file` / `remove_dir` 不跟随链接（与 `rm -r` 不同）；
    /// 扫描阶段已经拒绝过链接，这里只是保持同一口径。
    fn delete_tree(&self, path: &Path) -> Result<(), FileError> {
        let metadata = fs::symlink_metadata(path).map_err(|error| self.failed(path, error))?;
        if metadata.is_dir() {
            let reader = fs::read_dir(path).map_err(|error| self.failed(path, error))?;
            for entry in reader {
                let entry = entry.map_err(|error| self.failed(path, error))?;
                self.delete_tree(&entry.path())?;
            }
            fs::remove_dir(path).map_err(|error| self.failed(path, error))
        } else {
            fs::remove_file(path).map_err(|error| self.failed(path, error))
        }
    }

    fn entry_of(&self, path: &Path) -> Result<FileEntry, FileError> {
        let metadata = fs::metadata(path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => FileError::NotFound {
                path: self.display(path),
            },
            _ => self.failed(path, error),
        })?;
        let relative = self.display(path);
        if metadata.is_dir() {
            Ok(FileEntry::dir(relative))
        } else {
            Ok(FileEntry::file(relative, metadata.len()))
        }
    }
}

/// 逐行读一段范围，不把整个文件放进内存。
///
/// 三条判据：
/// 1. **按字节上限收尾，且不切断字符** —— 截断点退到上一行的行尾（整行为单位），
///    只有「单独一行就超上限」时才在字符边界上切，绝不产出半个 UTF-8 字符；
/// 2. **`truncated` 的语义是「你要的还没给完」**：撞到字节上限，或者范围到 endLine
///    而文件后面还有内容，都算；到 EOF 正好读完不算；
/// 3. 行分隔只用 `\n`，行尾的 `\r` 原样保留（CRLF 文件的读取结果与原始字节一致）。
fn read_line_range(
    resolved: &Path,
    path: &str,
    range: LineRange,
    max_bytes: usize,
) -> Result<(Vec<u8>, bool), FileError> {
    let file = fs::File::open(resolved).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => FileError::NotFound {
            path: path.to_owned(),
        },
        _ => FileError::Failed {
            message: format!("{path}: {error}"),
        },
    })?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut out = Vec::new();
    let mut line_no: u64 = 0;
    let mut truncated = false;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| FileError::Failed {
                message: format!("{path}: {error}"),
            })?;
        if read == 0 {
            break;
        }
        line_no += 1;
        if line_no < range.start {
            continue;
        }
        if line_no > range.end {
            truncated = true;
            break;
        }
        if out.len() + line.len() > max_bytes {
            let room = max_bytes.saturating_sub(out.len());
            out.extend_from_slice(&line[..char_boundary(&line, room)]);
            truncated = true;
            break;
        }
        out.extend_from_slice(&line);
    }
    Ok((out, truncated))
}

/// 从 `limit` 往回退到最近的 UTF-8 字符边界（切在字符中间会让整段读变成「不是 UTF-8」）。
fn char_boundary(bytes: &[u8], limit: usize) -> usize {
    let mut end = limit.min(bytes.len());
    while end > 0 && end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    end
}

/// 把 `range` 那几行替换成 `text`，其余行按字节原样复制到 tmp，再整体 rename 过去。
///
/// 行尾规则：被替换段连同它自己的终止符一起移除；`text` 不以换行结尾就补上**该段最后
/// 一行**的终止符。这样 CRLF 项目改一行不会变成全文件 diff，空的末行也不会凭空多一个
/// 换行。范围起点超过文件行数时报错，不静默追加到末尾 —— 「替换第 100 行」在 5 行的
/// 文件上应该是用户发现自己的错误，而不是文件被悄悄改了。
fn write_line_range(
    resolved: &Path,
    path: &str,
    text: &str,
    range: LineRange,
) -> Result<(), FileError> {
    let file = fs::File::open(resolved).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => FileError::NotFound {
            path: path.to_owned(),
        },
        _ => FileError::Failed {
            message: format!("{path}: {error}"),
        },
    })?;
    let parent = resolved.parent().unwrap_or_else(|| Path::new("."));
    let tmp_path = temp_path(parent);
    let tmp_file = fs::File::create(&tmp_path).map_err(|error| FileError::Failed {
        message: format!("{path}: 建不了临时文件：{error}"),
    })?;
    let mut reader = BufReader::new(file);
    let mut writer = std::io::BufWriter::new(tmp_file);
    let outcome = copy_lines(&mut reader, &mut writer, text, range);
    let finish = outcome.and_then(|()| {
        writer.flush().map_err(|error| FileError::Failed {
            message: format!("{path}: 写入临时文件失败：{error}"),
        })?;
        Ok(())
    });
    if let Err(error) = finish {
        drop(writer);
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }
    // 先落盘再改名：rename 成功而数据还在页缓存里，掉电后会得到一个空文件。
    let flushed = {
        let file = writer.into_inner().map_err(|error| FileError::Failed {
            message: format!("{path}: 收尾临时文件失败：{}", error.error()),
        })?;
        file.sync_all()
    };
    if let Err(error) = flushed {
        let _ = fs::remove_file(&tmp_path);
        return Err(FileError::Failed {
            message: format!("{path}: 落盘失败：{error}"),
        });
    }
    if let Err(error) = rename_with_retry(&tmp_path, resolved) {
        let _ = fs::remove_file(&tmp_path);
        return Err(FileError::Failed {
            message: format!("{path}: 替换原文件失败：{error}"),
        });
    }
    Ok(())
}

fn copy_lines<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    text: &str,
    range: LineRange,
) -> Result<(), FileError> {
    let mut line = Vec::new();
    let mut line_no: u64 = 0;
    let mut seen_start = false;
    let mut wrote = false;
    let mut terminator = "";
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| FileError::Failed {
                message: format!("读原文失败：{error}"),
            })?;
        if read == 0 {
            break;
        }
        line_no += 1;
        if line_no < range.start {
            writer.write_all(&line).map_err(write_failed)?;
            continue;
        }
        seen_start = true;
        terminator = line_terminator(&line);
        if line_no < range.end {
            continue;
        }
        if wrote {
            // 替换段已经写完，后面都是未触碰的行：按字节原样复制（行尾也不动）。
            writer.write_all(&line).map_err(write_failed)?;
            continue;
        }
        write_replacement(writer, text, terminator)?;
        wrote = true;
    }
    if !seen_start {
        return Err(FileError::Invalid {
            message: format!("文件只有 {line_no} 行，startLine 是 {}", range.start),
        });
    }
    if !wrote {
        // 范围越过 EOF：替换段一直到文件末尾，终止符取最后一行自己的。
        write_replacement(writer, text, terminator)?;
    }
    Ok(())
}

fn write_replacement<W: Write>(
    writer: &mut W,
    text: &str,
    terminator: &str,
) -> Result<(), FileError> {
    writer.write_all(text.as_bytes()).map_err(write_failed)?;
    if !text.ends_with('\n') && !terminator.is_empty() {
        writer
            .write_all(terminator.as_bytes())
            .map_err(write_failed)?;
    }
    Ok(())
}

fn line_terminator(line: &[u8]) -> &'static str {
    if line.ends_with(b"\r\n") {
        "\r\n"
    } else if line.ends_with(b"\n") {
        "\n"
    } else {
        ""
    }
}

fn write_failed(error: io::Error) -> FileError {
    FileError::Failed {
        message: format!("写临时文件失败：{error}"),
    }
}

fn temp_path(parent: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".hbot-files-tmp-{}-{seq}", std::process::id()))
}

/// Windows 上目标被编辑器/杀软短暂占用时 `rename` 会 `EPERM`；有界重试，不做无限等待。
fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    let mut last = None;
    for attempt in 0..20_u64 {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::AlreadyExists
                ) =>
            {
                last = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
            }
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("rename retries exhausted")))
}

impl FileStore for ProjectFiles {
    fn read(
        &self,
        path: &str,
        max_bytes: usize,
        range: Option<LineRange>,
    ) -> Result<ReadFile, FileError> {
        validate_file_read(path, max_bytes, range)?;
        let resolved = self.resolve(path)?;
        let metadata = fs::metadata(&resolved).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => FileError::NotFound {
                path: path.to_owned(),
            },
            _ => self.failed(&resolved, error),
        })?;
        if metadata.is_dir() {
            return Err(FileError::Invalid {
                message: format!("{path} 是目录，不是文件：要列目录请用 list()"),
            });
        }
        let (bytes, truncated) = match range {
            None => {
                let bytes = fs::read(&resolved).map_err(|error| self.failed(&resolved, error))?;
                let truncated = bytes.len() > max_bytes;
                (bytes, truncated)
            }
            Some(range) => read_line_range(&resolved, path, range, max_bytes)?,
        };
        // 截断点退到字符边界：切在字符中间会让整段读变成「不是 UTF-8 文本」，
        // 而文件本身明明是 UTF-8。
        let slice = &bytes[..char_boundary(&bytes, max_bytes)];
        let text = String::from_utf8(slice.to_vec()).map_err(|_| FileError::Invalid {
            message: format!(
                "{path} 不是 UTF-8 文本：文本读只接受 UTF-8，二进制请用 readBytes（回来是 base64）"
            ),
        })?;
        Ok(ReadFile {
            path: path.to_owned(),
            text,
            bytes: slice.len(),
            truncated,
        })
    }

    fn read_bytes(&self, path: &str, max_bytes: usize) -> Result<ReadBytes, FileError> {
        validate_file_read(path, max_bytes, None)?;
        let resolved = self.resolve(path)?;
        let metadata = fs::metadata(&resolved).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => FileError::NotFound {
                path: path.to_owned(),
            },
            _ => self.failed(&resolved, error),
        })?;
        if metadata.is_dir() {
            return Err(FileError::Invalid {
                message: format!("{path} 是目录，不是文件：要列目录请用 list()"),
            });
        }
        let file = fs::File::open(&resolved).map_err(|error| self.failed(&resolved, error))?;
        // 多读 1 字节只为判断「后面还有」：max_bytes 本身不设上限，但不会把整个大文件
        // 读进内存后再截断。
        let mut bytes = Vec::new();
        file.take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| self.failed(&resolved, error))?;
        let truncated = bytes.len() > max_bytes;
        bytes.truncate(max_bytes);
        Ok(ReadBytes {
            path: path.to_owned(),
            bytes,
            truncated,
        })
    }

    fn write(&self, path: &str, text: &str) -> Result<FileEntry, FileError> {
        validate_file_write(path, text)?;
        let resolved = self.resolve(path)?;
        if let Some(parent) = resolved.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|error| self.failed(parent, error))?;
            }
        }
        // 整文件覆盖：写后大小已知，按**差值**记账 —— 覆盖写只占新增的那部分，改短反而退账。
        let before = self.existing_bytes(&resolved)?;
        let after = text.len() as u64;
        let adjust = self.adjust_quota(path, before, after)?;
        if let Err(error) = fs::write(&resolved, text.as_bytes()) {
            let failure = self.failed(&resolved, error);
            self.undo_quota(&adjust, &failure);
            return Err(failure);
        }
        self.entry_of(&resolved)
    }

    fn write_range(
        &self,
        path: &str,
        text: &str,
        range: LineRange,
    ) -> Result<FileEntry, FileError> {
        validate_file_write_range(path, text, range)?;
        let resolved = self.resolve(path)?;
        // 最终大小写前算不出来（替换段与原文的行数、行尾都可能不同），所以按上界预留：
        // 新文本 + 最多两字节行尾；写完再按实际大小对账退回多留的那部分。
        let before = self.existing_bytes(&resolved)?;
        let upper = before.saturating_add(text.len() as u64).saturating_add(2);
        let adjust = self.adjust_quota(path, before, upper)?;
        if let Err(failure) = write_line_range(&resolved, path, text, range) {
            self.undo_quota(&adjust, &failure);
            return Err(failure);
        }
        match self.existing_bytes(&resolved) {
            Ok(actual) => self.settle_quota(path, before, adjust.reserved, actual),
            Err(error) => tracing::warn!(%error, path, "写完之后读不到文件大小：配额账本会偏高"),
        }
        self.entry_of(&resolved)
    }

    fn remove(&self, path: &str, recursive: bool) -> Result<u32, FileError> {
        let resolved = self.resolve(path)?;
        let metadata = fs::symlink_metadata(&resolved).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => FileError::NotFound {
                path: path.to_owned(),
            },
            _ => self.failed(&resolved, error),
        })?;
        if resolved == self.root {
            return Err(FileError::Denied {
                path: path.to_owned(),
            });
        }
        if metadata.file_type().is_symlink() {
            return Err(self.symlink_denied(&resolved));
        }
        if metadata.is_dir() {
            if !recursive {
                // 不递归时只删空目录：生成代码手里的递归删除等于一次静默的批量数据丢失。
                fs::remove_dir(&resolved).map_err(|error| match error.kind() {
                    io::ErrorKind::NotFound => FileError::NotFound {
                        path: path.to_owned(),
                    },
                    io::ErrorKind::DirectoryNotEmpty => FileError::Denied {
                        path: format!("{path}（目录非空：加 recursive: true，或先删里面的东西）"),
                    },
                    _ => self.failed(&resolved, error),
                })?;
                return Ok(1);
            }
            // 先数一遍再删：中途发现链接或超上限时树还是完整的，账本也还没动。
            // 计数与字节在同一次遍历里拿到 —— 删掉之后字节数就量不到了。
            let mut count = 0_u32;
            let mut bytes = 0_u64;
            self.count_tree(&resolved, &mut count, &mut bytes)?;
            self.delete_tree(&resolved)?;
            self.release_after_remove(path, bytes);
            Ok(count)
        } else {
            let bytes = metadata.len();
            fs::remove_file(&resolved).map_err(|error| self.failed(&resolved, error))?;
            self.release_after_remove(path, bytes);
            Ok(1)
        }
    }

    fn list(&self, path: &str, limit: usize) -> Result<Vec<FileEntry>, FileError> {
        validate_file_list(limit)?;
        let resolved = self.resolve(path)?;
        let mut entries = Vec::new();
        let reader = fs::read_dir(&resolved).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => FileError::NotFound {
                path: path.to_owned(),
            },
            io::ErrorKind::NotADirectory => FileError::Invalid {
                message: format!("{path} 不是目录"),
            },
            _ => self.failed(&resolved, error),
        })?;
        for entry in reader {
            let entry = entry.map_err(|error| self.failed(&resolved, error))?;
            if entries.len() >= limit {
                break;
            }
            entries.push(self.entry_of(&entry.path())?);
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }
}

/// 清单里 files 能力的声明（读 / 写两半）。
#[must_use]
pub fn declared_files(root: &Path) -> hbot_core::FilesCapability {
    hbot_code::manifest::load(root)
        .map(|manifest| manifest.capabilities.files())
        .unwrap_or_default()
}

/// 清单里的磁盘配额（字节）；`0` = 不限。
///
/// 读不到清单时按 0（不限）而不是报错：这个函数在 `start_bot` 里装配后端时调用，
/// 而清单读不了的情况已经由 `declared_files` / 编译那一路报过了 —— 在这里再造一个
/// 失败点只会让「项目根没问题但清单暂时坏掉」变成起不来。
#[must_use]
pub fn declared_file_quota(root: &Path) -> u64 {
    hbot_code::manifest::load(root)
        .map(|manifest| manifest.limits.file_quota_bytes())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbot_core::MAX_FILE_READ_BYTES;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("hbot-files-{}-{unique}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp root");
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn write_then_read_round_trips_inside_the_root() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        let entry = files.write("notes/a.txt", "hello").expect("write");
        assert_eq!(entry.size, 5);
        let read = files
            .read("notes/a.txt", MAX_FILE_READ_BYTES, None)
            .expect("read");
        assert_eq!(read.text, "hello");
        assert!(!read.truncated);
        assert!(root.0.join("notes/a.txt").exists());
    }

    #[test]
    fn escaping_the_root_is_refused() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        assert!(matches!(
            files.read("../secret", 16, None),
            Err(FileError::Denied { .. })
        ));
        assert!(matches!(
            files.write("a/../../x", "x"),
            Err(FileError::Denied { .. })
        ));
        // 绝对路径（`/etc/x`、`C:/x`）在槽位层的 `normalize_file_path` 就拒了，
        // 到不了这里；后端只负责"已经归一的相对路径不许借 .. 或链接逃出去"。
        assert!(!root.0.join("x").exists());
    }

    #[test]
    fn reading_a_missing_file_is_not_found() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        assert!(matches!(
            files.read("nope.txt", 16, None),
            Err(FileError::NotFound { .. })
        ));
    }

    #[test]
    fn listing_reports_files_and_directories_sorted() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files.write("b.txt", "bb").expect("write b");
        files.write("a/x.txt", "x").expect("write a");
        let entries = files.list("", 10).expect("list");
        let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, vec!["a", "b.txt"]);
    }

    #[test]
    fn removing_a_non_empty_directory_is_denied() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files.write("dir/a.txt", "x").expect("write");
        assert!(matches!(
            files.remove("dir", false),
            Err(FileError::Denied { .. })
        ));
        files.remove("dir/a.txt", false).expect("remove file");
        files.remove("dir", false).expect("remove empty dir");
    }

    #[test]
    fn a_truncated_read_says_so() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files.write("long.txt", "0123456789").expect("write");
        let read = files.read("long.txt", 4, None).expect("read");
        assert_eq!(read.text, "0123");
        assert!(read.truncated);
        assert_eq!(read.bytes, 4);
    }

    #[test]
    fn a_line_range_reads_exactly_those_lines() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files
            .write("log.txt", "l1\nl2\nl3\nl4\nl5\n")
            .expect("write");
        let read = files
            .read("log.txt", MAX_FILE_READ_BYTES, Some(LineRange::new(2, 4)))
            .expect("read");
        assert_eq!(read.text, "l2\nl3\nl4\n");
        // 第 5 行还在 → truncated 说明「后面还有你没要的内容」，用户据此翻页。
        assert!(read.truncated);
    }

    #[test]
    fn a_range_past_the_end_stops_at_eof_without_error() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files.write("log.txt", "a\nb\n").expect("write");
        let read = files
            .read("log.txt", MAX_FILE_READ_BYTES, Some(LineRange::new(5, 9)))
            .expect("read");
        assert_eq!(read.text, "");
        assert!(!read.truncated);
    }

    #[test]
    fn a_range_that_stops_before_the_end_says_truncated() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files.write("log.txt", "a\nb\nc\n").expect("write");
        let read = files
            .read("log.txt", MAX_FILE_READ_BYTES, Some(LineRange::new(1, 2)))
            .expect("read");
        assert_eq!(read.text, "a\nb\n");
        assert!(read.truncated, "第 3 行还在，用户没拿到，必须说明");
    }

    #[test]
    fn a_range_is_cut_on_byte_limit_without_splitting_a_character() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        // 每行三个汉字（9 字节 + 换行）；上限 10 字节只够第一行。
        files.write("log.txt", "一二三\n四五六\n").expect("write");
        let read = files
            .read("log.txt", 10, Some(LineRange::new(1, 2)))
            .expect("read");
        assert_eq!(read.text, "一二三\n");
        assert!(read.truncated);
    }

    #[test]
    fn crlf_files_keep_their_carriage_returns() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files.write("win.txt", "a\r\nb\r\n").expect("write");
        let read = files
            .read("win.txt", MAX_FILE_READ_BYTES, Some(LineRange::new(2, 2)))
            .expect("read");
        assert_eq!(read.text, "b\r\n");
    }

    #[test]
    fn recursive_remove_takes_the_whole_tree_and_counts_the_entries() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files.write("dir/a.txt", "a").expect("write a");
        files.write("dir/sub/b.txt", "b").expect("write b");
        files.write("keep.txt", "k").expect("write k");
        let removed = files.remove("dir", true).expect("recursive remove");
        // dir + a.txt + sub + b.txt
        assert_eq!(removed, 4);
        assert!(!root.0.join("dir").exists());
        assert!(root.0.join("keep.txt").exists());
    }

    #[test]
    fn read_bytes_returns_raw_binary_and_flags_truncation() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        let bytes: [u8; 6] = [0x00, 0xff, 0x80, 0x89, 0x50, 0x4e];
        fs::write(root.0.join("blob.bin"), bytes).expect("write bin");
        let read = files.read_bytes("blob.bin", 1024).expect("read");
        assert_eq!(read.bytes, bytes);
        assert!(!read.truncated);
        let cut = files.read_bytes("blob.bin", 4).expect("read cut");
        assert_eq!(cut.bytes, &bytes[..4]);
        assert!(cut.truncated);
    }

    #[test]
    fn a_line_range_write_replaces_only_those_lines() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files.write("log.txt", "l1\nl2\nl3\nl4\n").expect("write");
        files
            .write_range("log.txt", "x2\nx3", LineRange::new(2, 3))
            .expect("write range");
        let read = files
            .read("log.txt", MAX_FILE_READ_BYTES, None)
            .expect("read");
        assert_eq!(read.text, "l1\nx2\nx3\nl4\n");
    }

    #[test]
    fn a_line_range_write_keeps_crlf_untouched() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files.write("win.txt", "a\r\nb\r\nc\r\n").expect("write");
        files
            .write_range("win.txt", "B", LineRange::new(2, 2))
            .expect("write range");
        let raw = fs::read(root.0.join("win.txt")).expect("read raw");
        assert_eq!(String::from_utf8(raw).unwrap(), "a\r\nB\r\nc\r\n");
    }

    #[test]
    fn a_range_beyond_the_end_is_refused_instead_of_appending() {
        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        files.write("log.txt", "only\n").expect("write");
        let error = files
            .write_range("log.txt", "x", LineRange::new(5, 6))
            .unwrap_err();
        assert!(error.to_string().contains("HBOT_FILE_INVALID"), "{error}");
        let read = files
            .read("log.txt", MAX_FILE_READ_BYTES, None)
            .expect("read");
        assert_eq!(read.text, "only\n");
    }

    #[cfg(unix)]
    #[test]
    fn recursive_remove_refuses_to_follow_a_symlink() {
        use std::os::unix::fs::symlink;

        let root = TempRoot::new();
        let files = ProjectFiles::new(&root.0).expect("backend");
        let outside = TempRoot::new();
        fs::write(outside.0.join("secret.txt"), "s").expect("write secret");
        fs::create_dir_all(root.0.join("dir")).expect("mkdir");
        symlink(&outside.0, root.0.join("dir/link")).expect("symlink");
        assert!(matches!(
            files.remove("dir", true),
            Err(FileError::Denied { .. })
        ));
        // 拒绝发生在删除之前：树与链接目标都还在。
        assert!(root.0.join("dir/link").exists());
        assert!(outside.0.join("secret.txt").exists());
    }

    // ---- 磁盘配额（`limits.quotaMb`） ----------------------------------------

    /// 带账本的后端：`bot_file_usage` 有外键，所以库里必须先有项目行与 bot 行。
    struct QuotaFixture {
        root: TempRoot,
        store: Arc<Store>,
        project_id: hbot_store::ProjectId,
        bot_id: BotId,
    }

    impl QuotaFixture {
        fn new() -> Self {
            let root = TempRoot::new();
            let store = Arc::new(Store::open(root.0.join("hbot.db")).expect("open store"));
            let project_id = hbot_store::ProjectId::from_uuid(uuid::Uuid::from_u128(0x9abc));
            let bot_id = BotId::from_uuid(uuid::Uuid::from_u128(0x9abd));
            store
                .insert_project(&hbot_store::Project {
                    id: project_id,
                    name: "quota".to_owned(),
                    path: root.0.to_string_lossy().into_owned(),
                    created_at: 1,
                })
                .expect("insert project");
            store
                .insert_bot(&hbot_store::Bot {
                    id: bot_id,
                    project_id,
                    name: "quota-bot".to_owned(),
                    platform: "local".to_owned(),
                    config_json: "{}".to_owned(),
                    enabled: true,
                    created_at: 2,
                })
                .expect("insert bot");
            Self {
                root,
                store,
                project_id,
                bot_id,
            }
        }

        fn backend(&self, bot_id: BotId, limit: u64) -> ProjectFiles {
            ProjectFiles::new(&self.root.0)
                .expect("backend")
                .with_quota(
                    bot_id,
                    Arc::clone(&self.store),
                    Arc::new(hbot_adapters::SystemClock),
                    limit,
                )
        }

        fn files(&self, limit: u64) -> ProjectFiles {
            self.backend(self.bot_id, limit)
        }

        fn used(&self, bot_id: BotId) -> u64 {
            self.store.file_usage(&bot_id).expect("read usage")
        }
    }

    #[test]
    fn a_write_beyond_the_quota_is_refused_and_nothing_is_written() {
        let fixture = QuotaFixture::new();
        let files = fixture.files(10);
        files
            .write("a.txt", "0123456789")
            .expect("write fills the quota");
        assert_eq!(fixture.used(fixture.bot_id), 10);

        let error = files.write("b.txt", "x").unwrap_err();
        assert!(error.to_string().contains("HBOT_FILE_QUOTA"), "{error}");
        assert_eq!(fixture.used(fixture.bot_id), 10, "被拒的写不该记账");
        assert!(
            !fixture.root.0.join("b.txt").exists(),
            "拒绝要发生在写盘之前"
        );
    }

    #[test]
    fn an_overwrite_that_shrinks_gives_the_difference_back() {
        let fixture = QuotaFixture::new();
        let files = fixture.files(10);
        files
            .write("a.txt", "0123456789")
            .expect("write fills the quota");
        files.write("a.txt", "ab").expect("覆盖写变短不需要新额度");
        assert_eq!(fixture.used(fixture.bot_id), 2, "改短要退账");
        files
            .write("b.txt", "01234567")
            .expect("退回来的额度可以再用");
        assert_eq!(fixture.used(fixture.bot_id), 10);
    }

    #[test]
    fn removing_a_file_gives_its_bytes_back() {
        let fixture = QuotaFixture::new();
        let files = fixture.files(8);
        files
            .write("a.txt", "01234567")
            .expect("write fills the quota");
        assert_eq!(files.remove("a.txt", false).expect("remove"), 1);
        assert_eq!(fixture.used(fixture.bot_id), 0);
        files
            .write("a.txt", "01234567")
            .expect("删除之后额度回来了");
    }

    #[test]
    fn removing_a_directory_gives_back_the_whole_tree() {
        let fixture = QuotaFixture::new();
        let files = fixture.files(6);
        files.write("tmp/a.txt", "aa").expect("write a");
        files.write("tmp/sub/b.txt", "bbbb").expect("write b");
        assert_eq!(fixture.used(fixture.bot_id), 6);
        // 条目数含目录本身：tmp、tmp/a.txt、tmp/sub、tmp/sub/b.txt。
        assert_eq!(files.remove("tmp", true).expect("remove tree"), 4);
        assert_eq!(
            fixture.used(fixture.bot_id),
            0,
            "目录按整棵树的文件字节退账"
        );
    }

    #[test]
    fn a_failed_line_range_write_does_not_eat_the_quota() {
        let fixture = QuotaFixture::new();
        let files = fixture.files(16);
        files.write("log.txt", "only\n").expect("write");
        assert_eq!(fixture.used(fixture.bot_id), 5);

        // 行号越界：写前已经按上界预留，失败必须原样撤销。
        let error = files
            .write_range("log.txt", "x", LineRange::new(5, 6))
            .unwrap_err();
        assert!(error.to_string().contains("HBOT_FILE_INVALID"), "{error}");
        assert_eq!(fixture.used(fixture.bot_id), 5, "失败的写不该吃掉额度");
    }

    #[test]
    fn a_line_range_write_settles_to_the_real_file_size() {
        let fixture = QuotaFixture::new();
        let files = fixture.files(64);
        files.write("log.txt", "l1\nl2\nl3\n").expect("write");
        assert_eq!(fixture.used(fixture.bot_id), 9);

        files
            .write_range("log.txt", "X", LineRange::new(2, 2))
            .expect("write range");
        // 上界预留 9 + 1 + 2 = 12，写完的真实大小是 8（`l2` 换成了 `X`），
        // 所以既要把多留的 4 字节退回去，也要把变短的 1 字节退掉：账本落到 8。
        assert_eq!(fixture.used(fixture.bot_id), 8, "按实际大小对账");
    }

    #[test]
    fn a_quota_of_zero_is_unlimited_but_still_ledgered() {
        let fixture = QuotaFixture::new();
        let files = fixture.files(0);
        let big = "x".repeat(4096);
        files.write("big.txt", &big).expect("0 = 不限");
        assert_eq!(fixture.used(fixture.bot_id), 4096, "不限也记账（排障要看）");
    }

    #[test]
    fn another_bot_does_not_share_the_ledger() {
        let fixture = QuotaFixture::new();
        let other_id = BotId::from_uuid(uuid::Uuid::from_u128(0x9abe));
        fixture
            .store
            .insert_bot(&hbot_store::Bot {
                id: other_id,
                project_id: fixture.project_id,
                name: "other".to_owned(),
                platform: "local".to_owned(),
                config_json: "{}".to_owned(),
                enabled: true,
                created_at: 3,
            })
            .expect("insert other bot");

        fixture
            .files(10)
            .write("a.txt", "0123456789")
            .expect("write fills the first bot quota");
        assert_eq!(fixture.used(fixture.bot_id), 10);
        assert_eq!(fixture.used(other_id), 0, "账本按 bot 分，不能串");

        let other = fixture.backend(other_id, 4);
        other
            .write("b.txt", "abcd")
            .expect("另一只 bot 有自己的额度");
        assert_eq!(fixture.used(other_id), 4);
        assert_eq!(fixture.used(fixture.bot_id), 10);
    }
}
