# E:\H Bot 制造 架构与质量审查（只读）

审查对象：`E:\H Bot 制造` @ git `d9c512b`（工作区干净，审查前后 `git status --porcelain` 均为空）
审查方式：只读配置 / 源码 / 测试 + 运行只读命令与现有闸门；**未读 docs/、README、历史报告**，文中的判断只引用代码、命令输出、构建产物三类证据。
证据标记：`[实测]` = 本次运行命令/闸门得到；`[产物]` = 读现有 `out/` 构建产物得到；`[读码]` = 读源码推断（可能被执行路径证伪，未见运行）；`[未验]` = 受只读约束未能运行。

环境：Node v24.21.0，pnpm 11.7.0（`package.json`），Electron 42，Windows。
未运行项（会改写工作区，违反"不修改源仓库"约束）：`electron-vite build`、`electron-builder`、应用冷启动、ACL 沙箱实际跑命令、Linux/容器内实跑 headless。

---

## 结论

1. **边界是干净的，耦合在边界之内。** 四个运行时边界 `src/shared / src/main / src/renderer / src/preload` 的反向依赖实测全为 0：`shared→main 0`、`shared→renderer 0`、`renderer→main 0`、`renderer→Node 内置 0`、`main→renderer 0`、`preload→main 0`、`preload→renderer 0`。1,061 个源模块里**文件级循环依赖只有 1 个**（`src/main/services/agent-core` 下 11 个文件）。真正的结构债集中在 `src/main/services` 内部：94 条目录级依赖边里**42 对是双向的**。这是"域内高度互引"，不是"层间倒挂"。

2. **三个入口，两套装配。** 桌面（`src/main/index.ts`）与 headless（`src/main/runtime/standalone.ts` → `headlessAssembly.ts`）各自实现了一份 30–40 行、语义高度重叠的启动装配；`headlessAssembly` 全仓只被 `standalone.ts` 引用（`[实测]` grep 命中 2 处，均在 standalone.ts）。第三入口 `untrustedWorker.ts` 是独立传输层，与前者不共享装配。这就是重写时最该先合并的一处。

3. **headless 不依赖 GUI，但依赖"`electron` 模块可解析"。** 从 standalone 可达的代码被卷进 `out/main/chunks/FlowRuntime-*.js`，该 chunk **顶层**有 `let electron = require("electron")`（`[产物]`）；部署包里靠 `runtimeBundle.ts` 生成一个 `module.exports = {}` 的假 electron 包来满足它（`runtimeBundle.ts:30-40`）。所以 Linux 服务器能跑（`dockerExport.ts:227` 基础镜像 `node:22-slim`），前提是走了那条安装壳脚本；直接 `node out/main/standalone.js` 且环境里没有 electron 会 `MODULE_NOT_FOUND`（`[读码]`）。

4. **原生模块只有一个二进制位，且 ABI 会在开发循环里来回翻。** `better-sqlite3` 同时存在 ABI 137（Node 24）与 ABI 146（Electron 42）两种需求，`scripts/ensure-sqlite.js` 用"真建一次 `:memory:` 库"来探测并重装到单个 `build/Release/better_sqlite3.node`（1,919,488 B）。`pnpm test` 会把它切到 Node 版，此后 `pnpm dev` 必须再跑一次 electron 版——这是本仓库最稳定的开发期踩坑点。

5. **结构闸门的口径已经换过一轮，"行数"不再是判据。** 实测 7 个 ≥400 行的函数，分类是 assembly 3 / data-table 2 / single-transaction 2 / **multi-capability 0**；逻辑克隆（≥200 token 且含控制流）**0**。也就是说按仓库自己的判据，函数级技术债已经清零——剩下的 2,230 处 `any` 里有 **2,096 处集中在 134 个"从打包产物还原"的文件**（94%），属于**来源问题**而非设计问题。

6. **测试量大、偏契约、但反馈环慢。** 531 文件 / 6,399 用例 / **340.90s**（`--maxWorkers=2`）；其中 `setup` 123.46s、`environment` 253.04s，用例本身只占 137.96s——时间几乎全花在 jsdom 环境与全局 setup 上。测试基本不写快照（全仓 `toMatchSnapshot` 命中 0）、`mock 调用断言` 只占全部断言约 3%（352 / 11,498），所以"实现镜像测试"不是主流问题；真正的缺口是 **e2e 是死代码**：`tests/e2e/node-flow.spec.ts` 依赖未声明的 `@playwright/test`，且被 vitest include、tsconfig include 三重排除，从不运行。

7. **插件系统有契约、有版本、有沙箱，但沙箱不是进程边界。** 清单身份（id/version/permissions/requires）校验齐全，范围匹配 fail-closed；但外部插件在主进程内 `node:vm` 里执行（`codeGeneration` 关闭、5s 超时、JSON 信封），与仓库里已有的 `untrustedWorker.ts`（真正的子进程沙箱）不是同一套隔离。宿主 API 面只有 `contributeTool` / `contributePrompt`，版本 `0.1.0`，谈不上稳定。

8. **重写的重点不是"清理烂代码"，而是"合并装配 + 平台解耦 + 重建验收"。** 现有代码的边界纪律比典型同规模项目好；重写收益主要来自：单一装配、`electron` 单点依赖、双 ABI 消除、IPC 三处手写收敛为生成、以及把 `tests/guards` 下 39 个结构断言里真正有价值的部分（依赖方向、通道一致、节点唯一、ABI 探测）保留下来、其余换成"可运行的黑盒验收"。

---

## 入口与产物表

### 构建入口（`electron.vite.config.ts`，`[实测]` 逐行读）

| 入口 | 源文件 | 产物（现有 out/，`[产物]`） | 体积 | 装配位置 | electron 依赖 |
|---|---|---|---|---|---|
| 桌面主进程 | `src/main/index.ts`（503 行） | `out/main/index.js` | 2,301,866 B | `src/main/services/bot/botManager.ts:182-286`（`ensureBotRunning`） | 硬依赖；产物内 3 处 `require("electron")` |
| headless | `src/main/runtime/standalone.ts`（89 行） | `out/main/standalone.js` + `out/main/chunks/FlowRuntime-*.js` | 9,648 B + 1,008,897 B | `src/main/runtime/headlessAssembly.ts:81-137`（`startHeadless`） | 不建窗口，但 FlowRuntime chunk **顶层** `require("electron")` |
| 沙箱 worker | `src/main/runtime/untrustedWorker.ts`（377 行） | `out/main/untrustedWorker.js` | 9,494 B | 自带双传输（`utilityProcess` / `child_process.fork`），`INVOKE_TIMEOUT_MS 30000` | 无（可达仅 2 文件，0 处 electron，`[实测]` 可达性扫描） |
| preload | `src/preload/index.ts`（1,421 行） | `out/preload/index.js` | 63,205 B | 无装配，纯桥面 | 只有 `electron` 的 contextBridge/ipcRenderer |
| renderer | `src/renderer/index.html` | `out/renderer/index.html` + assets | index 2,328,528 B；CodeEditor 4,880,259 B；elk.bundled 2,549,839 B；FlowPanel 945,301 B；editor.worker 596,421 B；Terminal 431,168 B；index.css 171,465 B | — | 无 |

