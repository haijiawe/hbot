# 旧项目 Agent 安全审查（只读，代码级证据）

审查对象：`E:\H Bot 制造`（QQ 机器人低代码 Electron 应用）。方法：只读实现与测试，**未读 docs/、README、历史报告**。
审查时间：2026-11-16。所有 `文件:行号` 均相对于 `E:\H Bot 制造`。本文件只写证据与判断，不改源仓库任何文件。

范围：`src/main/services/agent-core/`、`services/agent-engine/`、`services/ai/`、`src/main/mcp/`、`src/main/ipc/ai/`、`agent-core/acl/`、`runtime/sandbox.ts`、`runtime/untrustedWorker.ts`、对应 `tests/`。

---

## 结论

1. **判据的主干是成立的，但"只有一份判据"这个前提在代码里已经不成立。** 审批、只读档、参数闸、角色写域四道判据原本都在 `services/agent-core/toolPipeline.ts` 里；第 191 批把默认执行路径切到 `services/agent-engine/loop.ts` 之后，新引擎在 `services/agent-engine/tools/exec.ts` 里**重写**了这条链（只复用了 `readOnlyDenialOf` 一个函数）。两份实现当前在"三类工具 + 只读档"上结论一致，但**没有机制保证以后一致**：新引擎不调用 `authorizeToolCall`，而旧引擎不知道 `withTimeout`/fail-closed 的写法（`agent-engine/tools/exec.ts:187-205` vs `agent-core/toolPipeline.ts:225-259`）。
2. **P0 只有一条，且不需要任何开关：审批"以后别再问"的记账键用错了粒度。** `ipc/ai/aiAgentCoreApproval.ts:181` 把 `ruleKind`（`'write' | 'command' | 'tool'`）塞进本轮的 `approved` 集合，`:153` 在比较 `match` **之前**就 `approved.has(ruleKind)` 命中直接放行。后果：用户在 `a.txt` 的写审批上点一次"以后别再问"，**同一轮里任何路径的写、任何命令都不再弹窗**（跨轮落盘的 `allowRules` 仍是精确匹配，见 `services/ai/allowRules.ts:57-62`）。这是用户可见的按钮语义被放大，不是"用户自己开的危险开关"。
3. **只读档（`read-only`）在本地工具上是硬的，在 MCP 注入工具上是空的。** `agent-core/toolPipeline.ts:144-153` 只拦 `category === 'mutation'`（除非它声明的是 `kind:'command'` 审批）；`mcp/client/manager.ts:120` 把**所有**远端 MCP 工具标成 `category:'query'`，`manager.ts:125-139` 的 handler 直接 `conn.callTool`，不传 `ctx.signal`、不查 writeGuard、不经沙箱、不声明审批（`agent-core/legacyTools.ts:135-143` 对非 `job_start` 一律 `return null`）。所以"只读档"对 `mcp__filesystem__write_file` 完全无效。源码注释自己承认这一点（`mcp/client/manager.ts:105-118`），前提是用户在 `config/mcp-client.json` 里显式启用 server（`client/config.ts` 的 `enabled` 默认 `false`）→ 按题面规则定 **P1**。
4. **`job_start` 是第二条命令通道，完全绕开指令沙箱。** `services/ai/tools/registry.ts:863-887` 只检查 `sandboxAllowsWrites(currentSandboxMode())`，然后 `services/agent-core/jobs.ts:184-187` 以 `shell: true` 起子进程，**不经过** `agent-core/sandbox.ts` 的 `FORBIDDEN` 字符检查，也不经过 `acl/confine.ts` 的受限令牌。注释（`registry.ts:852-861`）明说这是有意为之（要管道与 `&&`）。它要过一次 `kind:'command'` 审批（`legacyTools.ts:135-143`）→ **P1**（需要用户点一次允许）。
5. **角色写域这道闸事实上已经不存在，但注释与文档仍在声称它存在。** `shared/ai/roleDomains.ts:77-97` 把每个角色的 `ROLE_WRITE_DOMAINS` 都写成"全部"，`:155-157` 的 `mayRoleWritePath` 因而恒为 `true`；唯一的调用点在提案路径 `services/ai/proposal.ts:8`。`ipc/ai/aiAgentCoreApproval.ts:112`/`:127` 仍写"角色的写域闸在最前"，`services/agent-runtime/projectTurn.ts:105` 同口径 —— **代码里没有这个判断**。所以"角色写域"既不是漏洞也不是防线，重写时要么真做，要么删掉这套说法（别留一条恒真的"安全闸"骗后来人）。
6. **沙箱边界是内核强制的，但只在"命令"这条链上，且失败时是拒绝而不是降级**（`agent-core/sandbox.ts:210-239`、`acl/confine.ts:157-160` 的 `DOWN_TEXT`）。写路径上的 `resolveInWorkspace`（`agent-core/workspacePath.ts:12-20`）+ `FsObservationGate`（`agent-core/fsObservation.ts:80-93`，"没读过就不许覆盖已存在文件"）+ before-image 记账是可用的**应用层**护栏，但它挡不住工作区内的目录联接（junction），也不挡 `write_file` 之下的渲染进程落盘（`registry.ts:148-170` 只下发 `clientInstruction`，真正的 `fs.writeFile` 在渲染层，主进程的 fsGate 管不到）。
7. **凭据的主要问题不是"泄漏到日志"，而是"没有 safeStorage 时明文落盘且不告警"。** `services/ai/appAiConfig.ts:97-106` 与 `services/ai/projectAiConfig.ts:97` 都是 `encrypt ? encryptSecret(...) : plaintext`；同一个仓库里 `agent-core/speechConfig.ts:169` 是相反口径（`拒绝以明文保存 API Key`）。日志出口有脱敏（`services/infra/logger.ts:181` → `shared/security/redact.ts`），trace 不记正文（`services/ai/trace.ts`），诊断导出对项目配置脱敏（`services/ops/diagnostics.ts:303/:316`）—— 这三处是干净的。MCP client 的 `env`/`headers` 则是明文 JSON（`mcp/client/config.ts`），与 apiKey 的处理不一致。
8. **成本与上下文有上限，但上限是"每个工具一次"，不是"每一轮/每一轮的子代理"。** 工具结果 8 KB 阈值 + 溢出落盘（`services/ai/toolResultBudget.ts`、`services/ai/spillStore.ts`）、循环预算 80k token / 80% 触发压缩（`agent-core/contextGate.ts`）都成立；但新引擎的 `usage = next`（`agent-engine/loop.ts:322`）是**覆盖**而非累加，旧循环 `agent-core/core.ts` 干脆没有 `onUsage` —— 多步任务的 token 账会少算。工作流默认允许 1000 个代理（`agent-core/workflowEngine.ts` 的 `DEFAULT_MAX_TOTAL_AGENTS`），每个代理最多 8 轮模型调用，**没有本轮总预算闸**。
9. **取消链路在"用户点停止"这条路上是通的，在"应用退出"和"工具卡死"这两条路上不通。** `ipc/ai/aiAgentCore.ts:63-66` 会 `abort()`；但 `src/main/index.ts:422-503` 的 `before-quit` 没有 abort 正在跑的那一轮（只杀 jobs、子代理、LSP、终端）。超时是 `Promise.race`（`agent-engine/tools/exec.ts:232-236`、`agent-core/toolTimeout.ts`），超时后工具**继续在后台跑**；`mcp` 的远端调用连 signal 都没传（`manager.ts:125-139`）。
10. **测试能证明"只读档挡得住本地写工具/命令工具"和"Windows 受限令牌真的在写盘层面生效"，不能证明"审批链端到端"。** 见最后一节。

