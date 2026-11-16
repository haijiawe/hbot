# hbot-bench — IDE 性能预算手测台

把 `docs/05-quality-gates.md` §4 里的**性能预算真实量出来**：只测量、只打印，
不改任何实现 crate 的源码，也不改预算值（预算常量写在代码里，与文档一致）。

数字的落地记录见 [`docs/spikes/S8-perf-budgets.md`](../../docs/spikes/S8-perf-budgets.md)。

## 怎么跑

```bash
# 按 AGENTS.md 坑 29/45：每个 agent 一个独立 target dir，且放 E 盘（D 盘会被构建缓存吃干净）
$env:CARGO_TARGET_DIR = "E:\hbot-target\bench2"
cd E:\HBot-Next

cargo build -p hbot-bench --release                       # 预热，别把编译算进第一次测量
cargo run -q -p hbot-bench --release *> bench.log         # 量测（Windows 上别用管道，见坑 45 上下文）
```

长命令一律重定向到文件再读：PowerShell 管道被写满时会假死。

退出码（**这条会决定 CI 是不是假绿**）：

| 命令 | 有 FAIL 时退出码 | 用途 |
|---|---|---|
| `cargo run -p hbot-bench --release` | **1** | 闸门：CI/`check.sh` 用这个 |
| `cargo run -p hbot-bench --release -- --report` | 0 | 只报数不定罪（写 spike/看趋势时用） |
| `cargo test -p hbot-bench --release -- --ignored --nocapture` | **永远是 0** | 量测台自检：断言的是「量到了」，不是「达标」 |

⚠️ `cargo test -p hbot-bench --release`（**不带** `--ignored`）会跑 **0 个测试**：5 个量测函数都标了
`#[ignore]`（它们每个都要写几万个临时文件、跑满 1000 条事件，不适合进常规 `cargo test`）。
不带 `--ignored` 时它 exit 0 且什么都不打印 —— 别把它当验收。

## 五组夹具（数据怎么造的）

| 组 | 规模 | 预算（`docs/05-quality-gates.md` §4） | 判定行 |
|---|---|---|---|
| 搜索 10k | 10 `pkg*` × 10 `mod*` × 100 文件 = **10,000 文件 / 100 目录**；单文件 **200 B–4 KiB**（实测合计约 20.5 MB）；命中词 `HBOT_NEEDLE_9F3A` 埋在 **100 个文件（1.0%）**；`hbot_workspace::search` + 默认 `SearchOptions` | 全局搜索首屏 < 1 s | `search_10k_cold_first_call` / `search_10k_warm` / `search_10k_screen_100hits` / `search_10k_no_hit` |
| 1 MB 文件 | 恰好 **1,048,576 B / 16,384 行 × 64 B** UTF-8 文本；`read_file` / `write_file(expected_hash)` | 打开 1 MB 文件 < 150 ms（冷/热各一次） | `file_1mb_read_cold` / `file_1mb_read_warm` / `file_1mb_write` |
| 1000 条写库 | 临时 SQLite；先建 project→bot→conversation；再写 **1000 条 `enqueue_outbound`** | 1000 条 inbound **单事务** < 200 ms | `store_1000_writes_individual`（现行 API = 逐条事务） |
| QuickJS | `manifest::scaffold` 的 6 文件项目 → 编译 → spawn 1 个 isolate → dispatch **1000 条**入站事件 | 回归线 = S1 的 34.4 µs × 2 = 68.8 µs/事件 | `quickjs_1000_events_mean` |
| 内存 | 同一份 `CompiledProject` 克隆出 **8 个 isolate**，前后各采 5 次 `PrivateMemorySize64` 取中位 | 每 isolate < 8 MB（堆上限 32 MB） | `memory_per_isolate` |

夹具全部建在 `std::env::temp_dir()/hbot-bench-<label>-<pid>-<nanos>/`，`TempDir::drop` 负责删；
**不写进仓库**（`docs/05-quality-gates.md` §3 要求临时目录必须清理）。

## 读数字时要注意

* **只有 release 作数**：`require_release()` 在 debug 构建里直接 panic。
* 每条预算复跑 3 次取**中位数**，并打印 `*.spread`（单次最小–最大）。`docs/spikes/S8` 的表是
  **3 次进程运行**的 min / median / max，每个进程内部再取中位。
* **内存口径是 `PrivateMemorySize64`，不是 `WorkingSet64`**（AGENTS.md 坑 34、ADR-0002 §4：
  WebView2 的 DLL 被多个子进程各自映射，WS 会把共享页重复计入）。isolate 是**线程**，
  所以本进程就是「整棵树」，采样只查自己 PID；**不能用** `scripts/mem-probe.ps1 -IncludeChildren`
  —— 那个脚本会把自己（PowerShell 约 70 MB）算进被测树，对 8 MB 的判定是致命的。
* 搜索/文件两项的「冷」= **本进程第一次调用**（文件刚由夹具写出，仍在 Windows 文件缓存里），
  不是冷盘；真冷盘要管理员清 standby list 或重启，未做。
* **搜索必须同时看「裸遍历读盘」参照**（`search_10k_walk_metadata` / `search_10k_walk_read`）：
  它回答「慢的是搜索算法还是每文件 IO」。纯匹配在内存里只要 1–6 ms，剩下的全是每文件开销。
* 带 `参考（不判定）` 的行是归因参照物（裸 `fs::read`、rusqlite 单事务、内存里纯 `str::contains`），
  **不参与 PASS/FAIL**。尤其 `store_1000_writes_single_tx` 是**用 rusqlite 直接写的参照**，
  不是 `hbot_store` 的 API（store 没有批量 API，见 `docs/04-implementation-plan.md`）。
* 头部 `env_*` 行记录本次运行的机器与负载（`load_percent` / `concurrent_builds` / Defender 开关）。
  **10k 搜索对负载极敏感**：同一台机器实测 994 ms（5 个并发编译）vs 2200 ms（15 个），
  读数字前先看这两行。

## 与实现 crate 的关系

只读依赖 `hbot-workspace` / `hbot-store` / `hbot-code`（+ `rusqlite` 用于「单事务」参照），
均在 `xtask/src/deps.rs` 的 `hbot-bench` 允许清单内。本 crate 不改任何实现 crate 的源码；
改动目录仅 `crates/hbot-bench/**`。