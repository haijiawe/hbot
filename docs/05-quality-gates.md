# 05 质量闸门与性能预算

- 状态：Draft（与 [ADR-0002](adr/0002-agent-in-rust.md) 对齐）
- 日期：2026-11-16
- 原则：**一条命令能跑完；闸门是棘轮，只准变严；数字只有一个出处。**

---

## 1. 一条命令

```bash
./scripts/check.sh          # 本地与 CI 同一套（Rust + UI 静态检查 + 单测）
./scripts/check.sh --full   # 再加集成、E2E、打包 smoke
```

`scripts/check.sh` 内部顺序（失败即停）：

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo deny check
cargo xtask deps                 # 依赖方向
cargo xtask gen --check          # 生成物是否最新
cargo nextest run --workspace    # 单元 + crate 内集成
pnpm -C apps/web check           # tsc --noEmit + biome
pnpm -C apps/web test            # vitest
pnpm -C apps/web build           # 产物能构建
```

- 目标：`check.sh` < 90s（热缓存），干净机器 < 5 min。
- 不在本地跑全量性能；性能是 nightly。
- 禁止“我本地跑了 typecheck 就算过”。

---

## 2. 闸门清单

| # | 闸门 | 判据 | 红了怎么办 |
|---|---|---|---|
| G1 | `cargo fmt` | 全 workspace 格式 | `cargo fmt --all` |
| G2 | `clippy -D warnings` | 无警告；核心 crate 开 `unwrap_used`/`expect_used` 为 deny（测试与 `main` 启动例外） | 修代码，不加 `#[allow]` 除非写明理由 |
| G3 | `cargo deny` | 许可证白名单、禁止重复版本、禁止 git 依赖、`cargo audit` 无未处理漏洞 | 换依赖或登记豁免（写到期日） |
| G4 | 依赖方向 | `hbot-core` 不依赖 workspace；agent 不依赖 bot；workspace 不依赖 agent/bot/rpc；adapters 只依赖 core；rpc 只做编排；web 不 import 实现 crate；禁止循环 | 改 import 或加端口 |
| G5 | 生成物 | `xtask gen --check` 无 diff | 跑 `cargo xtask gen` 并提交 |
| G6 | 迁移 | 空库可跑到最新；已有库可增量；`user_version` 单调 | 改迁移；**不改已发布迁移** |
| G7 | 密钥扫描 | gitleaks 无命中 | 移除并轮换；历史提交也要处理 |
| G8 | UI 类型/格式 | `tsc --noEmit` + Biome 无错误 | 修；不用 `any`/`@ts-ignore` 逃 |
| G9 | UI 测试 | vitest 全绿；关键组件有测试 | 补测试，不删断言 |
| G10 | 结构 | 文件软 400/硬 800；函数软 60/硬 120；`todo!`/`unimplemented!` 不进库代码 | 拆；或写分类与理由 |
| G11 | 安全边界 | 每个 `unsafe` 有 `// SAFETY:`；命令/路径/网络/密钥各有拒绝路径测试 | 补测试；不能只靠 review |
| G12 | 无 Node 运行时 | `crates/` 不调用 `node`/`npm`/`pnpm`；`rg -n "Command::new\\(\"node\"|node_modules" crates/` = 0；核心测试在无 Node 镜像通过 | 移除；不要“先临时用一下” |
| G13 | 文档 | 命令可复制执行；内部链接存在；数字引用单一出处；文档有状态与日期 | 修文档；过期就改或删 |
| G14 | 打包 smoke | 三平台产物能装、能启动、`AppInfo` 正常 | 阻断发布，不阻断日常 PR |
| G15 | 性能 | 预算内；回归 >10% 阻断 | 定位；不许改基准 fixture 来“变绿” |
| G16 | IDE 交互 E2E | Playwright：建项目 → 编辑保存 → Agent Diff 接受/拒绝 → 全局搜索 → 问题跳转 → 运行 mock → 日志跳源码 → 断点/步进 | 补/修；不允许“只测后端” |
| G17 | IDE 性能 | 按键 p95 <16ms、1MB 文件 <150ms、10k 文件搜索首屏 <1s、20 标签 UI <300MB | 优化或按 06 §9 降级 |

`G10` 的“硬”是 review 阻断线，不是脚本自动失败线；脚本只报警，人决定分类（数据表/装配/单事务/真债）。