---

## 两套循环差异表

开关：`services/agent-runtime/projectTurn.ts:114-118` 的 `engineEnabled(config?)` —— `process.env.QQBAT_AGENT_ENGINE === '0'` → 旧；`config?.provider === 'ollama'` → 旧（`agent-engine/projectTurn.ts:118` 注释：Ollama 原生 `/api/chat` 与端口的 OpenAI 兼容路径不兼容）；否则 → 新。**默认新引擎。** 只有主聊天这一轮翻转：`subagent` / `workflow` / `ralph` 直接调 `runAgentTurn`，**永远走旧循环**（`agent-core/subagent.ts` 调 `runAgentTurn`、`workflowEngine.ts` 用 `startSubagent`、`ralph.ts` 每轮起子代理）。`ipc/ai/aiAgentCoreOnboarding.ts:82` 的引导轮也是旧循环。

| 能力 | 旧循环（`agent-core/core.ts:814-1031` + `toolPipeline.ts`） | 新引擎（`agent-engine/loop.ts` + `tools/exec.ts`） | 差异要点 |
|---|---|---|---|
| 骨架 | `runAgentLoop` | `runEngine` | 同为 `问模型 → 跑工具 → 推一轮`，进度事件与端口抽象不同 |
| 工具来源 | `listTools()` 或 `opts.tools` | 同（`runTurn.ts` 组装的 `turnTools`） | 同一份 `turnTools`（`agent-core/turnTools/turnTools.ts`），插件作用域/分组/dropUnusableTools 都在上游 |
| 审批 | `authorizeToolCall`（唯一入口，`toolPipeline.ts:225-259`） | 自己实现（`exec.ts:140-205`），只复用 `readOnlyDenialOf` | **判据重复**；新引擎没复用 `authorizeToolCall` 这个"导出给后来者用"的函数 |
| 无 gate 时 | **放行**（`toolPipeline.ts:254` 的 `if (approval && ctx.gate)`） | **拒绝**（`exec.ts:190` 的 `ports.approval ? … : false`） | 行为相反；旧行为有注释说是给脚本/测试留的 |
| 只读档 | `readOnlyDenialOf`（`toolPipeline.ts:144-153`） | 同一个函数（`exec.ts:170-185`），且放在 `if (gate)` 外面 | 两边都是"硬拒、不弹窗"；**这条已对齐** |
| 参数闸 | `executeTool` 内（`ai/tools/execute.ts`）+ mutation 提案前的 `argGateMessage` | 同（handler 内部） | 唯一入口是 `executeTool`，新引擎同样经它 |
| 并行选择 | `executionModeOf(tool)`：`category==='query'` → parallel，否则 exclusive（`scheduler.ts:69-72`） | `parallelEligible`：还额外要求 `approvalRequestOf() === null`（`loop.ts:97-107`） | 新引擎把"需要审批的工具"从并行批里踢出 → 审批卡不会一次弹 N 张；旧循环**可能同时弹多张** |
| 并发上限 | `maxParallelToolCalls ?? 10`（`core.ts:71`、`:818-820`） | 同默认 10（`loop.ts:51`、`:484`），来自 `effectiveMaxParallelToolCalls()`（设置项上限 50，`ai/parallelToolCallsSettings.ts:23-25`） | 一致 |
| 上下文压缩 | `applyCompaction`（`agent-core/contextGate.ts`，80k 预算/80% 阈值/保最近 8 轮） | 同一函数的端口版（`loop.ts` 的 compact 事件） | 算法同一份 |
| 溢出重试 | 捕获溢出错误 → `compactForContextOverflow` → 重试一次 | 同 | 一致 |
| 检查点 | `checkpoint('step')` 与 `checkpoint('tool-dispatch')`，失败即整轮失败 | 同（`exec.ts:215-230`） | 一致 |
| 插话 | 步边界消费 `inbox.step`（`core.ts` 循环内） | `inbox()` 端口（`loop.ts:360-365`），并 `repeat.reset()` | 一致 |
| 取消 | `signal.aborted` → `cancelled`；`isAborted` 检查在工具前后 | 同 + `batch.aborted` | 一致 |
| 工具超时 | `runToolWithDeadline`（`agent-core/toolTimeout.ts`，`AbortSignal.any`） | `withTimeout`（`exec.ts:67-81`，纯 race，**不 abort 工具**） | 旧的一个用 `AbortSignal.timeout` 合并，两者都"回收不了"；新引擎连 signal 都不往工具里塞 |
| 文件观察门 | `fsGate: new FsObservationGate()`（`core.ts:834`） | `loop.ts:120` 自己造（`input.fsObservation && !extras.fsGate`） | 一致（这条我一开始怀疑丢了，实测是有的） |
| PTC 嵌套派发 | `nestedDispatch` → `judgeNestedCall` → `authorizeToolCall` → checkpoint → `executeToolCall`（`core.ts:855-893`） | `nestedDeps` + 静默事件端口，走 `executeCall`（`loop.ts:281-291`） | 新引擎的嵌套**不过 `authorizeToolCall`**，靠 `executeCall` 自己的审批/只读判据；事件静默（避免几十张工具卡） |
| 用量统计 | `core.ts` 无 `onUsage`（grep 无匹配） | `usage = next` 覆盖（`loop.ts:322`） | **两边都不对**：旧的不报，新的少算 |
| 错误分类 | `normalizeToolOutcome` + `ok` 前缀判断 | 同 + `isAbortError` 只看 `name`（超时是普通 `Error`，永远不会被当取消） | 新引擎的超时会被报成"抛出异常" |
| 轨迹/日志 | `trace.record` + `appendSession(output.slice(0,200))` | 由端口回传给适配层（`agent-engine/adapters/turn.ts`） | 需要实测确认字段齐全（见未知） |
| 覆盖链路 | 主聊天（`QQBAT_AGENT_ENGINE=0`）、`subagent`、`workflow`、`ralph`、onboarding、ollama | 只有主聊天（默认） | **改任何判据都要两处一起看** |

