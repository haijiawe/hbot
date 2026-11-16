//! `hbot-bench` — IDE 性能预算的手动量测台。
//!
//! 只量 `docs/05-quality-gates.md` §4 里**还没有实测数字**的接缝，一项一个模块：
//!
//! | 模块 | 预算 | 数据规模 |
//! |---|---|---|
//! | [`search_bench`] | 全局搜索首屏 < 1 s | 10,000 个文件（200 B–4 KiB），三层目录，1% 埋命中词 |
//! | [`file_bench`] | 打开 1 MB 文件 < 150 ms | 1 MiB UTF-8，16,384 行 |
//! | [`store_bench`] | 1000 条批量写 < 200 ms | `hbot-store` 逐条 API（无批量 API，单事务口径另报参考值） |
//! | [`quickjs_bench`] | 回归基线（S1：mean 34.4 µs/事件） | 脚手架项目 + 1000 条入站事件 |
//! | [`memory_bench`] | 每 isolate < 8 MB（`PrivateMemorySize64`） | 8 个同时存活的 isolate，取私有提交增量 |
//!
//! 输出口径见 [`harness::budget_line`] / [`harness::budget_line_scalar`]：
//! `name | value | unit | budget | pass/fail | 口径说明`。
//! 一条命令全跑完（并打完机器/负载/Defender 快照）：
//!
//! ```text
//! cargo run -p hbot-bench --release
//! ```
//!
//! 或者走测试目标（同一份实现，`#[ignore]` 挡住日常 `cargo test`）：
//!
//! ```text
//! cargo test -p hbot-bench --release -- --ignored --nocapture
//! ```
//!
//! ## 退出码
//!
//! 默认 **闸门语义**：任何一条预算 FAIL 就返回 1，全部 PASS 才是 0，测量本身出错是 2。
//! 这一点必须保住 —— `cargo test` 那条路径永远 exit 0（它断言的是「量到了」而不是「达标」），
//! 如果二进制也默认绿，CI 就是假绿。只想看数字、不想让命令变红时加 `--report`（FAIL 也 exit 0），
//! 但**不要**把这个开关接到 CI 上。超预算的处置清单写在 `docs/spikes/S8-perf-budgets.md`。
//!
//! 为什么单开一个 crate：量测要同时看见 workspace / store / code 三个实现 crate，
//! 塞进任何一个的 `tests/` 都会逼它反向依赖别人（与 `hbot-parity` 同理）。
//!
//! 三条纪律（`docs/05-quality-gates.md` §4）：
//!
//! * 只有 release 数字作数 —— [`harness::require_release`] 在 debug 直接报错；
//! * 夹具与 SQLite 一律落在 `std::env::temp_dir()` 下带 pid 的唯一目录，用完即删；
//! * 不改预算值，也不为了让数字好看改统计口径 —— 口径写进每行的说明里。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod file_bench;
pub mod harness;
pub mod memory_bench;
pub mod quickjs_bench;
pub mod search_bench;
pub mod store_bench;
