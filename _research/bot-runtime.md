# 机器人运行时 / 消息链路 只读审查报告

> 对象：`E:\H Bot 制造`（Electron + TS 单应用）。
> 取证方式：只读 `src/main/**` 实现与 `tests/**` 实际测试；**未采信** 项目内任何 README / docs / 历史报告 / 代码注释中的结论性描述（注释只在能被实现验证时引用）。
> 未修改 `E:\H Bot 制造` 任何文件。
> 证据类型标注：`已读代码` = 本人逐行读过该实现；`已跑测试` = 本次实际执行过的测试；`未验证` = 由代码推得但未跑起来证实。本报告**没有**任何条目属于「已跑测试」（见 §7 说明：本项目没有可离线跑通的端到端运行时测试）。凡标注为「已读代码」的判断都给出了 `文件:行号`。

---

## 结论

1. **入站链路没有队列、没有并发上限、没有背压、没有去重**：官方网关 `ws.onmessage` 同步 `emitter.emit`（`src/main/services/official/ws.ts:23-78`）→ `EventBus` 同步广播（`src/main/services/runner/eventBus.ts:795-801`）→ `FlowRuntime` 用 `void this.dispatchEvent(...)` 甩出去（`src/main/runtime/FlowRuntime.ts:401`）。同一会话连续两条消息在两条独立 async 链里并行执行，顺序由各链的 await 时长决定 —— 回复乱序是结构性的，不是偶发。
2. **运行时不连接/断线期间的入站事件被静默丢弃**：`broadcastToClients` 只遍历现存 `clients`，`readyState !== OPEN` 就 `continue`，`clients` 为空时**不写日志、不落盘、不缓冲**（`src/main/services/runner/eventBus.ts:806-827`）；而 `startBot` 先起官方适配器、后建运行时（`src/main/services/bot/botManager.ts:250` 与 `:256`），这一段窗口内的消息必然丢。
3. **`wait_input` 的超时结算依赖「下一条消息到来」**：全仓唯一调用 `takeExpired` 的地方是 `settleWaitsAndResume`（`src/main/runtime/FlowInterpreter.ts:453`），而它只被 `dispatchEventInner` 调用（`:361`）；每秒一次的心跳 `tickSchedules`（`src/main/runtime/FlowRuntime.ts:212-238`）**整个函数里没有任何等待表访问**（有触发器时只跑触发器，无触发器时 `:219` 直接 `return 0`）。安静会话里的超时分支永远不触发。
4. **挂起等待的落盘只写不读**：`restoreWaitsFor`（`src/main/runtime/waitRegistry.ts:183`）在生产代码里**零调用**；`.qqbat/runtime/pending-waits.json` 因此只是"崩溃后能证明曾经有过等待"，重启后不会恢复任何链。测试直接测了这个函数（`tests/main/runtime/waitPersistence.test.ts:53-81`），却没测"运行时是否调用它"。
5. **等待唤醒条件过宽，会把别人的消息当成本人的输入**：`matches()` 里 `chatId`/`chatType` 只在双方都有值时才比较（`src/main/runtime/waitRegistry.ts:228`），`scope === 'any_in_group'` 直接 `return true`（`:233`，连 `userId` 都不比）。群里 A 触发的 `wait_input` 可被 B 的下一条消息消费 —— 这是"回复错人/错数据"的具体路径。
6. **一次事件只允许一条触发器链执行**：`dispatchEventInner` 遍历 `graph.nodes`，第一条 `matchTrigger` 命中的触发器就 `return true`（`src/main/runtime/FlowInterpreter.ts:414`），其余同时匹配的触发器静默不跑，顺序 = flow.json 里的节点数组顺序（画布上不可见、不可调）。
7. **`EventBus` 的 WebSocket 服务端无任何鉴权**：绑定 `127.0.0.1`、连接即 `clients.add`（`src/main/services/runner/eventBus.ts:684-687`），消息帧直接进 `handleAction`（`:706-713`）。本机任意进程可订阅全部事件、直接下发 `send_group_msg` 之类动作 —— 安全边界为零（"本机能连"不等于"只有本项目能连"）。
8. **官方 QQ 链路没有事件幂等**：全仓 `dedupe|idempot` 只命中 lark/dingtalk 两个 adapter（`src/main/services/lark/adapter.ts:47-56,117-121`、`src/main/services/dingtalk/adapter.ts:101`）；官方侧靠"收包即前移 `seq`"隐式规避重放（`src/main/services/official/ws.ts:26-27`，早于 `:74` 的派发）。代价是：`sessionId`/`seq` 只在内存（`src/main/services/official/client.ts` 的 `sessions` Map），**进程重启后无法 resume，断线期间的消息永久丢失**，且一旦上游重复投递就重复执行。
9. **平台分支没有侵入解释器**：核心 `FlowRuntime`/`FlowInterpreter` 不认平台；平台差异集中在 zod 枚举（`src/main/schemas/project/project.ts:33`）、`botManager` 的三分支（`:189-229`）、以及 `SandboxTranslator` 子类（`src/main/services/bot/sandboxTranslator.ts:14`）。新增平台是"新建 adapter 子类 + 5 处接线"，不是改核心。
10. **测试的真实边界**：解释器本身有**真集成**测试（`tests/helpers/runFlowGraph.ts:137-182` 用真 `FlowInterpreter` + 临时项目 + fake bot 跑整图，被 5 个测试文件使用）；而 `botManager`/官方 WS/翻译器/等待恢复全部是 mock 或未接线（`tests/main/services/botManager.test.ts:48-69` mock 了 10 个模块；`tests/e2e/node-flow.spec.ts` 只测渲染层且无 dev server 时整批 skip）。**"消息从 WS 进来到回复"这条链，没有任何一条测试穿过真实 socket。**

