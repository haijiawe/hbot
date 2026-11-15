# ADR-0001：重写技术栈

- 状态：**已被取代（Superseded by [ADR-0002](0002-agent-in-rust.md)）**。保留作决策历史；不要据此实施。
- 日期：2026-11-16
- 决策范围：新仓库 `E:\HBot-Next` 的语言、运行时、桌面壳、UI、存储、RPC、构建与测试
- 关联：`docs/00-assessment.md`、`docs/02-architecture.md`、`docs/05-quality-gates.md`

---

## 1. 背景与硬约束

新产品的边界不是“再做一个 Electron 单机应用”，而是：

1. Windows / macOS / Linux 三平台桌面。
2. Linux 无 GUI 后台，同一套核心逻辑。
3. 性能目标同时覆盖：启动/内存/UI、机器人吞吐稳定性、Agent 执行、构建测试/Agent 开发效率。
4. 适配聊天、游戏、客服等不同 bot，而不是只服务一种 QQ 场景。
5. 旧格式与旧插件**不做兼容**；允许重新定义项目格式、插件协议、数据模型。
6. 代码要能被人和 Agent 持续修改：边界可验证、命令可重复、文档可导航。

由此得到三条不可妥协的设计约束：

- **核心必须能脱离桌面壳运行**。否则 Linux 后台永远是二等公民。
- **UI 不能执行业务逻辑**。否则三端复用、Agent 自动化和安全边界都做不干净。
- **性能结论必须可测量**。不能因为“Rust 更快”就选 Rust，也不能因为“Electron 熟”就继续 Electron。

---

## 2. 候选方案

### A. Electron 42 + Node 24 + React（旧路线的改良版）

**形态**：Electron 主进程即核心，renderer 是 React；无头时用 `ELECTRON_RUN_AS_NODE` 或单独的 Node 入口。

**优点**

- 一套语言、一个运行时，原生模块（better-sqlite3、node-pty）路径成熟。
- Chromium 版本固定，Canvas/Monaco 行为在三平台一致。
- 旧项目经验、测试、打包脚本可直接复用一部分。
- 招聘与 Agent 训练语料最丰富。

**代价**

- 空闲内存与安装包体积的基线高；三平台各带一份 Chromium。
- 主进程容易再次变成“什么都做”的单体（旧项目已经证明）。
- 无头仍要处理 Electron 依赖；要么保留 Electron 运行时，要么维护第二个入口。
- 安全边界仍要靠进程拆分，不会因为换语言自动变好。

**适用**：如果最终放弃“Linux 纯 Node 后台”，或者 Canvas 在 WebView 上无法达标，这是最稳的退路。

### B. Tauri 2 + Rust 薄宿主 + Node 24 守护进程 + React（推荐）

**形态**：

- `apps/desktop/shell`：Rust/Tauri，只做窗口、托盘、单实例、更新、OS 密钥、启停守护进程、传递 `{port, token}`。
- `apps/daemon`：纯 Node 24 + TypeScript，承载 bot、流程、存储、Agent、插件、RPC。
- `apps/desktop/web`：React 19 + TypeScript，纯 Web，通过 WebSocket 连守护进程。
- Linux 后台：同一个 `apps/daemon`，由 systemd/Docker 启动，无 Tauri、无 WebView。

**优点**

- 核心零 Electron 依赖，桌面与服务器共用同一产物。
- 桌面壳可替换：UI 是纯 Web，RPC 是契约；Tauri 失败可换 Electron，不动核心。
- 安装包与空闲内存通常低于 Electron（具体数字必须实测，不用估算当结论）。
- Rust 只做“宿主能力”，代码量可控（目标 < 2,000 行），安全敏感操作（密钥、更新、子进程）有强类型边界。
- Node 保留 bot/AI/MCP/插件生态与 Agent 开发效率。

**代价**

- 两语言、两进程：Rust 宿主 + Node 守护进程，需要握手、健康检查、崩溃重启、日志汇聚。
- 三平台 WebView 不同（WebView2 / WKWebView / WebKitGTK），Canvas、Monaco、输入法、字体都要真机验证。
- Node 需要作为 sidecar 打包（约几十 MB），安装包不会像纯 Rust 那样小。
- 原生模块（better-sqlite3、node-pty）要按 Node ABI 预编译/打包，不能沿用 Electron ABI。
- RPC 走 loopback 时必须做 token、Origin、绑定地址、消息大小限制，否则是本机攻击面。

