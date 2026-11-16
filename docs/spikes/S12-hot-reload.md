# S12 热重载：改一行 `src/main.ts` → 新文案出现在 run log 要多久

- 日期：2026-11-17
- 环境：Intel i5-14600KF（14 核 / 20 逻辑核）/ 31.8 GB RAM / Windows 11 专业版 10.0.26300 /
  rustc 1.96.0（`cargo 1.96.0`）/ **debug 测试构建**（`cargo test -p hbot-rpc`，没有 `--release`）/
  `CARGO_TARGET_DIR=E:\hbot-target\hotreload`（E 盘；D 盘曾是事故点）/ NTFS（mtime 刻度 100 ns）
- 目的：用**产品真实路径**回答「改一行代码 → 多久生效」，并把「编译失败时继续服务上一版可跑代码」
  钉成回归测试。不做任何性能优化，也不给延迟设好看的上界。
- 方法（可复制，三条都是本次真跑过的验收命令）：
  ```powershell
  $env:CARGO_TARGET_DIR = "E:\hbot-target\hotreload"
  cargo test -p hbot-rpc --test hot_reload_e2e -- --nocapture     # 退出码 0
  cargo clippy -p hbot-rpc --all-targets -- -D warnings           # 退出码 0
  cargo fmt -p hbot-rpc -- --check                                # 退出码 0
  ```
  原始数据：`_recovery/hotreload-final-test.log`、`_recovery/hotreload-test*.log`、
  `_recovery/hotreload-final-clippy.log`、`_recovery/hotreload-final-fmt.log`（`_recovery/` 已 gitignore）。
  被测代码基线：`HEAD = 9a8cfca`。
- 结果：最终验收 run **中位 2008.7 ms / 最大 2014.8 ms / 最小 1972.5 ms（n=5）**；同类 4 次运行
  n=20 合起来 **中位 2015.0 ms / 最大 2017.4 ms**。编译失败窗口里 103 条消息全部由上一版回复
  （非空、非错误），编译错误行 1 条，修好后 1.950 s 恢复。三条验收命令退出码均为 0。
- 结论：**通过**。真机中位 **2008.7 ms**、最大 **2014.8 ms**（n=5，最终验收 run）；跨 4 次运行
  n=20 的中位 **2015.0 ms**、最大 **2017.4 ms**、最小 **1972.5 ms**。决定这个延迟的是
  **元数据指纹 TTL 2 s**（常量 `crates/hbot-rpc/src/app/runtime/fingerprint_cache.rs:47`，
  判据在 `:150` 的 `>= self.metadata_ttl`），其上再叠加 **300 ms 去抖**
  （`crates/hbot-rpc/src/app/runtime/sandbox_handler.rs:73`，等待在 `:378`）与一次编译（`:404`）。
  编译失败时**确实按上一版可跑代码服务**：坏代码窗口里 103 条消息全部由 v1 回复（非空、非
  `Response::Error`），编译错误行恰好 1 条，修好后 1.95 s 恢复成新版。
- 对设计的影响：用户感知的"保存后生效"预算是 **~2 s 地板**，不是 ~300 ms。想做到"保存即生效"，
  需要的是一条**保存事件驱动的失效通道**（写完文件直接把缓存标记 stale），而不是把 TTL 调小
  —— 调小 TTL 是拿"每条消息一次 `read_dir` + `stat`"换来的，本 spike 没有实现，见 §6。
  实现本身**一行未改**（`git status` 里只有新增的测试与本文件）。
- 未验证：多文件/大项目、release profile、5 s 内容地板的真实触发路径、Electron UI 感知延迟、
  连续快速保存的去抖合并、`onebot11`/QQ 平台、无 last-good 的首次编译失败、Windows 之外的平台。
  完整清单见 §8。

---

## 1. 被测链条与"一个延迟"的定义

全程走产品路径，**没有 fake 适配器、没有 mock、没有测试期 TTL 覆盖**：