---

## 3. 测试分层

| 层 | 位置 | 内容 | 何时跑 |
|---|---|---|---|
| 单元 | crate 内 | 纯函数、解析、队列、审批键 | 每次 |
| 契约 | `hbot-rpc` / `hbot-code` | DTO 往返、`hbot.d.ts` 与宿主 API 一致 | 每次 |
| 集成 | `tests/integration` | 真 SQLite（临时目录）、mock 平台、起停 bot | 每次 |
| E2E | `tests/e2e` | Playwright：建项目 → 编辑/保存 → Agent Diff → 搜索 → 运行 → 日志 → 调试 | PR / nightly |
| 性能 | `tests/perf` | RSS、启动、调度延迟、沙箱开销 | nightly |
| parity | `tests/parity` | 旧项目行为场景（见 04 §4） | M2 起每次 |
| 红队 | `tests/security` | 路径逃逸、命令注入、提示注入、资源耗尽 | 每次（快用例） |

规则：

- bug 先写失败测试；修完保留。
- 测试不连真实平台；平台用 mock/fixture。
- 时间、随机、UUID 注入；不用 `sleep` 等异步（用条件变量/超时）。
- 不长期 `#[ignore]` / `skip`；临时跳过必须带 issue 与到期日。
- 测试产生的临时目录必须清理；不写用户目录。

---

## 4. 性能预算

| 指标 | 预算 | 测量 |
|---|---|---|
| 桌面整机（private commit 求和） | < 260 MB | `scripts/mem-probe.ps1 -IncludeChildren -SplitChildren`，静置 10s 采 30s 取中位。**不要用 WorkingSet**：WebView2 共享页会被 6–7 个子进程重复计入（实测 WS 382 MB vs private 206 MB，见 ADR-0002 §4） |
| 核心空闲（private commit） | < 60 MB | `hbot serve` 无 bot、无会话 |
| 每多一个 bot | < 6 MB | 加 5 个 bot 的斜率（实测 0.10 MB/bot） |
| 每 isolate | < 8 MB（堆上限 32 MB） | 加载后 + 跑 1000 条消息（实测 0.30 MB） |
| 冷启动到核心可用（headless） | < 800 ms | `hbot serve` 打印 `{"type":"ready"}` 的时刻（实测 **36.4 ms 中位 / 50 ms p95**，n=7，已达标；见 `docs/spikes/S11-headless-startup.md`）。桌面形态另算，见 ADR-0002 §4.3 |
| 桌面到 IPC 首次可用 | < 1500 ms | `scripts/startup-probe.ps1`：进程启动 → web 第一次 `dispatch`（实测 1049 ms 中位 / 1425 p95）。**不要用桌面「核心可用」套 800 ms**：其中 915 ms 在 Tauri/WebView2 进入 `setup` 之前，核心只占 33 ms |
| UI 首屏 | < 1 s | 本地构建，中端机 |
| 初始 JS（gzip） | < 1.5 MB | 构建报告 |
| 入站 → 入队 p95 | < 10 ms | 注入 10k 条 |
| handler p95（本地无网络） | < 20 ms | 固定脚本 |
| 本地回复 p95 | < 50 ms | 端到端（mock 平台） |
| 1000 条突发不丢 | 是 | 背压测试，校验总数 |
| 100 会话并行 | 顺序正确、无饥饿 | 每会话 100 条 |
| 按键延迟 p95 | < 16 ms | 编辑器输入基准 |
| 打开 1MB 文件 | < 150 ms | 冷/热各一次 |
| 全局搜索首屏 | < 1 s（10,000 文件） | 固定夹具；**必须独占机器**（同盘 IO 或满核 CPU 争用会把同一份代码从 126 ms 推到 1017–1258 ms，实测见 `S8` 批次 H）。CI 的性能任务串行跑，不与构建并发 |
| 20 标签 UI 内存 | < 300 MB | UI 进程采样 |
| 调试插桩开销 | debug ≤ 5× release | 同一 fixture 对比 |
| 二进制 | 待 S1 实测，上限 60 MB | release 构建 |
| 安装包 | 待 S1 实测，上限 70 MB | 三平台产物 |
| `cargo check` 增量 | < 30 s | 改一个 crate |
| 全量 `check.sh` | < 5 min（冷） | CI 干净机 |

**测量纪律**：