`out/` 合计 **16.87 MB**（含 `tsconfig.node.tsbuildinfo` 511,032 B，不进包）。

### 打包（`electron-builder.yml` + `package.json`）

- `pack` = `node scripts/ensure-sqlite.js electron` → `electron-vite build` → `electron-builder --win --config electron-builder.yml`。
- `files: out/**/*` 只打编译产物；`asarUnpack` 放行 `**/*.node`、`better-sqlite3/**`、`node-pty/**`、`ssh2/**`、`out/main/chunks/**`、`out/main/untrustedWorker.js`（因为 `child_process.fork` 需要真实文件路径）。
- `npmRebuild: false`（`electron-builder.yml:20`）：不在打包期跑 node-gyp，改用 `ensure-sqlite.js` 预先换 ABI。
- `electronFuses`：`runAsNode:false`、`enableNodeOptionsEnvironmentVariable:false`、`enableNodeCliInspectArguments:false`、`onlyLoadAppFromAsar:true`、`enableEmbeddedAsarIntegrityValidation:true`——安全默认值齐；副作用是**打包后的 EXE 不能再用 `ELECTRON_RUN_AS_NODE` 跑 headless**。
- 目标：`nsis` + `portable`，仅 win x64（`H-Bot-Portable-${version}.exe`）；`resources/` 只有 `icon.png`（1,440 B）。

### 装配分叉点（这是"是否复用同一套装配"的答案：**否**）

| 关注点 | 桌面 | headless | 结论 |
|---|---|---|---|
| 端口解析 | `eventBus.ts:87 resolveUsableTranslatorPort(config, projectPath)`（`botManager.ts:182`） | `headlessAssembly.ts:64-77 resolveTranslatorPort(explicit, projectPath)`：显式 > `QQBOT_BUS_PORT` > `.qqbat/project.json` | **语义不同**。`QQBOT_BUS_PORT` 全仓只出现在 `headlessAssembly.ts:66`、`dockerExport.ts:277`（容器 healthcheck）、`project.ts:229`（模板注释）——桌面入口**不读**这个变量 `[实测]` |
| 翻译器启动 | `botManager.ts:229-238`：`startTranslator` → `getTranslatorStatus(void 0, projectPath).port` 校正实际端口 | `headlessAssembly.ts:91-98`：同样先启动再校正 | 同构，重复 |
| 端口回写 | 桌面**回写** `project.json`（`botManager.ts:239-247`） | headless **不回写** | 行为分叉 |
| 官方适配器 | `botManager.ts:248-254`：`startOfficial` 失败只 `log.warn` | `headlessAssembly.ts:102`：同类容错 | 同构，重复 |
| 运行时构造 | `new FlowRuntime(projectPath, translatorPort)`（`botManager.ts:256`） | `new FlowRuntime(projectPath, translatorPort, host)`（`headlessAssembly.ts:109`） | 第三参数是 WS host（`FlowRuntime.ts:74-78`，默认 `'127.0.0.1'`）；桌面无法指向远端 |
| 状态/日志回调 | `botManager.ts:257-281`：`onStatusChange` 写进 `bots` 表状态机（running/error/stopped）、`emit("statusChanged")`、`broadcastStatus`；`onLog` 走 `parseRuntimeLogLine` | `headlessAssembly.ts:111-117`：只把状态/日志拼成文本 `log(...)`，另维护一个局部 `runtimeStatus` 字符串，由返回的 `status()` 轮询读取（`:130-137`）；`startHeadless(opts: any)` 的入参只有 `{ log, projectPath, host, port }`，**没有状态回调** | 骨架同构但接口不同：桌面是事件驱动 + 状态机，headless 是"日志 + 轮询"。合并时需要抽象出 `Host.reportStatus/reportLog` 两种模式 |
| `.env` 读取 | 无 | `headlessAssembly.ts:24-60 loadDotEnvFile`（最小解析：不覆盖已存在 env、不剥行尾 `#` 注释） | 仅 headless |

**推论（`[读码]`）**：同一个项目在桌面点"启动"与在 Docker 里启动，可能落在不同端口（桌面忽略 `QQBOT_BUS_PORT`），且桌面会把结果写回 `project.json` 造成两套配置互相覆盖。这是重写时必须以"单一 `RuntimeAssembly` + 显式 `Host` 接口"消除的第一处。

`standalone.ts:4` 是唯一 `import { startHeadless } from "./headlessAssembly"` 的地方；`standalone.ts:21` 的注释声称"装配统一走 headlessAssembly"，与 `botManager.ts` 的实际实现**不一致**（注释不能作为证据，这里只用它说明意图与实现的落差）。

---

## 依赖与边界证据

### 规模与边（自写遍历脚本，排除 `__tests__` / `.d.ts` 口径差异，`[实测]`）

| 边界 | 文件 | LOC |
|---|---|---|
| `src/main` | 530 | 103,016 |
| `src/shared` | 145 | 34,740 |
| `src/renderer/src` | 386 | 79,519 |
| `src/preload` | 2（含 1 个 `.d.ts`） | 1,551 |

闸门口径（`audit`）为 **1,061 个源模块**。依赖边（`import`/`export ... from`，按文件计数）：

```
main->main      1493      main->shared     287
renderer->renderer 1690   renderer->shared 337
shared->shared   168      preload->shared    9
preload->preload   1
```

**违规方向全部为 0**（`[实测]` 脚本逐条统计）：
`shared→main 0`、`shared→renderer 0`、`renderer→main 0`、`renderer→Node 内置 0`（`fs`/`path`/`child_process`/`os` 等无匹配）、`main→renderer 0`、`preload→main 0`、`preload→renderer 0`。

这四条是重写时**应当保留并继续用机器守住的契约**，不是需要重新设计的东西。

### `src/main/services` 内部耦合（`[实测]` 目录级聚合）

- 29 个目录、**94 条目录级边**；其中**42 对双向互引**。
- 最重的几条：`ai→infra 46`（例 `ai/allowRules.ts:14`）、`agent-core→infra 42`（例 `agent-core/askUser.ts:19`）、`agent-engine→agent-core 31`（例 `agent-engine/adapters/turn.ts:14`）、`agent-runtime→ai 29` 与 `agent-runtime→agent-core 27`（`agent-runtime/index.ts:13`/`:15`）、`project→infra 22`、`ai→agent-core 20`（`ai/tools/registry.ts:31`）、`agent-core→ai 18`（`agent-core/adapters.ts:10`）。
- 双向对里值得单独点名的：`ai↔agent-core`、`agent-engine↔agent-core`、`agent-runtime↔agent-core`、`bot↔ai`、`bot↔runner`、`bot↔official`、`flow↔plugins`、`flow↔project`、`ai↔plugins`、`builtinNodes↔bot`。
- **这是目录粒度的互引，不等于文件级循环**：文件级 SCC 只有 1 个（11 文件，均在 `agent-core`：`adapters / core / modelAdapter / prompt / ralph / runTurn / scheduler / subagent / toolPipeline / workflowEngine / workflowTool`），已登记进 `tests/guards/cycles-baseline.json`，`fresh 0 / ghost 0`（`[实测]` 闸门）。

