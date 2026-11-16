//! 预算 2：打开 1 MB 文件 < 150 ms（读 + 写各一次）。
//!
//! 数据怎么造的（也打在结果里）：**1 MiB 整**的 ASCII 文本，16,384 行 × 64 字节
//! （每行 `NNNNN| ` + 56 个字符的滚动文本 + `\n`）。行是真行，不是把 1 MiB 塞进一行 ——
//! `read_file` / `write_file` 的成本里有 UTF-8 校验与 blake3 全量哈希，两者都按
//! `内容长度` 走，但真实编辑器打开的文件一定带行结构，夹具不该比现实更好或更坏。
//!
//! 三个数字：
//!
//! * `read`：`Workspace::read_file` 一次 —— 整文件 `fs::read` + blake3 + UTF-8 校验；
//! * `write`：`Workspace::write_file` 带 `expected_hash` —— 先读旧文件算哈希比对，
//!   再写 `.hbot-tmp-*`、`sync_all()`（fsync）、rename。**fsync 大概率是主项**；
//! * 参考行（不判定）：裸 `fs::read` 与「不带 expected_hash 的写」，用来把
//!   “哈希/校验”与“落盘”各自的开销分开 —— 判定仍只看用户真实走的那条路。
//!
//! 冷/热（`05-quality-gates.md` §4 要求“冷/热各一次”）：文件刚由本进程写出，读的时候
//! 内容在 Windows 文件缓存里，所以两个数都是**热缓存**；`read_cold` 只是“进程内第一次
//! `read_file`”，不是冷盘 —— 真冷盘要清 standby list（需管理员），本次没做，不冒充。
//! 判定的两条：`file_1mb_read_cold`（首次）与 `file_1mb_read_warm`（第 2–4 次中位数），
//! 以及 `file_1mb_write`（三次中位数，写路径每次都是全新的 tmp + fsync + rename）。

use std::path::Path;
use std::time::{Duration, Instant};

use hbot_workspace::{MAX_READ_BYTES, Workspace, hash_bytes};

use crate::harness::{self, Stats, TempDir, Unit};

/// 每行字节数（含换行）。
const LINE_BYTES: usize = 64;
/// 行数：16,384 × 64 = 1,048,576 = 1 MiB 整。
const LINES: usize = 16_384;
/// 文件名（工作区相对路径）。
const NAME: &str = "big.txt";
/// 每个数字复跑几次，取中位数。
const RUNS: usize = 3;
/// 预算：打开 1 MB 文件 < 150 ms（读写同预算）。
const BUDGET: Duration = Duration::from_millis(150);