- 固定机器/固定 fixture/固定数据量；记录 CPU、内存、OS、构建 profile。
- 用 `hyperfine` 做命令级；RSS 用平台 API 采样（Windows `GetProcessMemoryInfo`，Linux `/proc/<pid>/status`）；分配用 `dhat`。
- 一次只改一个变量；报告写“改前/改后/差值”，不写“感觉快了”。
- 回归 >10% 视为阻断；解释不了就先回滚。

---

## 5. CI

**与实际 `.github/workflows/ci.yml` 对齐（2026-11-16 核对）**。下面这张表替换了原先那份"计划中"的 job 清单（`check`/`test-windows`/`e2e`/`perf`/`audit` 五个 job 从未存在过，是设计稿遗留，留着会让人以为 e2e 已经在跑）。

| Job | 平台 | 内容 |
|---|---|---|
| `rust` | ubuntu-latest + windows-latest | `cargo fmt --check`（仅 ubuntu）/ `cargo clippy --workspace --all-targets -D warnings` / `cargo test --workspace --all-targets` |
| `web` | ubuntu-latest | `pnpm install --frozen-lockfile && pnpm run check`（`apps/web/package.json` 不存在时跳过） |
| `gates` | ubuntu-latest | `cargo run -q -p xtask -- deps`（依赖方向）+ 禁止 `crates/**` 出现 `Command::new("node")`/`npx`/`npm` |
| `bundle-windows` | windows-latest | 真出 NSIS + `scripts/smoke-nsis-install.ps1 -SkipWindowCheck` + 体积/哈希 |
| `bundle-macos` | macos-14 | 真出 dmg（未签名未公证）+ 体积/哈希 |
| `bundle-linux` | ubuntu-latest | **只**做 web build + Tauri 配置闸门；**不**出 deb/appimage（见下） |

**`rust` job 必须装 Linux 系统依赖**：Tauri 在 Linux 上**编译期**就要 `webkit2gtk-4.1`/`gtk3`/`soup3` 的 dev 包（build script 走 pkg-config），而 `--workspace` 会把 `crates/hbot-desktop` 一起编进来。缺这一步时 ubuntu job 在 `webkit2gtk-sys` 处红 —— 这是首次 push 就会踩到的，不是偶发。已装：`libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libsoup-3.0-dev patchelf`。

**`bundle-linux` 为什么不出 deb/appimage**：这条路径从未在 Linux 宿主上实测过；与其在 CI 里塞一条没验证过的打包步骤（红了不知道是代码问题还是打包配置问题），不如先留在 `docs/spikes/S5-packaging.md` §9 的「未验证」清单里。现在系统依赖清单已经明确（同上），要开只需在 `bundle-linux` 加同一段 apt + `cargo tauri build --bundles deb,appimage`。

### 5.1 当前实际验证状态

**2026-11-21 00:25 曾停摆一次（GitHub Actions 计费），现已恢复**：run `44760774459`（head `d48f9f8`）与 run `44771911890` 的第 1 次尝试，七个 job **全部在 2 秒内失败、没有 runner、没有日志**（下载日志得 `BlobNotFound`）。job 的 annotation 原文是：

> The job was not started because recent account payments have failed or your spending limit needs to be increased. Please check the 'Billing & plans' section in your settings

也就是 private 仓库的 Actions 计费问题，**不是代码问题**。处理方式：把仓库改成 public（public 仓库的 Actions 不计费），之后同一 run 的第 2 次尝试（head `00305a5`）七个 job 里六个 success、`bundle (macos, dmg)` 仍 `cancelled`。判读这类失败的快捷方式是 `GET /repos/<owner>/<repo>/check-runs/<job_id>/annotations` —— 它连日志都没有，翻日志只会得到 `BlobNotFound`。

**历史上 CI 已经真跑并且连续七次全绿**（截至 head `b45d1f3` 的 run `44774206452`；remote = `https://github.com/haijiawe/hbot.git`，2026-11-21 起是 public）：run `44730056833`（head `b9dc9ba`，首次全绿）、run `44735231807`（head `d0662e4`）、run `44738414777`（head `9648800`）、run `44747851031`（head `20caba5`，每 bot 磁盘配额那次）、run `44756454957`（head `9a035b8`，`onStop(crash)` 触发点那次）与 run `44771911890` 的第 2 次尝试（head `00305a5`，附件出站那次）的六个真实 job 全 success（`bundle (macos, dmg)` 仍 `cancelled`，矩阵里没有 macOS runner，属正常）。下表是"本地能验的"与"只能靠 CI/真机验的"的分界：

