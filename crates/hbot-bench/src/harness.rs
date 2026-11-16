//! 打印口径、统计与临时目录。
//!
//! 每条预算一行：`name | value | unit | budget | pass/fail | 说明`；复跑 3 次的项
//! 额外打一行 `.spread`（单次最小–最大），因为中位数会藏住窗口抖动。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// 判定失败的行数（供 `main` 决定退出码：有 FAIL 就非零，CI 才不会当没事）。
static FAILURES: AtomicUsize = AtomicUsize::new(0);

/// 已打印的预算行数。
static BUDGETS: AtomicUsize = AtomicUsize::new(0);

/// 已有几条预算行判 FAIL。
pub fn failure_count() -> usize {
    FAILURES.load(Ordering::Relaxed)
}

/// 已打印几条预算行。
pub fn budget_count() -> usize {
    BUDGETS.load(Ordering::Relaxed)
}

/// 一组样本的中位数与极值。
#[derive(Clone, Copy, Debug)]
pub struct Stats {
    /// 中位数（复跑 3 次时取中间那次的量级）。
    pub median: Duration,
    /// 最小样本。
    pub min: Duration,
    /// 最大样本。
    pub max: Duration,
    /// 样本数。
    pub n: usize,
}

impl Stats {
    /// 由样本算统计量；空样本返回全零（调用方应至少给 1 个样本）。
    pub fn of(samples: &[Duration]) -> Self {
        if samples.is_empty() {
            return Self {
                median: Duration::ZERO,
                min: Duration::ZERO,
                max: Duration::ZERO,
                n: 0,
            };
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        Self {
            median: percentile(&sorted, 0.5),
            min: sorted[0],
            max: sorted[sorted.len() - 1],
            n: sorted.len(),
        }
    }
}

/// 已排序样本的位次分位数（`p` ∈ `[0, 1]`，四舍五入到最近位次）。
pub fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let last = sorted.len() - 1;
    let index = (last as f64 * p).round().clamp(0.0, last as f64) as usize;
    sorted[index]
}

/// 结果表里数值的显示单位。
#[derive(Clone, Copy, Debug)]
pub enum Unit {
    /// 毫秒，两位小数。
    Ms,
    /// 微秒，一位小数。
    Us,
}

impl Unit {
    /// 单位列的字面量。
    fn label(self) -> &'static str {
        match self {
            Self::Ms => "ms",
            Self::Us => "µs",
        }
    }

    /// 把一个时长渲染成该单位下的数值（不带单位后缀）。
    pub fn render(self, value: Duration) -> String {
        match self {
            Self::Ms => format!("{:.2}", value.as_secs_f64() * 1e3),
            Self::Us => format!("{:.1}", value.as_secs_f64() * 1e6),
        }
    }
}

/// 记账：`pass == false` 时累加失败数，并统一累加预算行数。
fn record(pass: bool) {
    if !pass {
        FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    BUDGETS.fetch_add(1, Ordering::Relaxed);
}

/// 打印一条**预算**行，返回是否达标。
///
/// 判定用中位数（`docs/05-quality-gates.md` §4 的预算表就是按中位数写的），
/// 单次范围另起一行展示，不参与判定。
pub fn budget_line(name: &str, stats: &Stats, unit: Unit, limit: Duration, note: &str) -> bool {
    let pass = stats.median <= limit;
    record(pass);
    let verdict = if pass { "PASS" } else { "FAIL" };
    println!(
        "{name} | {} | {} | < {} | {verdict} | {note}",
        unit.render(stats.median),
        unit.label(),
        unit.render(limit)
    );
    if stats.n > 1 {
        println!(
            "{name}.spread | {} – {} | {} | n={} | - | 单次最小–最大（同进程复跑，热缓存）",
            unit.render(stats.min),
            unit.render(stats.max),
            unit.label(),
            stats.n
        );
    }
    pass
}

/// 打印一条**数值类**预算行（MB、条数等非时长口径），返回是否达标。
///
/// 与 [`budget_line`] 同一套记账与判定语义（`value <= limit` 即 PASS），
/// 只是数值是裸浮点而不是 [`Duration`]：内存预算的单位是 MB，不是时间。
pub fn budget_line_scalar(name: &str, value: f64, unit: &str, limit: f64, note: &str) -> bool {
    let pass = value <= limit;
    record(pass);
    let verdict = if pass { "PASS" } else { "FAIL" };
    println!("{name} | {value:.2} | {unit} | < {limit:.2} | {verdict} | {note}");
    pass
}

/// 打印一条**不判定**的参考行（计数、堆大小、夹具规模等）。
pub fn info_line(name: &str, value: &str, unit: &str, note: &str) {
    println!("{name} | {value} | {unit} | - | - | {note}");
}

/// 小节标题（各测量项之间留空行）。
pub fn section(title: &str) {
    println!("\n== {title} ==");
}

/// 跑一段 PowerShell，返回**非空**输出行。
///
/// 为什么走 PowerShell：本 crate `#![forbid(unsafe_code)]`，拿不到
/// `GetProcessMemoryInfo` / `GetMpComputerStatus`；而这两样（进程私有提交、
/// Defender 实时保护状态）恰好是「口径必须写清」的前提，不能靠猜。
/// `pwsh`（PowerShell 7）不在就退 `powershell.exe`（Windows 自带）。
pub fn run_powershell(script: &str) -> Result<Vec<String>, String> {
    let mut last_error = String::from("没有可用的 PowerShell（pwsh / powershell 都起不来）");
    for exe in ["pwsh", "powershell"] {
        match std::process::Command::new(exe)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                script,
            ])
            .output()
        {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                return Ok(text
                    .lines()
                    .map(|line| line.trim().to_owned())
                    .filter(|line| !line.is_empty())
                    .collect());
            }
            Ok(output) => {
                last_error = format!(
                    "{exe} 退出码 {:?}：{}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Err(error) => last_error = format!("{exe}: {error}"),
        }
    }
    Err(last_error)
}