**适用**：桌面是“控制台 + 编辑器”，核心是长期运行的 bot/Agent 服务；这正是本项目。

### C. Tauri 2 + 纯 Rust 核心 + React

**形态**：Rust 承载 bot、流程、存储、Agent、插件；Tauri 前端只做 UI。

**优点**

- 单二进制、低内存、启动快、无 Node 运行时。
- 并发与资源控制强；适合高吞吐协议网关。
- 安全边界清晰，插件可用进程/能力模型。

**代价**

- QQ/飞书/钉钉/企微的协议库、OneBot 生态、MCP SDK、AI SDK 在 Rust 侧成熟度明显低于 Node。
- 低代码表达式、用户自定义节点、动态插件需要自建解释器/沙箱；JS 生态的现成能力用不上。
- Agent 生成 Rust 代码的首次正确率、编译反馈循环、重构速度都低于 TypeScript。
- 重写量最大；在“功能对等”之前会长期没有可用产品。

**适用**：产品已经确定只做高吞吐协议网关，UI 很薄，且团队 Rust 能力强。当前不是。

### D. Go 核心 + Tauri/Wails UI

**优点**

- 单静态二进制、goroutine 并发、部署简单；Linux 后台很合适。
- 内存与启动优于 Node，生态比 Rust 更偏工程化。

**代价**

- 桌面 UI 仍要 WebView；Wails 的生态与 Tauri 相比更窄。
- 低代码/Agent/插件生态弱于 Node；动态加载与沙箱在 Go 里并不好做。
- 团队与 Agent 的 TypeScript 资产无法复用。

**适用**：如果核心确定为“协议网关 + 少量管理 UI”，Go 是强候选；但不是当前“低代码 + Agent + 插件”的产品形态。

### E. Web/PWA + 本地服务

**优点**：零桌面壳、跨平台、部署最简单。

**代价**：文件系统、终端、SSH、托盘、自动更新、离线、系统密钥都需要额外桥接；用户体验不像桌面产品。

**适用**：作为辅助的“远程管理面板”可以，不作为主形态。

---

## 3. 对比

图例：✅ 明显合适；⚠️ 可用但有代价；❌ 明显不合适。

| 维度 | A Electron | **B Tauri + Node** | C 纯 Rust | D Go | E Web |
|---|---|---|---|---|---|
| bot/协议/AI/MCP 生态 | ✅ | ✅ | ❌ | ⚠️ | ✅ |
| UI 内存/安装包 | ❌ | ✅ | ✅ | ✅ | ✅ |
| UI 行为三平台一致 | ✅ | ⚠️ | ⚠️ | ⚠️ | ✅ |
| 启动速度 | ⚠️ | ✅ | ✅ | ✅ | ✅ |
| Linux 无 GUI 复用 | ⚠️ | ✅ | ✅ | ✅ | ✅ |
| 低代码/动态插件 | ✅ | ✅ | ❌ | ⚠️ | ✅ |
| Agent 开发效率 | ✅ | ✅ | ❌ | ⚠️ | ✅ |
| 安全隔离 | ⚠️ | ✅ | ✅ | ✅ | ⚠️ |
| 重写风险 | 低 | 中 | 高 | 高 | 中 |
| 长期维护成本 | 中 | 中 | 中高 | 中 | 低 |

**结论**：B 是唯一同时满足“核心可 headless、UI 轻、生态可用、壳可替换”的方案。
它不是“性能最强”的方案（纯 Rust 在单机吞吐上更强），但它是**在当前产品约束下总成本最低、可逆性最好**的方案。

---

## 4. 决策

采用 **B：Tauri 2 + Rust 薄宿主 + Node 24 守护进程 + React 19**。

### 4.1 语言与运行时