判断（不要把这些一律当债）：`infra` 被 12 个域依赖是**正常的基础设施方向**；`agent-core ↔ ai` 的双向是"工具注册表 ↔ 审批/管道"这一对难拆的整体，第 191 批引入 `agent-engine` 后变成三方互引，这是**真债**（新旧两套循环骨架并存，见风险 R6）。

### 层内越界的具体形态

1. **`electron` 泄漏到领域代码**：`src/main/services` 里 7 处引用 `electron`，5 处已做懒加载（`infra/logger.ts:140`、`infra/paths.ts:134`、`builtinNodes/projectPath.ts:11`、`devBackup/devMode.ts:38`、`project/dataService.ts:821`），**2 处是静态 import**：`services/official/config.ts:2`（`import * as electron from "electron"`）、`infra/safeStorage.ts:2`（`import { safeStorage } from 'electron'`）。`[产物]` 佐证：`out/main/chunks/FlowRuntime-*.js` 顶层 `let electron = require("electron")` 正是这类静态导入被打包器提升到模块作用域的结果。
2. **preload 没有业务逻辑**（好）：**1,421 行**里 0 个 `function` 声明、308 个 `ipcRenderer.invoke`、38 个 `.on`、1 个 `.send`、3 个 `contextBridge`。但**表面积 309 个通道**（见下），是"薄但宽"。
3. **renderer 不碰 Node**（好）：无 `from 'electron'`、无 Node 内置模块引用；`tsconfig.web.json` 的 `types` 甚至显式带了 `node`（为了测试），但代码层没有越界。
4. **`shared` 干净**（好）：0 处 `electron`、0 处 DOM（`tsconfig.node.json` 的 lib 无 DOM，靠这条硬约束兜住）。

### IPC 契约（`[实测]` 三处对账）

| 侧 | 数量 | 位置 |
|---|---|---|
| 主进程注册 | **309 个字面量通道**（`handle` + `on`），**0 重复** | `src/main/ipc/**`，59 个文件 |
| preload 调用 | 308 个 `invoke`（307 个唯一名，1 个通道被两处调用）、38 个 `on`、1 个 `send` | `src/preload/index.ts` |
| 渲染层类型 | `QqbatApi` 声明（2,235 行） | `src/renderer/src/types/ipc.ts` |
| 对账闸门 | AST 展开点号路径对账 preload ↔ 类型；`fresh` 时红 | `tests/guards/ipcSurfaceParity.test.ts` |

`invoked but not handled` = **0**（`[实测]` 双向差集）。也就是说 IPC 目前是**自洽**的，且被闸门守着——但代价是"一条通道写三处"（闸门文件头注释亦如此描述）。重写时这条应改为**从一份 schema 生成三处**（zod → handler 签名 → preload 方法 → 渲染层类型），这是最大的一块可机械消除的手写量。

### 还原代码的来源痕迹（解释 `any` 与格式债）

- **176 个源文件**首行带 `// 由 10/18 打包产物还原（chunk: …）` 标记（按 chunk 来源分组：`index.js` 60、`FlowRuntime-*.js` 37、`registration-*.js` 26、`ai-*.js` 19、`codegen-*.js` 6，其余零散）`[实测]`。
- `any` 基线 **2,230 处 / 158 文件**（取自 `tests/guards/any-baseline.json`）：其中**134 个还原文件占 2,096 处（94%）**，其余 24 个文件合计仅 134 处 `[实测]`。
- 结论：`any` 不是"团队习惯"，主要来自**反编译还原**（双引号、分号、`any` 三件套）。重写时这批文件的收益不是"改类型"，而是"重新设计接口形状"。

### 插件系统与扩展点（对应问题 7）

**发现与加载**

- 准入规则唯一：`pluginDirContract.ts` 的 `listPluginDirEntries` —— 只认"非 `.` 开头的**目录** + 目录内有 `manifest.json`"，不按前缀/命名约定过滤。
- 加载器：`pluginLoader.ts` 的 `loadProjectPlugins` 遍历目录 → `pluginScanner` 解析 → registry `Map<name, {pkg, specs, enabled, path}>`；**单个插件失败不抛**（`catch` 后跳过），`reload` 先卸载再装；`registerPlugin` 幂等（已注册则 warn 跳过）；`unregisterPlugin` 按前缀卸 handler + 卸 trigger；`setPluginEnabled` 只切 `enabled` 与触发器注册。
- 三种来源：内置（`builtinHostPlugins.ts`）、项目内运行时（`pluginLoader.ts`）、市场安装（`market.ts` + `windowPackageScanner.ts`）；另有 AI 生成脚手架（`aiNodeScaffold.ts` / `customNodeScaffold.ts` / `aiGeneratedScanner.ts`）。
- 规模：`src/main/services/plugins` 27 文件（最大 `market.ts` 28,106 B、`pluginScanner.ts` 14,016 B、`pluginLoader.ts` 13,779 B），`src/shared/plugins` 9 文件（`windowPackage.ts` 21,811 B、`windowView.ts` 13,726 B、`market.ts` 11,949 B）。

**版本化**

- `shared/plugins/manifest.ts`：`id` 匹配 `/^[a-z0-9][a-z0-9._-]*$/i`、`version` 必须 `x.y.z`、`permissions: string[]`、`requires?: string`；`validatePluginIdentity` 返回 `{ok,value} | {ok:false,error}`。
- `shared/plugins/version.ts`：`satisfiesPluginRange(host, range)` 只支持 `>= <= ^ ~ =`，**无法解析的范围 fail-closed 返回 false**（宁可装不上，不可放行）。
- 宿主版本 `MAIN_PLUGIN_HOST_VERSION = '0.1.0'`（`services/plugins/host.ts:50`）；不兼容时**注册但不激活**（`registerMainPlugin` 的判据在 `host.ts:160`）。
- 市场侧 `latestCompatibleVersion(versions, HOST_VERSION)` + 安装记录 `installed.json`（`{id,name,version,requires,source,sha256,installedAt}`）+ 目录复合哈希 `sha256`（按路径排序后 `path\0file-hash` 再外层 hash）+ 写入串行化 + `quarantineIfBroken`。

**隔离（关键判断）**