---

## 消息主链路

### A. 官方 QQ（微信/QQ 单聊与群聊）—— 实际函数链

1. `startBot` 装配：`snapshotBeforeRun`（`botManager.ts:161`，失败只 warn）→ `loadProjectPlugins(..., {reload:true})`（`:169`）→ 读 `.qqbat/project.json` → `resolveUsableTranslatorPort`（`:182`，变更即回写 project.json `:183-188`）→ `adapterKind` 分支（`:189`）。
2. 官方分支：`startOfficial(projectPath)`（`botManager.ts:250`，**失败只 warn**）→ `new FlowRuntime(projectPath, translatorPort)`（`:256`）→ `attachRuntime` → `this.runtimes.set()` **先于** `await runtime.start()`（`:335-336`），所以"表里有实例"不等于"已连上"。
3. 翻译器：`startTranslator(projectPath, port)`（`eventBus.ts:167`）建 `new EventBus(projectPath, port)`，`EventBus.start()` 里 `new WebSocketServer({host:'127.0.0.1', port, path:BUS_WS_PATH})`（`:661-665`）并订阅官方事件：`onOfficialMessage → handleOfficialEvent`（`:725-728`）。
4. 官方网关收包：`ws.onmessage`（`official/ws.ts:23`）→ `session.seq = s`（`:27`）→ op0 非 READY/RESUMED 时 `emitter.emit("official:message", projectPath, {type:t, data:d, parsed})`（`:74-78`）。
5. 形状翻译：`EventBus.handleOfficialEvent(type, data)`（`eventBus.ts:872`）把官方事件翻成 OneBot-ish 事件：`GROUP_AT_MESSAGE_CREATE`→`message_type:'group'`、`C2C_MESSAGE_CREATE`→`private`、频道事件→`guild`；`user_id`/`group_id` 换成自分配整数 UID，openid 存 `_openid`；`message_id = hashToInt(eventId)`（`:955/1004/1056/1115`）；随后 `rememberContext(...)`（`:535-545`）把"触发消息上下文"记进 `ctx`（按会话 openid）与 `ctxByMsgId`（按 `hashToInt(eventId)`）。
6. 广播：`broadcast(data)` → 订阅者 `broadcastToClients`（`eventBus.ts:795-827`）→ 逐 client `ws.send`；**clients 为空 = 丢弃**。
7. 运行时接收：`FlowRuntime.connect()` 以 WS 客户端连 `ws://127.0.0.1:<port>${BUS_WS_PATH}`（`FlowRuntime.ts:296`）→ `ws.on('message')` → `handleMessage`（`:367`）→ `post_type` 过滤（`:395`）→ `void dispatchEvent(postType, event)`（`:401`）。
8. 解释执行：`dispatchEvent`（`:404`）→ `interpreter.handleEvent`（`:408`）→ `FlowInterpreter.handleEvent/dispatchEvent`（`FlowInterpreter.ts:342` 起，整链包在 `runWithProjectRoot` 的 AsyncLocalStorage 里）→ `settleWaitsAndResume`（`:361`）优先，未命中则 `dispatchEventInner` 走 `matchTrigger`（`:414`）→ `execBlock`（`:684`）逐节点 `runWithDeadline`（`:898`）。
9. 回复：节点里 `bot.callApi('send_group_msg', ...)`（`FlowRuntime.ts:438`，默认超时 30s，echo 键 `ts_<n>`）→ `EventBus.handleAction`（`eventBus.ts:1145`，**不 await**，每个 action 独立异步）→ `src/main/services/translator/actions/message.ts:127 sendGroupMsg` → `host.ctxFor(params, openid)`（`eventBus.ts:611`）取被动回复上下文 → `sendOfficialMessage` 走 `https://api.sgroup.qq.com`。

