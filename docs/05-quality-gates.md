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
| 桌面空闲总 RSS | < 220 MB | 启动后静置 60s，采样 3 次取中位 |
| 核心空闲 RSS | < 60 MB | 无 bot、无会话 |
| 每多一个 bot | < 6 MB | 加 5 个 bot 的斜率 |
| 每 isolate | < 8 MB（堆上限 32 MB） | 加载后 + 跑 1000 条消息 |
| 冷启动到可交互 | < 800 ms | 进程启动 → 首帧可点击 |
| UI 首屏 | < 1 s | 本地构建，中端机 |
| 初始 JS（gzip） | < 1.5 MB | 构建报告 |
| 入站 → 入队 p95 | < 10 ms | 注入 10k 条 |
| handler p95（本地无网络） | < 20 ms | 固定脚本 |
| 本地回复 p95 | < 50 ms | 端到端（mock 平台） |
| 1000 条突发不丢 | 是 | 背压测试，校验总数 |
| 100 会话并行 | 顺序正确、无饥饿 | 每会话 100 条 |
| 按键延迟 p95 | < 16 ms | 编辑器输入基准 |
| 打开 1MB 文件 | < 150 ms | 冷/热各一次 |
| 全局搜索首屏 | < 1 s（10,000 文件） | 固定夹具，流式首屏 |
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

| Job | 平台 | 内容 |
|---|---|---|
| `check` | ubuntu-latest | fmt/clippy/deny/xtask/nextest/UI check |
| `test-windows` | windows-latest | nextest（跳过平台不支持的）+ UI 单测 |
| `build-desktop` | win/mac/ubuntu | Tauri 打包（PR 只 build，不发布） |
| `e2e` | ubuntu + xvfb | Playwright（WS 模式优先） |
| `perf` | self-hosted（夜间） | 预算表 + 趋势 |
| `audit` | 每日 | cargo audit、pnpm audit、gitleaks |

缓存：`~/.cargo`、`target`（按 lockfile 哈希）、`node_modules`。PR 只跑受影响 crate 的测试时，必须仍跑 `check.sh` 的静态部分。

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