- 外部/AI 插件执行在主进程内的 `node:vm`：`createHardenedContext`（`codeGeneration: {strings:false, wasm:false}`）、5 s 超时、宿主侧注入 `__qqbatHostCall(JSON)` 信封 `{ok,value} | {ok,error}`；工具调用以代理 `Tool` 形式转发回沙箱（`sandboxPluginBridge.ts`，`timeoutMs = timeout + 500`）。
- **不是进程边界**：同进程意味着 CPU 抢占、`process` 全局可见面、内存无上限、原型链逃逸面都还在（`host.ts` 头注释也自述"边界仍是主进程内 vm"）。
- 仓库里**已有**真进程沙箱 `src/main/runtime/untrustedWorker.ts`（`utilityProcess` / `fork` 双传输、30 s 调用超时、可达仅 2 文件、0 处 electron）。**同一个仓库里两套隔离等级，插件走了弱的那套**——这是重写时的收敛点（见 R9）。
- 权限：唯一权威是 manifest（`pluginScanner.ts:331` 强制覆盖插件代码自写的 `permissions`），已知权限只有 `*` / `message.send` / `api.<name>` / `builtin.<kind>`（`isKnownPermission`）；`permissionsOfPackage` 归一 `[]` = 什么都不许。宿主 API `contributeTool` / `contributePrompt` 未声明权限直接 throw。

**公共 API 稳定性（结论：不稳定，且没有版本化的 API 面）**

- 宿主只暴露两个贡献点（`contributeTool` / `contributePrompt`）+ 工具 handler 内的 `getSetting/getSettings`；**没有**版本号、能力协商、deprecation 机制，只有 `requires` 的粗粒度范围。
- 宿主版本 `0.1.0` 意味着任何 0.x→0.y 都可能破坏；`satisfiesPluginRange` 的 `^` 在 0.x 语义上易误判（`^0.1.0` 是否含 `0.2.0` 取决于实现细节——**这一点未写测试证明**，`[读码]`）。
- 渲染层插件面（`shared/plugins/windowPackage.ts` / `windowView.ts` / `windowSlots.ts`）与主进程插件面（`host.ts`）是**两套独立契约**，文档里需要分别写。

**作者 / Agent 分别需要的文档**

| 读者 | 必须有的东西 | 现状（代码可证） |
|---|---|---|
| 插件作者 | manifest 全字段 schema + 权限清单 + `requires` 语义（含 0.x 注意事项）+ `activate(api, ctx)` 生命周期 + 工具声明与返回值格式 + 设置读写 + 市场哈希/签名规则 + 调试方式（如何在沙箱里看报错） | 只有类型与实现；**无生成的 schema 文档**（`docs:nodes:check` 只覆盖节点目录，不覆盖插件 manifest `[读码]`） |
| Agent（模型） | 可被工具化的"插件脚手架"：新建插件 → 写 manifest → 写 handler → 本地加载 → 自测；以及"当前项目装了哪些插件、各自权限"的可查询面 | 有 `aiNodeScaffold.ts` / `customNodeScaffold.ts`（生成骨架）与 `scoping.ts`（按会话预设过滤工具），但没有"插件能力自述"文档，Agent 只能靠读源码猜 |
| 维护者 | 隔离等级差异（vm vs 子进程）、`installed.json` 的完整性算法、失败隔离（`quarantineIfBroken`） | 代码里有，未成文 |

**两处实测缺陷（低严重度）**

- `pluginScanner.ts:94`：`author: manifest.permissions?.includes("author") ? void 0 : void 0` —— 恒 `undefined` 的死三元（还原残留），插件作者信息永远为空。
- 自建插件默认 `permissions: ['*']`（`pluginManifest.ts`）：用户自建节点等价内置权限，与"外部插件最小权限"的叙事相反；这是**有意的便利**还是遗漏，代码里没有注释说明，重写时必须显式决策。

---

## 质量闸门实测

`node scripts/check-quality.mjs` 实跑（16 道，全绿，`[实测]`；逐项耗时取自各闸门自报）：

| # | 闸门 | 耗时 | 实测数字 | 判定 |
|---|---|---|---|---|
| 1 | typecheck | 10,228 ms | node 0 错（2.1s）、web 0 错（8.1s）；日志落 `_recovery/` | 绿 |
| 2 | clones | 3,020 ms | 888 跨文件克隆（type-1 135 / type-2 773）；**逻辑克隆（≥200 token + 控制流）0**；声明式数据重复 15（不阻断）；轻量重复（60–199 token）**873**；基线 hard 0 / total 941 | 绿 |
| 3 | fnlen | 322 ms | 3,797 个函数；≥400 行 **7**（assembly 3 / data-table 2 / single-transaction 2 / **multi-capability 0**）；≥200 行 55 | 绿 |
| 4 | any | 288 ms | 2,230 处 / 158 文件（= 基线） | 绿 |
| 5 | emptyCatch | 314 ms | **0** | 绿 |
| 6 | coverage | 213 ms | 610 / 1,061 模块被测试 import（**57.5%**）；532 个测试文件计入；451 条未覆盖登记 | 绿（可达性口径，非行覆盖） |
| 7 | format | 78 ms | 棘轮：只查暂存文件 | 绿（见 §6 的 CI 空转说明） |
| 8 | audit | 160 ms | prod 依赖 20 / dev 依赖 21；1,061 文件；22 个第三方包 | 绿 |
| 9 | dup | 965 ms | 76 个节点 kind 各一份，**0 重复拷贝** | 绿 |
| 10 | catalog | 753 ms | 76 节点 / 76 条 en-US 文案 / 0 缺 | 绿 |
| 11 | identity | 716 ms | 76 kind 唯一、46 toolName 唯一；4 条 warning（`loop` count、`wait_input` timeout、`time_op` timestamp、`time_read` timestamp 输入输出同名） | 绿 |
| 12 | userdata | 102 ms | 3 文件 6 处直连用户目录 | 绿 |
| 13 | vendor | 43 ms | 22 个 vendored 文件 + 5 个 adapter；上游快照不可得，只校验目标哈希 | 绿 |
| 14 | syncio | 50 ms | 14 处 / 3 文件 | 绿 |
| 15 | cycles | 416 ms | 1 个 SCC（11 文件，`agent-core`），基线 1，fresh 0，ghost 0 | 绿 |
| 16 | docsstats | 91 ms | 37 条文档数字一致 | 绿 |

合计约 **18–20 s**（各闸门自报耗时之和，非墙钟）。

### 构建 / 开发循环实测与 Agent 易踩坑（对应问题 6）

| 动作 | 命令 | 实测耗时 | 说明 |
|---|---|---|---|
| 类型检查 | `node scripts/typecheck.mjs` | **10,228 ms**（node 2.1 s + web 8.1 s） | 两套 tsconfig 串行，失败时把完整日志写到 `_recovery/typecheck-*.log` |
| 16 道静态闸门 | `node scripts/check-quality.mjs` | **≈18–20 s**（各闸门自报之和） | 其中 `clones` 3.0 s、`dup` 1.0 s、`catalog` 0.75 s 是主要开销 |
| 全量单测 | `node .../vitest.mjs run --maxWorkers=2` | **340.9 s**（用例本身 138 s，`setup` 123 s，`environment` 253 s） | 环境/初始化占 2/3 以上 |
| 构建 | `electron-vite build` | **未跑** `[未验]` | 会重写 `out/`，违反只读约束 |
| 打包 | `electron-builder --win` | **未跑** `[未验]` | 会写 `release/`，且依赖签名/NSIS |
| 开发启动 | `pnpm dev` = `ensure-sqlite electron && electron-vite dev` | 未跑 `[未验]` | 热更新由工具提供：renderer 走 Vite HMR；main/preload 变更由 electron-vite 重建并重启应用（`electron.vite.config.ts` 未固定 dev server 端口，冲突时由工具自行换端口）`[读码]` |

