# UI / 低代码画布 审查报告（源：`E:\H Bot 制造`）

审查口径：只读 `src/renderer/src`、`src/preload/index.ts`、`src/shared/{nodes,protocol,domain,types}`、`src/main/ipc`、`src/main/services/{flow,ops/git}`、对应测试与闸门。未读 docs/、README、历史报告。**未修改源仓库任何文件。**
证据标记：`[代码]`＝静态代码可直接读出；`[注释-真机]`＝代码注释里记录的线上事故；`[闸门]`＝现有测试/结构检查断言；`[推理]`＝静态推理、未实测（附复现步骤）。

---

## 结论

1. **首屏并不轻。** 启动链 `main.tsx → App → Workspace → TopBar + ThreeColumn` 无路由、无 lazy；画布 `FlowPanel→BlockEditor→@xyflow/react` 确实 lazy，但**整个节点目录**（17 个 spec 文件、约 1 万行）+ **6072 行英文文案表** 通过 `LeftPanel.tsx:2 → NodePalette → lib/node/nodeCatalog.ts → shared/nodes/catalog/registry.ts → builtinCatalog` **静态进入主 chunk 并在首帧执行**，即使用户停在 launch 首页、从不开画布。`[代码]`
2. **状态有三份真源**：渲染层 `flowStore`、主进程内存（仅一份 `mtime:size` revision Map + 活动项目指针）、磁盘 `.qqbat/flow.json`。编辑期权威是渲染层，主进程另有 5 个"读磁盘"的能力，靠 `flushFlow()`（读前冲刷）手工兜住。**这不是单一真源，是"冲刷屏障"**，代码自己承认（`lib/canvas/flushFlow.ts:23-37` 的"边界（诚实说明）"）。`[注释-真机]`
3. **"界面显示已保存、磁盘没保存"窗口真实存在且比防抖更长**：保存徽章读的是 store 的 `dirty`/`saving`/`saveError`，而落盘要过 1500ms 防抖（`stores/flow/persistence.ts:62`）；切项目、关窗口**都不冲刷**，最后一次防抖定时器被 `markGraphLoaded()` 直接 `clearTimeout` 丢掉（`persistence.ts:75-82`）→ 最近 ≤1.5s 的编辑**无声消失**，同时 `loadGraph/reset` 调 `clearHistory()`，撤销栈一起清空。**这是本报告唯一的 P0 数据丢失路径（确定性，不依赖概率）。**
4. **落盘守卫有洞**：主进程 `saveFlow()` 的"活动项目校验 + 磁盘 revision 校验"只在 `reason === 'auto' | 'manual'` 时执行（`services/flow/flow.ts:269`）。而 `flushFlow` 走的 `saveNow(projectPath)` 不带 reason → 默认 `'other'`（`persistenceSlice.ts:165`），`generateCode` 走 `'generate'`（`persistenceSlice.ts:199`）——**这两条都绕过守卫**。5 个 `flushFlow` 调用点（含开"运行面板"页签前、打开数据模板前）都在无守卫状态下写盘。`[代码]`
5. **撤销粒度与耐久性都不达标**：文本字段 300ms 空闲才提交一次（`blocks/DebouncedInputs.tsx:13,39-41`），每次提交压一个历史点，而 `MAX_HISTORY` 动态降到 20/30/50（`stores/flow/historyStack.ts`）→ 在 275 节点项目里连续改十几个字段就能把历史冲干净。**分组变更不参与历史**（`stores/flow/types.ts` 的 `FlowSnapshot` 只有 `nodes/edges`），**系统（多触发器）变更会压栈但 undo 只还原 nodes/edges**（`systemSlice.ts:28` vs `historySlice.ts` 的 `undo`）→ 半可撤销，还原后引用可能悬空。`[代码]`
6. **契约是全手写的三份拷贝**：一条 IPC 要改 ①主进程 handler ②`src/preload/index.ts`（1421 行、308 个 `invoke`）③`src/renderer/src/types/ipc.ts`（2188 行、37 个顶层命名空间）。**没有代码生成**，只靠三条 AST 闸门对账（`tests/guards/ipcSurfaceParity.test.ts` 头注释明写"三处手写"；注释记录首次跑时 preload 228 / QqbatApi 228）。zod 覆盖不全：308 个 `ipcMain.handle` 里只有 222 个走带 `schema` 的 `ipcHandle`，**约 86 条通道运行时无 schema**（差额集中在 `ai/mcpClientIpc.ts`(5/0)、`ai/aiAgentCore.ts`(5/0)、`project/launchData.ts`(23/22)、`ops/metrics.ts`(4/0)、`project/project.ts`(15/7)）。`[代码][闸门]`
7. **节点 SDK 是"8~10 个文件、跨三个运行时边界"**，而且**存在两代节点 schema 并存**：契约层 `NodeSpecV2`（`shared/types/nodeV2.ts`）与老 `NodeSpec`（`shared/types/flow.ts`）在渲染层用 `as unknown as` 硬转合并（`lib/node/nodeCatalog.ts:115,126` 注释自认"V2 是超集…用 `as unknown as` 断言"）；自定义/插件/AI 生成节点注入的都是 v1 `NodeSpec`，主进程另有 `NodeRegistry`（`src/main/nodes/registry.ts`，builtin/custom/aiGenerated 三张 Map）。运行时 handler 与目录分离（`src/main/runtime/handlers/registration.ts` 手写逐条挂）→ **同一节点的"UI 形状"和"运行实现"没有任何编译期关联**。`[代码]`
8. **UI 自动化测试基本为零**：`tests/renderer` 约 170 个文件里**没有** BlockEditor / PropertyDrawer / 拖拽 / 连线 / 撤销重做的交互测试（只有纯逻辑单测：autoLayout、layoutSubgraph、flowValidation、canvasConstants、branchColors…）。唯一的画布端到端测试 `tests/e2e/node-flow.spec.ts`（214 行）**永不运行**：`.spec.ts` 不在 `vitest.config.mts` 的 include 里，仓库没有 playwright 依赖/脚本，文件内每次 import 都带 `@ts-expect-error playwright 为非必装依赖`。且 `vitest.config.mts` 的 coverage 排除 `**/stores/**`。**拖拽/连线/撤销/自动保存的真实行为，现有测试一条都没钉。** `[代码][闸门]`