### B. 串行 / 并行 / 队列

| 环节 | 行为 | 证据 |
|---|---|---|
| 官方网关收包 | 单线程同步 emit，无排队 | `official/ws.ts:23-78` |
| T→运行时传输 | 有界 **出站** 背压，但只丢心跳（`isCritical` 判定），业务消息永不丢也永不排队 | `eventBus.ts:806-827` |
| 事件派发 | `void dispatchEvent`，**无队列、无并发上限、无 per-session 串行** | `FlowRuntime.ts:401` |
| action 执行 | `handleAction` 不 await，逐个独立跑 | `eventBus.ts:1145,1165` |
| 定时/生命周期 | 每秒 tick，`ticking` 重入闸门；无定时触发器的项目提前 return | `FlowRuntime.ts:212-245` |
| 现成的队列实现 | `MessageQueue`（并发/超时/限流）**生产零引用**，是死代码 | `src/main/services/infra/messageQueue.ts:1-166`，grep 仅命中自身与测试 |

### C. 背压 / 去重 / 乱序 / 重试 / 超时 / 取消

- **背压**：仅出站 WS（`pendingSends > 1000` 或 `bufferedAmount > 1MB` 时丢非关键帧）；入站零背压，慢流程会让 pending async 链无限累积（每条消息一个链）。
- **去重**：核心零去重。lark/dingtalk 在 adapter 里用 `Set` 去重（上限 5000，`lark/adapter.ts:51-54`）。官方链无。
- **乱序**：结构性可能（A 见上）。同一用户两条消息，第 2 条先回是正常现象。
- **重试**：网关重连有指数退避（`official/ws.ts:175` `min(1000·2^n, 30000)`，5 次后停）与翻译器重连（`FlowRuntime.ts:355-361` 固定 3s，无限次）；**action 无重试**，节点无重试（`execRetryExec` 是节点级显式语义，`FlowInterpreter.ts:1478`）。
- **超时**：action 30s（`FlowRuntime.ts:438`）；节点 30s（`FlowInterpreter.ts:108 NODE_TIMEOUT_MS`）+ `runWithDeadline`（`:898`）—— 但 `LONG_WAIT_HANDLERS`（`wait_input`/`delay`）与 `SELF_TIMED_HANDLERS`（`http_request`/`api_call`）不设 deadline（`:882`）；`wait_input` 超时 1s–30min（`waitRegistry.ts:132-133`）。
- **取消**：`FlowRuntime.stop()` 会 reject 所有 pending action（`FlowRuntime.ts:108-137`）、`clearWaits(projectPath)` 清挂起（`botManager.ts:370`）；但**没有任何 API 能取消一条正在跑的链**——`runWithDeadline` 超时后 handler 仍在后台跑，副作用不入账（`FlowInterpreter.ts:898` 区域注释自承）。节点级"协作式中断" `shouldAbort` 只存在于测试/预览通道（`tests/helpers/runFlowGraph.ts:166`）。