---

## 审批判据链

### A. 本地工具（主聊天，默认新引擎；旧引擎顺序等价）

按实际执行顺序，逐道 gate：

| # | Gate | 位置 | 判据 | 失败结果 |
|---|---|---|---|---|
| 0 | 工具可见性 | `agent-core/turnTools/turnTools.ts`（`buildTurnTools` → `scopeToolsByPlugins` → 允许名单 → `filterToolsByGroups` → `dropUnusableTools`） | 插件作用域、用户勾选、分组开关、git 仓库判断 | 工具直接不存在（模型看不到，调用=未知工具） |
| 1 | 工具查找 + 解析 | `agent-engine/tools/exec.ts:124-128`（旧：`toolPipeline.ts:276-281`） | 名字在表里、`parseError` 为空 | `[工具失败] 未知工具…` / 参数 JSON 坏 |
| 2 | 参数闸 | `ai/tools/execute.ts:189-204` `argGateMessage`（必填 + 类型/enum/min/max，不隐式转换） | 缺参数、类型不符 | `⚠️ …本次调用没有执行，也没有产生任何改动。` |
| 3 | 声明式审批对象 | `agent-core/toolGate.ts` `approvalRequestOf`：`tool.approval?.(args,ctx)` 优先，否则 `category==='mutation'` → `{kind:'tool'}` | 工具是否声明审批 | `throw` = 拒绝 |
| 4 | **只读档硬拒** | `toolPipeline.ts:144-153` `readOnlyDenialOf`（新引擎 `exec.ts:170-185`，**在 `if (gate)` 外**） | `approval.kind==='write'` → 拒；`category==='mutation'` 且审批不是 `command` → 拒 | 直接拒，**不弹窗**（`当前是只读档（read-only），这次不会写文件：…`） |
| 5 | 角色写域 | **只有提案路径**：`services/ai/proposal.ts:8` + `shared/ai/roleDomains.ts:155-157`（恒真） | 事实上无判据 | 无 |
| 6 | 全权直通 | `ipc/ai/aiAgentCoreApproval.ts:149` | `effectiveFullAccess` | `true`（跳过 7-9，**不写规则表**） |
| 7 | 已记住的规则 | 同上 `:153`：`approved.has(ruleKind)` **或** `allowRules` 的 `kind+match` 精确匹配 | 本轮种类（粒度错误，见 P0）/ 跨轮精确动作 | `true` |
| 8 | 无审批通道 | 同上 `:162-165`（窗口没了/已销毁） | `win` 不存在 | `throw`（`unavailable` 文案），**不会干等 5 分钟** |
| 9 | 弹窗 + 超时/取消 | `:166-179` → `services/ai/toolApproval.ts`（`APPROVAL_TIMEOUT_MS = 5*60*1000`，`requestApproval` 监听 `ai:approvalResolved`，`abort` → `reason:'aborted'`） | 用户点允许/拒绝/以后别再问 | 拒绝或超时 → `throw`（模型收到"没有执行"的文案） |
| 10 | "以后别再问"记账 | `:180-185`：`approved.add(ruleKind)` + 落盘 `addRule({kind,match})` | `decision === 'always'` | 本轮同类全放行（缺陷）+ 跨轮精确规则 |
| 11 | 执行期沙箱 | `agent-core/sandbox.ts:193-279` + `acl/confine.ts` + `acl/runnerScript.ts` | 只读档 git 子命令白名单、`FORBIDDEN` 元字符、受限令牌 + write SID + ACE（+ 低完整性） | `ok:false` + 中文拒绝文案；建不出令牌 → `DOWN_TEXT` |
| 12 | 写路径护栏 | `agent-core/workspacePath.ts:12-20`、`agent-core/fsObservation.ts:80-93` | 工作区内、先读后写、mtime/size 未变 | `throw`（`路径越界` / `你还没读过它` / `磁盘变了`） |
| 13 | before-image | `ai/tools/registry.ts:148-170` `recordFileBefore`（主进程写路径在 `core.ts:303-314` 自己记账） | 每次写 | 供撤销，不是判据 |