---

## 首屏与数据流

### 启动路径（全部静态 import，除注明）

```
src/renderer/src/main.tsx
  :4   import './lib/infra/storageKeys'      // 必须最先：顶层做 localStorage 旧键迁移；zustand persist 在模块导入时 rehydrate，晚一步就丢布局偏好
  :7   import './plugins'                    // 树内插件清单必须先于 App 注册（含两个智能体窗口），否则右栏首帧拿不到窗口
  :18-20 globals.css / themes.css / @xyflow/react/dist/style.css   // xyflow 全局 CSS 即使画布 lazy 也在首屏
  :23  installFlowPersistence()              // 自动保存订阅在渲染前装好
  :26  void bootstrapExternalWindowPluginsFromBridge()
  :29  startExternalWindowAutoSync()
  :31-37 createRoot('root').render(<StrictMode><ErrorBoundary><App/></ErrorBoundary></StrictMode>)
App.tsx            ToastProvider → Workspace（无路由、无 lazy）
pages/Workspace.tsx
  :16  useGlobalShortcuts()                  // Ctrl+P 文件、Ctrl+T 浏览器、Ctrl+` 终端
  :18-21 botStore(bots/selectedBotPath/loading/init)
  :26-29 useEffect → init()                  // 订阅 botManager.onStatusChanged
  :33-37 normalizeAppMode() 兜回（旧 mode:'browser' 已删）
  :40-44 无项目 → NewProjectDialog（只弹一次）
  :47-51 默认选中第一个 bot
  :56-76 selectedBotPath 变化 → project.getMeta()（带 cancelled 竞态守卫，.catch 静默）
  :78-107 <header><TopBar/></header> + <ThreeColumn/> + TestRuntimeSubscriber + TestDialog + NewProjectDialog
