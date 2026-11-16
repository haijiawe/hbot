//! `cargo run -p hbot-bench --release` —— 一条命令跑完全部五组，按固定顺序（互不干扰）。
//!
//! 退出码（默认 **闸门语义**）：
//!
//! * `0` = 测量全部完成，且**所有预算都 PASS**；
//! * `1` = 测量完成，但至少一条预算 FAIL（超预算）。**这条必须能红** —— 否则 CI 就是假绿：
//!   `cargo test` 走的路径永远 exit 0（它断言的是「量到了」，不是「达标」），
//!   所以闸门只能由这个二进制承担；
//! * `2` = 某一步本身出错（建夹具/开库/编译失败等），**测量没有完成**。
//!
//! 只想看数字、不想让命令变红（写 spike、看趋势）时加 `--report`：FAIL 也 exit 0。
//! 两个语义都要有，但默认必须是闸门 —— “默认绿”会让超预算悄悄过关。
//!
//! 五组必须串行：10k 搜索会把磁盘/CPU 打满，并行量别的项没有意义。

use std::process::ExitCode;

use hbot_bench::{file_bench, harness, memory_bench, quickjs_bench, search_bench, store_bench};

fn main() -> ExitCode {
    let report_only = std::env::args().any(|arg| arg == "--report");
    harness::print_header();
    match run_all() {
        Ok(()) => {
            let failures = harness::failure_count();
            let total = harness::budget_count();
            if failures == 0 {
                println!(
                    "\nsummary | {total} 条预算全部 PASS | - | - | PASS | 机器/夹具/数据量见上方各行"
                );
                ExitCode::SUCCESS
            } else {
                println!(
                    "\nsummary | {failures}/{total} 条 FAIL | - | - | FAIL | 如实报告：不改预算值、不改实现迎合数字；处置清单见 docs/spikes/S8-perf-budgets.md"
                );
                if report_only {
                    println!(
                        "exit | 0 | - | - | - | --report：只报数不定罪（闸门语义是默认，不要用它接 CI）"
                    );
                    ExitCode::SUCCESS
                } else {
                    println!(
                        "exit | 1 | - | - | - | 默认闸门语义：有 FAIL 就红（只想看数字请加 --report）"
                    );
                    ExitCode::from(1)
                }
            }
        }
        Err(error) => {
            eprintln!("\nhbot-bench 未能完成测量：{error}");
            ExitCode::from(2)
        }
    }
}

/// 顺序执行五组（串行是刻意的：10k 搜索会把磁盘/CPU 打满，并行量别的项没有意义）。
fn run_all() -> Result<(), Box<dyn std::error::Error>> {
    search_bench::run()?;
    file_bench::run()?;
    store_bench::run()?;
    quickjs_bench::run()?;
    memory_bench::run()?;
    Ok(())
}
