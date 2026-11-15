# S1 QuickJS 隔离

- 日期：2026-11-16
- 环境：Intel i5-14600KF（14 核）/ 31.8 GB RAM / Windows 10.0.26300 / `rustc 1.96.0 (ac68faa20 2026-05-25)` / `cargo build --release`（`opt-level=3`）/ rquickjs 0.14（features = `futures` + `loader`，**不用** `parallel` / `allocator` / `rust-alloc`，即 QuickJS 自带分配器）/ `CARGO_TARGET_DIR=D:\hbot-target\codefix`
- 目的：用实测数字判定「QuickJS 能不能当 bot 脚本沙箱」——多文件 TS 项目、异步宿主 API、内存/时间限额、每 bot 一个 isolate 的成本；并记录本轮修掉的「spawn 永久挂死」根因。
- 方法：`crates/hbot-code/examples/s1_bench.rs`（编译 / spawn / 派发 / 堆）+ `scripts/mem-probe.ps1` 从进程外采 RSS（Windows 进程内 RSS 按设计返回 0）。

## 方法（可复制的命令）

```powershell
$env:CARGO_TARGET_DIR='D:\hbot-target\codefix'
cd E:\HBot-Next

# 编译冷/热、isolate spawn、1000 事件延迟、isolate 堆
cargo build --release -p hbot-code --example s1_bench
cargo run -q -p hbot-code --release --example s1_bench *> D:\hbot-target\s1_bench2.log

# 进程 RSS（进程内 RSS 在 Windows 上恒为 0，必须从外面采）
pwsh -File scripts/mem-probe.ps1 -Name s1_bench -Seconds 3 -Json     # 8 个 isolate 存活期间
pwsh -File scripts/mem-probe.ps1 -Name s1_bench -Seconds 3 -Json     # 全部 drop 之后的基线
```

`scripts/mem-probe.ps1` 采的是 `WorkingSet64`（任务管理器「内存（工作集）」）与 `PrivateMemorySize64`（已提交私有内存），每 200 ms 一次、3 秒内 14–15 个采样。bench 在 8 个 isolate 全都活着时停住 `HBOT_S1_HOLD_MS`（默认 6000 ms）供外部采样，drop 后另停 `HBOT_S1_BASELINE_HOLD_MS`（默认 4000 ms）采基线。

**采样条件说明**：三次运行期间同一工作区还有其它 crate 在并发编译（多个 agent 同时作业），CPU 不是独占，所以下表给 **中位数 + (最小–最大)** 范围；堆数字是确定性的，三次一致。

## 结果

项目形态：`src/main.ts` 用 `import { defineBot, reply } from 'hbot'`，入口按 `docs/02-architecture.md` §6.1 写；`files` 列指「入口 + `files-1` 个 `./libN` 模块」，入口里对每个 lib 都做一次真实调用（`f{index}(ctx.text)`），所以链接和加载都真的发生。

| 项目规模 | 编译 冷 | 编译 热（内容哈希命中） | isolate spawn（含预加载） | 1000 事件 mean | p50 | worst | isolate 堆 used / malloc |
|---|---|---|---|---|---|---|---|
| 1 文件 | 3.10 ms (2.74–11.81) | 299 µs (287–609 µs) | 1.97 ms (1.08–8.89) | 34.4 µs (32.1–59.3) | 19.7 µs | 2.74 ms (1.99–8.79) | 100 KiB / 112 KiB |
| 10 文件 | 18.30 ms (2.78–42.40) | 1.43 ms (792 µs–2.77 ms) | 2.34 ms (1.32–3.33) | 46.1 µs (22.8–51.1) | 23.4 µs | 413 µs (363–2720) | 112 KiB / 127 KiB |
| 50 文件 | 40.88 ms (5.37–54.65) | 4.43 ms (3.32–4.86) | 2.33 ms (1.50–2.97) | 72.1 µs (62.3–95.8) | 56.8 µs | 1.18 ms (1.04–6.07) | 168 KiB / 196 KiB |

| 每 bot 成本 | 值 |
|---|---|
| 8 个 isolate 顺序 spawn 总计 | 9.25 ms (6.94–9.84) → 约 1.2 ms/个 |
| 进程工作集：0 个 isolate（基线） | 7.3 MB |
| 进程工作集：8 个 isolate 存活 | 9.2 MB → **+1.9 MB / 8 个 ≈ 240 KiB/isolate** |
| 进程私有内存：0 → 8 个 isolate | 1.1 MB → 3.2 MB（+2.1 MB ≈ 260 KiB/isolate） |
| 8 个 isolate 全部 drop 后 | 进程内 RSS 计数回 0（Windows 按设计恒为 0）；drop 耗时未单独记录，但 `Drop` 现在最多等 `handler_timeout + 1s` |

限额：`Limits::default()` = 32 MiB 堆 / 512 KiB 栈 / 2 s 单次处理；50 文件项目的堆只用到 168 KiB，离 32 MiB 上限约 195 倍余量。`s1_probe` 端到端实测：`replies: ["echo: hi"]`，`memory used=100 KiB`（一次真实 handler 调用，不是「报告成功但没跑」）。