---

## 风险清单

| 严重度 | 位置 | 触发条件 | 影响 | 最小修法 | 证据类型 |
|---|---|---|---|---|---|
| **P0** | `eventBus.ts:806-827` + `botManager.ts:250,256` | 启动装配窗口（官方适配器已连、运行时未连）或运行时重连的 3s 窗口 | 该窗口内所有入站消息静默丢弃，无日志无落盘，用户侧表现为"机器人没反应" | 入站事件先进有界队列（容量+丢弃计数告警），运行时连上后按序回放；或把 `EventBus` 挂在进程内直连（`onEvent` 已存在，`eventBus.ts:782-787`）避免 socket 往返 | 已读代码 |
| **P0** | `waitRegistry.ts:228,233` | 群里 A 触发 `wait_input`，B 先发言；或 scope=`any_in_group` | B 的消息被当作 A 的输入写进 `__var__:`，A 的链带着别人的内容继续跑并回复 A | `matches()` 强制比较 `userId`+`chatId`（缺字段即不匹配），`any_in_group` 至少要校验 `chatId` 并显式声明"谁都能应答" | 已读代码 |
| **P0** | `eventBus.ts:684-713` | 本机任意进程（含其它用户态程序、被投毒的插件子进程）连 `ws://127.0.0.1:<port>/<BUS_WS_PATH>` | 可订阅全部消息事件（含 openid）、可下发 `send_group_msg` 以机器人身份发消息 —— 完全绕过流程与权限 | 握手加一次性 token（启动时生成，经 IPC 只给本运行时）；或换 `net` 本地 socket + `SOCK` 权限；至少校验 `Origin`/随机 path 并拒绝未知 client | 已读代码 |
| **P0** | `FlowRuntime.ts:401` | 同一会话连续两条消息（尤其第一条链含 `http_request`/`delay`/`wait_input` 之外的慢节点） | 两条链并行，回复顺序与到达顺序不一致；同一条"查询余额→扣款"式链可交错导致重复扣款 | 每会话一条串行队列（key = `chatType:chatId`），跨会话并行；队列长度上限 + 超时丢弃并回一句"忙" | 已读代码 |
| **P1** | `FlowInterpreter.ts:453` + `FlowRuntime.ts:212-238` | 安静群/单聊里 `wait_input` 到期，期间无任何新事件 | 超时分支永不执行（心跳里没有等待结算，`tickSchedules` 全函数不碰等待表），链永久挂起；用户下一条消息（哪怕是别人的，见上条）被当输入吞掉 | 心跳里独立结算等待（`takeExpired` → `resumeFromWait` 走超时出口），与触发器逻辑解耦 | 已读代码 |
| **P1** | `waitRegistry.ts:183`（`restoreWaitsFor`） | 进程崩溃/重启后 | 落盘的 `pending-waits.json` 从不加载，挂起链全部消失；用户重开软件后"机器人不说话"且无任何提示 | 启动时 `restoreWaitsFor(projectPath)` 并对恢复的等待重发提示语；或明确删掉落盘代码，别留"看起来有恢复"的假象 | 已读代码 |
| **P1** | `FlowInterpreter.ts:414` | 同一事件被多个触发器匹配（多流程/多个 `message_received` 节点） | 只有数组里第一个命中，其余静默不执行；用户无法从画布看出哪条会跑 | 改为全部执行（并按节点 id 稳定排序），或在启动时静态检出"重叠触发器"并报错 | 已读代码 |
| **P1** | `official/ws.ts:26-27,35-42` + `client.ts` 的 `sessions` | 断线重连窗口 / 进程重启 | `sessionId`/`seq` 只在内存 → 重启后只能 `identify`，断线期间事件永久丢失（不会重放补回） | 持久化 `session_id`+`seq`（`userData/runtime/official-session-<project>.json`）以支持 op6 resume；仍不保证的消息需在文档/UI 明说 | 已读代码 |
| **P1** | `pluginScanner.ts:283-290` | 加载任意插件包 `index.js` | 插件顶层代码在**主进程** vm 里同步执行（上限 5s）→ 恶意/写坏的 `while(true)` 类代码阻塞整个主进程（所有机器人一起卡）；`node:vm` 不是安全边界（`sandbox.ts:11-26` 自承） | 插件入口脚本也放进 utilityProcess（与节点 handler 同一 worker），主进程只收 JSON 结果 | 已读代码 |
| **P1** | `persistentStore.ts:622-672` `batchSaveValues` | 批量写其中一条失败（如值不可序列化、键超长） | per-key `try/catch` 只把错误塞进 `errors[]`、**不 rethrow**（`:645-662`）→ 事务照常提交，一批数据半成功；且返回的 `keysList: Object.keys(dataDict)`（`:671`）把**全部**输入键都报成成功，调用方拿到的是"成功 + 若干错误"的自相矛盾结果 | 整批一个事务，任一失败即 rethrow 回滚；`keysList` 只回成功的键 | 已读代码 |
| **P1** | `FlowInterpreter.ts:1609-1613` `execParallel` | 并行分支写同名变量 | 合并 last-write-wins，静默覆盖，无告警 | 合并时检测冲突键并记 warn（附 nodeId），或给并行分支独立命名空间 | 已读代码 |
| **P1** | `FlowInterpreter.ts:565-600` `resumeFromWait`（重试分支） | `expectedPattern` 不匹配且 `maxRetries>0` | 每次重试 `registerWait` 换新 id 且把 deadline 重置为完整 timeout → 总等待时长 = `timeout × (retries+1)`，超时语义与画布配置不符 | 重试时保留原 deadline（只在原窗口内重试） | 已读代码 |
| **P2** | `botManager.ts:342-378` | 启动在 `runtimes.set` 之前失败（如 `new FlowRuntime` 抛错） | `stopBot` 找不到 runtime，无人调 `setStatus('stopped')` → 状态永久停在 `stopping`；`stopAllBots` 的过滤条件（`:401` 只收 running/starting）也漏掉它 | `stopBot` 末尾无条件 `instance.status='stopped'` | 已读代码 |
| **P2** | `botManager.ts:380-383` `restartBot` | 用户连点两次重启 | `stopBot`+`startBot` 无互斥，两次调用可交错（第二次的 start 撞上第一次的 stopping） | 按 projectPath 加 in-flight Promise 串行化 | 已读代码 |
| **P2** | `FlowInterpreter.ts:212-230` `loadFlow` | 每个事件 | 每事件一次同步 `statSync`（主进程阻塞）；flow.json 变更后正在跑的链仍用旧 graph，`resumeFromWait` 重新 load 可能跨版本、找不到出口节点 | 用 `fs.watch` 推送版本号，事件只读内存版本；链内固定 graph 版本 | 已读代码 |
| **P2** | `persistentStore.ts:344` `getDbPath` | 每个持久化节点 | 每次 `migrateLegacyGlobalDb` 至少一次 `existsSync`；`saveValue` 单行写也包一层 `db.transaction`（`:580`） | 迁移只跑一次（模块级 flag）；单语句写不开事务 | 已读代码 |
| **P2** | `baseSqliteStore.ts:63` | 所有 `.qqbat/*.db` | 只设 `journal_mode=WAL`，没有 `synchronous`/`busy_timeout`（对比 `persistentStore.ts:265-268` 设了 4 项）→ 并发写时 `SQLITE_BUSY` 直接抛 | 统一 pragma：`busy_timeout=5000`、`synchronous=NORMAL` | 已读代码 |
| **P2** | `eventBus.ts:611-620` `ctxFor` | 发送时未带 `message_id`（手写 `callApi`、富媒体、异步回调） | 退回"该会话最后一条消息"槽位 → 被动回复可能带上别人的 `event_id`/`msg_seq` | 槽位改为"按会话+触发链"绑定，或干脆不带 `event_id` 走主动消息（有配额限制但语义正确） | 已读代码 |
| **P2** | `FlowInterpreter.ts:882` | 节点是 `wait_input`/`delay`/`http_request`/`api_call` | 这四类不设 deadline，卡住的 http 会拖住整条链（无链级超时） | 加链级总超时 + 可取消的 fetch（AbortController） | 已读代码 |
| **P2** | `plugins/pluginLoader.ts:285` `setPluginEnabled` | 禁用插件后 | 只摘触发器，画布上已存在的节点仍可执行（历史语义），用户以为"已停用" | 禁用时同时从 handler 表摘除，或 UI 明确标注"仅停触发器" | 已读代码 |
| **P2** | `scheduler.ts:34` cron 解析 | `dom` 与 `dow` 同时受限 | 一个 `&&` 把两者串起来（`fieldMatches(dom,...) && fieldMatches(dow,...)`）→ 必须同时满足；传统 cron 是 OR，用户抄来的 `0 9 1 * 1` 不会按预期触发 | 改成 OR（两者都被限定时），或在节点配置里显式给出"AND/OR"开关并写清语义 | 已读代码 |
| **P2** | `waitRegistry.ts:223` `saveWaitsFor` | 每次 `registerWait` | 同步 `writeFileSync` 整个等待表（含变量快照），在主进程上做磁盘 IO | 改异步 + 合并防抖（同项目串成一条写链） | 已读代码 |
| **P2** | `eventBus.ts:33-45` `writeWithRetry` | Windows `rename` 撞句柄 | 只有 3 次 × 100ms 重试，且只用于 uid 映射；`atomicWrite` 有 `withIoRetry`（EPERM/EACCES/EBUSY/ENOTEMPTY，5 次 620ms，`retryIo.ts:14`），两条路径不一致 | 统一走 `atomicWrite`/`withIoRetry` | 已读代码 |
| **P2** | `dynamicHandlers.ts:55` 的模块级单例 worker | 多项目同时跑插件节点 | 所有项目/所有插件共用一个子进程；一个插件的死循环/崩溃影响全部（`untrustedWorkerHost.ts:322` 把所有 pending 一起 reject） | 每项目或每插件一个 worker（至少隔离崩溃域） | 已读代码 |