MCP server（**外部 IDE 连本应用**）是另一条链：`mcp/server.ts:322` `applyToolGuard` → `mcpToolGuard.ts:50` `mcpToolVerdict` → `ai/toolGuard.ts:215-259` `classifyTool(..., {context:'mcp', mode: QQBAT_TOOL_GUARD 默认 acceptEdits})` → 只读档/改图类 → `requestApproval`（`mcp/server.ts:157-221`，**不传 AbortSignal**）→ 写目标 `denialForAppManagedWrite` / `denialForWrites`（`mcp/server.ts:336/:341`）→ `executeTool` → 渲染层 `clientInstruction`。

### B. 可绕过/可放大审批的路径（逐条）

| 路径 | 触发条件 | 绕过的是哪一道 | 结论 |
|---|---|---|---|
| `approved` 按 `ruleKind` 命中 | 任一写审批点"以后别再问" | Gate 9（弹窗），本轮内所有 write/command/tool 类 | **P0**（`aiAgentCoreApproval.ts:117,153,181`） |
| MCP 注入工具 `category:'query'` | 启用任一 MCP server（`mcp-client.json`，默认关） | Gate 4（只读档）、Gate 12（写路径护栏）、Gate 11（沙箱）、审批（`approval` 返回 null）、取消（无 signal） | **P1**（`mcp/client/manager.ts:120,125-139`；`agent-core/legacyTools.ts:135-143`） |
| `job_start` | 用户批过一次 command | Gate 11（完全不走沙箱，`shell:true`） | **P1**（`ai/tools/registry.ts:863-887`、`agent-core/jobs.ts:184-187`） |
| 无 gate 的宿主 | 任何新宿主/插件用 `createAgentRuntime(host)` 不传 `approval` | 旧循环 Gate 6-10 全跳过（`toolPipeline.ts:254`） | **P2**（生产路径目前都传了；`agent-runtime/index.ts:72-89` 是公开入口） |
| `effectiveFullAccess` | 用户显式开"完全权限" | Gate 7-10 全跳过（含写域外？`write_file` 仍受 `resolveInWorkspace`，命令在 `danger-full-access` 下不受 `FORBIDDEN`） | 设计如此，**P1**（必须写清前提） |
| 工作区内 junction / symlink | 项目里存在指向外部的目录联接 | Gate 12（`resolveInWorkspace` 只做字符串判断，不解析 realpath） | **P2**（`agent-core/workspacePath.ts:12-20`） |
| 渲染层落盘 | `write_file` 走 `clientInstruction`（助手/MCP 路径） | 主进程的 fsGate 与沙箱都管不到渲染层实际写 | **P2**（`ai/tools/registry.ts:148-170`） |
| MCP HTTP 无 token | 手工构造 `createMcpServer({authToken:''})` | Bearer 鉴权（`transport-manager.ts:84`） | **P2**（默认 `randomUUID()`，`mcp/server.ts:74-75`） |