| 检查 | 状态 | 证据 |
|---|---|---|
| Windows 全量 `fmt`/`clippy`/`test --workspace` | ✅ 本地实跑通过 | `126 suites / 2038 passed / 0 failed / 14 ignored`（`rustc 1.100.0`） |
| `apps/web` 的 `check`/`test` | ✅ 本地实跑通过 | `47 文件 / 427 用例` |
| `xtask deps` / `gen --check` | ✅ 本地实跑通过 | `0 violation(s)`、生成物一致 |
| CI `gates` / `web` / `bundle (ubuntu)` / `bundle (windows, nsis)` | ✅ runner 上通过 | run `44730056833` / `44735231807` / `44738414777` / `44747851031` / `44756454957` / `44771911890`(attempt 2) 六次的四个 job 都 success |
| CI `rust` 的 `fmt` / `clippy` 两平台 | ✅ runner 上通过 | run `44730056833`：`rust (ubuntu-latest)` 的 `fmt` 与 `clippy`、`rust (windows-latest)` 的 `clippy` 都是 success。此前红过的两处（`clippy::result_large_err`、Linux-only `unused import`）已修，见 `docs/pitfalls.md` 坑 115 |
| CI `rust` 的 `test` 步 | ✅ **两平台全绿且复验通过**（run `44730056833`，head `b9dc9ba`：`rust (ubuntu-latest)` 的 job id `136267205342`，与 `rust (windows-latest)` 的 `test` 都是 success；run `44735231807`，head `d0662e4`、run `44738414777`，head `9648800` 与 run `44756454957`，head `9a035b8` 再次两平台全绿）。这一路共抓出 10 处红点 | ① ubuntu：runner 磁盘耗尽（`signal 7 [Bus error]` + `Free space left: 19 MB`），已加腾磁盘步 + `CARGO_PROFILE_DEV_DEBUG=0`（坑 118）；② windows：设置 watcher 的 mtime 盲点（坑 117），判据已改成比文件字节；③ ubuntu：Linux 的 `RLIMIT_CPU` 软硬上限相等 → `SIGXCPU` 永不出现（坑 119，已确认 `cpu_limit_terminates_a_busy_loop ... ok`）；④ run `44697883029`（head `3e61746`）抓到两条**测试自身的平台/时序假设**：`hbot-device/tests/api.rs` 把 Windows 的能力当跨平台契约（坑 120）、`hbot-bot/tests/runtime.rs` 只等「执行完」不等「落库完」（坑 121）；⑤ run `44700956651`（head `75d73cc`，此时 windows 已 success）抓到 `hbot-redteam/tests/a_path_escape.rs` 的四条 Windows 语义用例没按平台门控（坑 122），改后本地 20 passed；⑥ run `44713015747`（head `5a91c94`）抓到两处：`b_command_injection.rs` 把 `cmd` 写死成被测程序（坑 123）、窗口 OCR 夹具 1121 宽超过 runner 的 1024 虚拟屏导致右边被裁（坑 124）；⑦ run `44717160170`（head `f2edad5`，此时 **windows 已 success**）抓到 `hbot-workspace` 的 `create_dir("a.txt/sub")`：POSIX 返回 `ENOTDIR`、Windows 返回 `NotFound`，同一个语义错误走两个分支，`io_error` 的兜底把「用户输入错」变成 `Internal`（坑 125，已给 `NotADirectory` 加 `Invalid` 分支 + 单测）；⑧ run `44721680378`（head `2c5edfa`，此时 **windows 已 success**）抓到 `hbot-rpc/tests/hot_reload_e2e.rs` 的「时间下界」断言前提被 `bot_start` 自己那次 `stat` 满足掉（坑 126，改成预热前 `sleep(TTL + 250ms)`，断言口径不变）；⑨ 同一个 run 的 windows 侧抓到 `hbot-device/src/windows.rs` 的 `win32_probe` 断言绝对值 `count() == 0`，而它是进程级计数器、被并行的 `virtual_key_of` 抬到 6（坑 127，改成比增量 + 把 `bump()` 挪到真发 Win32 调用的那一行）；⑩ run `44727059044`（head `8305048`，此时 **windows 已 success**）抓到 `hbot-bot/tests/runtime.rs` 的 `outbound_is_persisted_then_retried_until_sent`：`wait_until` 只等假适配器「发出去了」，而 `snapshot().outbox.sent` 是 outbox 在 `mark_outbound_sent` 落库**之后**才 `fetch_add` 的，于是断言读到 `left: 0`（坑 128，等待条件补成「已投递 **且** 已记账」） |
| CI `bundle (windows, nsis)` | ✅ runner 上通过 | run `44730056833` 与 `44735231807` 都 success（含 NSIS 静默安装 → 启动 → 卸载 smoke 与体积/哈希） |
| CI `bundle (macos, dmg)` | ❌ 未验 | run 里该 job 直接 `cancelled`；无 Mac 本机 |
| Linux target 编译（`hbot-core`/`hbot-command`/`hbot-workspace`/`hbot-parity`） | ✅ 交叉 `cargo check` 通过 | 直接验证了 `confinement_linux.rs`（libc/Landlock）与 `paths.rs`（XDG）确实能编 |
| Linux target 编译（其余 crate） | ✅ 由 CI 覆盖 | `rust (ubuntu-latest)` 在真 Linux runner 上编全 workspace（本地仍做不到：`ring`/`psm` 需要目标平台 C 工具链） |
| Linux 上**运行**测试 | ✅ 由 CI 覆盖 | 同上 job 的 `cargo test --workspace` |
| `scripts/check.sh` 在**干净机器**上 | ❌ 未验 | 本机有 `node_modules`，`check.sh` 会走 web 分支；干净机器上会走 skip 分支，未实测 |