---

## 重写必须保留的行为

1. **`flow.json` 的"文件即真源"**：用户/插件/AI 直接改盘上的 JSON，运行时按 mtime 热加载（`FlowInterpreter.ts:212`）。这条比任何 IPC 写接口都重要 —— 保留"磁盘可编辑 + 自动生效"，但把 `statSync` 换成 watch。
2. **节点 handler 与插件的关系**：内置节点走静态表，插件节点按 `kind` 覆盖/追加（`handlerRegistry.ts:89`），插件卸载按 `plugin_<name>_` 前缀整体摘除（`dynamicHandlers.ts:285`）。
3. **平台隔离的形状**：`SandboxTranslator`（本地 WS 服务端 + 假 selfId + `capabilities()` 词表，`sandboxTranslator.ts:14-227`）让 lark/dingtalk 与官方走同一运行时协议。这个"翻译器 = 协议适配器"的接缝值得保留（但应改进程内接口，不要再绕一层真 socket）。
4. **被动回复语义**：`ctxFor` 先按 `message_id` 精确命中、再退回会话槽位（`eventBus.ts:611`），并且只有"同一会话"才带 `message_id`（`builtinNodes/messageSendBase.ts:529-545`）。这是 QQ 官方被动回复的硬约束，重写时不能简化成"随便带"。
5. **插件权限在宿主侧判**：`permissionFor`/`denyReason` 在**宿主**（`pluginPermissions.ts:19-80`），插件自己声明的 `permissions` 被 manifest 强制覆盖（`pluginScanner.ts:331`）。这条防线必须保留且不能下移到插件进程。
6. **沙箱的工程细节**：`Object.create(null)` contextify、`codeGeneration:{strings:false,wasm:false}`、不拼接源码（`compileInSandbox` 注释 `sandbox.ts:849-870`）、只接受 JSON 字符串信封、绝不 await 沙箱 realm 的 thenable（`unwrapEnvelope` `:834`）。这些是踩过逃逸坑之后的形状，重写照抄成本远低于重新发现。
7. **`userId` 整数化 + openid 另一列**：`getOrCreateUid`/`resolvePlayerUid` + `_openid`（`eventBus.ts:493-530`）让流程侧只面对数字（表达式、存档、UI 都简单），平台 id 藏在映射里。保留这个分层，但把 `uid_mapping.json` 换成 SQLite（当前 `.bak` + 3 次重试的 JSON 落盘在消息热路径上）。
8. **`atomicWrite` + `withIoRetry`**：`.tmp` → `rename`，EPERM/EACCES/EBUSY/ENOTEMPTY 重试 5 次共 620ms（`fileOps.ts:160-235`、`retryIo.ts:14-40`）。Windows 上这是必需品。
9. **`on_ready` 等连接就绪才触发**，`on_startup` 早于连接且只跑一次（`FlowRuntime.ts:103-105,311-313`）。这两个语义用户已经在用。
10. **headless 与桌面同一套装配**：`standalone.ts` → `startHeadless`（`headlessAssembly.ts:76`）复用 `startTranslator`+`startOfficial`+`FlowRuntime`，docker 部署与桌面行为一致。保留"一个装配函数两个入口"。