/// 环境快照行的统一注解。
const ENV_NOTE: &str = "本次运行时的机器/负载快照；Defender 的实时保护与按访问保护会直接\
改写 10k 文件搜索的 IO 数字，不记下来读者没法判断数字是否被扫描拖慢";

/// 打一份机器/负载/Defender 快照（每次运行都打，让每份日志自带条件）。
///
/// 采样失败不算测量失败：拿不到就写「未采样」+ 原因，绝不用猜的值填空。
fn print_environment() {
    let script = r#"
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$cs = Get-CimInstance Win32_ComputerSystem
$os = Get-CimInstance Win32_OperatingSystem
$builds = @(Get-Process -Name cargo,rustc,link -ErrorAction SilentlyContinue).Count
"cpu_name=$($cpu.Name.Trim())"
"cpu_cores=$($cpu.NumberOfCores)"
"cpu_threads=$($cpu.NumberOfLogicalProcessors)"
"ram_gb=$([math]::Round($cs.TotalPhysicalMemory / 1GB, 1))"
"os_version=$($os.Version)"
"os_build=$($os.BuildNumber)"
"load_percent=$($cpu.LoadPercentage)"
"concurrent_builds=$builds"
try {
  $mp = Get-MpComputerStatus
  "defender_rtp=$($mp.RealTimeProtectionEnabled)"
  "defender_oap=$($mp.OnAccessProtectionEnabled)"
} catch {
  "defender_rtp=unknown"
  "defender_oap=unknown"
}
"#;
    match run_powershell(script) {
        Ok(lines) => {
            for line in lines {
                match line.split_once('=') {
                    Some((key, value)) => info_line(&format!("env_{key}"), value, "-", ENV_NOTE),
                    None => info_line("env_probe", &line, "-", ENV_NOTE),
                }
            }
        }
        Err(error) => info_line("env_probe", "未采样", "-", &error),
    }
}

/// 机器与构建条件只打一次（多个 `#[ignore]` 测试各跑一次时也是）。
pub fn print_header_once() {
    static HEADER: OnceLock<()> = OnceLock::new();
    HEADER.get_or_init(print_header);
}

/// 机器与构建条件：固定机器/固定夹具/固定数据量，才有可比性（§4 测量纪律）。
pub fn print_header() {
    let profile = if cfg!(debug_assertions) {
        "debug（数字不作数）"
    } else {
        "release"
    };
    let cpu = std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown".to_owned());
    let cpus = std::env::var("NUMBER_OF_PROCESSORS").unwrap_or_else(|_| "?".to_owned());
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    println!("hbot-bench | IDE 性能预算手测台（docs/05-quality-gates.md §4）");
    println!("profile | {profile} | - | - | - | 只有 release 数字作数，debug 没有意义");
    println!("cpu | {cpu} / {cpus} 逻辑核 | - | - | - | 机器信息用于与既有 spike 数字对齐");
    println!("os | {os} {arch} | - | - | - | 固定机器/固定夹具/固定数据量");
    println!(
        "temp | {} | - | - | - | 夹具与 SQLite 都在此目录下，跑完即删",
        std::env::temp_dir().display()
    );
    print_environment();
}

/// 拒绝在 debug 构建里产出“数字”。
pub fn require_release() {
    if cfg!(debug_assertions) {
        panic!(
            "debug 构建的耗时数字没有意义：请用 `cargo run -p hbot-bench --release` \
             或 `cargo test -p hbot-bench --release`"
        );
    }
}

/// 进程内串行锁：`cargo test` 默认多线程跑测试，量测必须一次只跑一个。
///
/// 用锁而不是要求 `--test-threads=1`，是因为漏传参数时错误的数字比慢更糟。
pub fn serial_lock() -> MutexGuard<'static, ()> {
    static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();
    SERIAL
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `std::env::temp_dir()` 下带 pid + 纳秒的唯一目录，`Drop` 时整棵删除。
///
/// 为什么不写进仓库：`docs/05-quality-gates.md` §3 要求“测试产生的临时目录必须清理；
/// 不写用户目录”，10k 文件的夹具也不该出现在工作区里被 git / 搜索看到。
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// 建一个唯一目录（`hbot-bench-<label>-<pid>-<nanos>`）。
    pub fn new(label: &str) -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("hbot-bench-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// 目录根。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 拼一个子路径（不创建）。
    pub fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // 清不掉也不该让测量本身失败：Windows 上偶发句柄占用会延迟删除。
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