> CI 注释称整轮（闸门 + 测试 + build）约 4 分钟，但**单测试一项实测 340.9 s（5.7 min, `--maxWorkers=2`）**，CI 配置里同样是 `--maxWorkers=2` → 该注释数字至少已过期，重写时不要把 CI 预算建在它上面 `[实测 vs 注释]`。

**Agent / 新人最容易踩的 10 个坑**（每条都给了代码或命令位置）

1. **原生 ABI 来回翻**（R4）：跑过 `pnpm test` 后直接 `pnpm dev` 会因 `better-sqlite3` 是 Node ABI 137 而 Electron 需要 146 起不来。必须先 `node scripts/ensure-sqlite.js electron`——`dev`/`pack` 脚本已内置，但直接调 `electron-vite dev` 就会踩。
2. **类型检查别用 `pnpm run typecheck`**：本机 pnpm 的依赖状态检查在无 TTY 时会中止；用 `node scripts/typecheck.mjs` 或 `.\\node_modules\\.bin\\tsc.cmd`（仓库给的就是这条路，`package.json` 的 `typecheck` 脚本即 `node scripts/typecheck.mjs`）。
3. **格式闸门在本地/CI 语义不同**（R12）：`check-quality` 的 `format` 项只读 `git diff --cached`，不改暂存区就等于不检查；CI 里另有 `.github/scripts/check-ci-format.mjs` 按 base 比对。改了格式却"闸门全绿"是常见误判。
4. **`vitest.config.mts` 的 `include` 一收窄，测试会静默消失**：当前 include 是 4 条（`tests/**/*.test.ts(x)`、`src/main/__tests__`、`src/main/services/builtinNodes/__tests__`、`src/**/*.client.spec.{ts,tsx}`）。任何"优化"都要用 **531 文件 / 6,399 用例**这两个数对账，否则会出现"跑得飞快"的假绿。
5. **测试不许写 `projects/`**：`tests/setup/globalGuard.ts` 在整轮运行前后给 `projects/` 下每个文件拍哈希，写到真实用户项目就整轮红（而且单看某个用例是绿的）。
6. **含空格的仓库路径**（`H Bot 制造`）会让 `node-gyp` 失败，所以 `electron-builder.yml` 必须保持 `npmRebuild: false`；改这一条前先确认换机器/换路径。
7. **`src/main/services/ipc` 三处手写**（R5）：新增通道要同时改主进程 handler、`src/preload/index.ts`、`src/renderer/src/types/ipc.ts`，漏一处分别是"运行期报错"和"TS 调不到"。
8. **生成物不要手改**：`ai/capabilityCatalog.generated.ts`（3,174 行，`scripts/gen-capability-catalog.mjs`）、`shared/nodes/i18n/catalog.en-US.ts`（由 `scripts/i18n-build-catalog.mjs` 产出）、`tests/guards/*-baseline.json`（用各闸门 `--update`）。手改会在下一次生成/闸门更新时被覆盖。
9. **88 个测试文件直接读源码文本**（`readFileSync` + 断言）：重命名文件/改导出名会以"莫名其妙的字符串断言失败"出现，而不是类型错误。改结构性东西前先 `grep` 一下 `tests/guards`。
10. **Windows 上 `rename` 覆盖目标可能 EPERM**（不是写失败）：`atomicWrite`（`src/main/services/infra/fileOps.ts`）+ `retryIo.ts`（EPERM/EACCES/EBUSY 重试）就是为此存在；自己新写"写临时文件再 rename"的地方要复用它们，别当写失败吞掉。

### 结构债 vs 数据表：逐条判读

**不是债（按闸门自身口径也不该拆）：**

- `shared/nodes/i18n/catalog.en-US.ts` 6,074 行：纯文案表，零分支。
- `ai/capabilityCatalog.generated.ts` 3,174 行：生成物（`scripts/gen-capability-catalog.mjs`）。
- `src/main/services/ai/tools/registry.ts` 1,650 行：handler 注册表（装配）。
- `mcp/server.ts|createMcpServer` 450 行：**data-table**（工具清单）。
- `ConfigFieldsSection.tsx` 401 行：**data-table**（字段描述）。
- `renderer/src/lib/launch/dataBrowserData.ts` 2,083 行：数据。
- `shared/nodes/groups/message.ts` 1,140 行：节点声明分组。

**是债（但量级不大）：**

- `src/main/runtime/FlowInterpreter.ts` **1,737 行**：单文件承载表达式求值 + 节点分派 + 插话/暂停/取消状态机；同目录 `FlowRuntime.ts` 461 行负责 WS 连接/重连/待决表/调度。两者合计 ~2,200 行构成运行时内核，**这是全仓最该按职责拆的内核文件**（`fnlen` 看不到，因为能力散在类方法里）。
- `botManager.ts` 类：`ensureBotRunning` + `attachRuntime` + 平台翻译器分支（飞书/钉钉/官方）在一个类里，改写时按"平台装配器"抽出去。
- `preload/index.ts` 1,421 行 / 309 通道：宽度债，靠生成消掉。
- `agent-core` 11 文件 SCC + `agent-engine` 新骨架：两套循环并存（`QQBAT_AGENT_ENGINE` 开关），是**重写最大的直接动因**。

**函数级：举证 7 条 ≥400 行**

| 函数 | 行数 | kind |
|---|---|---|
| `BlockEditor.tsx|BlockEditor` | 959 | assembly |
| `CodePanel.tsx|CodePanel` | 500 | assembly |
| `TopBar.tsx|TopBar` | 458 | assembly |
| `mcp/server.ts|createMcpServer` | 450 | data-table |
| `renderer/src/stores/ai/applySlice.ts|createApplySlice` | 403 | single-transaction |
| `ConfigFieldsSection.tsx` | 401 | data-table |
| `agent-runtime/projectTurn.ts|runProjectTurn` | 500 | single-transaction |

`multi-capability` 为 0 意味着：**按本仓库自己的判据，函数级技术债已清零**。所以"文件大"不能直接作为重写理由；重写理由应落在装配重复、平台耦合、循环骨架三件事上。

---

## 测试迁移建议

### 现状（`[实测]`）