| 层 | 选择 | 约束 |
|---|---|---|
| 桌面宿主 | Rust stable，Tauri 2.x | 只做宿主能力；业务代码 0 行；目标 < 2,000 行 |
| 核心守护进程 | Node 24 LTS，TypeScript 6.x，ESM | 0 个 Electron import；可在 Windows/macOS/Linux 直接 `node` 启动 |
| UI | React 19 + TypeScript 6.x + Vite 8 | 纯 Web；不 import Node/核心包；只依赖 `contracts` |
| 契约 | zod 4 + JSON-RPC 2.0 | 单一真源；UI 类型由契约推导 |
| 存储 | SQLite 3 + better-sqlite3 12，WAL | 只有 `packages/storage` 能碰 SQL |
| 测试 | Vitest 4 + Playwright | 单元/集成/契约/E2E 分层 |
| 构建 | pnpm 11 + `tsc -b` + esbuild + Vite | 不引入 Nx；Turborepo 只在 CI 超过 5 分钟后再评估 |
| 质量 | Biome（格式+lint）+ `tsc` + dependency-cruiser | 减少工具数量，Agent 只需记两条命令 |

### 4.2 为什么核心是 Node，不是 Rust/Go

- bot 负载主要是 **I/O 与协议编排**：WebSocket、HTTP、SQLite、定时器。Node 的事件循环足够；真正的瓶颈通常是队列、锁、序列化和每次操作开库，而不是语言。
- Agent/工具/MCP/插件生态在 TypeScript 侧最完整；重写时能少造轮子。
- 低代码表达式、动态节点、插件沙箱需要动态语言与成熟 JS 生态；Rust/Go 要自建更多基础设施。
- 性能要靠**架构**拿：每会话串行队列、有界背压、持久化等待、连接池、批量写、worker 隔离。这些在 Node 里都能做，且可测量。
- Rust 只放在“宿主”和“未来热点”上；如果某个 CPU 热点真的成为瓶颈，用 `napi-rs` 写一个原生模块挂在端口后面，而不是重写整个核心。

### 4.3 为什么桌面壳是 Tauri，不是 Electron

- UI 是控制台/编辑器，不是 3D 游戏；Tauri 的 WebView 足够，且内存/安装包更轻。
- 核心已经是独立 Node 进程，桌面壳只做生命周期；Electron 的“主进程即 Node”优势用不上。
- Rust 宿主适合做密钥、更新、单实例、子进程监管这些安全敏感但代码量小的事。
- **可逆**：UI 是纯 Web，RPC 是契约；如果 WebView 在三平台不达标，换 Electron 壳即可，核心与 UI 不动。

### 4.4 为什么不用 ORM / GraphQL / tRPC

- **不用 Prisma/TypeORM**：SQLite 场景下 ORM 的迁移/生成/运行时开销不值；手写 SQL + 类型化 row mapper 更透明，Agent 也更容易读。
- **不用 GraphQL**：本地单客户端，查询复杂度不需要 GraphQL；会增加 schema/解析器负担。
- **不用 tRPC**：tRPC 把客户端和服务端绑在 TypeScript 类型系统里，未来 Rust/CLI/第三方接入会受限；JSON-RPC + zod 更中性。
- **不用 LangChain 类框架**：Agent 循环、工具策略、审批是产品核心，必须自己掌控；框架会把关键判据藏进黑盒。

### 4.5 版本策略

- Node：24 LTS，`.node-version` 固定；不使用实验特性。
- Rust：stable，`rust-toolchain.toml` 固定；不追 nightly。
- Tauri：2.x；升级前跑三平台 smoke。
- React/TypeScript/Vite：跟随 stable；每季度评估一次，不在功能分支里顺手升级。
- 依赖：`pnpm-lock.yaml` 入库；新增依赖需要在 PR 说明“为什么标准库/现有依赖不够”。

---

## 5. 后果

### 5.1 正面

- 桌面与 Linux 后台共用一套核心，减少“两套装配”的漂移。
- UI 可独立开发、可浏览器调试、可用 Playwright 做 E2E，Agent 不需要启动整个桌面。
- 壳可替换，技术选型不是单行道。
- 核心可用纯 Node 做负载测试与 CI，不需要 GPU/显示器。

### 5.2 负面与缓解