```text
bot_inject (Command::BotInject)
  → Dispatcher → Supervisor → 真 SandboxHandler            （app/commands/bots.rs:144 装配）
  → FingerprintCache::current()                            （生产 TTL：2 s / 5 s）
  → 命中缓存？ → 是：直接跑上一版                            （fingerprint_cache.rs:186-191）
  → 否：去抖 300 ms → compile_project_debug（swc + QuickJS 编译）
  → isolate 换新 → 执行 onMessage → reply() → outbox → LocalAdapter::send
  → run log 表里出现 `[reply] <文案>`                       （IDE 日志面板读同一张表）
```

三个"测试替身"是**故意避开**的，否则量到的就不是产品延迟：

| 替身 | 本测的做法 |
|---|---|
| `SandboxHandler::with_fingerprint_ttls` / `with_debounce` | 不用（两者都是 `#[cfg(test)]`）。走 `bot_start` → `SandboxHandler::new`，拿到 `DEFAULT_METADATA_TTL` / `DEFAULT_CONTENT_FLOOR` / `HOT_RELOAD_DEBOUNCE` |
| 假 `Clock` | 不用假时钟（`AppState::open(.., Arc::new(SystemClock), ..)`）。注意指纹 TTL 用的是 `Instant` 而非注入的 `Clock`，所以假时钟本来也影响不到它 |
| 手写 handler 源码 | 项目由 `hbot_code::manifest::scaffold` 生成，之后只改 `src/main.ts` 里那一行 reply 文案 —— 就是用户按 Ctrl+S 的那个动作 |

**「一个延迟」的两个端点**（后面所有毫秒都指这一段）：

- 起点：`std::fs::write(src/main.ts, …)` **返回**的那一瞬（文件已关闭，等价于编辑器保存完成）；
- 终点：`store.list_logs` 里第一次读到以**新版本标记**开头的 `[reply] ` 行。

每个版本的文案带一个长度随版本增长的标记（`v1····`、`v2········`…），所以每次保存都同时改变
`(len, mtime_nanos)` —— 这两项正是 `MetaSignature` 的分量（`fingerprint_cache.rs:58`）。这样
"元数据层面能不能看出差别"这个前提在每次 trial 里都被 `assert_ne!(写前元数据, 写后元数据)` 钉住，
而不是默认成立。

回复靠**唯一注入文本**（`m0`、`m1`…）精确匹配：日志表按条数截断（`list_logs(limit)`），
靠"第几条回复"数数是不可靠的。

**为什么没有"延迟 ≤ 某值"的断言**：编译耗时随用户项目线性增长，没有任何上界。测试里只有一个
20 s 的**挂死守卫**（消息被丢了 / 卡死时带诊断失败，而不是挂到 CI 超时），它不对任何产品行为
做断言。真正被断言的是可推导的**下界**（见 §4）与"上一版继续服务"这一类事实。

## 2. 实测延迟

### 2.1 最终验收 run（`--nocapture`，默认线程数，n=5）

| trial | 写入 → 新文案 (ms) | 预热回程 (ms) | stale 注入条数 | 切换那条消息回程 (ms) | stale 回程中位 (ms) |
|---|---|---|---|---|---|
| 1 | 2012.2 | 324.6 | 104 | 332.0 | 15.3 |
| 2 | 1997.7 | 330.7 | 100 | 319.2 | 15.1 |
| 3 | 2008.7 | 329.0 | 76 | 344.4 | 16.4 |
| 4 | 1972.5 | 343.7 | 85 | 319.1 | 15.9 |
| 5 | 2014.8 | 331.2 | 96 | 333.0 | 16.6 |

**中位 2008.7 ms，最大 2014.8 ms，最小 1972.5 ms（n=5）。**

### 2.2 同一份测试的 4 次运行（共 n=20）

| run | 口径 | 中位 (ms) | 最大 (ms) | 最小 (ms) | 备注 |
|---|---|---|---|---|---|
| A | `--test-threads=1` | 2007.9 | 2017.0 | 2001.7 | 无回程打点 |
| B | 默认线程数 | 2016.5 | 2016.9 | 2015.2 | 无回程打点 |
| C | 默认线程数 | 2016.5 | 2017.4 | 2003.7 | 有回程打点 |
| D | 默认线程数（最终验收） | 2008.7 | 2014.8 | 1972.5 | 有回程打点 |