- 命令：`node node_modules/vitest/vitest.mjs run --maxWorkers=2` → **531 文件**（529 passed / 2 skipped）、**6,399 用例**（6,395 passed / 4 skipped）、**340.90 s**。
- 分项：transform 12.45 s、`setup` **123.46 s**、import 81.26 s、tests **137.96 s**、`environment` **253.04 s**。
- 分布：`tests/main` 290、`tests/renderer` 154、`tests/shared` 42、`tests/guards` 39、`tests/runtime` 2、`_smoke` 1、`e2e` 1（死）、根 1；另 `src/**/__tests__` 2。
- mock 浓度：`vi.mock(` **232 处 / 60 个文件**（其余约 470 文件是纯函数/契约测试）；mock 调用断言 352 处 vs 全部 `expect(` **11,498** 处（≈3%）。
- 快照：`toMatchSnapshot` / `toMatchInlineSnapshot` **0 处**（全仓）。
- 结构型测试：**88 个测试文件用 `readFileSync` 读源码文本**（闸门矩阵），这是"读到源码就断言"的一类，属于**结构闸门**而非行为测试。
- 私有大文件（迁移时优先重写而非搬运）：`projectWritesViaFileOps.test.ts` 58,573 B、`remaining-ipc-3` 57,805 B、`deadExports` 50,812 B、`aiAgentCore-ipc` 46,660 B、`remaining-ipc-1` 39,580 B、`state-metrics-runtime-ipc` 38,355 B。

### 哪些是"实现镜像"，哪些是行为契约

| 类别 | 例 | 判断 | 迁移方式 |
|---|---|---|---|
| 纯函数/领域规则 | `tests/shared/*.test.ts` 42 个（`flowNormalize`、`varRef`、`policyProfiles`、`streamAssembler`、`goalView`、`windowView`、`market`、`syntax` 等） | **行为契约**，输入输出明确 | **直接迁移**，改 import 路径即可，是黑盒验收的现成资产 |
| 节点执行语义 | `tests/main/runtime/*`（`miscNodesE2E`、`nodeDeadline`、`configQueryNode`、`handlerCoverage`）、`tests/main/services/builtinNodes/*` | **接近行为契约**（按节点 kind 给输入/期望输出） | 迁移为"节点契约测试套件"：76 个 kind 每个至少 1 组 IO 用例 |
| 结构闸门 | `tests/guards/*` 39 个（IPC 对账、deadExports、capabilityLayering、toolDefinitionDispatchSync、cycles/fnlen/any 基线…） | **不是行为测试**，是"仓库形状"断言 | **不要搬运实现**；重写时按新架构重新表述，保留 4 条硬闸门（IPC 三处一致性、节点目录唯一性、依赖方向、原生 ABI） |
| IPC 处理器测试 | `tests/main/ipc/*`（`remaining-ipc-1/2/3`、`aiAgentCore-ipc`、`botManager-ipc`…） | **半镜像**：大量 mock 掉 service 层，断言的是"handler 如何调用 service"（调用形状而非用户可见结果） | 重写为**黑盒**：直接构造 project 目录 + 调用域服务，断言磁盘/DB/返回值；或改用"通道契约测试"（每个通道：合法参数 1 例 + 非法参数 1 例） |
| 组件测试 | `tests/renderer/**` 154 个；`ChatParts.test.tsx` 28 个 mock（子组件全替成 marker，断言"拼装顺序"） | **部分镜像**：断言的是当前组件拆分方式，拆分一变全红 | 只迁移**用户可见行为**（空态/禁用/错误提示/i18n key），拼装顺序类断言丢弃 |
| e2e | `tests/e2e/node-flow.spec.ts` 12,528 B，`import { test, expect } from '@playwright/test'`，package.json 无 playwright | **死代码**：不在 vitest include（只收 `tests/**/*.test.ts(x)` 与 `src/**/*.client.spec.{ts,tsx}`），tsconfig.node include 也不含 `tests/e2e`，从无 CI 命中 | 重写时要么删，要么补 playwright 依赖并接入 CI；不要假装"有 e2e" |

### 迁移优先级（按 ROI）

1. **P0 直接搬**：`tests/shared` 42 个 + `tests/main/services/builtinNodes` 若干 → 它们不依赖 Electron、不依赖装配，是重写后的第一道回归网。
2. **P0 重建**：`tests/guards` 里的 4 条硬契约（IPC 三处一致、节点 kind 唯一、依赖方向零违规、原生 ABI 探测）。这四条重写后必须**第一周就存在**，否则新仓库会立刻开始漂。
3. **P1 重写为黑盒**：IPC/服务层测试。建议形态：以"项目目录夹具 + 真实 SQLite + 真实文件系统"为输入，断言对外可观察结果（`projects/<name>/.qqbat/*.json`、事件总线消息、IPC 返回体）。这类测试可直接充当验收脚本。
4. **P1 补真空缺**：现有套件**没有**覆盖到"打包产物能被启动"（无 smoke）、"headless 能在 Linux 容器里跑"（无 docker 测试）、"升级/回滚"。重写时至少补：`build → 启动 → 首窗/首连 → 退出` 的 smoke（Electron 可用 `--headless`/xvfb 或直接用 `standalone` 入口做 CI 冒烟）。
5. **P2 丢弃**：拼装顺序类组件断言、`readFileSync` 式结构断言（用 lint/依赖图工具替代）、以及所有"断言注释/文档数字"的闸门。

### 测试耗时结构（指导重写时的测试分层）

`environment 253 s` + `setup 123 s` 说明**绝大多数用例跑在 jsdom 里但根本不需要 DOM**（`tests/main` 290 个、`tests/shared` 42 个）。重写时用 vitest projects 分三档：`node`（服务/纯函数）、`jsdom`（组件，约 154 文件）、`e2e`（真进程）。仅此一项，反馈环预计可从 **340 s 降到 60–90 s**（`[推断]`，依据是用例自体耗时仅 137.96 s）。

---

## 风险清单

严重度：P0 = 重写前必须解决或明确继承；P1 = 影响交付节奏/可运维性；P2 = 局部质量。