/// 跑 1 MiB 文件读/写测量并打印结果行。
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    harness::require_release();
    harness::section("2. 1 MB 文件：read_file / write_file（hbot_workspace，含 blake3 + fsync）");

    let temp = TempDir::new("file")?;
    let root = temp.path().to_path_buf();
    let content = build_text();
    assert_eq!(content.len(), 1_048_576, "夹具必须正好 1 MiB");
    assert_eq!(content.lines().count(), LINES, "夹具必须正好 16,384 行");

    // 夹具落盘本身不计入测量。
    std::fs::write(root.join(NAME), &content)?;
    let workspace = Workspace::open(root.clone())?;
    let rel = Path::new(NAME);

    harness::info_line(
        "file_fixture",
        &format!("{} B / {} 行", content.len(), LINES),
        "bytes",
        &format!(
            "1 MiB 整 ASCII UTF-8，每行 {LINE_BYTES} B（`NNNNN| ` + 56 字符 + 换行）；read 上限用 MAX_READ_BYTES={MAX_READ_BYTES}"
        ),
    );

    // ---- 读 -------------------------------------------------------------
    let raw_read = measure(RUNS, || {
        std::fs::read(root.join(NAME))
            .map(|bytes| {
                assert_eq!(bytes.len(), 1_048_576, "裸读回来的长度不对");
            })
            .map_err(|error| error.to_string())
    })?;

    let mut first_read: Option<Duration> = None;
    let mut reads = Vec::with_capacity(RUNS);
    let mut hash = String::new();
    // 1 次“冷”（进程内首次）+ RUNS 次热。
    for index in 0..=RUNS {
        let started = Instant::now();
        let file = workspace.read_file(rel, MAX_READ_BYTES)?;
        let elapsed = started.elapsed();
        if index == 0 {
            first_read = Some(elapsed);
        } else {
            reads.push(elapsed);
        }
        assert_eq!(file.size, 1_048_576, "read_file 报的 size 不对");
        assert_eq!(
            file.content.len(),
            1_048_576,
            "read_file 回来的内容长度不对"
        );
        assert_eq!(
            file.hash,
            hash_bytes(file.content.as_bytes()),
            "read_file 的哈希必须与内容一致（否则后面带 expected_hash 的写会 Conflict）"
        );
        hash = file.hash;
    }

    // ---- 写 -------------------------------------------------------------
    // 内容不变 ⇒ 每次写出来的哈希都一样，所以复跑时沿用上一次返回的哈希即可，
    // 三次写都走「带 expected_hash」的真实路径（读旧文件 → 比对 → tmp → fsync → rename）。
    let mut writes = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = Instant::now();
        let written = workspace.write_file(rel, &content, Some(&hash))?;
        writes.push(started.elapsed());
        assert_eq!(written.size, 1_048_576, "write_file 报的 size 不对");
        hash = written.hash;
    }
    let on_disk = std::fs::metadata(root.join(NAME))?.len();
    assert_eq!(on_disk, 1_048_576, "落盘大小不对（写坏了）");

    // 参考：不校验旧哈希的写（省掉“读旧文件 + 一次 blake3”）。
    let mut writes_no_hash = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = Instant::now();
        let written = workspace.write_file(rel, &content, None)?;
        writes_no_hash.push(started.elapsed());
        assert_eq!(written.size, 1_048_576, "参考写报的 size 不对");
    }

    harness::info_line(
        "file_1mb_raw_fs_read",
        &Unit::Ms.render(Stats::of(&raw_read).median),
        "ms",
        "参考：裸 std::fs::read 同一文件（无 blake3、无 UTF-8 校验）；差额≈校验成本，不参与判定",
    );
    harness::info_line(
        "file_1mb_write_no_expected_hash",
        &Unit::Ms.render(Stats::of(&writes_no_hash).median),
        "ms",
        "参考：write_file(..., None)，省掉“读旧文件 + 比对哈希”；差额≈冲突检测成本，不参与判定",
    );

    harness::budget_line(
        "file_1mb_read_cold",
        &Stats::of(&[first_read.unwrap_or_default()]),
        Unit::Ms,
        BUDGET,
        "冷口径 = 进程内第一次 read_file（整文件 fs::read + blake3 + UTF-8 校验）；文件刚由本进程写出 ⇒ 仍是热缓存，真冷盘只会更慢",
    );
    harness::budget_line(
        "file_1mb_read_warm",
        &Stats::of(&reads),
        Unit::Ms,
        BUDGET,
        "热口径 = 第 2–4 次的中位数；read_file：整文件 fs::read + blake3 + UTF-8 校验 + 构造 String；1 MiB / 16,384 行",
    );
    harness::budget_line(
        "file_1mb_write",
        &Stats::of(&writes),
        Unit::Ms,
        BUDGET,
        "write_file(expected_hash)：读旧文件比对 → .hbot-tmp-* 写入 → sync_all(fsync) → rename(最多 5 次重试)；1 MiB，热缓存",
    );

    Ok(())
}

/// 造 1 MiB 整的文本：16,384 行 × 64 字节。
fn build_text() -> String {
    let base = b"lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor \
                  incididunt ut labore et dolore magna aliqua. ";
    let mut text = String::with_capacity(LINE_BYTES * LINES);
    for index in 0..LINES {
        let prefix = format!("{index:05}| ");
        text.push_str(&prefix);
        let filler = LINE_BYTES - 1 - prefix.len();
        for offset in 0..filler {
            // 每行起点错开，避免所有行逐字相同（读起来像真实文本）。
            let byte = base[(index * 7 + offset) % base.len()];
            text.push(byte as char);
        }
        text.push('\n');
    }
    text
}

/// 跑 `runs` 次，收集每次的耗时（`Result` 在错误时原样冒泡成字符串）。
fn measure<F>(runs: usize, mut f: F) -> Result<Vec<Duration>, String>
where
    F: FnMut() -> Result<(), String>,
{
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        f()?;
        samples.push(started.elapsed());
    }
    Ok(samples)
}