---

## 可以放弃的行为

1. **主进程内的 WS 中转**（`EventBus` 服务端 + `FlowRuntime` 客户端连回环地址）。同进程还要走真 socket，带来：无鉴权端口、启动窗口丢事件、连接/重连状态机三份（网关、翻译器、运行时）。换进程内 `EventEmitter`/直接函数调用，跨进程部署再单独做一条传输。
2. **`translators` 按端口全局复用、`stopBot` 故意不停翻译器**（`eventBus.ts:242-250` 注释明说）。结果是"停了机器人，端口和订阅还在"，多项目时行为难以推理。改为"生命周期跟 bot 走"，端口租约单独管理。
3. **`wait_input` 的 `restoreWaitsFor` 落盘恢复**：功能从未接线（零调用），且"重启后自动继续三天前的等待"本身语义可疑。重写要么做完整的持久化会话（含链进度），要么明确"重启即放弃挂起"并在 UI 提示。
4. **`MessageQueue`**（并发/超时/限流，`infra/messageQueue.ts`）：死代码，但它的**语义**要保留（重写应直接作为主链路的入口队列），实现可重写。
5. **`hashToInt(eventId)` 当 `message_id`**：32 位有符号哈希，碰撞即错误共用上下文槽位。新协议应用字符串/`Number` 安全 id 或直接保存 eventId 映射表。
6. **`center/tickSchedules` 与等待结算耦合的心跳**：现在一秒一次的空转只服务触发器，等待超时反而没接上。重写把"时间驱动的两件事"（触发器、等待超时）做成一个明确的 ticker。
7. **插件 `index.js` 在主进程 vm 执行**：加载期只为了拿 `nodes`/`triggers` 声明。改成一个纯 JSON manifest + 可选 worker 预检，主进程永不执行插件代码。
8. **`GROUP_MESSAGE_CREATE` 的 `to_me` 字符串猜测**（`eventBus.ts:1078` `content.includes('<@') || startsWith('/')`）。平台没有权威字段时应在 UI 明示"@我"不可靠，而不是猜。
9. **`adapter` 三值 zod 枚举 + `botManager` if/else**：`src/main/schemas/project/project.ts:33` 与 `botManager.ts:189-229`。重写成注册表（`Map<adapterId, factory>`），新增平台只注册一个工厂。
10. **`e2e/node-flow.spec.ts`** 这种"没起 dev server 就静默 skip、且只测渲染层"的 E2E。它给出的是虚假的覆盖率信号。