| # | 严重度 | 位置 | 触发条件 | 影响 | 最小修法 | 证据类型 |
|---|---|---|---|---|---|---|
| R1 | **P0** | `src/main/services/bot/botManager.ts:182-286` vs `src/main/runtime/headlessAssembly.ts:64-137` | 同一项目同时被桌面与 Docker/SSH 部署使用；或设置了 `QQBOT_BUS_PORT` | 两个入口用不同端口解析（桌面不读 `QQBOT_BUS_PORT`）、桌面回写 `project.json` 覆盖容器配置；装配逻辑双份维护，改一处漏一处 | 抽 `RuntimeAssembly.start(opts)` 单一实现，桌面/headless 只提供 `Host`（status/log 回调 + 是否回写端口）；端口策略收敛到一个函数 | 读码 + 实测（grep `QQBOT_BUS_PORT` 仅 3 处，桌面无） |
| R2 | **P0** | `out/main/chunks/FlowRuntime-*.js`（源：`services/official/config.ts:2`、`infra/safeStorage.ts:2` 静态 import electron） | Linux 上直接 `node out/main/standalone.js`，或部署时未执行 `nodeInstallShell`（生成 electron stub） | `MODULE_NOT_FOUND: electron`，headless 启动即死；即便跑起来，`electron.app` 为 undefined，任何未加 `?.` 的访问会崩 | 新仓库设"平台层"：唯一 `electron` 入口 + 全部懒加载/`import type`；headless 构建用独立 alias 把平台层替换成纯 Node 实现 | 产物（顶层 `require("electron")`）+ 读码（`runtimeBundle.ts:30-40` 造 stub 即为绕开此问题） |
| R3 | **P0** | `src/main/services/agent-core/acl/confine.ts:116`（非 win32 直接 `return null`）、`agent-core/sandbox.ts:210-240` | 在 Linux/macOS 上跑 agent 命令；或 Windows 机器缺 PowerShell 导致建不出受限令牌 | `workspace-write` 退化为 6 个命令名白名单（`FALLBACK_COMMANDS` at `sandbox.ts:66`）；`read-only` 在没有 ACL 时**落到普通 spawn**，只剩命令名检查（`sandbox.ts:166` 的 `readOnlyDenial` 仅拦 git 子命令以外的名字） | 新仓库把沙箱做成平台适配器：Windows 用受限令牌，POSIX 用 `bwrap`/`nsjail`/容器；无适配器时**显式拒绝**而不是静默降级 | 读码（分支结构清晰可验）；未在非 Windows 实跑 |
| R4 | **P1** | `scripts/ensure-sqlite.js`（ABI 137 ↔ 146 单二进制） | 先 `pnpm test`（切到 Node ABI）再 `pnpm dev` / 打包 | 桌面启动报 `better-sqlite3` ABI 不匹配；新人第一小时必踩 | 短期：`dev`/`pack` 脚本前置已存在（保持）；长期：`node:sqlite` 全量替代或按 runtime 分两份 `.node`（`build/Release/better_sqlite3-{node,electron}.node` + 运行时选择） | 实测（`node_modules/better-sqlite3/build/Release/better_sqlite3.node` 1,919,488 B；`ensure-sqlite` 探测输出 "ABI 137"）+ 读码 |
| R5 | **P1** | `src/main/ipc/**`（309 通道）+ `src/preload/index.ts`（308 invoke）+ `src/renderer/src/types/ipc.ts`（2,235 行） | 新增/重命名一条通道，漏改三处之一 | 类型里有、preload 没有 → 运行期 `is not a function`；preload 有、类型没有 → TS 用户调不到（能力存在但不可见） | 新仓库由 zod schema 生成三处（handler 签名 + preload + 渲染层类型），禁止手写通道字符串；保留 AST 对账作为回归 | 实测（双向差集 0、0 重复）+ 读码（`tests/guards/ipcSurfaceParity.test.ts` 头注释描述的"三处手写"） |
| R6 | **P1** | `src/main/services/agent-core/**`（11 文件 SCC）+ `src/main/services/agent-engine/**`（新骨架） | 修改任何"一轮对话"的行为 | 两套循环并存（`QQBAT_AGENT_ENGINE` 开关），判据共用但骨架重复；修 bug 容易只修一边；`agent-core` 内部 11 文件互相 import，改一处牵连全环 | 新仓库只保留 `agent-engine` 形态，把 `toolPipeline`/`contextGate`/审批/参数闸作为**单一端口**注入；SCC 必须打散为 `loop → ports → adapters` 单向 | 实测（cycles 闸门 1 个 11 文件 SCC）+ 读码 |
| R7 | **P1** | `package.json` 依赖 `node-pty ^1.1.0`；`pnpm-workspace.yaml` 的 `node-pty: false` | 在 Linux 上使用终端面板，或想在 CI 里真跑终端 | `node-pty` 预编译只有 darwin/win32（arm64+x64），**无 linux-x64**；且 `allowBuilds: false` 意味着永不从源码构建 → Linux 上终端功能必然不可用（只能靠 mock 测） | 新仓库把终端做成可选能力：能力探测 + 显式禁用 UI；或改用 `@lydell/node-pty`/`child_process` + `pty` 替代方案并在 Linux CI 里真跑 | 实测（读 `node-pty` 包内 `prebuilds/` 目录清单）+ 读码 |
| R8 | **P1** | `tests/e2e/node-flow.spec.ts`（12,528 B，playwright 依赖未声明，三重排除） | 任何人把"测试全绿"当作"可交付" | 打包/启动/IPC 端到端零自动化覆盖；e2e 文件给人"已覆盖"的错觉 | 删掉或在重写仓库里用 playwright + `_electron` 真跑 3 条冒烟（首窗、新建项目、运行一次流程） | 实测（include/tsconfig 双向确认 + 依赖表无 playwright） |
| R9 | **P1** | `src/main/services/plugins/**`（27 文件，`sandboxPluginActivator.ts` / `sandboxPluginBridge.ts`） | 安装第三方/AI 生成插件 | 隔离是**主进程内 `node:vm`**：`codeGeneration` 已关、有 5 s 超时与 JSON 信封，但同进程意味着原型链/`Function` 逃逸面、CPU 抢占、内存无上限；与仓库里已有的 `untrustedWorker`（真子进程）不是同一套 | 插件执行统一收到子进程（复用 `untrustedWorker.ts` 的双传输），主进程只留 manifest 校验与代理 | 读码（`host.ts` 头注释自述"边界仍是主进程内 vm"；`kernel.ts`/`sandboxPluginBridge.ts` 实现）+ 未做逃逸验证 |
| R10 | **P2** | 176 个"还原"文件（其中 134 个贡献 2,096 处 `any`，占全仓 94%） | 在这些文件上做增量修改 | 类型信息缺失，改动靠猜；风格与 `prettier` 基线冲突（`tests/guards/prettier-baseline.txt` 现有 711 条存量不合规） | 重写取代；若要保留，逐文件补接口并缩短棘轮基线 | 实测（首行标记计数、`any-baseline.json` 分桶、prettier 基线条数） |
| R11 | **P2** | `src/main/services/plugins/pluginScanner.ts:94` | 读取插件 `author` 字段 | `author: manifest.permissions?.includes("author") ? void 0 : void 0` 恒为 `undefined`（还原残留的死三元），作者信息永远为空 | 一行修正或删除该字段 | 读码 |
| R12 | **P2** | `scripts/prettier-gate.mjs`（`check-quality` 的 `format` 闸门） | 在 CI 里依赖 `check-quality` 的 format 项 | 本地跑时只读 `git diff --cached`，CI 无暂存区 → **该闸门在 CI 中空转**；真正生效的是 `.github/scripts/check-ci-format.mjs` | 把两条合成一条：`--all` 模式按 base 比对，而不是只读暂存区 | 实测（读 `check-quality.mjs:32` 与 gate 脚本）+ 读码 |
| R13 | **P2** | `src/main/services/official/config.ts:2`、`infra/safeStorage.ts:2` | 任何 headless/测试环境 import 到这两条链路 | 把 `electron` 变成硬依赖（见 R2 的根因）；测试里必须 mock 整个 electron 模块 | 改成从平台层取 `safeStorage` 的窄接口，注入而非 import | 读码 + 产物 |
| R14 | **P2** | `src/main/services/infra/sqlite.ts`（`node:sqlite` 降级） | `better-sqlite3` 加载失败 | 自动降级到 `node:sqlite`，两者 SQL/参数绑定行为不完全一致（`better-sqlite3` 支持同步 prepared + 事务函数），降级后某些查询语义可能变 | 保留降级但**记录并暴露**（启动横幅 + 日志 + 容量上限），或干脆不降级（fail fast） | 读码 |