**20 个样本合起来：中位 2015.0 ms，最大 2017.4 ms，最小 1972.5 ms。** 离散度 < 2.3%
（最大 − 最小 = 44.9 ms），说明这个数字是被一个**常数门**卡住的，不是被编译抖动主导。

### 2.3 "保存后立刻发的那一条"（用例 3）

| 观察点 | 实测 |
|---|---|
| 写完成后 **14.9 ms** 注入第一条 | 仍然回 `v1····`，该条消息回程 **14.9 ms**（没有编译、没有去抖等待） |
| 之后持续注入 | 距写完成 **1991.9 ms** 才切到 `v2········` |

第一条的 14.9 ms 回程本身就是证据：它走的是 `current_at` 的缓存返回分支（`:186-191`），
**一个文件内容都没读**；如果它触发了 stat，回程会落进 300 ms+ 那一档。

## 3. 编译失败 → 继续服务上一版 → 修好恢复（用例 2）

坏文件：`src/main.ts` 少了 `},\n});` 两行收尾（表达式中间 EOF），写完后一直注入。

| 断言 | 实测结果 |
|---|---|
| 日志里出现编译错误行 | **2.008 s** 后出现（写完成 → 错误行落库），触发编译的那条消息回程 324.9 ms |
| 错误行原文 | `生成代码编译失败：HBOT_COMPILE: src/main.ts:5:37: Expected ',', got '<eof>'` |
| 编译错误行条数 | **恰好 1 条**（同一份坏指纹不反复编译 —— `fallback_on_failure` 的作用） |
| 坏代码窗口内的回复 | **103 条消息全部由 v1 回复**，每条都非空、都是 `Response::Ok`（没有 `-32002`、没有空回复） |
| `消息 … 处理失败` 行 | **0 条**（消息没有被丢在半路） |
| 坏代码不变时再注入 3 条 | 仍然 v1，编译错误行仍然是 1 条 |
| 修好后恢复 | 写入 → `v2········` 文案 **1.950 s**；恢复后连发一条稳定在 v2；编译错误行仍为 1 条（历史行不新增） |

也就是说：**编译失败不会把用户踢回"报错"或"没反应"，而是继续按上一版可跑代码服务**，
这正是需求里要钉的那条。

## 4. 归因：哪个常量在决定这 2 秒

### 4.1 归因模型与逐 trial 校验

一次生效的耗时可以拆成三段（符号见 §1）：

```text
latency = (TTL − Δ) + switch_round_trip
  Δ                  = 上一次 stat 的时刻 → 写入完成 的间隔（0 ≤ Δ ≤ 预热回程）
  TTL                = 2000 ms（metadata TTL）
  switch_round_trip  = 去抖 300 ms + 一次编译 + isolate 换新 + 派发（实测 319–344 ms）
```

用 `Δ = 预热回程`（保守取值）代入 §2.1 的五次 trial：

| trial | TTL − 预热回程 (ms) | + 切换回程 (ms) | 模型预测 (ms) | 实测 (ms) | 差 (ms) |
|---|---|---|---|---|---|
| 1 | 1675.4 | 332.0 | 2007.4 | 2012.2 | +4.8 |
| 2 | 1669.3 | 319.2 | 1988.5 | 1997.7 | +9.2 |
| 3 | 1671.0 | 344.4 | 2015.4 | 2008.7 | −6.7 |
| 4 | 1656.3 | 319.1 | 1975.4 | 1972.5 | −2.9 |
| 5 | 1668.8 | 333.0 | 2001.8 | 2014.8 | +13.0 |

五个 trial 的残差都在 **±13 ms** 以内（= 10 ms 轮询粒度 + 一次 `list_logs` 的开销），
且残差没有系统性偏向 —— 模型成立：**2 s 来自 TTL，300 ms + 编译来自切换那一条消息**。

