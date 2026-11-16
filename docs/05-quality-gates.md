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
| 冷启动到核心可用（headless） | < 800 ms | `hbot serve` 打印 `{"type":"ready"}` 的时刻（实测 878 ms 中位 —— **当前不达标**，见 ADR-0002 §4.2） |
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

### 5.1 当前实际验证状态（别把这当成"CI 已绿"）

**本仓库没有 git remote，15 个提交全在本地，所以 `ci.yml` 从未被执行过一次。** 下表是"本地能验的"与"只能靠 CI/真机验的"的分界：

| 检查 | 状态 | 证据 |
|---|---|---|
| Windows 全量 `fmt`/`clippy`/`test --workspace` | ✅ 本地实跑通过 | `674 passed / 0 failed`（上一批；本批改动待重跑） |
| `apps/web` 的 `check`/`test` | ✅ 本地实跑通过 | `13 文件 101 用例` |
| `xtask deps` / `gen --check` | ✅ 本地实跑通过 | `0 violation(s)`、`40 个文件一致` |
| Linux target 编译（`hbot-core`/`hbot-command`/`hbot-workspace`/`hbot-parity`） | ✅ 交叉 `cargo check` 通过 | 直接验证了 `confinement_linux.rs`（libc/Landlock）与 `paths.rs`（XDG）确实能编 |
| Linux target 编译（其余 crate） | ❌ **未验** | 被 `ring`（rustls）与 `psm`（SWC 的 `stacker`）挡住：两者在构建期需要**目标平台的 C 编译器/汇编器**，本机只有 MSVC。`cargo install cargo-zigbuild` 已装但 `zigbuild` 只提供 `build` 不含 `check`，未继续 |
| Linux 上**运行**任何测试 | ❌ 未验 | 同上，且需要真实 Linux 宿主 |
| macOS 编译与 dmg | ❌ 未验 | 无 Mac 机器；`bundle-macos` job 从未执行 |
| `scripts/check.sh` 在**干净机器**上 | ❌ 未验 | 本机有 `node_modules`，`check.sh` 会走 web 分支；干净机器上会走 skip 分支，未实测 |

> **结论**：M0 出口的「CI 至少 Linux + Windows 编译通过」当前**不成立**（Windows 本地成立、Linux 未成立）。要真正闭合只有两条路：① 加一个 remote 让 CI 跑一次；② 给一台 Linux 机器（或能跑 WSL2 的环境 / 完整交叉工具链）。

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