---

## 重写切分建议

原则：**保留边界纪律，替换装配与平台耦合，重建验收**。不要为了"重写"重写 `src/renderer`（79.5 K LOC、0 越界、UI 价值高）。

### M0 契约冻结（1 周，纯文档+生成物）

- 冻结 5 份机读契约：① 309 条 IPC 通道表（名称/入参 schema/出参/副作用类别）② 76 个节点 kind + 参数 schema（现成 `catalog`/`identity` 闸门的输入）③ 项目磁盘格式（`.qqbat/project.json`、`projects/<name>/**`）④ 插件 manifest（`id/version/permissions/requires/entry`）⑤ 事件总线消息（`runner/eventBus.ts` 的 WS 协议）。
- 产出物：`contracts/*.json`（由现有代码反向导出一次即可，`scripts/` 里已有 `export-agent-framework`、`gen-nodes-reference` 之类可复用）。**这一步不做完，后面所有模块都是猜。**

### M1 平台层（2 周，收益最大）

- `platform/electron.ts`：唯一 `electron` 引用点；导出窄接口（`paths` / `safeStorage` / `app` / `dialog` / `shell`）。
- `platform/runtime.ts`：`RuntimeAssembly` 单一装配（合并 R1），`Host` 接口区分 desktop / headless / test。
- `platform/sqlite.ts`：双 ABI 选择或 `node:sqlite` 替代（消 R4）。
- `platform/sandbox.ts`：命令沙箱平台适配器（消 R3），**无适配器时拒绝执行**。
- 验收：`headless` 产物在 `node:22-slim` 里 `node dist/standalone.js <project>` 直接跑通（无 electron stub）。

### M2 运行时内核（3 周）

- 拆 `FlowInterpreter.ts`（1,737 行）为 `ExpressionEval` / `NodeDispatch` / `TurnControl` 三块，`FlowRuntime.ts`（461 行）保留传输与调度，接口先行。
- 节点执行：76 个 kind 一个注册表 + 一个执行器，**handler 签名统一**（现 `runtime/handlers/registration.ts` 64 处 `any`、`kwargs.ts` 55 处是反例）。
- 验收：`tests/shared` 迁移版 + 每 kind 一组 IO 用例（现 `miscNodesE2E` 已有雏形）。

### M3 服务域（4 周，按依赖方向切）

顺序按"被依赖最多、最稳定"优先：`infra` → `project` → `runner/eventBus` → `official`/`translator`(lark/dingtalk) → `bot` → `ai`（provider/流式/工具）→ `agent-engine` → `mcp` → `plugins` → `ops`（git/ssh/docker）。

- 规则：域间只允许 `domain → domain-api`（接口文件），禁止见 `impl`；目录级双向边在 CI 里用 `dependency-cruiser` 或扩展现有 `check-cycles` 守住（现在的 `check-cycles` 只查文件级 SCC，**建议加"目录级双向边不新增"的棘轮**）。
- 反面教材：`ai↔agent-core`、`agent-engine↔agent-core` 的双向（R6），新仓库一律单向。

### M4 前端（3 周，改造而非重写）

- 保留 `src/renderer` 结构（components/lib/stores 分层清楚）。
- 两件事必须做：① IPC 调用改为生成客户端（消 R5 手写）；② 面板 `lazy` 分包策略保留（仓库记录：静态 import 九个页签时主 chunk 2,729.8 kB，改 lazy 后 2,289.1 kB；该数字来自仓库文档，本次未复测）。
- 组件测试重写为行为断言（见 §测试迁移里"组件测试"一行）。

### M5 验收与发布（2 周，与 M2 并行开始）

- 三层测试：`node`（服务/纯函数，目标 <60 s）、`jsdom`（组件，目标 <90 s）、`e2e`（playwright + `_electron`，3–5 条冒烟）。
- 一条命令的交付闸门：`typecheck + lint + unit + e2e-smoke + build + package` ≤ 12 min。
- 打包矩阵：至少 win-x64 + linux-x64（`node:22-slim` 容器内跑 headless 冒烟），原生模块在 CI 里**真加载一次**（不要只 `require`，要 `new Database(':memory:')`——这是现有 `ensure-sqlite.js` 的正确做法，值得继承）。

### 明确"不要带过去"的清单

1. 桌面/headless 双份装配（R1）。
2. `electron` 静态 import 进领域代码（R2/R13）。
3. 单 ABI 二进制来回切（R4）。
4. 309 条通道手写三处（R5）。
5. `agent-core` 11 文件 SCC + 新旧双循环（R6）。
6. `node-pty` 作为必备依赖（R7）。
7. 死 e2e 与"读源码文本"式结构断言（R8/测试 §）。
8. 2,230 处 `any` 的还原代码（R10）——重写是唯一合算的清理方式。
9. 711 条规模的 prettier 棘轮基线（新仓库从第一天全绿，不要继承棘轮）。

### 值得原样继承的清单

1. 四条边界与"跨边界不许 `../../`"的别名纪律（`@shared/@main/@renderer` 双处声明）。
2. `check-quality` 的闸门编排思路（一个数组串起来，`--all` 追加测试）。
3. 沙箱的"真探测"哲学：`ensure-sqlite` 用真建库验证、`vendor-audit` 用哈希锁字节、`ipcSurfaceParity` 用 AST 而非正则在文本上对账。
4. 节点目录"76 kind 唯一 + 46 toolName 唯一"的单一真源做法。
5. 窗口/面板的几何单一真源（`panelGeometry.ts`）与 `uiStore` 的 clamp 区间契约。

---

### 附：本次未能取得的证据（供后续补齐）

| 缺什么 | 为什么没做 | 建议怎么补 |
|---|---|---|
| 构建耗时、打包耗时、安装包体积 | `electron-vite build` / `electron-builder` 会改写 `out/`、`release/` | 在 `E:\HBot-Next` 或仓库副本上跑一次并记录 `Measure-Command` |
| 冷启动到首窗时间 | 需要启动 Electron（会写 `_devdata`、可能触发单实例锁） | 用 `--user-data-dir` 指向临时目录 + `performance.mark` 打点 |
| Windows ACL 沙箱的真实隔离效果 | 实跑会在 `%TEMP%` 建目录并编译 C#（约 1.5 s/命令） | 在隔离机跑 `agent-core/acl` 的既有测试并记录 `INTEGRITY_SKIPPED` 日志 |
| Linux 容器内 headless 实跑 | 本机无 Docker/Linux 环境（`dockerExport` 只生成了 Dockerfile 文本） | 用 `dockerExport` 产出的 Dockerfile 在 CI 里构建并跑健康检查 |
| 插件 `node:vm` 逃逸面 | 需要写攻击样例并执行（会被判为"修改/执行不可信代码"） | 单独安全评估，别在架构审查里顺手做 |