反证：如果只有 TTL 而没有去抖，预测会落在 `2000 − 330 + 32 ≈ 1700 ms`，与实测 1972–2015 ms
差 300 ms；实测里那 300 ms 就实打实出现在"切换那条消息的回程"里（319–344 ms vs 缓存命中
15–17 ms，相差一个数量级）。

### 4.2 决定延迟的代码行

| 作用 | 位置 |
|---|---|
| **主门：常量 2 s** | `crates/hbot-rpc/src/app/runtime/fingerprint_cache.rs:47` `pub const DEFAULT_METADATA_TTL: Duration = Duration::from_secs(2);` |
| **主门：判据（只有 `now − signature_at >= TTL` 才重新 stat）** | `crates/hbot-rpc/src/app/runtime/fingerprint_cache.rs:150` `(Some(at), true) => now.saturating_duration_since(at) >= self.metadata_ttl,` |
| 主门：不刷新时的缓存返回（为什么 stale 回复只有 15 ms） | `crates/hbot-rpc/src/app/runtime/fingerprint_cache.rs:186-191`（`if !need_content { … return Ok(fingerprint) }`） |
| 附加：去抖 300 ms 常量 | `crates/hbot-rpc/src/app/runtime/sandbox_handler.rs:73` |
| 附加：去抖等待点 | `crates/hbot-rpc/src/app/runtime/sandbox_handler.rs:378`（`sleep_until(deadline)`） |
| 附加：真正编译 | `crates/hbot-rpc/src/app/runtime/sandbox_handler.rs:404`（`compile_project_debug`） |
| 缓存命中判定（在编译之前） | `crates/hbot-rpc/src/app/runtime/sandbox_handler.rs:365`（`hot.hit(&fingerprint)`） |
| 失败时取上一版 | `crates/hbot-rpc/src/app/runtime/sandbox_handler.rs:192-199`（`fallback_on_failure`）→ `:200`（`last_good`）→ `:443`（`mark_failed`） |

一条容易误判的常量：**5 s 内容地板（`fingerprint_cache.rs:52`）本次没有参与**。它只在
"元数据签名没变"（同 len + 同 `mtime_nanos`）时兜底（`:164-175`），而本测每次保存都同时改了
len 与 mtime（写前后 `assert_ne!` 每次都过）。所以 5 s 不是这次的延迟来源；它覆盖的是另一条
更隐蔽的路径（`mtime` 刻度内同长度改写），那条路径本 spike **没有实测**，只有单测覆盖（§8）。

## 5. 做了什么

只新增两个文件，**没有改任何实现**：

1. `crates/hbot-rpc/tests/hot_reload_e2e.rs`（新增，3 个用例，无 `#[ignore]`）
   - `hot_reload_latency_after_saving_main_ts` —— n=5 延迟采样 + 中位/最大 + TTL 下界断言
   - `compile_error_keeps_serving_last_good_then_recovers` —— 编译失败 → last-good → 恢复
   - `first_message_right_after_save_is_served_by_the_cached_version` —— 钉住"保存后 TTL 内
     第一条仍由缓存版本服务"（含前提检查：机器慢到前提不成立时带实测值失败，而不是静默放过）
2. `docs/spikes/S12-hot-reload.md`（本文件）

`git status` 里除这两个文件外没有其他改动；`crates/hbot-rpc/src/**`、`crates/hbot-code/**`、
任何 `Cargo.toml` 都未被触碰（`cargo fmt` 只跑了 `-p hbot-rpc -- --check`，且只对**本文件的**
违规用 `rustfmt` 单文件修过；`--check` 最终覆盖整个包也是 0）。

> 仓库现状备注（避免误会）：`HEAD = 9a8cfca` 里**已经**包含这个测试文件的一个中间快照
> （00:20:01 的提交把它一起带走了，不是我这边的 `git commit`）。本文件描述的是**工作区里的
> 最终版**（多了回程打点、`rustfmt` 之后的形状），相对 HEAD 是 `+42 −22`。

## 6. 考虑过但没做的优化