---

## 风险清单

| ID | 严重度 | 位置 | 触发条件 | 影响 | 最小修法 | 证据类型 |
|---|---|---|---|---|---|---|
| R1 | **P0** | `src/main/ipc/ai/aiAgentCoreApproval.ts:117,153,181`（`approved` 集合的 `ruleKind`） | 用户在任意一次写/命令/工具审批上点"以后别再问"；同一轮（≤12 步）内后续同类调用 | 该轮内**任意路径的写、任意命令**不再弹窗：可覆盖已读过的项目文件、新建 `.git/hooks/*` 等，等于把逐次审批降级为一次性审批 | `approved` 改为存 `match`（`approved.has(match)`），或直接删掉本轮集合、只依赖落盘的 `allowRules`（已是精确匹配） | 代码位置证明（`:153` 先比 kind 再比 match，`:181` 存的是 kind）；**无单测覆盖** |
| R2 | **P1** | `src/main/mcp/client/manager.ts:105-121,125-139`；`src/main/services/agent-core/legacyTools.ts:135-143`；`agent-core/toolPipeline.ts:144-153` | 用户启用 `config/mcp-client.json` 里的 server（`enabled` 默认 false），模型调用 `mcp__<server>__<tool>` | 远端写工具在 read-only 档照样执行；不经本地写闸门/角色/沙箱；`ctx.signal` 不传 → 取消不生效；结果照常进上下文 | 给 MCP 注入工具单独分类（`mcp`），在链上加一道与本地等价的 gate：只读档 deny + 每次审批 + 传 signal + 强制 `category !== 'query'` 的写/执行类工具进审批；或在 UI 明示"外部工具不受沙箱约束" | 代码位置证明 + 注释自认（`manager.ts:105-118`）；**无测试** |
| R3 | **P1** | `src/main/services/ai/tools/registry.ts:852-887`；`src/main/services/agent-core/jobs.ts:184-187` | 模型调 `job_start`（用户批过一次 `kind:'command'`）；任意档位（含 workspace-write） | 完全绕开受限令牌与 `FORBIDDEN` 元字符检查，命令以用户权限执行，可写工作区外 | 让 `job_start` 走 `runInSandbox`（保留管道需求就放宽 `FORBIDDEN` 但保留令牌），或至少在审批文案里标"这条不走沙箱" | 代码位置证明；`tests/main/services/ai/jobs-tools.test.ts:110-125` 只证明 read-only 拒绝，**未证明沙箱** |
| R4 | **P1** | `src/main/services/ai/toolGuard.ts:254-255`；`src/main/mcp/server.ts:157-221` | 只读档下 `run_command` 经 MCP 上下文分类；或收到 `mode:'readonly'` 时 | `classifyTool` 对 `run_command` 返回 `ask`（不是 `deny`）：MCP 侧会弹窗，用户点允许后仍靠执行期沙箱兜底；两套判据对同一档位给出不同答案 | 把 `run_command` 加进"只读档 deny"的判据（或让 `classifyTool` 复用 `readOnlyDenialOf`） | `tests/main/services/ai/toolGuard.test.ts:138-172` 钉了 deny 的集合成员，**未覆盖 `run_command`** |
| R5 | **P1** | `src/main/ipc/ai/aiAgentCoreApproval.ts:112,127,149`；`src/main/services/agent-runtime/projectTurn.ts:105` | 用户开启"完全权限"（`effectiveFullAccess`） | 审批全部跳过；注释声称"仍受角色写域约束"，而角色写域恒真（`shared/ai/roleDomains.ts:77-97,155-157`）→ **注释与实现双错** | 要么实现真写域并把判断放回 `:149` 之前，要么删掉注释与 `roleDomains` 的死代码 | 代码位置证明（`grep mayRoleWrite` 只有 `proposal.ts` 一处调用） |
| R6 | **P2** | `src/main/services/agent-core/toolPipeline.ts:249-259`；`src/main/services/agent-runtime/index.ts:72-89` | 新宿主/插件/测试用 `createAgentRuntime` 或 `runAgentTurn` 不传 `approval` | 声明了审批的 mutation 工具**静默执行**（旧循环）；新引擎同场景是拒绝 → 同一个 API 两种安全语义 | 删掉"无 gate 就放行"的分支（生产已全部接线），或让无 gate 一律 fail-closed | 代码位置证明；`agent-runtime/index.ts:80` 的 `host.approval ? … : {}` 是唯一兜底 |
| R7 | **P2** | `src/main/services/agent-engine/loop.ts:294,322`；`src/main/services/agent-core/core.ts`（无 `onUsage`） | 任何多步任务 | 用量被覆盖（新）或不上报（旧）→ 成本/账单低估；无法据此做预算熔断 | `usage = sumUsage(usage, next)`；旧循环补 `onUsage` | 代码位置证明（grep `usage` 结果） |
| R8 | **P2** | `src/main/services/agent-engine/tools/exec.ts:232-236`；`agent-core/toolTimeout.ts`；`ai/tools/execute.ts:222-239` | 工具超时 | 超时只让调用方拿到错误，工具/HTTP/子进程**继续跑**；同一文件可能被"已超时"的写与下一次写并发覆盖 | 超时即 abort（把 signal 真正传进 handler），并对进程类工具 kill 进程树 | 代码位置证明 + 代码注释自认"无法回收" |
| R9 | **P2** | `src/main/services/agent-core/acl/confine.ts`（timeout 分支，注释说明只 `child.kill()`） | 沙箱内命令超时 | 受限令牌的子进程树可能存活（Windows 无进程组），留下后台进程 | 用 Job Object（`CreateJobObject` + `TerminateJobObject`）承载子进程 | 代码位置 + 注释自认；**未实测** |
| R10 | **P2** | `src/main/services/ai/appAiConfig.ts:97-106`；`src/main/services/ai/projectAiConfig.ts:97,127`；`src/main/mcp/client/config.ts`（env/headers 明文） | Electron `safeStorage.isEncryptionAvailable() === false`（Linux 无 keyring、部分 Windows 配置） | API key / MCP 令牌明文落盘，**无任何告警**；同仓库 `speechConfig.ts:169` 是拒绝保存 | 统一走 `speechConfig` 口径：不可加密则拒绝保存并提示；MCP env/headers 也加密 | 代码位置证明（三条不同口径）；无测试 |
| R11 | **P2** | `src/main/ipc/ai/aiProvider.ts:99`；`src/main/services/ai/appAiConfig.ts:85,177,412,434` | 渲染进程读取配置 | 明文 key 进入渲染进程（`ai:getAppConfig` 设计如此）→ 任何渲染层 XSS/恶意插件可读 | 改为"只在主进程用、渲染层只拿掩码 + 是否可用的布尔" | 代码位置 + 注释自认是设计 |
| R12 | **P2** | `src/main/runtime/sandbox.ts:32,794-815`；`src/main/runtime/untrustedWorker.ts`、`untrustedWorkerHost.ts` | 模型/插件执行 `run_code`、工作流脚本、不可信节点 | `node:vm` **不是安全边界**（文件自述）；untrustedWorker 无内存上限（grep 无 `maxOldSpace`/`resourceLimits`）→ 失控分配可拖垮主进程 | 把 `run_code`/工作流真正放进独立进程 + 资源限制；或明确它是"防误用"而非"防恶意" | 代码位置 + 文件头自述；`tests/runtime/safeEvalSandbox` 只测逃逸路径（已知手法），**不是边界证明** |
| R13 | **P2** | `src/main/index.ts:422-503` | 用户关窗/退出时一轮对话正在跑（含工具正在写盘） | 退出流程**没有 abort** `RUNNING` 的那一轮（`ipc/ai/aiAgentCore.ts:46`）：工具继续执行到进程结束，可能半写；未清理的 `withTimeout` promise 与监听器随进程消失 | `before-quit` 里先 `cancelAgentCoreRun` 全部项目，再等一轮 `finally`（有超时兜底） | 代码位置证明（`before-quit` 无 RUNNING 引用）；无测试 |
| R14 | **P2** | `src/main/mcp/client/manager.ts:125-139`；`services/ai/toolResultBudget.ts` | MCP server 返回超大结果 / 慢调用 | 结果进上下文只受 8 KB 阈值 + 溢出落盘约束（`pruneText` 在 `spillRef` 为 null 时**原样返回**，fail-open）；慢调用只有 `callTimeoutMs`（默认 60 s，无 signal） | 对 MCP 结果强制走 `budgetToolResult` 并保证 spill 可用；给 `callTool` 传 signal | 代码位置证明 |
| R15 | **P2** | `src/main/services/agent-core/workflowEngine.ts`（`DEFAULT_MAX_TOTAL_AGENTS = 1_000`）；`agent-core/subagent.ts`（`MAX_SUBAGENT_ROUNDS = 8`、`MAX_CONCURRENT_SUBAGENTS = 3`） | 一次 `workflow` 调用声明大量 item | 最多 1000 个子代理 × 8 轮模型调用，无本轮总预算闸 → 成本爆炸（每个子代理只读，但模型调用要钱） | 加"本轮总 token/总代理数"预算并默认调低；超限返回 `budget-limited` | 常量证明；`tests/main/services/agent-core/workflow*` 测流程不测计费 |
| R16 | **P2** | `src/main/services/agent-core/contextGate.ts`（80k 预算）与 `agent-core/runTurn.ts`（历史注入） | 超长对话 + 大工具结果 | 压缩只作用于 `messages`，`history` 注入与 `agent-inbox` 可绕过预算；压缩本身要再调一次模型（额外计费） | 预算按"实际发送的字符/估算 token"统一扣减，压缩调用单独记账 | 常量 + 流程证明；无计费测试 |
| R17 | **P3** | `src/main/mcp/transport-manager.ts:84` | 手工以空 `authToken` 启动 MCP HTTP server | 本机任意进程可连 SSE 端点（只绑 127.0.0.1，故仅本机） | 空 token 直接拒绝启动（不要 `if (this.authToken && …)`） | 代码位置 + 注释自认"只应在测试里出现" |
| R18 | **P3** | `src/main/services/agent-core/workspacePath.ts:12-20` | 项目内存在指向外部的 junction | 字符串级"工作区内"判断被文件系统联接绕过，写/读落到工作区外 | `realpathSync` 后再比较（并处理不存在的新文件路径） | 代码位置证明；未实测 |