---

## 未知与需要实测

1. **官方网关重连后到底会不会重放事件**：代码把 `seq` 收包即前移（`official/ws.ts:26-27`），因此 op6 resume 不会重发已收事件 —— 但这是**协议行为推断**，未用真实网关验证。需要实测：断网 30s 再恢复，同一 `eventId` 是否出现两次。
2. **`wait_input` 超时在真实会话中的表现**：按代码推断"下一条消息才结算"，需要一台真机、一个安静群、等 60s 观察（同时验证 `any_in_group` 的跨用户唤醒）。这是本轮最该先做的实测。
3. **Windows 上 `.qqbat` 目录的并发写**：`uid_mapping.json`（`writeWithRetry` 3×100ms）、`pending-waits.json`（同步写）、`flow.json`（atomicWrite）三者在同一目录高频写，是否会出现真实的 EPERM 竞争，未压测。
4. **`node:sqlite` 回退路径**（`sqlite.ts:123`）在真实项目上是否可用（`node:sqlite` 的 API 与 better-sqlite3 不完全同构，`baseSqliteStore`/`persistentStore` 的 `pragma`/`prepare` 调用是否全兼容未验证）。
5. **插件 vm 逃逸的可达性**：`codeGeneration.strings=false` + null 原型 contextify 是硬化过的，但 `node:vm` 官方不承诺安全边界。需要一个专门的逃逸测试集（`this.constructor.constructor`、`Error.prepareStackTrace`、`Symbol.toPrimitive` 等）才能定级。本条目前按 P1 记。
6. **`untrustedWorker` 的 `utilityProcess` 路径**在 Electron 打包版里是否真的启用（`workerTransport.ts` 的 `hasUtilityProcess()` 分支；开发态走 `fork`）。两条路径的超时/崩溃语义是否一致未验证。
7. **多账号并发**：`botManager` 按 projectPath 一个 bot，多账号 = 多个项目。多个项目同时跑时的端口分配（`resolveUsableTranslatorPort`）、`translators` 复用、以及"停一个项目是否影响另一个的官方会话"（`sessions` 按 projectPath+时间戳 key，`stopOfficial` 只停自己）需要并发实测。
8. **`GROUP_MESSAGE_CREATE`（非 @ 的群消息）是否真的能收到**：依赖 intents 申请与平台策略，代码里已经把它当普通群消息处理，但没证据表明真实机器人能收到全量群消息（多数 QQ 机器人只能收 @）。
9. **`i18n`/日志/遥测**未纳入本次审查（不在范围内），但重写时若改动链路，`emitSystemLog`/`emitNodeEvent` 的 IPC 出口需要跟着核。