## 本轮修掉的挂死（S1 阻断项）

**根因（实测，不是推测）**：`Sandbox::spawn` 挂死与 ES module loader、解析器、tokio/rquickjs 的 job pump 全都无关。`serve()` 返回 `Err` 后，`worker()` 进入「把队列里剩下的命令一律拒绝，直到收到 Shutdown」的循环，而 `reject()` 对 `Command::Shutdown` 只做了空操作、**循环没有 break**；另一头 `impl Drop for Sandbox` 是 `send(Command::Shutdown)` + `worker.join()` —— 发送端就在 `drop` 自己手里，通道永远不会 disconnected，Shutdown 被当空操作吞掉后 worker 回到 `recv` 阻塞，`join()` 永不返回。于是**任何 bootstrap 失败的项目都会把宿主永久挂住**（CPU 空闲、无输出）；`spawn` 里的 `recv_timeout(backstop)` 也救不了，因为它的超时错误路径正是 `drop(Sandbox)`。

**必须改的行**：`crates/hbot-code/src/runtime/isolate.rs:318`（drain 循环改成 `Command::Shutdown => break`）、`crates/hbot-code/src/runtime/isolate.rs:325`（线程结束前 `finished.send(())`）、`crates/hbot-code/src/runtime/isolate.rs:235`（`Drop` 改为有界等待：`shutdown_budget()` = `handler_timeout + 1s`，超时 `tracing::error!` 后 detach 线程，不再无条件 join）。

**为什么之前的 fixtures 必挂、11 个测试却全过**：`host_api::install` 只往 `globalThis` 装 `__hbot`，并没有全局 `defineBot` / `reply`；`examples/s1_probe.rs`、`examples/s1_bench.rs`、`tests/repro_bench.rs` 用的是裸 `defineBot`/`reply`，入口模块必然抛 `ReferenceError`，必然走失败路径，必然踩上面那个死锁。`tests/s1_quickjs.rs` 的 11 个测试都 `import { defineBot } from 'hbot'`，bootstrap 成功，所以从没踩到。

顺带更正一条错误记录：旧注释说「加载失败时 rquickjs 只设异常、import promise 一直 pending」（原 `crates/hbot-code/tests/s1_quickjs.rs:324`）。实测**不成立**：被拒绝的 import 会把 promise 以异常 reject，`await` 直接返回 `Err`。已在 `crates/hbot-code/tests/s1_quickjs.rs:324` 更正，失败消息现在经 `describe()` 变成带真实 JS message + TS 行号的 `HBOT_JS_EXCEPTION`。

## 结论

**有条件通过。** 性能与内存都有很大余量：单次派发中位 20–57 µs、每 bot 约 240 KiB、8 个 isolate spawn 9 ms；真正的风险不在性能，而在**错误路径的生命周期**——本轮挂死就是失败项目 + `Drop` 无条件 join 的组合。现在的边界是：bootstrap 失败会在 1 秒内以真实 JS 错误返回；无法停止的 worker（`await new Promise(() => {})` 这类永不 settle 的 handler）会在 `handler_timeout + 1s` 后被 detach 并记 `tracing::error!`，宿主不会冻结。

## 对设计的影响

- 入口形态保持 `docs/02-architecture.md` §6.1 不变（`import { defineBot } from 'hbot'` + `export default defineBot({...})`）。**不**为了迁就旧 examples 去加 `defineBot`/`reply` 全局——那会变成第二份真源；改的是 examples 和 fixture。
- `Sandbox` 的公开 API（`spawn` / `dispatch` / `memory` / `limits`）未变，只把「drop 会阻塞到线程结束」改成有界等待（`Drop for Sandbox` 文档已同步）。
- 新增 `crates/hbot-code/tests/s1_isolate_lifecycle.rs`（4 个回归测试：文档形态必需能跑通且 `ctx.text` 可见、裸全局要快速报错、加载期死循环要快速超时、wedged isolate 的 drop 必须快速返回），`tests/repro_bench.rs` 已删除（其内容被第一个测试覆盖）。

## 未验证

- 真实 agent 写的项目：>50 文件、多级目录、循环 import、`export *` 再导出。resolver 支持这些形态（`crates/hbot-code/src/modules.rs` 有单测），但没有用大项目实测编译耗时与加载。
- 8 个 isolate **同时高频** dispatch 的 p99（本轮只测单 isolate 串行 1000 事件 + 8 个 isolate 空转存活）。
- 长时间运行（数小时 soak）下的堆稳定性与 detach 线程的累积。detach 只在「handler 永不 settle」这种极端情况下发生，本轮只验证了它不会挂住宿主，没验证长期累积。
- QuickJS 宿主函数的完整语义面（本轮只覆盖 `reply` / `log` / `sleep`；`ctx` 上没有别的宿主能力面）。
- Windows 上单 isolate 的精确 RSS：用的是「基线 vs 8 个存活」的差值法（约 240 KiB），不是单进程单 isolate 直采。