> 严重度口径：P0 = 默认可触发、且造成真实安全后果（R1）。P1 = 需要用户显式开启/批准一次（R2-R5）。P2/P3 = 设计债、可观测缺口、需特定环境。

---

## 重写必须保留的安全不变量

1. **判据只有一份，且只能被调用、不能被重写。** 一个 `authorizeToolCall(tool, args, ctx)` 覆盖"声明的审批 → 只读档硬拒 → 角色写域 → 问人"，新循环只允许调用它；`readOnlyDenialOf` 这类子判据必须**放在 `if (gate)` 外面**（旧仓库第 182 批踩过：放进去就成了"没声明审批的 mutation 工具照跑"的后门）。
2. **只读档是与审批通道无关的硬拒。** 不允许"弹个窗让用户点允许"来跨过只读档；只读档拒绝必须给模型明确文案（"这次不会写文件"），且不进审批队列。
3. **审批记账按"动作"而不是"种类"。** "以后别再问"必须绑定 `tool+args`（工具）或规范化后的项目相对路径（写）或整条命令（命令）；**禁止**用 `kind`/工具名做键。落盘规则要能在 UI 里看见、能删（旧仓库 `allowRules.ts` 的 `MAX_ALLOW_RULES = 50` + 删除接口是对的）。
4. **fail-closed 是默认方向。** 无审批通道 → 拒绝；窗口不在 → 立刻拒绝（不要空等超时）；未知工具/未分类工具 → 问人而不是放行；审批超时 → 拒绝（旧仓库 5 分钟，文案明确"没人点"）。
5. **写路径三道应用层护栏不可省**：工作区内解析（拒绝绝对路径与 `..`，并解析 realpath 防 junction）、`FsObservationGate`（没读过不许覆盖已存在文件；读过但 mtime/size 变了也不许）、before-image 记账 + 每步检查点（可回滚）。**写必须在主进程完成**；若必须交给渲染层，主进程要能验证"它真的写了什么"（旧仓库 `applied` 计数就是为此，但缺少内容校验）。
6. **命令沙箱以内核为边界**：受限令牌（`DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED`）+ 由工程路径导出的 write SID + 该目录 ACE + 可选低完整性标签；`read-only` 只加私有 temp 的 ACE。**建不出令牌时必须整体拒绝**（旧仓库的 `DOWN_TEXT`），绝不能退回"名字白名单后原样跑"；`FALLBACK_COMMANDS` 那种兜底要么删掉要么只用于"只读 git 子命令"。
7. **命令白名单只是策略层，不是安全层。** 只读档的 git 子命令名单 + 危险参数（`--output`/`-O`/`--ext-diff`）拒绝要保留，但不要把它当作"跑起来就安全"的依据。
8. **参数闸唯一入口**，mutation 提案前也必须过（旧仓库踩过"缺参数的重活变成空洞提案"）。类型不做隐式转换，`NaN`/`Infinity` 不算数字。
9. **外部工具（MCP）与内部工具同等对待，或明确降级并告知用户。** 若允许"外部工具不受沙箱约束"，必须在 UI 上写清并单独开关；不允许默认把 `category` 写成 `query` 来绕过写闸门。
10. **凭据：不可加密就拒绝保存，不要静默明文。** apiKey、QQ `appSecret`、Lark/DingTalk secret、MCP `env`/`headers` 走同一套；日志出口统一脱敏（Bearer/sk-/ghp_/URL userinfo/query 参数/JSON 字段）；trace 只记元数据（`{ts,kind,name,ms,ok,inChars,outChars}`），不记 prompt/args/结果正文；诊断导出对项目配置脱敏、路径替换为 `<project>`。
11. **超时与取消必须贯穿**：AbortSignal 从 UI → 循环 → 工具 handler → provider fetch → 子进程（Job Object）→ MCP `callTool`；超时视为取消（不能只 race）；退出前先 cancel 再清理。
12. **预算与上限是一等公民**：工具结果 8 KB/头尾裁剪 + 溢出落盘（0600、7 天、每项目 1 小时节流）、循环 80k token/80% 压缩、并发工具默认 10（上限 50）、子代理 3 并发/8 轮、工作流总代理数与总时长、**用量累加**（不是覆盖）。
13. **压缩不得破坏 tool-call 配对**（`assistant(toolCalls)` 必须与 `tool(toolResult)` 成对保留），否则 API 直接 400。
14. **子代理只读白名单按名字钉死**（旧仓库 `READ_ONLY_TOOL_NAMES`），并禁止子代理再开子代理、禁止向用户提问（否则会出现"审批卡在子代理里"）。
15. **每个"安全闸"都要有对应的单测钉住其集合成员与顺序**（旧仓库三道：`toolGuard.test.ts` 分类型、`toolDefinitionDispatchSync` 声明↔handler、`agent-tool-defs.test.ts` 必填参数）。