> **工具链必须钉死**：`rust-toolchain.toml` 写 `channel = "1.100.0"`，`ci.yml` 四处 `dtolnay/rust-toolchain@1.100.0`。
> 之前两边都写 `stable`，等于「各机器拿各自的最新 stable」，于是 CI 的 clippy 比本地新、只报在 CI（坑 115）。
> 升级 Rust 时**两个文件一起改**，并在仓库根用 `cargo --version` 确认。

---

## 6. 发布验收（每个里程碑）

- [ ] `check.sh --full` 绿。
- [ ] 对应里程碑出口标准逐条勾选（见 04）。
- [ ] 三平台安装/卸载/升级 smoke。
- [ ] 升级不丢项目与会话；DB 迁移可回滚（备份）。
- [ ] 崩溃后重启能恢复未处理消息。
- [ ] 性能预算无回归。
- [ ] 安全拒绝路径有测试。
- [ ] IDE 链路 E2E 绿：建项目 → 编辑保存 → Agent Diff 接受/拒绝 → 搜索 → 问题跳转 → 运行 mock → 日志跳源码 → 断点步进（M3 起）。
- [ ] 变更日志与文档更新。

---

## 7. 变更管理

- 闸门只能变严；放宽必须写 ADR 或在 PR 说明“为什么这条判据不适用”。
- 基线（豁免清单）只准变短；每条豁免写原因与到期日。
- 依赖新增：说明用途、许可证、体积/内存影响、替代方案。
- 协议变更：先改 `hbot-rpc/dto.rs`，生成物与 UI 同步；破坏性变更升主版本并写迁移说明。
- 数据迁移：只增不改；已发布迁移永不编辑；回滚靠备份，不靠 down migration。

---

## 8. 文档闸门

- 每份设计文档头部有：状态（Draft/Accepted/Superseded）、日期、关联 ADR。
- 文档里的命令必须能直接复制执行；路径必须存在（链接检查）。
- 性能/规模数字只在本文维护，其他文档引用本文，不复制。
- 决策写 ADR；ADR 一旦 Accepted，修改要追加“变更记录”，不静默改写。
- 删除文档前确认没有其他文档引用（`rg` 检查）。

---

## 9. Agent 提交前自检

```text
1. 改动是否落在正确 crate/feature？
2. 是否新增了第二套契约/循环/装配？
3. 是否引入 Node 或 npm 运行时依赖？
4. 是否有静默吞错、无上限队列、无超时 await？
5. 安全边界是否新增拒绝路径测试？
6. 生成物是否重新生成并入库？
7. 文档与 ADR 是否同步？
8. check.sh 是否全绿？
```

八条里任何一条答“否/不确定”，就不要提交。