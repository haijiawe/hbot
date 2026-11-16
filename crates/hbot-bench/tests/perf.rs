//! `#[ignore]` 版入口：`cargo test -p hbot-bench --release -- --ignored --nocapture --test-threads=1`
//!
//! 与 bin 目标（`cargo run -p hbot-bench --release`）共用 `src/` 里的同一份实现，
//! 不是第二份测量逻辑。每个测试自己拿 [`harness::serial_lock`]，所以即使漏了
//! `--test-threads=1`，量测也不会互相插队（错误数字比慢更糟）。
//!
//! 为什么默认 `#[ignore]`：这些是手动/夜间性能测量，需要 release 构建与一台安静的机器，
//! 不该混进日常 `cargo test`（debug 数字没有意义，见 `harness::require_release`）。

use hbot_bench::{file_bench, harness, memory_bench, quickjs_bench, search_bench, store_bench};

/// 公共前置：串行锁 + 机器信息只打一次 + 拒绝 debug 构建。
fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = harness::serial_lock();
    harness::print_header_once();
    harness::require_release();
    guard
}

#[test]
#[ignore = "手动性能测量：release + 安静机器；cargo test -p hbot-bench --release -- --ignored --nocapture"]
fn search_10k_files_under_1s() {
    let _guard = setup();
    search_bench::run().expect("10k 文件搜索测量失败");
}

#[test]
#[ignore = "手动性能测量：release + 安静机器；cargo test -p hbot-bench --release -- --ignored --nocapture"]
fn open_1mb_file_under_150ms() {
    let _guard = setup();
    file_bench::run().expect("1 MiB 文件读写测量失败");
}

#[test]
#[ignore = "手动性能测量：release + 安静机器；cargo test -p hbot-bench --release -- --ignored --nocapture"]
fn batch_write_1000_rows() {
    let _guard = setup();
    store_bench::run().expect("批量写 1000 条测量失败");
}

#[test]
#[ignore = "手动性能测量：release + 安静机器；cargo test -p hbot-bench --release -- --ignored --nocapture"]
fn quickjs_1000_events() {
    let _guard = setup();
    quickjs_bench::run().expect("QuickJS 事件吞吐测量失败");
}

#[test]
#[ignore = "手动性能测量：release + 安静机器；cargo test -p hbot-bench --release -- --ignored --nocapture"]
fn isolate_private_memory_under_8mb() {
    let _guard = setup();
    memory_bench::run().expect("isolate 内存测量失败");
}