---

## 未知与需要实测

1. **R1 的真机复现**：在 `a.txt` 写审批点"以后别再问"，同一轮再让模型写 `b.txt` —— 预期**不弹窗**（代码这么写）。需要真机确认 UI 的"以后别再问"按钮语义是否确实是"这个动作"，以及是否有人误以为是"这一类"。
2. **MCP 注入工具在 read-only 档的实际行为**：配置一个会写盘的 MCP server（如 filesystem），把沙箱切到 read-only，让模型 `mcp__…__write_file` —— 预期能写（代码路径如此）。同时验证 `mcp__…` 调用是否会并行（`category:'query'` → `executionModeOf` = parallel），以及取消按钮是否无效。
3. **`job_start` 的越界写**：workspace-write 下 `job_start` 跑 `cmd /c echo x > C:\outside.txt` —— 预期能写（未经令牌）。这是沙箱承诺与实现的直接冲突，需实测确认。
4. **受限令牌的真实覆盖面**：`aclConfineAvailable()` 只探测 PowerShell；需实测"C# runner 编译失败/被杀软拦"时是否真的落到 `DOWN_TEXT` 拒绝（`confine.ts:282-285`），而不是某种静默降级；以及 `LOW_INTEGRITY=skipped` 的机器比例（某台机器拒写 SACL 时低完整性失效，写限制层仍在）。
5. **退出时的一轮**：在长工具执行中关窗，观察是否还有写盘/网络请求；`before-quit` 的 4 秒 `Promise.race` 之后 `flushAndClose` 是否会与仍在跑的工具竞争。
6. **用量与计费**：多步任务下 `loop.ts:322` 的覆盖是否导致账单少算（对照 provider 返回的 usage）；工作流 100 个代理的实际花费。
7. **MCP client 的 `env` 与 `headers` 是否会进入日志**：`env: buildChildEnv(cfg.env)` 只传白名单（`connection.ts:131-142`），但 `cfg.headers` 会进 `requestInit`；SDK 报错时是否把 header 回显到 `[工具失败]` 文案/`log.warn('MCP 工具调用失败', {server,tool,reason})`（`connection.ts`）需要抓一次实际错误。
8. **压缩是否会被"history 注入"绕过**：`contextGate.ts` 只处理 `messages`，`runTurn` 另行拼 `history`/`goal`/`schedule`/mention 提示；超长历史（几十轮）是否会让实际请求超过模型窗口 —— 需实测一次长会话。
9. **新引擎的 trace/事件字段是否与旧循环等价**（`agent-engine/adapters/turn.ts` 的端口转发），特别是 `turnId`/`agent`/`inChars`/`outChars` 是否齐全 —— 影响"成本归因"与事后取证。
10. **测试证据的边界**：现有单测能证明"本地只读档拒绝 write_file/edit、pipeline 硬拒即使 gate 允许、MCP server 的闸门对改图类返回 ask、Windows 上受限令牌能写工作区且只读档写不进去"（`tests/main/services/agent-core/aclConfine.test.ts:60-119` 是真机 spawn）；**不能证明**：MCP 注入工具、`job_start`、`approved` 集合、退出取消、用量累加。整个仓库**没有** e2e/Playwright 套件（`tests/**/*e2e*` 无文件），所以"审批弹窗 → 点允许 → 真的写盘"这条链没有任何端到端证据，只有组件级 mock。