```

`ThreeColumn.tsx`：`:183-191` `resolvePanelLayout()`（真源 `lib/layout/panelGeometry.ts`，左 180–500/默认 240、右 300–600/默认 320）→ `:222` `<AppFrame>`；`:277-296` 中栏外包 ErrorBoundary；`:328` `<BottomPanel/>` 横跨三栏（**静态**，533 行）。左右栏拖拽用 PointerCapture + `requestAnimationFrame` 节流（`:30-133`）。

首屏**静态可达**的重模块（按代价排序）：

| 模块 | 载体 | 说明 |
|---|---|---|
| 全量节点目录 | `LeftPanel.tsx:2 → NodePalette → lib/node/nodeCatalog.ts:59` | 触发 `shared/nodes/builtinCatalog.ts` 聚合 17 个 spec 文件（message 1138 行、storagePlayer 963、calc 855、controlFlow 772、actionPlatform 764、dataList 734…） |
| 英文文案表 6072 行 | `shared/nodes/catalog/build.ts:16,26` `import { CATALOG_EN_US }` / `COPY_BY_LOCALE` | 静态表，直接引用；**中文用户也付这份解析成本** |
| CodePanel（代码页） | `CenterPanel.tsx:6` | **不是 lazy**（只有 FlowPanel / ButtonPanelManager 是），510 行 |
| BottomPanel / 终端 / 输出 | `ThreeColumn.tsx:328` | 静态 |
| 启动页 8 个面板 | `LaunchPanel.tsx:29-48` | 已 lazy（Dashboard/Console/DataBrowser/DataTemplate/ButtonPanelManager/Config/Agent/LogViewer），`HomePanel` 静态 |
| 画布本体 | `CenterPanel.tsx:27` `lazy(FlowPanel)` | 只在 `mode==='dev' && tab==='blocks' && projectPath` 挂载（`:334-351`） |

首屏**数据请求**（都是异步 IPC，不阻塞首帧）：`botManager.list()`（botStore.init）、`project.listRecent()`（`HomePanel.tsx:75`）、`project.getMeta()`（`Workspace.tsx:56-76`、`HomePanel.tsx:100`）、`botManager.register()`（`HomePanel.tsx:106`）；进入 dev 模式后 `NodePalette.tsx:106` 挂 `useNodeCatalogScan` 扫自定义节点目录（`customNode:scan` / `customNode:watch`）。

**主线程重活**（`[代码]`，未做实测）
- 首帧执行全量目录构建 + 6072 行文案表解析；`NODE_CATALOG_V2` 是模块初始化时构造的数组字面量。
- `findSpec(kind)` 每次调用都 `[...4 个数组].find()` 新建数组再线性扫（`lib/node/nodeCatalog.ts:126,183`）；`CanvasNode.tsx:110` 每个节点 `useMemo(() => findSpec(...))`，`nodeDisplayLabel` 再查一次 → 渲染/语言切换时 n 次 × 目录长度 的分配。
- 画布挂载后：`BlockEditor` 自身 30+ 个 store 订阅（`BlockEditor.tsx:153-202`，其中 `:191` 与 `:193` **重复订阅 `dirty`**）；拖拽每帧写 store（见下节）。
- **不是**首屏问题的：`lib/launch/dataBrowserData.ts`（2049 行，仓内最大单文件）只被 launch 组件引用，随 `DataBrowserPanel` 在 lazy chunk 里。

---

## 状态真源表

| 状态 | 渲染层 | 主进程 | 磁盘 | 同步方向 / 机制 | 漂移窗口与后果 |
|---|---|---|---|---|---|
| 画布 nodes/edges | `flowStore`（`flowStore.ts:91` 拼 17 个 slice） | 无内存态，只有 `flowDiskRevision.ts` 的 `mtime:size` Map + `activeProjectRegistry` | `.qqbat/flow.json` | 单向 renderer→main：1500ms 防抖自动保存（`persistence.ts:62,109`）+ 5 处 `flushFlow` + `generateCode` 前强制保存 | **0–1.5s**；切项目/关窗不冲刷 → 丢失（P0-1）。`loadError` 存在时 `saveNow` 拒写（`persistenceSlice.ts:136-139`），此时界面仍可编辑、永不落盘 |
| nodeGroups / systems（触发器系统） | 同上 | — | flow.json（`graphSlice.ts:exportGraph`） | 同上 | 分组变更**不进历史**；systems 变更压栈但 undo 不还原（P1-4） |
| UI 布局（栏宽/页签/终端高） | `uiStore`（261 行，`persist` name=`STORAGE_KEYS.ui` v4，`partialize` 只存 10 个字段） | — | localStorage | 单向 | 无；`terminalOpen`/`bottomTab`/`rightDoc`/pinned **刻意不持久化** |
| 机器人运行状态 | `botStore.status`（本地乐观置 `'starting'`） | `botManager` 真实进程状态 | 无 | main→renderer 事件 `botManager.onStatusChanged`（`botStore.init` 订阅） | 双份：乐观值与事件短时不一致；`startBot/restartBot` 前强制 `saveNow(force)`，失败则**不启动**（`botStore.ts:52-59,83-86`） |
| 当前项目 | `projectStore.currentProject`（22 行，无持久化） | `project` 服务的 meta | `<project>/project.json` 等 | IPC 拉取 + `getMeta` 覆盖 | 双份（有）；`Workspace.tsx:56-76` 用 cancelled 标志防竞态 |
| AI 会话 | `aiStore`（102 行 barrel + 10 slice，**无 persist**），扁平字段 ≡ `conversations[path]` 双表示由 `viewOf/viewPatch` 维持 | `ai` 服务 + thread 落盘 | `<project>/.qqbat/ai-*` | IPC 拉取 + 乐观更新（`threadsSlice.ts:114` 注释写明"乐观更新，因标题是用户刚敲的那句"） | 乐观更新失败需回滚；`loadAllThreads` 在 preload 缺方法时**明说**而不是装作"没有对话"（`threadsSlice.ts:49-52`） |
| 面板漂移角标 | `panelDriftStore`（30 行，内存） | 远端 QQ 面板 | — | 打开面板时算一次快照 | 刻意是"上次核对"而非实时（文件头注释明确要防"骗人的实时指标"） |
| 启动面板注册表 | 无（`CenterPanel.tsx:203-213` 先 `flushFlow` 再 `panel.listRegistered`） | 读 **flow.json 里的 `admin_panel` 节点** | flow.json | 读盘（依赖冲刷屏障） | 冲刷失败时读到旧图 → 新面板不显示（错误被 `console.error` 记住，不阻断） |
| 保存历史快照 | — | `historyService` | `<project>/.qqbat/history`（50 份） | 只写 | **渲染层零调用** `qqbat.history.*`（preload `index.ts:1063-1068` 暴露但无人用）→ 无"时光机"UI；50 份 = 最近 50 次**保存**（含自动保存），实际只覆盖几分钟 |
| 节点 `label` / `expanded` | `node.data.label`（恒 zh-CN 规范名）/ `expanded` | — | label 落盘；`expanded` 被 `exportGraph` 剥离 | 单向 | 语言切换不改落盘名（`getCanonicalNodeLabel`，`nodeCatalog.ts:186-199`） |

**跨 store 写入排序**：没有事务、没有全局写队列。顺序由调用点决定：`botStore.startBot` 先 `await saveNow(force)` 再启动（正确顺序，失败即中止）；`CenterPanel` 先 `await flushFlow` 再 `panel.listRegistered`（正确顺序）；`persistenceSlice.generateCode` 先 `await saveNow` 再 `codegen.generate`（正确顺序）。**但切项目路径上没有排序可言——因为完全没有保存调用**（`BotManagerPanel.tsx:83-107` 的 `selectAndLoad` 只 `selectBot` + `getMeta`）。

---

## 风险清单

### P0-1 切项目 / 关窗口不冲刷：最近 ≤1.5s 编辑与整个撤销栈一起无声消失
- **位置**：`src/renderer/src/stores/flow/persistence.ts:75-82`（`markGraphLoaded` 里 `clearTimeout`）、`:85,104-110`（1500ms 定时器）；`src/renderer/src/stores/flow/persistenceSlice.ts:223-227`（`reset`→`clearHistory`）、`loadGraph` 内 `clearHistory()`；`src/renderer/src/components/bots/BotManagerPanel.tsx:83-107`（切项目不冲刷）；`src/main/index.ts:422-445`（`before-quit` 只等 `flushStateJournal`+`shutdownTelemetry`，无 flow 冲刷）
- **触发**：改任意字段/拖节点后 **1.5s 内**①点左栏另一个 bot ②关闭窗口 ③（dev 模式下直接切 launch 页签不触发，但切 bot 触发）
- **影响**：最后一次防抖窗口内的编辑**永久丢失**；同一路径 `loadGraph`/`reset` 清空 `_past/_future`，所以**连撤销也回不去**。无任何提示（`dirty` 只驱动徽章）。
- **最小修法**：`BotManagerPanel.selectAndLoad` 开头 `await flushFlow(prevPath)`（失败则 toast 并中止切换）；主进程 `before-quit` / 窗口 `close` 事件加一次同步冲刷（或渲染层 `beforeunload` 拦截 + `saveNow(force)`）；切换时若 `dirty && saveError` 就**禁止切换**而不是静默丢。
- **证据**：`[代码]`（路径完整可读）+ `[注释-真机]`（`persistence.ts` 头注释记录正是为修"搭完流程直接关窗口全白干"而加了自动保存，但没覆盖切项目/退出）

### P0-2 防抖文本框跨节点串写：切换节点后 300ms 内，上一节点的输入会被写进新节点
- **位置**：`src/renderer/src/components/blocks/DebouncedInputs.tsx:16-18`（`onCommitRef.current = onCommit` **每次渲染更新**）、`:21-26`（`useEffect([externalValue])` 只 `clearTimeout` **不提交**）、`:33-41`（`setTimeout(() => onCommitRef.current(v), 300)`）、`:49-55`（卸载时才 flush）；消费点 `ConfigFieldsSection.tsx:227,389,407,419,459` + `ConfigFieldRow key={field.key}`（**key 不含 nodeId**，`PropertyDrawer.tsx:673-680` 渲染时也没有按节点 id 的 key）
- **触发**：在节点 A 的某文本字段输入（值为 `X`）→ **300ms 内**点选节点 B，且 B 的同名字段值**恰好也是 `X`**（空串最常见）→ 定时器未被清掉（`externalValue` 没变，effect 不跑）→ 300ms 到点后 `onCommitRef.current` 已指向 B 的闭包 → `updateNodeConfig(B.id, {field: '新文本'})`
- **影响**：**静默写错节点**。文本落进 flow.json，再经 `generateCode` 进入生成代码 → 属于"错发"。用户只看到 A 的编辑没生效、B 莫名多了内容。
- **最小修法**：`onCommitRef` 里带上节点身份（`setValue` 时快照 `nodeId`，提交时校验 `nodeId === 当前 nodeId` 否则丢弃或先 flush 到旧节点）；或给 `ConfigFieldsSection`/字段行加 `key={node.id}` 强制换实例（配合 `:49-55` 的卸载 flush 即可正确落到旧节点）。
- **证据**：`[推理]`（未实测）。复现：两个相邻 `message_send`（`message` 均为空）→ 在 A 的 message 输入 `abc` → 200ms 内点 B → 等 1s → 看 B 的 message。**若复现不成立，降为 P1 并记为"防抖丢字"**（同样的代码在 `externalValue` 变化时会直接丢弃未提交文本）。

### P0-3 加载时 schemaVersion 迁移**直接覆盖原文件**，且备份失败被空 catch 吞掉
- **位置**：`src/main/services/flow/flow.ts:216-239`（`loadFlow`：`historyService.backup` 在 `try/catch {}` 里 `:224-226` → `runMigrations` → `:229 fs.writeFile` 覆盖 flow.json → **之后**才 `flowGraphSchema.safeParse`）
- **触发**：打开一个 schemaVersion 较老的 flow.json；同时磁盘满/权限/`atomicWrite` 冲突导致备份失败（被吞）
- **影响**：迁移结果若有偏差、或迁移后校验失败（`:236-238` throw），**原稿已被覆盖且没有备份**；渲染层此时 `reset()` + `setLoadError`，用户拿到的是一张空画布 + 一句"校验失败"。属于"无法恢复的编辑丢失"。
- **最小修法**：迁移前**必须**成功落地一份备份（备份失败 → 直接拒绝迁移并提示）；写盘用 `atomicWrite` 且写临时文件 → 校验通过 → 再替换；把 `.bak` 留在磁盘上而不是只靠 `history` 目录。
- **证据**：`[代码]`（顺序与方法名均可读）；`history.ts:33-59` 的 `backup()` 是 `copyFile`，失败会抛 → 被这个空 catch 吞掉。

### P1-1 落盘守卫只覆盖 auto/manual，`flushFlow` 与 `generateCode` 裸写
- **位置**：`src/main/services/flow/flow.ts:269`（`if (reason === 'auto' || reason === 'manual')` 才校验活动项目 `:270-274` 与磁盘 revision `:275-280`）；绕行方 `persistenceSlice.ts:165`（`reason ?? 'other'`）、`:199`（`reason: 'generate'`）；调用点 `lib/canvas/flushFlow.ts:23,41` 与 5 处使用（`PropertyDrawer.tsx:233`、`CenterPanel.tsx:207`、`DataTemplateEditor.tsx:88`、`usePlayerArchiveView.ts:92`、`usePlayerListData.ts:214`）
- **触发**：①另一个窗口/外部编辑器刚改过 flow.json（或用户手工改了）②切到"运行面板"页签/打开数据模板/打开玩家档案 → `flushFlow` 无条件写盘
- **影响**：外部修改被旧画布覆盖，且此路径**不 amend git**（见 P1-6）→ 变更以新 commit 形式落在历史里，用户不易察觉；活动项目校验也跳过（`saveNow` 自己的 `loadedPath` 兜底仍在，所以跨项目写错的对象面已收窄，但主进程侧的守卫等于不存在）。
- **最小修法**：`reason` 只作为**提交信息**的提示，不作守卫开关；守卫只按"是否显式 force 覆盖"分级（例如 `force` 才跳过 revision 校验，且必须记入日志 + 界面提示）。`flushFlow` 应传 `reason:'manual'`。
- **证据**：`[代码]` + `[注释-真机]`（`flow.ts` 与 `persistenceSlice.ts:110-132` 都记录了"把 RPG 项目 275 节点图写进测试项目"的事故，现口径靠 `loadedPath` 兜底）

### P1-2 磁盘 revision 守卫是纯内存 Map，重启后恒 fail-open
- **位置**：`src/main/services/flow/flowDiskRevision.ts`（30 行，`knownRevisions: Map<string,string>`，`key = normalizeProjectPath`）；`readFlowDiskRevision` 读不到返回 `null` → 视为"无需比较"
- **触发**：重启应用后，磁盘文件在应用关闭期间被改（另一台机器同步、手工编辑、git 切换分支）→ 首次保存比较 `null` → 放行
- **影响**：外部修改被覆盖。与 P1-1 叠加时几乎必然。
- **最小修法**：revision 落盘（放进 `.qqbat/state.json` 之类已有状态文件），或在 `loadFlow` 时把当前 `mtime:size` 与 flow.json 内的元数据（`metadata.updatedAt`）双向核对。
- **证据**：`[代码]`

### P1-3 无任何 flow.json 文件监听；"外部修改"只在自动保存/手动保存那一刻被（半）发现
- **位置**：全仓 `chokidar|fs.watch|watchFile` 只命中 `src/main/ipc/project/dataNotify.ts`、`src/main/services/project/dataNotifyService.ts`、`src/main/services/skills/skillCatalog.ts`、`src/renderer/src/hooks/useNodeCatalogScan.ts`、`src/main/ipc/flow/customNode.ts:136`（只监听自定义节点目录）
- **触发**：两个应用实例/外部工具同时编辑同一项目
- **影响**：界面永远不提示"文件已被外部修改"，直到保存被拒绝（而且 `reason` 不对时连拒绝都没有）。
- **最小修法**：主进程 watch `<project>/.qqbat/flow.json`（已在监听同目录的 customNode 目录，可复用 chokidar 实例），变化时推 `flow:externalChanged` 给渲染层 → 顶部条提示"选择重新加载/保留当前"。这是重写时必须有的能力（用户已经踩过同类事故）。
- **证据**：`[代码]`

### P1-4 撤销语义不自洽：分组不可撤销、系统"压栈但还原不掉"、历史深度按节点数缩水
- **位置**：`src/renderer/src/stores/flow/types.ts`（`FlowSnapshot { nodes, edges, historyDepth? }` —— 只含拓扑，注释明写"分组不参与历史"）；`systemSlice.ts:28` 压栈 vs `historySlice.ts` 的 `undo` 只 `set({nodes, edges})`；`historyStack.ts`（`MAX_HISTORY=50`，`nodes>200 → 20`、`>80 → 30`）；`historySlice.ts:80` 的 `dragHistoryPushed` 配对
- **触发**：①建/删/重命名分组后 Ctrl+Z（无反应）②加/删一个触发器系统后 Ctrl+Z（nodes 回退、systems 不回退 → 节点可能指向不存在的系统）
- **影响**：用户认为"撤销了"，实际留下不一致状态；再次保存就把不一致写进 flow.json。
- **最小修法**：`FlowSnapshot` 扩到 `{nodes, edges, nodeGroups, systems}`（内存代价可控，`structuredClone` 已有降级路径）；分组操作走同一个 `pushHistory`；写一条闸门测试断言"每种 mutation 都能被 undo 完全还原"（用状态快照 diff）。
- **证据**：`[代码]`

### P1-5 撤销深度按节点数缩到 20，而一次编辑会话按 300ms 空闲切段
- **位置**：`historyStack.ts`（动态 `MAX_HISTORY`）；`DebouncedInputs.tsx:13`（默认 300ms 才提交一次）；`nodeSlice.ts:44-56,104,144`（`updateNodeConfig`/`renameNodeInputPort`/`updateNodeDescription` **每次提交都 `pushHistory`**，仅节点不存在时早退）
- **触发**：275 节点项目连续编辑 20+ 个字段（每次停顿 >300ms 就是一个历史点）
- **影响**：早期历史被挤掉，"改错了想撤回去"在最需要的大项目里最先失效。
- **最小修法**：历史按**编辑事务**而非按提交段合并（同一节点同一字段 5s 内的连续提交共用一个历史点，`WeakMap<nodeId+field, historyIndex>`）；深度与内存上限挂钩（按字节而非条数裁剪）。
- **证据**：`[代码]`

### P1-6 git 自动提交对 `reason='other'/'generate'` 每次新建 commit（auto/manual 才会 amend）
- **位置**：`src/main/services/ops/git.ts:252-253`（`working = reason==='auto'||'manual'` → `isWorkingHead` → `--amend`）、`:277`（`WORKING_TRAILER`）、`:286-306`（`git add .qqbat/flow.json` + `src/plugins/` + commit）；`flow.ts:305-306`（每次保存都调，失败只 `log.warn`）；`:333-347` `ensureGitignore` 会**写入用户项目的 .gitignore**
- **触发**：`flushFlow`（打开运行面板/数据模板/玩家档案）与 `generateCode`
- **影响**：项目 git 历史被大量 `other` 提交污染；`--amend` 路径还会**改写**"编辑中"提交（多人协作时是危险的默认值）。副作用还包括自动改用户 `.gitignore`。
- **最小修法**：渲染层只发两种 reason（`autosave` / `generate`），由主进程决定 commit 策略；amend 仅限"本地未推送且是本次会话的 working 提交"，并在暂停/退出时收尾成正式提交。
- **证据**：`[代码]` + `[注释-真机]`（`git.ts:190-209,224-242` 记录"git 静默失败导致版本历史永远为空"的真机反馈，说明这条链路曾被认真修过）

### P1-7 契约三处手写、无生成；zod 覆盖 222/308
- **位置**：`src/preload/index.ts`（1421 行；308 `invoke` / 38 `on` / 1 `send`；37 个顶层命名空间）；`src/renderer/src/types/ipc.ts`（2188 行手写 interface）；`tests/guards/ipcSurfaceParity.test.ts:7-11`（"三处手写"）；`src/main/ipc/**`（`ipcMain.handle` 308 vs `ipcHandle` 222 vs `schema:` 223）
- **无 schema 的重点通道**：`ai/mcpClientIpc.ts`（5/0，且注释自认"schema 报错是英文 zod 措辞，用户要中文指引"→ 改由 `tests/guards/ipcPayloadContract.test.ts` 的 CONTRACTS 补）、`ai/aiAgentCore.ts`（5/0）、`ai/ai.ts`（1/0）、`ai/mcpServerIpc.ts`（1/0）、`ops/metrics.ts`（4/0）、`ops/metricsExport.ts`（2/0）、`project/launchData.ts`（23/22）、`project/project.ts`（15/7）、`system/file.ts`（12/9）、`ops/debugger.ts`（10/1）、`bot/official.ts`（13/12）、`flow/customNode.ts`（5/3，其中 `customNode:watch/unwatch` 收 `payload: unknown`）
- **新增一条 IPC 要改 3 处**（handler / preload / `types/ipc.ts`），带载荷再加第 4 处 `src/main/schemas/<域>/`、第 5 处闸门里的 CONTRACTS
- **影响**：漂移有一半靠闸门兜（`ipcSurfaceParity` 用 AST 取点号路径对账，注释记录首跑 228/228 一致），但**参数形状**只有一半通道有运行时校验；`unknown` 在类型面上等于没有契约。
- **最小修法（重写时）**：一份 zod schema 作为唯一源 → 用 `z.infer` 生成 preload 签名与 renderer 类型（代码生成，`pnpm gen:ipc`）+ 闸门校验生成物是最新的。这条能把"三处手写"降成"一处 + 生成"。
- **证据**：`[代码][闸门]`

### P1-8 双节点 schema（NodeSpec v1 / NodeSpecV2）+ 契约与运行时无编译期关联
- **位置**：`src/renderer/src/lib/node/nodeCatalog.ts:115-130`（`getAllSpecs()` 与 `getAllSpecsWithDeprecated()` 用 `as unknown as NodeSpec[]` 把 V2 目录、`customNodes`、`pluginNodes`、`aiGeneratedNodes` 混在一个数组里；注释自认"group 字段类型不同，故用 `as unknown as` 断言"）；`src/shared/types/nodeV2.ts`（133 行）vs `src/shared/types/flow.ts`（`Port.showWhen` 是函数，`structuredClone` 会炸 → `historyStack.cloneForHistory` 有降级分支）；运行时 `src/main/runtime/handlers/registration.ts`（手写逐条 `registerPositional('message_received', …)`）
- **影响**：新增节点时"UI 能拖出来"与"运行时会执行"之间没有编译器保护；`as unknown as` 一旦字段漂移就是运行时 undefined。
- **最小修法**：单一 `NodeSpec`（v2）+ 运行时实现按 `kind` 用 `Record<kind, Handler>` 的类型映射强制穷尽（`satisfies`），并让目录构建期校验 `kind` 在 handler 表里存在（现在只有 `tests/guards/toolDefinitionDispatchSync` 之于 AI 工具，节点没有等价物）。
- **证据**：`[代码]`

### P2-1（性能，**未验证**）1000 节点时每帧 O(n²) 的渲染热路径
- **位置**：`src/renderer/src/components/canvas/CanvasNode.tsx:217-224`（`subflowCallCount` selector **每个节点**都遍历全部 `s.nodes` 找 `subflow_call` → 渲染 n 个节点即 O(n²)，且 `nodes` 引用一变全体重算）；`BlockEditor.tsx:405`（`getExecutionChains(nodes, edges)` 依赖 `nodes` 引用，拖拽每帧变）；`src/renderer/src/lib/canvas/executionOrder.ts:91-112`（每个出队节点 `edges.filter(...)`，且 `isControlEdge` 每条边 `nodes.find` → 单触发器 O(V·E·V)，`:84` 对每个 trigger 各跑一遍）；`BlockEditor.tsx:413-438`（抽屉上游 BFS 每次 nodes/edges 变重建邻接表，`:430` BFS 内 `nodes.find`）；`useCanvasProjection.ts:123-173`（边投影 memo 依赖含 `nodes` → 每帧重算全部边）；`useDragSession.ts:115-149`（`position` 变更**每帧**写 store，注释说明 2026-10-02 试过不写会弄坏拖拽）
- **已有的救命开关**：`BlockEditor.tsx:772` `onlyRenderVisibleElements`（注释实测：263 节点/471 边 开=3.8ms/帧≈112 次/秒；关=69ms/帧、最慢 241ms，4.7s 拖拽只处理 23 次鼠标移动）。**重写时这个必须保留。**
- **测量方法**：`tests/renderer/lib/autoLayout.benchmark.test.ts` 只测布局。要量画布：①造 1000 节点 fixture（`lib/canvas/nodeFactory.ts` + 生成器脚本）②`performance.mark` 包住 `onNodesChange` 到下一帧 `requestAnimationFrame`（即 `useDragSession.ts:88-113` 里已有的 `[drag-profile]` 日志，DEV 下已打印"渲染帧数/平均 ms/最慢 ms/写回位置次数"）③对比 `onlyRenderVisibleElements` 开关两次，④用 React DevTools Profiler 数 committed 次数。**在没有这组数据前，本项一律按"未验证"处理。**
- **证据**：`[代码]` + `[注释-真机]`（3.8ms vs 69ms 是作者实测记录，但那是 263 节点、且非本次环境）

### P2-2 `findSpec` / `nodeDisplayLabel` 每次调用重建数组并线性扫
- **位置**：`lib/node/nodeCatalog.ts:126-128,183-185`（`[...getCatalog(), ...customNodes, ...pluginNodes, ...aiGeneratedNodes].find(...)`）；消费点 `CanvasNode.tsx:110`（每节点）、`nodeDisplayLabel`（渲染标签时再查）
- **影响**：1000 节点首渲染 = 1000 次数组拼接（约 76×4 元素）+ 1000 次线性扫；语言切换再翻倍。`visibleCatalog()` 也在每次 `getAllSpecs()` 里跑一遍过滤。
- **最小修法**：模块级 `Map<kind, spec>` 缓存 + `catalogVersion` 失效（`bumpCatalog` 已经有版本号，直接接上即可）。
- **证据**：`[代码]`

### P2-3 `BlockEditor` 30+ 订阅 + `dirty` 重复订阅
- **位置**：`BlockEditor.tsx:153-202`（`:191` `flowDirty`、`:193` `dirty` 同值两次订阅）；`useFlowBridge.tsx:142-154`（`newIssueSummary` 每次 issues 变重算 baseline 差集）
- **影响**：任何一次 store 写入都会让 BlockEditor 走一遍 selector 集合；重复订阅是纯浪费。
- **证据**：`[代码]`

### P2-4 不可撤销/不可恢复的操作清单（需要界面明示）
| 操作 | 位置 | 现状 |
|---|---|---|
| 切项目 | `BotManagerPanel.tsx:83-107` | 丢弃未保存编辑 + 清空撤销栈，无提示（P0-1） |
| 加载失败后重置 | `persistenceSlice.ts` `reset()` + `BlockEditor.tsx:274-282` `handleLoadFailure` | 画布清空、`loadError` 阻止再保存；只有 `.qqbat/history` 里可能有原稿（无 UI 可恢复） |
| 生成代码 | `persistenceSlice.ts:188-221` | 会覆盖生成目录；本身不可撤销（但先保存，数据不丢） |
| 建/改/删分组 | `nodeGroupSlice.ts`（未压历史） | 完全不可撤销 |
| 删除节点（被 subflow_call 引用） | `graphSlice.ts` `removeNode`/`canRemoveNode` | **静默 `return {}`**（拒绝但无提示），用户会觉得"点了没反应" |
| 导入模板 / AI 应用 | `templateSlice.ts:25,129,211`、`stores/ai/applySlice.ts:91-181` | 压历史，可撤销（applySlice 用 `beginHistoryBatch` 包住） |

---

## 重写必须保留的交互

1. **自动保存 + 显式保存徽章**：`savedAt` / `unsaved` / `saving` / `failed` 四态（`BlockEditor.tsx:806-829`）+ Ctrl/Cmd+S（`persistence.ts:120`）。**但必须补上"切项目/退出前冲刷"**（否则等于把 P0-1 一起继承）。
2. **保存口径的教训**：`explicitPath || loadedPath || storePath`，并且三者不一致时告警（`persistenceSlice.ts:125-131`）。**绝不能**用"当前打开的项目"当保存目标。
3. **"读前冲刷"屏障**：凡是主进程要读 flow.json 的入口，先 `await flushFlow()`（`lib/canvas/flushFlow.ts`）。重写时更好的做法是主进程不再读盘，但**迁移期这条屏障必须留**。
4. **`onlyRenderVisibleElements`**（`BlockEditor.tsx:772`）+ 模块级 `nodeTypes/edgeTypes` 常量（`:145-149`）+ `RF_SNAP_GRID` + `deleteKeyCode={null}`（`:760`，删除走自己的撤销逻辑）。
5. **拖拽事务**：`beginDragHistory` / `cancelDragHistory`（零位移不产生历史点，`useDragSession.ts:60-113`）+ DEV 下的 `[drag-profile]` 帧耗时日志（这是最有价值的性能回归探针）。
6. **复合操作单事务**：`pushHistoryPoint(before) → beginHistoryBatch() → 改动 → endHistoryBatch()`（`PropertyDrawer.tsx:433-446`、`lib/command/insertChain.ts:53-88`、`applySlice.ts:91-181`）。
7. **连线替换的显式提示**：`useNodeInteractions.ts:68-85`（`onConnect`：同 source+sourcePort 已存在 → `replacedEdge` toast `:81`；同 target+targetPort 已存在 → `replacedTargetEdge` toast `:82`）+ `isValidConnection`（`:120`）只放行 control→control 且拒绝自连（`:13` 注释：A→A 自环会让生成的代码死循环）。
8. **数据口单入边 / 控制口多入边（汇合点）规则**（`graphSlice.ts` `addEdge`，注释带可证明的安全论证）+ `deleteNodeAndReconnect`（单事务重连全部控制前驱、按 `continuationPorts` 选延续出口）。
9. **端口 ID 编解码**：`encodeHandleId = ${kind}__${id}`，解码必须 `indexOf('__')` 切分（`lib/canvas/reactFlowAdapter.ts`，因为 `branch_switch` 的 case 名可含 `__`）。
10. **节点投影缓存三件套**（`useCanvasProjection.ts`）：`rfNodeCache`（节点引用未变就复用 RF 对象，否则丢 `measured` → 尺寸抖动）、`measuredRef`、`edgeDataCacheRef`（key = `edgeId::executing::branchColor::branchLabel`）。
11. **选择/高亮语义**：`setSearchHighlights` 必须幂等（`selectionSlice.ts`，注释记录：原来无条件 `new Set` → 拖拽每帧写 store → 掉帧）；`highlightNodes` 3s 自动清；`selectAllNodes` 时 `selectedNodeId=null`，`setMultiSelect` 同集合时**清空**（切换语义）。
12. **保存徽章与校验摘要条的"新问题"区分**：`splitIssuesByBaseline(issues, issueBaseline)`（`useFlowBridge.tsx:142-154`）——老问题不反复报警。
13. **`nodeDisplayLabel`**：落盘恒 zh-CN 规范名、显示按当前语言、用户改过名则不翻译（`nodeCatalog.ts:186-199`）。这条双语策略必须保留，否则会污染用户项目。
14. **`GROUP_ORDER` 认知顺序** + 未知分组按字母序排末尾（`nodeCatalog.ts:139-158`）。
15. **列表式自动布局不折行**（`lib/canvas/autoLayout.ts` `layoutBlocksAsList`，`LIST_COL_GAP=70`/`LIST_BLOCK_GAP=140`；注释记录删掉 `LIST_WRAP_AT=12` 的原因："宽度可以滚，边乱了没救"）+ `needsLayout` = 全 (0,0)。
16. **ELK 惰性加载**（`lib/canvas/elkLoader.ts`）+ autoLayout 不 import elk（被 5 个画布组件 + `applySlice` 静态引用）。
17. **启动页 lazy**（`LaunchPanel.tsx:29-48`，注释实测 65 模块约 400 kB 离开主 chunk）。**但要把节点目录一起 lazy 掉**（现在是静态的，见结论 1）。
18. **preload 的四层防御**（`ipc/system/_shared.ts`）：来源白名单（按 WebContents id，不按 url 前缀）+ 参数 + 权限 + 业务，顺序不可交换；`trustedWebContents` 是 Set of id；`INTERNAL_CALL` 用 `Symbol.for` 防渲染层伪造。
19. **`customNode` 目录监听 + 热注入**（`customNode:watch` → `customNode:changed` → `setCustomNodes` + `bumpCatalog`）：插件作者改 JSON 就能在画布里看到。

## 可以简化/放弃的交互

1. **`history:list/restore/read` 三条通道**（preload `index.ts:1063-1068`）：渲染层零调用、无 UI。重写要么给它做一个真 UI（配合"50 份只覆盖几分钟"的问题一起改），要么**整条删掉**，并把 `.qqbat/history` 的备份策略改成"按时间桶/按会话"而不是"按保存次数"。
2. **`.qqbat/history` 50 份双轨备份**：和 git 自动提交功能重叠（`gitService.autoCommit` 保存即提交 + `restoreFlow`）。保留一条即可，建议保留 git（可 diff、可追责）+ 一个"崩溃恢复草稿"。
3. **`panelDriftStore` 角标**：刻意是"上次核对"快照（注释自己说怕变成骗人的实时指标）。这类"不新鲜的指标"对新手是噪音，可降级为面板内的提示条。
4. **`PropertyDrawer` 的 `onMoveUp/onMoveDown` 传 `NOOP`、`canMoveUp/canMoveDown=false`**（`BlockEditor.tsx:946-960`）：列表式上下移已废，接口还留着。直接删字段，别在重写里继承空壳 API。
5. **`getAllSpecsWithDeprecated()` 与 `getAllSpecs()` 两个入口**：保留"查找要全量、列表要过滤"的**语义**，但实现改成两个 Map，不要两个数组工厂函数。
6. **`ipcSurfaceParity` 式的三处手写**：见 P1-7，用生成替代。这条不是"交互"，但它决定了重写的日常开发成本。
7. **`CodePanel` 的静态导入**（`CenterPanel.tsx:6`）：和 FlowPanel 一起 lazy。
8. **把 6072 行英文文案表静态引入**：改成按 locale 动态 `import()`。
9. **`launchData` 的 23 条通道**（`project/launchData.ts` 306 行 / schema 112 行）：其中 `panel.queryTable` 是分页拉取（`LaunchPanel.tsx:58-172` `DEFAULT_PAGE_SIZE`），语义可保留；但"23 条通道里 22 条有 schema、1 条没有"这种不一致值得在重写里统一成"一个资源 + 一个 query 描述符"。

## 未知与需要实测

1. **P0-2 跨节点串写**是否真能复现（`DebouncedInputs.tsx:21-26` vs `:33-41`）。给的操作路径见 P0-2；**这是本报告唯一依赖推理未实测的 P0**，请优先验证。若不复现，降 P1 并保留"切节点丢字"结论。
2. **1.5s 丢失窗口的实际体感**：需要实测"编辑后多久切项目会丢"。测量法：打点 `persistence.ts:109`（定时器设点）与 `markGraphLoaded`，log `flowRevision` 与 `loadedProjectPath`；或在 DEV 下加一条"切项目时 dirty && 未保存"的 console.warn，跑 20 次切项目统计。
3. **1000 节点真实帧耗时**：本报告所有性能结论均为 `[代码]` 推理，唯一实测数字来自注释（263 节点 3.8ms/帧）。测量法见 P2-1（`[drag-profile]` 日志 + 开关对比 + Profiler committed 次数）。**不要在没有这组数据前决定"要不要上虚拟化/worker"。**
4. **主 chunk 的真实体积构成**：需要跑一次 `electron-vite build` 后用 visualizer 或直接 grep `out/renderer/assets/*.js` 里的目录特征字符串（如某个节点 label）。预期：节点目录 + en-US 文案占可观比例。**本次未构建，未验证。**
5. **`as unknown as` 的 v1/v2 混用是否已经在真机造成字段缺失**：需要在有自定义节点/AI 生成节点的项目里跑 `findSpec` 后逐字段比对。未知。
6. **git amend 路径在用户真实仓库里的表现**：`isWorkingHead` 的判定条件未逐行确认；若用户自己也有 `WORKING_TRAILER` 提交，可能被误 amend。需要读 `buildWorkingMessage` / `stripWorkingTrailer` 全实现并实测。
7. **多窗口（插件窗口）是否可能同项目双写 flow.json**：`pluginWindowPackageIpc` / `windowPackage` 允许外部窗口；本次未确认这些窗口能否调用 `flow:save`（信任来源白名单按 WebContents id，插件窗口若在名单内就能写）。**需要在真机开两个插件窗口验证。**
8. **`panelSync` 的多窗口同步范围**：只看了 IPC 存在，未读实现；不清楚它同步的是面板配置还是画布。
9. **撤销在"AI 应用提案"路径下的完整性**：`applySlice.ts:91-181` 用了批处理但在失败分支 `:180-181` 先 `endHistoryBatch()` 再 `pushHistoryPoint(snapshot)`（顺序与成功分支相反）——需要实测失败回滚后 Ctrl+Z 的行为，可能多出一个空历史点。
10. **Windows 上 `atomicWrite` 的 EPERM 概率**（AGENTS.md 记了真机两次）：flow.json 是否也会撞？`retryIo.ts` 的重试是否覆盖 `flow.ts:303`。需要读 `atomicWrite` 的调用链确认（本次未读 `infra/fileOps.ts`）。