| 代价 | 缓解 |
|---|---|
| 两语言、两进程 | Rust 宿主限制在 4 个命令；核心通过一个 `ShellBridge` 接口访问宿主能力；宿主不写业务 |
| sidecar 生命周期 | 宿主启动 daemon 并读 stdout 握手；daemon 心跳；崩溃指数退避重启；退出时先 SIGTERM 再 kill |
| WebView 差异 | M1 前做 Monaco + xyflow 三平台 spike；CI 跑 UI smoke；保留 Electron 壳退路 |
| Node 原生模块打包 | 使用 Node ABI 预编译；每个平台/架构单独产物；CI 矩阵验证 `require` |
| RPC 本机攻击面 | 绑定 `127.0.0.1`、随机端口、256-bit token、Origin 校验、消息大小上限、速率限制 |
| 安装包仍含 Node | 可接受；若必须更小，未来把核心热点迁 Rust，或提供“无 Node 的协议网关”精简版 |

---

## 6. 可逆性与重估信号

### 6.1 如何换掉桌面壳

`apps/desktop/web` 只依赖：

- `window.__HBOT_BOOT__`：`{ rpcUrl, token }`
- 4 个宿主命令：`app_info`、`daemon_start`、`daemon_stop`、`secret_get/set`

Electron 壳只要实现同一个 `ShellBridge`，UI 与 daemon 不动。
因此“Tauri 选错”的最大代价是重写壳（约 1–2 周），不是重写产品。

### 6.2 何时推翻本 ADR

出现以下任一情况，重新评估：

1. 三平台 WebView 上，500 节点画布拖拽中位帧率 < 45fps，且优化后仍不达标。
2. sidecar 冷启动 p95 > 1.5s，或 daemon 空闲 RSS > 120MB。
3. 目标企业环境无法安装/升级 WebView2 或 WebKitGTK。
4. Tauri 打包/签名/自动更新在任一平台阻塞超过 2 周。
5. Rust 宿主代码超过 3,000 行，或开始出现业务逻辑。
6. Node 核心在压测中成为 CPU 瓶颈，且 `napi-rs` 热点迁移无法解决。

### 6.3 5 天 Spike（先验证，再转 Accepted）

| # | Spike | 通过标准 |
|---|---|---|
| S1 | Tauri 壳启动 Node sidecar，stdout 握手，WebSocket 通 | 三平台各跑通一次；退出无孤儿进程 |
| S2 | Monaco + xyflow 500 节点拖拽 | 三平台 ≥45fps；输入法/滚动正常 |
| S3 | better-sqlite3 在 sidecar 中读写 | 三平台安装包可 `require`；WAL 正常 |
| S4 | OS 密钥读写 | Windows Credential Manager / macOS Keychain / Linux Secret Service 各通一次；无则明确降级 |
| S5 | Linux 无 GUI 启动 daemon | 无 Electron/WebView 依赖；systemd 下可启停；RPC 可连 |
| S6 | POSIX 命令隔离可用性 | macOS/Linux 上 `bwrap`/`nsjail`/容器任一能限制写范围；都不可用则 v1 禁用这两个平台的命令工具（明确写入能力矩阵，不退化成名字白名单） |

任一 Spike 失败：先修方案；S2 失败优先考虑 Electron 壳，而不是放弃“核心/UI 分离”。S6 失败不是技术栈失败，是**能力范围**的决定，必须写进 README 与 UI。

---

## 7. 备选决策记录

如果 Spike 失败，按以下顺序回退：

1. **Electron 42 壳 + 同一个 Node daemon + 同一个 Web UI**：核心与契约不变，只换壳。
2. **Electron 主进程即 daemon + renderer UI**：进一步简化进程，但会牺牲 headless 复用，作为最后手段。
3. **纯 Rust 核心**：只在确认“协议网关是唯一核心、插件/低代码可砍”时重启评估；不要在当前产品形态下直接跳。

---

## 8. 参考资料

- Tauri 2 Sidecar：<https://v2.tauri.app/develop/sidecar/>
- Tauri 2 Node.js sidecar：<https://v2.tauri.app/learn/sidecar-nodejs/>
- Electron 进程模型：<https://www.electronjs.org/docs/latest/tutorial/process-model>
- Electron 原生模块：<https://www.electronjs.org/docs/latest/tutorial/using-native-node-modules>
- Node.js 24：<https://nodejs.org/en/about/previous-releases>
- MCP TypeScript SDK：<https://github.com/modelcontextprotocol/typescript-sdk>