| 想过 | 为什么没做 |
|---|---|
| 把 `DEFAULT_METADATA_TTL` 从 2 s 降到 200–300 ms | 不在授权写域内（`src/**` 只读）。且这是**取舍**不是免费午餐：TTL 就是"每条消息做一次 `read_dir` + `stat` 的间隔"，降到 200 ms 等于把每条消息的元数据扫描频率提高 10 倍 |
| 加一个 `HBOT_*` 环境变量覆盖 TTL | 产品里没有这个开关（`with_fingerprint_ttls` 是 `#[cfg(test)]`），加它等于改实现 |
| "保存即生效"：写完文件主动把缓存标记 stale | 这才是正确的产品解法（文件监听 → `mark_stale()`），但它要改 `fingerprint_cache.rs` / 保存链路，属于实现改动，本 spike 只负责量出"现在是多少" |
| 把第一条消息强制 refresh | 同上：等于把每条消息都变成代价最高那条路径 |
| 用 release 构建量一遍 | 会更快（编译/isolate 启动更快），但**不会改变 2 s 门**（它由 `Instant` 与常数决定），而任务要的是产品默认口径 |

## 7. 风险与漏检边界

1. **单机单项目**。项目是 scaffold 出来的最小项目（一个 `src/main.ts`）。编译耗时随文件数增长，
   所以 2 s 是**地板**，不是天花板；大项目上真实数字只会更大（叠加在 TTL 之上）。
2. **debug 构建**。`cargo test` 是 dev profile；release 下 `compile_project_debug` 与 isolate
   启动会更快，但**门（2 s TTL）不动**。§2 的数字是 dev profile 口径。
3. **5 s 内容地板的盲区没实测**（见 §4.2）。要实测需要构造"同长度 + 同 `mtime_nanos`"的改写，
   本测每次都故意改长度。
4. **没测 UI 层**。本测读的是 run log 表（`list_logs`），面板/桌面端只是读同一张表 + 事件推送；
   面板刷新节奏、日志面板自动滚动等**未验证**。
5. **没测并发保存**。去抖窗口 300 ms 的合并行为只有 `sandbox_handler.rs` 的单测
   （`debounce_coalesces_concurrent_compiles`）覆盖；本测是"保存 → 等生效 → 再保存"的串行节奏。
6. **没测非 `local` 平台**。`onebot11` / QQ 官方那条链路上还会多一次网络往返，本 spike 一个字节
   都没碰。
7. **`stale 注入条数`（76–104）不是产品指标**，它只是"我在 2 s 里发了多少条"的倒影
   （每条 ~16 ms 回程）。别拿它当"用户会看到 100 条旧回复"——真实用户不会 2 s 内发 100 条。
8. **时钟**：延迟用 `Instant` 单调时钟测，不受系统时间调整影响；但跨进程不可比（本测都在同进程内）。
9. **用例 3 有一条显式前提**：它要求「预热回程 + 保存后立即注入」< 2 s（实测 ≈ 345 ms，
   余量约 6×）。如果机器被别的构建压满导致这条前提不成立，该用例会**带着实测值失败**而不是
   静默放过 —— 那不是产品回归，是这台机器当时测不了这条结论。其余两条用例的下界断言在慢机器上
   会退化成恒真（下界随预热回程一起降到 0），不会给出假的"超标"。`--nocapture` 的日志里
   每条 trial 都打出了 `预热回程` / `切换回程`，出现红的时候先看这两个数。

## 8. 未验证（明确清单）

- 多文件 / 大项目下的生效延迟（只有最小 scaffold 项目）
- release profile 下的数字（只有 dev profile）
- 5 s 内容地板的真实触发路径（同 len + 同 mtime 刻度的改写）
- Electron/桌面 UI 里的感知延迟（日志面板刷新、事件推送时序）
- 连续快速保存（< 300 ms 内多次 Ctrl+S）在**产品路径**上的合并行为
- `onebot11` / QQ 平台上的同类延迟
- bot **未启动**或**首次编译失败（无 last-good）**时的行为（后者只有 `sandbox_handler.rs` 单测
  `first_compile_failure_without_last_good_returns_error` 覆盖，本文件没重复测）
- Windows 之外的平台（Linux/macOS 的 mtime 粒度与 fs 通知行为不同）