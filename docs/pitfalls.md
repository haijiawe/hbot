# 已踩过的坑

> 从 [`AGENTS.md`](AGENTS.md) 拆出来的全表。**编号稳定，永远不要重排或复用**：
> 代码注释与设计文档里的「坑 66」「坑 69」都指向这里的序号。新增一条就接在末尾顺延。

> 每条只写「现象 / 根因 / 判据 / 去哪儿看」，不写修复过程。旧项目的坑是**证据**：
> 它们说明某条不变量为什么必须存在（对应 `docs/00-assessment.md`）。

1. **启动/重连窗口丢消息**：旧项目先连平台、后建订阅（`botManager.ts:250,256`），`eventBus.ts:806-827` 只遍历现存客户端。新项目必须先建 supervisor/队列，再 connect。
2. **本地总线开成 socket**：旧 `eventBus.ts:684-687` 连接即加入，无鉴权，任意本机进程可冒充机器人。新项目内部总线**进程内**；对外只有 127.0.0.1 + token 的 RPC。
3. **同会话并发**：旧 `FlowRuntime.ts:401` `void dispatch`，同会话两条消息并行、回复乱序。新项目每会话串行是硬约束。
4. **等待表只在内存**：旧 `FlowInterpreter.ts:437-440` 重启即丢、超时靠下一条事件。新项目所有等待/定时落库，调度器主动结算。
5. **审批 always 按大类放行**：旧 `aiAgentCoreApproval.ts:151-184` 对 a.txt 点“别再问”，同轮任意路径都放行。新项目规则键必须含规范化参数。
6. **迁移先覆盖后校验**：旧 `flow.ts:222-232` 备份失败被空 catch 吞掉，覆盖后才 schema 校验。新项目顺序：读 → 校验 → 备份成功 → tmp → fsync → rename。
7. **命令沙箱回退白名单**：旧非 Windows 直接退化名字白名单（`confine.ts:116`、`sandbox.ts:66`）。新项目建不出隔离就拒绝。
8. **后台命令绕开沙箱**：旧 `job_start` 只查写档，`spawn(shell:true)`（`registry.ts:863-887`、`jobs.ts:184-188`）。新项目所有命令同一入口。
9. **`node:vm` 当安全边界**：旧 `sandbox.ts:30-34` 自己承认不是。新项目用 QuickJS isolate，且限制内存/时间。
10. **半成功批量写**：旧 `persistentStore.ts:622-672` 逐键 catch，全部报成功。新项目单事务，全有或全无。
11. **契约三处手写**：旧 preload 1,421 行 / 类型 2,188 行 / 308 invoke，新增改 4–5 处。新项目 Rust 单一真源 + 生成。
12. **首屏静态加载巨型目录**：旧 `nodeCatalog` 把 6,072 行文案拉进首屏。新项目动态 import + 预算。
13. **按行数当复杂度**：旧项目已改为“函数能力分类”。新项目同样看职责，不搞行数 KPI。
14. **Windows 原子替换 EPERM**：目标被占用时 rename 会失败。写入要重试 + 串行化；不要当成“写成功”。
15. **密钥静默明文**：旧 `safeStorage` 不可用时直接写明文。新项目拒绝或显式确认 + 审计。
16. **闸门本地与 CI 语义不同**：旧 `format` 闸门本地只查 `git diff --cached`，不改暂存区等于没查。新 `scripts/check.sh` 必须对全树判定，本地与 CI 同一条命令。
17. **测试配置一收窄就假绿**：旧项目曾把 vitest include 改窄，364 个测试文件静默不跑。新项目的 nextest/Playwright 配置要有“期望测试数对账”，数量突变即红。
18. **测试断言源码文本**：旧仓库 88 个测试读源码字符串，重命名会以离奇断言失败出现。新项目用类型/契约/行为；结构约束交给依赖图与 `cargo`。

## 7.1 新仓库自己已经踩过的坑（都是实测）

19. **`Drop` 里的无界 `join()` 会把宿主挂死**。`crates/hbot-code/src/runtime/isolate.rs:235`：sandbox 的 worker 线程在 bootstrap 失败后卡在 `recv`，发送端又在 `Drop` 内部，通道永不 disconnect，`join()` 永不返回 → 进程 0% CPU 假死、日志无错。规则：**任何 `Drop` 里的等待必须有超时**（`recv_timeout(handler_timeout + 1s)`），超时就 `error!` 并 detach，绝不 join 到底；`tests/s1_isolate_lifecycle.rs` 是这条的回归。
20. **唤醒只在“全局归零”时通知会造成假死**。`crates/hbot-bot/src/queue.rs:492-494` 只在 `total == 0` 时 `notify_waiters()`，而 `enqueue` 等它：会话 A 满 → 读循环阻塞 → A 腾出容量但别的会话仍有在途 → A 的等待者永不醒，表现是「bot 突然不回消息、日志干净」。规则：**每处理完一条就按会话通知**，且 `enable()` 必须先于 `try_enqueue` 注册。
21. **按 UUID 主键排序 ≠ FIFO**。`Store::list_pending_outbound` 曾是 `ORDER BY id`，恢复时出站顺序被打乱。规则：任何“待处理队列”的读取都要 `ORDER BY created_at, rowid`；UUID 只当主键，不当顺序。
22. **注释声称“下次启动会重放”，但生产零调用**。`crates/hbot-bot/src/supervisor.rs:677` 说消息已落库会重放，实际 `list_unprocessed_inbound` 一个生产调用点都没有，崩溃窗内的消息永远停在未处理。规则：**「会重放/会恢复」这类声称必须有测试证明**，否则它就是一句谎；恢复路径要写清语义（我们不承诺 exactly-once，承诺 **at-least-once** + 幂等键）。
23. **热路径上禁止 O(项目) 操作**。`sandbox_handler` 曾每条消息重算全项目指纹（全量读盘 + blake3）。规则：每消息路径只允许 O(1) 判断 + 内存缓存；文件变化靠 watcher 脏标志，内容指纹按需重算。
24. **编译失败不能让 bot 停摆**。一个 typo 就 `Err` → inbound 不标记 → 每条消息都失败。规则：**保留 last-good 版本继续服务**，错误进 `RunLog` + UI；只有「从来没有成功过」才真报错。
25. **`tauri-build` 需要 `crates/hbot-desktop/icons/icon.ico`**，否则报 `` required for generating a Windows Resource file during tauri-build ``。图标是 PNG payload 的 ICO（6 字节头 + 16 字节目录项 + PNG 体），`icons/` 已入库，别删。
26. **`#[tauri::command]` 函数不能是 `pub`**：crate root 会 `E0255 __cmd__dispatch is defined multiple times`。命令写 `async fn`，只在 `run()` 里注册。
27. **PowerShell 的 .NET API 用进程 CWD**，`Set-Location` 不改它（`[System.IO.File]::WriteAllBytes` 会写到启动目录）。脚本一律用工具的 `workdir`，或全程 `Join-Path $PSScriptRoot`。
28. **长命令把输出写进管道会假死**：`cargo run` 输出没人读，进程卡在写、CPU 0%、看起来像死锁。规则：长任务一律 `*> 日志文件` 再读文件，不要靠终端缓冲区。
29. **`CARGO_TARGET_DIR` 必须按 agent 分目录**（`D:\hbot-target\<line>`）：多个 agent 共用同一个 target 会互相等锁，看起来像“卡住”。
30. **契约改了必须同批改三处**：`dto`（Rust）→ `cargo xtask gen`（TS 生成物入库）→ `apps/web` 消费点。`gen --check` 与 web 的 `tsc` 会各自抓到一半，别只修一半就提交。
31. **调试断点属于 bot，不属于 agent 会话**（`crates/hbot-core/src/debug.rs`，2026-11-16 冻结）。同一份源码在两个 bot 的 isolate 里可以是不同版本（热重载时机不同），断点按会话走会串味。`DebugSink::paused` 是**同步阻塞**的，只能在 isolate worker 线程里调用；实现方必须自带超时（建议 5 分钟）并返回 `Continue`，否则挂死宿主。
32. **命令执行 fail-closed**：`hbot-command` 的能力矩阵实测是「Windows/Linux 有进程树 Containment、**没有写隔离**、macOS 全无」。因此 `read_only` 直接拒、`workspace_write` 也拒（`IsolationUnavailable`）、只有 `danger` 能跑。**不许为了"能用"放松这条**——装不上隔离就是拒绝，放宽等于把旧项目"非 Windows 退化成名字白名单"的坑再踩一遍（旧 `sandbox.ts:66,166`）。
33. **parity 测试要在别人的文件静止时跑**。并行改 `hbot-code` 期间跑 `crates/hbot-parity`，红可能是中间态（例如宿主 `log` 收集被改）：先看失败点是不是"断言没走到"而不是"结果不对"，再决定是打回还是复验。
34. **内存一律按 `PrivateMemorySize64` 在进程树上求和**，不要用 `WorkingSet64`：WebView2 的 DLL 被 6–7 个子进程各自映射，WS 会把共享页重复计入（实测整机 WS 382.5 MB vs Private 205.5 MB）。口径写在 `docs/adr/0002-agent-in-rust.md` §4。
35. **档位与命令白名单是配置，不是常量**：`HBOT_SANDBOX` / `HBOT_COMMAND_ALLOW` 或 `<data_dir>/agent.json` 的 `sandbox` / `allowedPrograms`。写错档位字符串是 `-32002`，**不许静默退回默认档**（那会把 `danger` 悄悄降成 `workspace_write`，用户以为命令能跑却一直被拒）。白名单默认空 = 一个程序都不许，且只放具体程序，别把 `cmd`/`sh`/`powershell` 放进去 —— 放解释器等于绕开"命令不经 shell"这条设计（`crates/hbot-rpc/src/app/agent/config.rs`）。
36. **前端构建不产出 `.map`**：`tauri build` 会把整个 `apps/web/dist` 内嵌进安装包，实测 58.16 MiB 里有 44.20 MiB 是 sourcemap（关掉后 13.96 MiB）。dev server 的 sourcemap 在内存里，不受影响。
37. **运行时的编译必须用 `compile_project_debug`**（`crates/hbot-rpc/src/app/runtime/sandbox_handler.rs`）：只有 debug 构建才插了 `__hbot_trace` 探针。退成 `compile_project`（release）不会报错，只会让 `debug_set_breakpoints` **静默永不命中** —— 这类"不报错的失效"比崩溃难查一个数量级。同时"没有断点就不 arm"是必须的：arm 后 probe-dense handler 实测 4.64×（上限 5×，余量 8%）。`hbot check` 与 diagnostics 反而**应该**用 release 编译（只要类型与错误，不要探针）。
38. **`ctx.log` 在 handler 抛错时也要留下**：日志在 `hbot-code` 的 `RawEvent` 里已经收集了，别让它在 `finish()` 的错误分支被丢掉（旧实现就是这样）。用户看到"我的 bot 出错了，但出错前那三行 log 一条都没有"时，几乎无法定位。
39. **数据目录只有一份算法：`hbot_core::paths::data_dir()`**（2026-11-16 修）。此前有三份拷贝：`hbot-rpc::dispatch`、`hbot-cli::serve` 各算一份给 `%APPDATA%\hbot`，桌面壳用 Tauri 的 `app.path().app_data_dir()` 给 `%APPDATA%\com.hbot.app` —— **同一个产品的桌面端和后台端各开一个库**，`hbot replay` 又按另一份找库，用户会看到"我刚跑过的消息在回放里不见了"。任何新入口（新 CLI 子命令、托盘、服务化）都从这里取，**不许再写第四份**。
40. **数据目录不要绑定 Tauri 的 `identifier`**。`app_data_dir()` 的路径由 identifier 派生，改 identifier 就等于静默搬家（用户数据留在旧目录没人再去看）。identifier 只决定安装/签名身份：`0.1.0` 发布前已从 `com.hbot.app` 改成 `com.hbot.desktop`（原值以 `.app` 结尾，bundler 会告警——与 macOS 的 bundle 扩展名撞车），**发布之后不要再改**，否则升级会变成两个互不相识的应用。
41. **Windows PowerShell 5.1 会把 UTF-8 无 BOM 的 `.ps1` 当 GBK 读**，中文注释直接炸成 `Unexpected token '}'`。`scripts/*.ps1` 必须带 BOM（`EF BB BF`）；用 `write`/`edit` 写完要检查首三字节。同理，**写 JSON 给 Rust 读时不要用 `Set-Content -Encoding UTF8`**（PS 5.1 会加 BOM，而 `serde_json` 不接受 BOM）——用 `[System.IO.File]::WriteAllText($p, $json, (New-Object System.Text.UTF8Encoding $false))`。
42. **类型声明与运行时不符，是最难查的一类 bug**。实例：`hbot.d.ts` 声明 `ctx.text: string`，而 `event_json` 里 `MessageCtx::text()` 是 `Option<&str>`，纯图片入站的 `payload` 没有 `text` → JSON 落成 `null` → 用户代码 `ctx.text.length` 抛异常，而 `tsc` 一个字都不报。规则：**任何面向用户代码的类型声明都要有一条"运行时真的给出这个类型"的测试**（构造缺字段的入站，断言不抛），否则声明就是谎话。
43. **`vitest.config.ts` 的 `include` 只许扩，不许收**。这次为了 `.tsx` 组件测试把 `['src/**/*.test.ts']` 扩成 `['src/**/*.test.ts','src/**/*.test.tsx']`；反过来收窄会让测试文件静默不跑（旧项目踩过 364 个文件的坑，见坑 17）。组件测试靠文件首行 `// @vitest-environment jsdom` 逐个挂，**不要**把全局 `environment` 改成 jsdom：38 个 store 测试是纯逻辑，拉进 jsdom 只会更慢。
44. **并行开发请按 crate 划边界，不要靠"小心"**。共享工作树上，别人把 `crates/hbot-command` 改到不可编译时，`cargo clippy -p <你的 crate>` 也会红（`-D warnings` 作用整个构建图），你会误判成自己的问题。做法：① 每个单元独占 `CARGO_TARGET_DIR`；② 判断自己的对错用 `-p` 且先看失败点是不是"根本没编译到我的代码"；③ 实在被挡住就 `git archive HEAD` 出干净快照、只覆盖自己的文件验证，验完再回真树复跑。
45. **构建缓存会把磁盘吃干净，而症状看起来像代码坏了**。2026-11-16 实测：8 个并行单元各一个 `CARGO_TARGET_DIR`，`D:\hbot-target` 累积到 **49 GB**，D 盘只剩 4.8 GB，于是出现 `ENOSPC: no space left on device`、`rustc-LLVM ERROR: IO failure`、后台任务莫名退出——两个子代理都把它当成自己的 bug 查了一轮。规则：① 构建缓存放**剩余空间大的盘**（本机 `E:` 有 310 GB，`D:` 只有 380 GB 且常被占满），用 `E:\hbot-target\<单元名>`；② 单元收工就删掉自己的 target 目录，别留着；③ 看到 `ENOSPC`/`IO failure` 先 `Get-PSDrive` 看剩余空间，再怀疑代码。
46. **"按 crate 划写域"仍然会让两个人互相堵住**（2026-11-16 踩到）。`cargo` 的编译单元是**整个 crate**，不是文件：`search-io` 把 `crates/hbot-workspace/src/search.rs` 写成中间态时，另一个单元对 `project.rs`/`files.rs` 的改动**一行都验不了**（`cargo test -p hbot-workspace` 直接编译失败），而两边改的文件根本不重叠。规则：① 同一 crate 里并行，**共享文件必须明确归属一个人**，其余人只能读；② 改到一半就把 crate 弄成编译不过的人，**有义务第一时间广播"已可编译"**，别人据此放行；③ 被挡住的一方**不要空等也不要越界代改**——先做能做的（写文档、设计变异脚本、静态自查），并靠"错误是否全部落在他人的文件里"来判读自己的部分是否已经编译通过。
47. **对外错误文案不许带项目根的绝对路径**（RT-01，2026-11-16 修）。`NotFound` 之类的错误会把 `E:\HBot-Next\projects\demo\src\main.ts` 整条路径拼进 message，而这条 message 会作为**工具输出喂给模型**、也会进日志——用户的目录结构因此泄漏进模型上下文与日志文件。规则：面向模型的错误只给**项目内相对路径**（`hbot-workspace` 统一走 `display_rel` / `Workspace::display_path`，`io_error` 收 root 参数）；操作系统的原始错误（`std::io::Error`）可以保留，但要与相对路径拼接后再外传。新增错误构造点时先问一句"这条 message 会不会被模型看到"。

48. **网络授权判据只有一份：`hbot.json` 的 `permissions.http`**（策略 C，2026-11-16 定，见 `docs/02-architecture.md` §6.6）。判据在 `crates/hbot-code/src/permissions.rs`；适配层（`hbot-adapters`）**不再按地址位置**（回环/私网/云元数据）二次拒绝——`FetchOptions::allow_private` 默认 `false` 保留严格口径，而 `hbot-rpc` 的 `AdaptersHttpTransport::new()` 恒置 `true`，因为上游已经判过。别在适配层"顺手加一条 SSRF 检查"：那会立刻造成"清单写了 `127.0.0.1:11434` 却连不上"的两道判据打架，而且**最终解释权在第一道**，第二道只是让人困惑。要收紧就收紧清单。已验证的边界：`read-only` 档仍在授权之前拒（不建 socket）、`http://2130706433/` 这类歧义数字主机仍拒（解析歧义，非位置）。

49. **Landlock 的 `handled_access_fs = 0` 意思是"什么都不管"，不是"什么都不许"**（2026-11-17）。
   `crates/hbot-command/src/landlock.rs` 第一版把**放行掩码**当成了 handle 掩码，于是 `read_only`
   档变成零规则集 —— 限制层装上了却等于没装，而所有测试都是绿的（因为没人从 Linux 内核那侧验）。
   现在拆成 `write_permissions()`（放行的位）与 `handled_write_bits()`（接管的位，恒为全集、按 ABI 收窄）。
   同族：`pre_exec` 装载失败要 `_exit(127)` **并**往 stderr 写标记，父进程必须同时看到两者才改判
   `IsolationUnavailable`（只判 exit code 会把"命令自己退 127"误判成隔离失败）。**本机没有 Linux 内核，
   本层一次没真跑过**，证据只到"交叉编译 + 纯逻辑单测"，别写成"已验证"。

50. **条件断点在 JS 侧求值，因此它不是沙箱**（2026-11-17）。条件是一段真的 JS 表达式，在 isolate
   里用 `new Function` + `with(scope)` 求值 —— 那里才有 `ctx`、局部变量与模块作用域。后果：
   ① 条件里**能调宿主函数**（`ctx.reply()` 会真发消息、`ctx.device.ocr()` 会真截屏）；
   ② 给名字赋值不生效（scope 是快照），但改对象属性会真生效。规则：条件永远只在
   "探针判为候选命中且非单步" 时求值（否则又会撞 4.64× 的开销上限），坏条件/抛错/引用不存在的
   名字**一律当没命中**、同一 `file:line` 只 warn 一次、绝不打断 handler。UI 要如实提示上面两条，
   不要写"受限求值器"这种让人以为安全的描述。

51. **断点与日志点是两张表，不是给断点加一个 `kind` 字段**（2026-11-17）。`crates/hbot-core/src/debug.rs` 的
    `Breakpoints` 同时有 `files`（会停下）与 `logs`（只打印），各自 `set()` / `set_logs()` **整份替换单个文件**。
    若合成一张带种类的表，"只改条件"和"只改日志点"会互相清掉对方：UI 单独加一个日志点，同文件所有断点消失。
    因此 `hbot-rpc` 的 `plan_breakpoints` 必须按"这次请求里该文件出现了什么"分别构造两路，**只在非空时才调**对应 setter。
    另一个哨兵：`DebugSink::logged` 有默认空实现（让只想处理暂停的实现不必改），所以真实 sink 忘了覆写时
    **日志点在 UI 里什么都不会出现，且没有任何报错** —— 加新的 sink 实现时先搜一遍 `fn logged`。

52. **资源上限必须 fail-closed：装不上就拒绝执行，不是"装不上就放开"**（2026-11-17）。`crates/hbot-command/src/limits.rs`
    与 `containment*.rs` 里，CPU/内存/进程数/IO 四条限额谁的平台原语装不上，就让这次执行返回
    `CommandDenied::{CpuLimit,MemoryLimit,IoLimit,ProcessLimit}`（`HBOT_COMMAND_*_LIMIT`），**绝不静默降级成无限额**。
    下游 `crates/hbot-agent/src/tools/shell.rs` 的 `denied_to_result` 把它们归到 `TooLong` 一类（命令真跑了并被杀：
    不记 denied 以免污染审批率，也不记 internal 以免误导排障）。`CommandOutput` 带 `usage: ResourceUsage`，
    改 core 公开结构后先 `cargo check --workspace --all-targets` 再报清单（加字段会打断所有测试字面量）。

53. **`Ctx<'js>` 不许存进 `Async` 闭包 —— 否则整个测试进程在退出时崩**（2026-11-18，代价 >30 分钟）。症状：所有
    `hbot-code` 的 isolate 集成测试（连 `s3_device.rs`）在**进程退出那一刻**崩
    `Assertion failed: list_empty(&rt->gc_obj_list), file …quickjs.c, line 2704`，退出码 `0xc0000409`
    （`STATUS_STACK_BUFFER_OVERRUN`），加 `--test-threads=1` 也一样；单条跑、连跑三次全量都可能恰好全绿，所以会被误判成 flake。
    原因：`let handle = ctx.clone();` 把这个 `Ctx` 存进闭包后，QuickJS 运行时的引用计数不归零，`JS_FreeRuntime` 撞断言。
    做法：把 `Ctx<'js>` 作为 **`Async` 闭包的首参**取回来 —— `Async(move |handle: Ctx<'js>, key: Value<'js>, …| { … })`。
    `FromParam for Ctx` 的 `param_requirement()` 是 `none()`，不占位置参数，所以 **JS 侧调用签名不变**。
    新增任何 `Async` 宿主函数（`state_api.rs`、`device_api.rs`）时先照这个写法，别 `clone()` 进闭包。

54. **`String(error)` 比 `error.message` 多一层 `Error: ` 前缀**（2026-11-18）。`Exception::throw_message` 造出来的是
    `Error` 实例，JS 侧 `String(err)` 走 `Error.prototype.toString()` 得到 `"Error: " + message`，于是断言
    `assert!(e.contains("HBOT_STATE_INVALID"))` 在 `startsWith` 口径下会红。所有 `HBOT_*` 前缀断言一律用
    `error.message`（或先剥前缀）。同理 `hbot.d.ts` 里写的错误码要能在 `error.message` 里整条看到。

55. **`Arc<具体类型>` 不能 `Arc::clone(&x)` 变成 `Arc<dyn Trait>`**（2026-11-18）。`fn new(state: Arc<dyn BotState>)`
    收到 `Arc::clone(&counting)` 会报 `expected &Arc<dyn BotState>, found &Arc<CountingState>`。
    做法：显式标注目标类型 —— `let backend: Arc<dyn BotState> = state.clone(); let history: Arc<dyn HistoryReader> = state;`。
    同一个值要同时喂两个 trait 时别复制粘贴 `Arc::clone`，一次标注类型、一次直接 move。

56. **新增 `Option<T>` 配置字段时，`new()` 必须从 `config` 取值**（2026-11-18，差点漏掉整条接线）。`SandboxHandler::new`
    里照抄相邻 `device` 字段写成 `state: None`，把 `config.state` 整个丢掉 —— 结果状态能力永远是"宿主没装配"，
    5 条端到端测试全报 `HBOT_STATE_UNAVAILABLE`。规则：builder 类构造里**每个字段的初值只能来自 `config.<同名字段>`**，
    `None` 只允许出现在"这个字段在 config 里本来就没有"的地方；新增一个 `Option` 字段后，补一条"走真派发链路的测试"
    （`crates/hbot-rpc/tests/state_capability.rs` 就是照这个理由写的：它钉的是 `AppState::start_bot` → config → worker → isolate 的接线，
    别处覆盖不到）。

57. **稳定版 clippy 的两个新口径**（2026-11-18）：`map_or(true, …)` → `Option::is_none_or`（`unnecessary_map_or`），
    `map_or(false, …)` → `is_some_and`；`manual_strip`（`if raw.starts_with("\\\\?\\") { raw[4..].to_owned() }`）
    要改 `strip_prefix`。`-D warnings` 下这两条会让**别人的 crate**也红，所以改完自己那侧先 `cargo clippy -p <your crate> --all-targets`，
    再看一眼 `cargo clippy --workspace --all-targets` 的失败点是不是落在别人文件里（判读方法见坑 44）。

58. **"等模型"必须从 handler 预算里抵账，否则这个能力在默认配置下等于不存在**（2026-11-18）。handler 预算是
    `limits.handlerMs`，上限 2000ms；真实补全 3–30 秒。`crates/hbot-code/src/ai.rs` 的 `call()` 等完模型后
    重新 `deadline.arm(left)`（等于把墙钟抹掉），代价是必须有上限：每事件 4 次 / 累计 60 秒，
    `build_event` 每事件 `reset_event()`（不清零就变成"每 isolate 4 次"，第二次事件立刻报 `HBOT_AI_BUDGET`）。
    **测这条必须忙等，不能用 `ctx.sleep`**（`sleep` 自己也被夹到剩余预算，测不出抵账）：
    `crates/hbot-code/tests/s2_ai.rs` 的 4 条就是这个形状。

59. **宿主状态/模型/设备三类能力都有"声明层 vs 装配层"两个码，别混用**（2026-11-18）。
    `HBOT_*_NOT_DECLARED` = 项目清单里没写（补救动作是改 `hbot.json`）；`HBOT_*_UNAVAILABLE` = 宿主没装配
    （补救动作是去 IDE 设置里配）。把它们混成一个码，用户会在错误的文件里找问题
    （`kv` 是 `StoreBotState` 每次重读清单 + `StateSlot::handles()`；`ai` 是 `ProviderAiClient` 每次重读 +
    `AiSlot::client()`；`device` 是 `ManifestCapabilityGate` + `SessionScopedBackend`）。**声明层一律每次重读、
    不许构造时快照** —— 缓存声明会让"改了清单但 isolate 没换"变成静默失效。

60. **清单（`hbot.json`）参与热重载指纹**（2026-11-18，`crates/hbot-rpc/src/app/runtime/fingerprint_cache.rs` 的
    `source_files`）。isolate 出生时按清单装配能力，只算 `src/**` 会留下「改清单不改源码 → 指纹不变 →
    isolate 不换 → 新声明永不生效」的洞。代价：**任何清单改动都触发一次全量 isolate 重建**（正确性优先）；
    真嫌频繁应给清单单独做"只比 `capabilities`/`permissions` 子集"的指纹，而不是把它移出指纹。
    测试：`manifest_change_alone_changes_the_fingerprint` 与端到端
    `declaring_kv_in_the_manifest_only_takes_effect_without_touching_sources`（生产 TTL 2s/5s 不能注入，
    所以轮询 ≤80×100ms，单条 8–11 秒）。

61. **用 PowerShell 数组按下标增删 Rust 源码里的行会静默漂移**（2026-11-18，改 `sandbox_handler.rs` 时踩了
    五次）。`(Get-Content).LineNumber` 只要一次 `Select-String` 命中多处（或返回 `Object[]`），索引就错：
    曾把 `pub mod bots;` 覆盖成 `pub mod ai;`、整行删掉 `pub mod debug;`、`Self { … }` 丢三行、插出重复字段。
    规则：① 先确认"这个模式真的只匹配一处"（先打印命中数）；② 改完立刻 `git diff <file>` 逐 hunk 复查；
    ③ 多行结构（结构体字面量、模块表、builder 链）整块重写而不是逐行插删；④ 大改用 `edit` 工具（按字面量匹配）。

62. **`cargo fmt`/`cargo check` 报 `unexpected closing delimiter … src/app/mod.rs:2:1` 是文件被结构化编辑
    交错坏了**（2026-11-18）。症状：`git diff` 只显示文档注释被削短，fmt 却在第 2 行报语法错。
    原因：对同一文件做结构化编辑时把 `//!` 文档块与 `pub mod` 列表插在了一起。**别试图 fmt 修复**（解析就
    失败），按"文档块 :1-N / 模块表之后"两段重建。同族：`#[must_use]` 加在返回 `Result`/`Option` 的函数上
    会触发 `clippy::double_must_use`（删属性，改成 `#[must_use = "…"]` 同样触发）。

63. **清单类结构的构造一律 `..Default::default()`**（2026-11-18）。`Capabilities` 在 `state` 之后又长了 `ai`
    字段，所有写 `Capabilities { state: … }` 字面量的地方立刻 `E0063`。清单还会继续长（`timer`/`jobs`/`session`），
    写 `..Capabilities::default()` 只写关心的字段就不会每加一个能力都要改一遍下游。

64. **条件/日志点之外，`isolate.rs` 里还有第二个"事件上下文"**（2026-11-18）。`onTimer` 的 ctx 由
    `build_timer_event` 造，**刻意比消息 ctx 窄**（只有 `name`/`reply`/`log`/`sleep`/`ai`）。两个坑：
    ① 那个函数的局部变量**不能叫 `event`** —— `dts.rs` 的 `ctx_function_keys` 扫描器按 `event.set(` 行首
    收集"消息 ctx 上的宿主函数"，叫 `event` 会把 `name`/`ai` 混进去而红；② timer 的 `reply` 必须是**独立函数**
    （不是消息路径那个），否则"计划没声明 `conversation`"就无从判、只能猜一个会话发出去。定时器没有入站身份，
    `device.set_current_bot(None)` / `state.set_current_bot(None, None)` 每次显式清空 —— 别让它沿用上一个事件的身份。

65. **负向清单测试是按字符串匹配的，注释里也不能出现被禁名字**（2026-11-18）。`dts.rs` 的
    `hbot_dts_does_not_promise_unimplemented_apis` 断言 `HOST_DTS` 不含 `"onStart"` 等，而 `HOST_DTS` 里那句
    "`onStart` / `onStop` / `onSchedule` 仍未实现"本身就让它红。要说明"还没实现"就换个说法（"生命周期钩子
    仍未实现，见 docs/07 §4.1"），别把被禁字符串写进 d.ts 文本。

66. **给 `SandboxHandler` 加能力时要加三处，缺一处就"看着能用、其实没接上"**（2026-11-18，`ai`/`state`/`timer`
    /`device` 各踩过一次同名坑）。① `SandboxHandlerConfig` 字段；② **`new()` 里从 `config` 取**（写 `None`
    会静默丢掉整个能力）；③ `worker()` 里在 `spawn_with_http` 之后调对应的 `set_*`。再补 ④ `start_bot` 的
    字面量、⑤ `hbot-parity/tests/support/mod.rs` 的字面量（否则 `--all-targets` 编译红）。
    **判据**：每个能力都要有一条"没声明 → 报 `NOT_DECLARED` 而不是 `UNAVAILABLE`"的 dispatch 级测试，
    它同时证明"接线真的发生了"。

67. **`BotRegistry` 里的后台循环必须登记进 `aux`**（2026-11-18）。`bot_stop` 只发停止信号给 supervisor 并等
    它的 task，别的循环（tick 等）不会自己停 —— 不登记就是泄漏，而且它每轮可能正持有槽位锁，拖住 supervisor
    排空。顺序是**先 abort 辅助任务、再发停止信号**；`attach_aux` 返回 `false`（登记表里已没有这个 bot）时
    调度方要立刻 abort 自己。

68. **作业队列只能按 bot 认领，全局 claim 会让 A 跑 B 的作业**（2026-11-19，`C4.jobs` 落地时发现）。
    `crates/hbot-store` 的 `claim_due_jobs(now, limit)` 是**全局** SQL（没有 `bot_id` 条件）。
    每个 bot 有自己的 tick 循环、各自 claim 一次 —— 用它就等于 A 的 tick 把 B 的作业抢走，
    然后在 **A 的 isolate** 里跑。后果不是崩溃，而是跨 bot 隔离当场破：作业名对、载荷对、
    只有上下文全错（B 的结算用了 A 的 `ctx.reply` 目标 / A 的清单预算），日志里完全看不出来。
    所以派发只许走 `claim_due_jobs_for_bot`（同一份 SQL 判据，多一个 `bot_id` 条件；
    全局那份留给排障与重放工具）。存储层有一条 `per_bot_claim_never_takes_another_bots_row` 钉住它。

69. **作业 tick 不能复用定时器那条 `tick`**（2026-11-19）。`SandboxHandler::tick` 第一句是
    `if declared_schedules(&root).is_empty() { return Ok(()) }` —— 复用给作业会让"只声明了
    `capabilities.jobs`、没声明 `capabilities.timer`"的项目**永远派发不到作业**，而且症状是
    "作业不跑"而不是"配置读错了"，排障方向会被带偏。所以作业独立成 `tick_jobs`，ticker 每拍先后调两个。
    同一条纪律也说明：**每个"声明了才有"的能力都要自己第一句读清单**，别指望共享的循环替它判。

70. **作业是先编译、后认领，不是先认领、后编译**（2026-11-19）。`claim_due_jobs_for_bot` 一步就把行
    改成 `running` 且 `attempts + 1`。如果这一步发生在 `compiled()` 之前，编译失败的那一刻行已经在
    `running` 里，下一拍 `state != 'pending'` 拿不到它 —— 作业白等一个进程重启（只有 `resume_interrupted_jobs`
    能救）。同族的坑：**任何"先改状态、后做事"的队列都要先确认前置条件已满足**，否则失败会变成"卡住"。

71. **`hbot_core::JobId` 与 `hbot_store::JobId` 是两个类型**（2026-11-19）。它们底层都是 `uuid::Uuid` 的
    newtype、值也相同，但 `hbot-code` 的 `dispatch_job_collecting` 要 `hbot_core::JobId`，而
    `JobRow.id` 是 `hbot_store::JobId`。转换走 `hbot_core::JobId::from_uuid(*row.id.as_uuid())`，
    **不要** `to_string().parse()` 绕一圈 —— 主键是库里读出来的，多一次字符串往返就多一处可能悄悄变形的地方。

72. **作业的预算按行冻结、重试次数按清单现读**（2026-11-19，`C4.jobs` 的设计决定）。两者方向相反是刻意的：
    预算写进 `bot_jobs.budget_ms`（入队那一刻的类型快照），改清单不该让**已经排队**的那条作业换预算；
    `retries` 每次失败时重读清单，因为它是"这个类型失败几次后放弃"的策略，不是这一次执行的快照。
    反之（预算现读、重试冻结）= 一次改清单把所有在途作业的退避节奏全打乱，排障时极难解释。

73. **`bot_stop` 取消作业只动 `pending` / `running`，终态一律不碰**（2026-11-19）。`cancel_jobs_for_bot`
    的 `WHERE state IN ('pending','running')` 是硬约束：把 `done` 改写成 `cancelled` 会抹掉一条真实的成功
    记录（面板上就看不到"这个作业跑过了"）。恢复路径同理：`resume_interrupted_jobs` 只退回 `running`，
    `cancelled` 是终态，重启不会把它捡回来跑 —— 那是"用户按了停止"，不是"进程崩在中间"。

74. **`await ctx.session.prompt(...)` 不能真的「等」—— 那会把同会话队列死锁**（2026-11-19，`C2.session` 落地）。
    同会话队列严格 FIFO、一次只跑一个 handler（§4.2），而等待要等的**正是这个会话的下一条消息**：
    那条消息排在队尾，正等着 handler 把队列还给它。按字面实现会死锁，而症状是「bot 突然不回这个会话的
    任何消息」，日志里一句错都没有。正确形状是**注册**：`prompt` 写一行等待 + 发提示 + 返回，
    下一条匹配的消息由宿主在 `run()` 里先 `claim_wait` 拦下来交给 `onResume`。
    任何「在 handler 里等某个外部事件」的新能力（未来的 `webhook` 应答）都要先过这一关。

75. **等待的唯一性必须交给数据库的部分唯一索引，不是「先查后插」**（2026-11-19）。
    `0007` 的 `CREATE UNIQUE INDEX bot_waits_conv_pending ON bot_waits (bot_id, conversation_id) WHERE state = 'pending'`
    是「同会话只允许一个未结等待」的唯一落地判据。先查后插在并发下两次都会读到「没有」，然后都去插。
    部分索引（只覆盖 `pending`）而非普通唯一索引：同一会话**历史上**的等待（超时的、被回答的）必须能共存。
    冲突要翻译成可区分的业务错误（`StoreError::WaitPending { key }` → `HBOT_SESSION_PENDING`），
    否则用户只看到一条裸的约束失败，不知道该先回哪个。

76. **`claim`（唤醒）与 `expire`（超时）各必须是一条 SQL**（2026-11-19）。拆成「先查后改」时：
    前者会让重投的消息**唤醒两次**（用户发一句、bot 回两遍，第二次时等待已经 `fulfilled`）；
    后者会让两轮 tick 各自结算同一批行。`claim` 的 SQL 里有四个条件缺一不可：会话匹配、仍 `pending`、
    `deadline > now`（过了点的等待不能被一句迟到的消息救活）、`scope = 'conversation' OR owner = ?sender`
    （与 `WaitScope::accepts` 同义 —— 判据写进 SQL 才能与「改 fulfilled」同一条语句完成）。

77. **恢复钩子的 ctx 是「第三个上下文」：比消息窄、比作业宽**（2026-11-19）。`onResume` 拿得到
    `waitKey` / `waitPrompt` / `message` 与那条消息的平铺字段（`ctx.text` 等），但**不挂**
    `kv` / `history` / `device` / `jobs` / `session`。理由：恢复上下文里的「当前会话」是**等待行**声明的那个，
    而 `scope: conversation` 时回答者可能来自别的会话 —— 借这条消息的身份去读状态就会读错人。
    `build_resume_event` 里四个身份槽（device / state / ai / jobs）每次都显式清空。

78. **`IdGenerator` 加一个方法会让所有测试替身一起红**（2026-11-19）。`wait_id` 加进 trait 后，
    `hbot-rpc/src/app/ids.rs`、`hbot-bot/tests/common/mod.rs`、`hbot-redteam/tests/e_resource_exhaustion.rs`
    三处 `IdGenerator` 实现全部 `E0046`。这是**好事**（类型安全逼着每个替身表态），但要在加方法那一批里
    一起改完，别等末轮 `cargo test --workspace` 才发现。判读方法：`cargo check --workspace --all-targets` 一次看全。

79. **主动发消息必须先保证 `conversations` 行存在，否则失败发生在「已经报告成功」之后**（2026-11-19，`C3.send` 落地）。
    `outbound.conversation_id` 有指向 `conversations` 的外键（`0001_init.sql`），而 `Store::open` 对每条连接 `PRAGMA foreign_keys = ON`
    （`crates/hbot-store/src/db.rs`）。主动消息的常态正是「平台从没推过这个会话」—— 没有行，于是出站那一刻撞
    `FOREIGN KEY constraint failed`，而这时 `ctx.send` 早已返回、用户只看到「发了但对方没收到」。规则：`ctx.send` 在交给
    `replies` 通道前先补建会话行（`Store::insert_conversation_if_absent`），会话 ID 用与 supervisor 同一个确定性派生
    `hbot_bot::conversation_id_for`，不要自己拼。

80. **幂等补建用 `INSERT OR IGNORE`，不要复制「嗅探错误文本」那条路**（2026-11-19）。`hbot-bot` 的 `is_conversation_conflict`
    靠 `message.contains("UNIQUE constraint failed") && contains("conversations")` 判重，且它是 `pub(crate)` —— 在 `hbot-rpc` 里
    既用不了、也不该出现第二份同样脆弱的判据（错误文案一改就静默失效）。判据应当交给唯一约束本身：`INSERT OR IGNORE` +
    返回值 `inserted > 0`。同理，任何「先查后插」在并发下都会两次读到「没有」，别拿它当幂等。

81. **`ctx.notify` 的收件人不许由调用点决定**（2026-11-19）。目标来自清单 `capabilities.notify: { conversation, kind }`，
    并且 `kind` 必填（`unknown` 在**加载清单时**就失败，不等第一次调用）。一旦目标能在运行时指定，一段被注入的 handler
    代码就能把 bot 读到的数据发到任意群 —— 那是外发泄漏，不是通知。与之相对，`ctx.send` 的目标本来就该来自调用点
    （它就是「主动发给谁」），所以两个能力的判据必须分开写，别合并成一个 `send`。

82. **`ctx.send` 复用 `replies` 通道，`reply_to` 恒为 `None`**（2026-11-19）。`SandboxHandler.replies: mpsc::Sender<OutboundRequest>`
    是唯一出站路径；自己写 `outbound` 行会立刻长出第二份「目标怎么构造」的逻辑（`target_json` 放什么、`conversation_id` 怎么派生），
    两份总有对不上的一天。而 `reply_to: None` 是硬约束：主动消息不绑定任何入站事件，不得冒用旧消息 ID（§4.1）。
    被动回复仍然只能走 handler 的返回值。另：`StoreSendQueue::send` 在 `spawn_blocking` 里跑，所以用 `blocking_send` 是对的；
    在 async 上下文里直接调它会把 reactor 卡住。

83. **webhook 不进 `inbound` 表，`onWebhook` 的返回值就是 HTTP 应答**（2026-11-19，`C1.webhook` 落地）。
    外部系统没有平台身份（无 sender / conversation / message_id），塞进 `inbound` 会让
    `UNIQUE (bot_id, external_id)` 去重键与回复目标同时失去定义。形状是**请求即事件**：handler
    返回 `null` → 204、返回字符串 → `text/plain`、返回对象 → JSON、返回 `{status, body, contentType}`
    → 那是应答信封。**不要**做成「注册 + 后续投递」——调用方在等 HTTP 响应，晾着它没人能保证它等到
    （与坑 74 同一条理由）。

84. **webhook 鉴权的三种失败统一回 401，不分别给码**（2026-11-19）。路由没声明、密钥环境变量没设、
    密钥不匹配 —— 三者的补救动作对调用方是同一个（去问部署方要正确的密钥），而把它们区分开等于
    送探测者一个「这个路由存在」的探针。另外 **密钥只写环境变量名（`secretEnv`）不写进清单**：清单是
    项目文件，会入库、会被分享；`environ` 没设时**一律拒绝**，绝不退化成「没配就放行」。服务只绑
    `127.0.0.1` 也不构成鉴权 —— 本机任意进程都能发 POST。

85. **`Ctx::json_parse` 吃的是 `Value`，不是 `String`**（2026-11-19，写 `build_webhook_event` 时踩到）。
    把一段 `String` 直接喂进去会同时报两条看着无关的错：`E0308 expected Value, found String` 与
    `E0277 Vec<u8>: From<Value>`（后者是类型推断顺着 `IntoJs` 走岔了）。想在函数里把一段文本变 JS 值时，
    先在 Rust 侧用 `serde_json::from_str` 判一次，成功了再 `ctx.json_parse(value.to_string())`。
    顺带：webhook 的 `bodyJson` 用这条路 —— 非 JSON 的请求体是**正常形状**（表单提交），给 `undefined`
    而不是抛异常。

86. **`WebhookServer` 把 `TcpListener` move 进接受循环后，`addr()` 就拿不到了**（2026-11-19）。
    `TcpListener` 不是 `Copy`，`tokio::spawn(async move { ... listener.accept() ... })` 之后原变量已被移动，
    结构体再存 `listener` 字段会报 `E0382 use of moved value`。正确形状：**绑定时就把 `local_addr()` 存下来**，
    结构体只留 `address: SocketAddr`，接受循环拿走 listener。测试要靠 `addr()` 拿系统挑的端口，所以这一步
    不是可选的。

87. **生命周期钩子的失败语义与别的钩子相反**（2026-11-19，`C1.lifecycle` 落地）。`onStart` 抛错
    **就是**启动失败（`bot_start` 回滚到 stopped）—— 不报错会让「起来了但没初始化完」变成一种状态，
    后续每条消息都在半初始化里跑。`onStop` 抛错**只落一行 error 日志**，`bot_stop` 仍返回成功：
    停机路径上没有任何人在等它的结果，给它一个错误只会诱使调用方判断「停得好不好」。
    两者的预算也不同：`onStart` 用 `limits.handlerMs`，`onStop` 用 `hbot_core::STOP_BUDGET`（5 秒，
    因为「不能拖住停机」比「不能拖住会话」更硬）。

88. **编译失败时生命周期钩子要跳过，不能让 `bot_start` 因此失败**（2026-11-19，被 `bot_runtime`
    的 `compile_error_is_reported_and_inbound_stays_unprocessed` 抓出来的）。`run_lifecycle` 第一句
    是 `compiled()`，如果把它 `?` 出去，一个 typo 就让 `bot_start` 永久失败 —— 而用户修好代码后
    没有任何路径能让它恢复。正确形状：`compiled()` 失败只 `warn` 一行并返回 `Ok`，让 bot 起着、
    消息留在未处理、改好代码自动重试（与坑 24 同一条纪律）。**通用判据**：任何接在 `bot_start`
    路径上的新步骤，都要先想「用户代码坏了会不会把它永久挡住」。

89. **生成代码可写的持久状态只有三个口子，`sql` 的隔离靠「一个 bot 一个库文件」而不是 `WHERE bot_id = ?`**（2026-11-20，`C2.sql` 落地时定）。表名前缀或 `WHERE bot_id = ?` 都要求「每一处都记得加」，漏一处就是**静默**跨 bot 越权：A 的 `DELETE FROM users` 抹掉 B 的数据，日志里只看到一句正常 SQL。现在库文件是 `<data_dir>/bot_sql/<bot_id>.db`，别的 bot 的表根本不在这个连接的命名空间里，越权在**物理上**不可能。代价是每 bot 一个文件，相对越权可以忽略。

90. **运行时禁 DDL，表结构只能来自项目里的 `migrations/*.sql`**（2026-11-20）。`ctx.sql.exec` 只接受五种动词（`SELECT`/`INSERT`/`UPDATE`/`DELETE`/`WITH`），`CREATE`/`ALTER`/`DROP`/`ATTACH`/`PRAGMA`/`VACUUM`/`REINDEX` 一律 `HBOT_SQL_DENIED`。迁移在 `bot_start` 里、`onStart` **之前**由宿主执行一次，按文件名去重并记在 `schema_meta(name, applied_at)`。**迁移失败不阻断启动**（与坑 24/88 同一纪律）：写错的 `CREATE TABLE` 不该让 bot 永远起不来，落一行 error 让它起着、改好再重启。

91. **`query` 只能读、`exec` 才能写，判据是动词不是「有没有副作用」**（2026-11-20）。`SqlVerb::is_read()` 只认 `Select`：`WITH … DELETE` 的动词是 `With`，所以它在 `query` 里被拒、必须走 `exec`。别为了「WITH 看起来像查询」把它放行 —— 那会让读接口变成写接口，而读接口的调用点通常没有写的心智准备。

92. **JS 侧不区分整数与浮点，绑定到 SQLite 时必须区分**（2026-11-20）。JS 里 `7` 也是 double，直接绑定会让 `INTEGER PRIMARY KEY` 的比较走类型亲和性，出现「明明插进去了却查不到」。做法：整数值且 `|v| < 2^53` 绑定成 `Integer`，其余成 `Real`；参数只收五种标量（null / 布尔 / 数字 / 字符串 / Blob），**拒绝对象与数组**而不是 `JSON.stringify` —— 静默转换会把「传错类型」变成「看起来成功的写入」，错误发生在很久以后、离调用点很远。

93. **写语句必须「先 rollback 再报错」**（2026-11-20）。`BotSqlSchemas::exec` 在事务里跑，受影响行数超 `MAX_SQL_AFFECTED`（10000）时先 `rollback()` 再返回 `Denied`。顺序反了就是「报了错但数据已经落了」—— 已经写下去的行没法撤回一部分，判据只能放在提交之前。

94. **进程退出/切项目不许裸调 `bots().stop_all()`**（2026-11-20，接 `shutdown` 触发点时定）。
    `BotRegistry::stop_all()` 只停 supervisor —— 它**跳过 `onStop`**、也不注销 webhook、不取消未完成作业与等待。
    于是用户写好的「存档 / 关连接 / 写最后一行日志」在 Ctrl-C、桌面端退出、切项目这三条**最常见**的收尾路径上
    一条都不跑，而 `bot_stop` 上跑得好好的；现象是「手动停能保存，关窗就丢」，很难联想到是两条不同的路径。
    规则：收尾一律走 `AppState::shutdown()`（内部 `stop_all_with_reason(Shutdown)`，逐个 `stop_with_reason`）。
    同族纪律：`crash` 这个原因**暂时没人发**是刻意的 —— 现在唯一的「崩」是 isolate 崩溃，而它有 last-good
    版本兜底（坑 24），bot 仍然活着；在那一刻调 `onStop(crash)` 会让清理逻辑跑在一个还活着的 bot 上，
    之后每条消息都在被清空的环境里执行。**宁可这个原因暂时没人发，也不要发错**。

95. **抓窗口像素与枚举窗口是两项能力，别合成一个布尔**（2026-11-20，按窗口截图落地时定）。
    `DeviceCapabilityReport` 现在是 `window`（聚焦/最小化/关闭/启动）与 `window_capture`（`PrintWindow` 抓像素）两个字段。
    合成一个的后果：macOS 上"能枚举窗口"为真、"能抓像素"要屏幕录制授权，UI 会显示可用然后每次都失败。
    同批的三条形状：① `WindowCaptureRequest` 是独立类型，不给 `ScreenshotRequest` 加 `window` 字段（两个目标塞一个结构体会长出"都给了算什么"的歧义，而授权层不能有歧义）；
    ② `DeviceBackend::capture_window` 带**默认实现**返回 `HBOT_DEVICE_HOST_LIMIT`（否则每个夹具都要写 `unimplemented!`，而"这台机器做不到"本来就是 `HOST_LIMIT` 的语义）；
    ③ 隐私面按 `DeviceCapability::Screen` 授权而不是 `Window`（它读的是像素）。
    实现细节里两个判据：`PrintWindow` 的返回值**不作数**，要看位图是否全零（全零再退窗口 DC 的 `BitBlt`，仍空报 `FAILED` 而不是给全黑图）；
    **已最小化的窗口必须直接拒**（`HBOT_DEVICE_HOST_LIMIT`）—— 最小化窗口的矩形是 `-32000` 那一带的占位值，抓出来的东西没有意义。
    JS 侧 `screen({ window })` 与 `monitor` 互斥（两个都给当场拒），返回值里**没给的那个字段保持 `undefined`**，不要用 `0` 冒充"主屏"（`0` 是合法主屏序号，冒充就再也分不出"主屏"与"没给"）。

96. **`ctx.files` 的路径判据只在槽位层，后端不要再判一遍**（2026-11-20，`C3.file` 写测试时踩到）。
    `hbot-core` 的 `normalize_file_path` 已经拒了绝对路径、盘符、ADS（`:`）、前导 `/`、`\`、`.` 与 `..`，
    所以 `ProjectFiles`（`crates/hbot-rpc/src/app/runtime/files.rs`）收到的**永远是归一后的相对路径**；
    它只负责后半件事：逐段 `canonicalize` 并要求落在项目根里（字面归一挡不住指向根外的符号链接）。
    我一开始写了 `files.write("/etc/x", …)` 期望 `DENIED`，实际它被当成 `etc/x` 落在根里 —— 这不是漏洞
    （槽位层根本不会把这个路径传下来），而是**判据放错了层**。规则：一个判据只在一层落地，另一层只做补足。

97. **文件能力读写分开声明，别合成一个布尔**（2026-11-20）。判据是 `capabilities.state.files.{read,write}`，
    `FilesCapability::is_declared()` 只是"两半里有没有真的"，闸上必须按方向取（`allows_read` / `allows_write`），
    且 `HBOT_FILE_NOT_DECLARED` 的 `capability` 字段要说清是哪一半 —— 只说"没声明"用户会去加错那一半。
    一个只会读报表的 bot 不该能删存档；合成 `files: true` 会让"我只给了读权限"变成无法兑现的话。

98. **文件的三个边界语义别反着写**（2026-11-20）：① 读超过 `maxBytes` **截断并标 `truncated`**（读是"我想看一部分"，
    让调用方自己决定要不要分段）；② 写超过 1 MiB **报错**（写是"保存这一份"，静默截断等于存了半份而看着成功）；
    ③ 非 UTF-8 读**报错**而不是替换成 U+FFFD（悄悄替换等于给用户一段被改过的数据）；④ `remove` **只删空目录**，
    不做递归（递归删目录在生成代码手里就是一次静默的批量数据丢失）。

99. **e2e 里按"旧长度"当索引取回复会拿到上一条**（2026-11-20，`files_capability` 两条用例红在这）。
    `Store::list_logs` 是**新的在前**，所以 `let before = replies().len(); inject(); replies().nth(before)`
    在第二次调用时取到的正是上一次那条。做法：发送前存快照 `Vec<String>`，等到"出现不在快照里的回复"，
    再取第一条不在快照里的（`find(|reply| !before.contains(reply))`）。同族：任何"等新行"的断言都要比
    **内容差集**，不要比长度再按下标取。

100. **`onSchedule` 与 `onTimer` 是同一个钩子的两个名字，不是两条派发路径**（2026-11-20，P2 定案）。
     需求里这个钩子叫 `onSchedule`（`docs/01` FR-15），首个发布实现成了 `onTimer`，于是 `docs/07` §4.1 长期挂着一句「`onSchedule` 未做、且与 `onTimer` 不是同一件事」。
     正确形状（`crates/hbot-code/src/runtime/isolate.rs` 的 `bootstrap`）：**两个名字在取钩子时合并成同一个 `Hooks.timer`**；两个都写 → 启动失败并同时点名两者（静默挑一个会让「我删了 `onTimer` 怎么行为变了」变成只能读源码才能解释的排查）；两个都没写 → 到点报错文案点名 `onSchedule` 并说明 `onTimer` 是旧名。
     **不新增清单字段**（继续用 `capabilities.timer`），也不把 `onTimer` 标成 deprecated —— 删掉它会让已有项目升级那一刻**静默失去定时行为**。
     同族判据：给钩子改名/加别名时先问「宿主会不会因此多一条执行路径」。会，就说明这个改法错了。

101. **迁移号加一，要把「schema 版本断言」全找一遍，不止一处**（2026-11-20，加 `0008` 时踩到）。
     `crates/hbot-store/src/tests.rs` 的 `upgrading_from_0004_creates_bot_kv_and_keeps_existing_rows` 里有**两组独立断言**：
     一组是 `user_version` / `recorded.len()` / `recorded[n]`，另一组是紧接着的 `store.schema_version() == 7`（文案里也带着旧迁移名）。
     只改前者时单 crate 的 `-p hbot-store` 可能因为过滤条件恰好不跑它，直到末轮 `cargo test --workspace` 才红。
     规则：加迁移时搜 `schema_version()`、`user_version`、`recorded.len()`、旧迁移文件名四种字面量，一次改全。

102. **往已有 `mod tests { … }` 的 `}` 之后追加测试会多出一个大括号**（2026-11-20）。
     症状是 `error: unexpected closing delimiter: '}'`，而位置指向文件的模组收尾处。
     原因：拿「文件最后几行」当锚点时，那段锚点本身就含 `mod tests` 的收尾 `}`，插入后再补一个就成了两个。
     做法：锚点取某个 `fn` 的收尾（或插在文件末尾之前），别拿最后一行当锚点；插完 `git diff` 看一眼括号层级。

103. **`assert!` 里把`,`写成`;`会报成两条看起来无关的错**（2026-11-20）。
     实际写成了 `assert!(x.is_empty(); )` 附近漏个括号 / 用了 `.expect("…");`，编译器报
     `E0600 cannot apply unary operator '!' to type Vec<WaitRow>` 加一条 `unexpected string literal` 在不同行。
     两条错误看着像两处问题，其实是同一处的语法错位 —— 先看报错行附近的标点，别急着改类型。

104. **示例项目必须入库，用户夹具必须忽略 —— 两者不能共用目录**（2026-11-20）。
     `projects/` 被 `.gitignore` 整目录忽略（`bots.json` 里有真实平台 `token` / `clientSecret`），
     于是「三类示例项目各一个」这条出口标准在仓库里**一个可跑的项目都没有**，新克隆的人无从下手。
     做法：示例搬进 `examples/`（只写 `"platform": { "kind": "local" }`，不带 `bots.json`），
     并让 `crates/hbot-cli/tests/examples.rs` 断死「名字恰好是这三类 + 每个都过 `check_project` + 目录里没有 `bots.json`」。
     判据：**示例是代码，夹具是数据**；一份文件同时当两者，必然是凭据进库或标准无从验证。

105. **生成物有几份就要刷新几份：`cargo xtask gen` 现在也管 `examples/*/hbot.d.ts`**（2026-11-20）。
     `hbot.d.ts` 是**每个项目一份**的副本（Monaco 与 `hbot check` 都读它），加示例就等于多三份随宿主 API 漂移的文件。
     `gen --check` 只比 `apps/web/src/generated/` 时，漂移的后果是「用户打开示例，Monaco 报得莫名其妙」而不是 CI 红。
     做法：`xtask` 依赖 `hbot-code`（读同一份 `dts.rs`），`gen` 写、`gen --check` 比对，一次覆盖全部含 `hbot.json` 的示例目录。
     同族：新增任何「每个项目一份」的生成物，都要问一句「闸门扫得到它吗」。

106. **按字节截断文件会切断 UTF-8 字符，症状是「合法文件报不是 UTF-8」**（2026-11-20）。
     `ctx.files.read` 的整文件路径原本是 `&bytes[..max_bytes]` 再 `String::from_utf8`：上限正好落在某个三字节汉字中间时，
     `hbot-rpc` 的 `ProjectFiles::read` 会回 `HBOT_FILE_INVALID`（「不是 UTF-8 文本」），而文件本身一个坏字节都没有 —— 排障会一路查到编码上。
    做法：截断点退到最近的字符边界（`while end > 0 && (bytes[end] & 0xC0) == 0x80 { end -= 1 }`）；按行读那条新路径按整行收尾，
    只有「单独一行就超上限」时才退字符边界。任何「按字节上限收尾」的地方都要问一句：这一刀会不会切在字符或转义序列中间。

107. **递归删除要逐层 `symlink_metadata` 并拒绝符号链接，而且要「先扫后删」**（2026-11-20）。
     `ctx.files.remove(path, { recursive: true })` 用 `fs::metadata`（跟随链接）判类型，就可能在项目里删掉一个指向 `~/.ssh` 的目录；
     `fs::remove_dir_all` 遇到符号链接的行为还因平台而异。做法：`count_tree` 先走一遍（每个条目 `symlink_metadata`，`is_symlink()` 一律
    `HBOT_FILE_DENIED` 且点名「不跟进链接」，顺便数条目与 `MAX_FILE_REMOVE_ENTRIES` 对账），确认整棵树可删之后才 `delete_tree` 自底向上删。
    先扫后删的第二个好处：超限与链接在**动手之前**就报错，不会留下删了一半的目录。

108. **按行写必须保留未触碰行的原始行尾**（2026-11-20）。把所有行读出来用 `\n` 拼回去，会让一个 CRLF 项目整文件 diff ——
    用户看到「我只改了一行，却有一千行变更」。做法：未触碰行按**字节**原样复制（`read_until(b'\n')` 拿到的整行，含 `\r`）；
    被替换段沿用**首行**的终止符（`line_terminator` 返回 `\r\n` / `\n` / `""`），末行本来没有换行就不补。

109. **`ctx.sleep` 会把请求夹到剩余预算然后正常返回 —— 所以「预算到点掐断」的测试必须忙等**（2026-11-20）。
    写一条 webhook 超时 e2e 时用了 `await ctx.sleep(1500)`，路由预算 200 ms，结果回的是 `200 {ok:true}`：
    `crates/hbot-code/src/runtime/host_api.rs` 的 sleep 实现是 `requested.min(deadline.remaining())`，到点就醒来、继续跑完 handler。
    做法：要证明「预算掐断」，handler 里写 `const started = Date.now(); while (Date.now() - started < 1500) {}`（先例
    `crates/hbot-code/tests/s2_ai.rs:324`）。同族：任何「用一个会自适应的原语去测自适应本身」的断言都是假绿。

110. **`Store::list_logs` 返回的是「新的在前」—— 别用「旧长度当下标」取新回复**（2026-11-20）。
     `examples_e2e` 的 `ask()` 最初写 `replies[before_len..]`，第二轮取到的是上一轮的回复（`/count` 两次都读成 1，而 kv 行确实是 1→2）。
    做法：按**文本出现次数**比较（`first_new_reply(previous, current)`：同一句出现得更多才算新的一条），或直接按 `createdAt` 过滤。
    判据：日志/事件的排序方向不属于「取最新一条」的判据，把它写进断言就会在方向改变时静默错位。

111. **按行写要有「替换段已经写完」这个显式状态**（2026-11-20）。
    `copy_lines` 最初在 `line_no == range.end` 分支里写替换文本，但循环继续 —— 后面每一行都满足 `line_no >= range.end`，于是**一直覆写**：
    把 `l1\nl2\nl3\n` 的第 2 行替换成 `X2`，得到 `l1\nX2\nX2\n`。修法是写完立刻 `wrote = true`，之后每行走「原样复制 + continue」。
    同族：任何「按条件区间改写」的流式循环，都要把「区间已结束」当成一个状态，而不是靠条件表达式在每轮重新判断。

112. **`Async` 闭包里不能捕获任何 JS 对象（`Function` / `Object` / `Value`），只能捕获 Rust 数据**（2026-11-20，坑 53 的同族）。
    想「注册等待后顺手调一下 `ctx.reply`」，于是把 `Function<'js>` clone 进 `Async` 闭包 —— 编译过、单测过，整个测试进程在退出时崩
    `Assertion failed: list_empty(&rt->gc_obj_list), file …quickjs.c, line 2704`（退出码 `0xc0000409`），加 `--test-threads=1` 也一样。
    做法：把「发一条回复」下沉成 Rust 侧操作 —— `Arc<Mutex<HostOutput>>` 的 `push_reply`，与 handler 自己调 `ctx.reply` 走同一条缓冲。
    判据：`Async` 闭包的捕获列表里出现任何非 Rust 类型的，先停手。

113. **`ctx.session.prompt` 必须在「注册成功」那一刻就把提示发出去**（2026-11-20）。
    它第一版只写等待行、发提示这件事留给 tick 的重发路径 —— 于是没有声明 `reminders` 的等待**一句提示都不发**：
    handler 跑完、行是 `pending`、日志干净，用户看到的是「bot 不回我」，而 bot 其实正在等一个从没人问过他的问题。
    既有测试没抓到，是因为它们的 handler 在 `prompt` 之后自己 `reply('asked')` 了。判据：**每个「注册」型 API 都要问一句「用户在这一刻看到了什么」**；
    并且补一条测试断言「提示先于 handler 自己的回复」（`crates/hbot-rpc/tests/session_capability.rs` 的 `the_prompt_reaches_the_user_the_moment_the_wait_is_registered`）。

114. **恢复 ctx 少了 `sql`，「等待」就只剩一句口头回复**（2026-11-20）。
    `onResume` 的 ctx 刻意比消息窄，但漏掉了 `sql` / `files` —— 而等待被唤醒之后，用户那句话几乎一定要落库（工单、订单号、地址）。
    `examples/support` 的 `onResume` 调 `ctx.sql.exec` 直接抛 `cannot read property 'exec' of undefined`，只有真跑示例才会发现。
    判据：**narrow ctx 只该砍掉「靠事件身份才能安全工作」的面**（`kv` / `history` / `device` / `jobs` / `session`）；
    按 bot 绑定的面（`sql` / `files`）没有这条顾虑，砍掉就是无谓的残缺。
115. **本地闸门全绿 ≠ CI 绿：`channel = "stable"` 意味着本地与 CI 各拿各的版本**（2026-11-20）。
    首次 push 后 CI 的 `rust (ubuntu-latest)` / `rust (windows-latest)` 都停在 `clippy` 一步红：
    `error: the Err-variant returned from this function is very large --> crates/hbot-agent/src/gateway.rs:357:10`
    （`-D clippy::result_large_err`，`GatewayError` 168 字节）。而本地 `rustc 1.96.0 / clippy 0.1.96` 跑同一条命令 **EXIT=0**：
    CI 那份 stable 是 **1.100.0**，比本地新 4 个小版本，这条 lint 只在它那里报。`rust-toolchain.toml` 原本写的是
    `channel = "stable"` —— 那不是「钉版本」，是「各机器拿各自的最新 stable」，所以本地闸门在结构上就不可能等于 CI。
    **根因修法**：`rust-toolchain.toml` 写死 `channel = "1.100.0"`，`.github/workflows/ci.yml` 的 4 处
    `dtolnay/rust-toolchain@stable` 全改成 `@1.100.0`（这个 action 的 `@rev` 就是版本号）。改完在仓库根跑
    `cargo --version` / `cargo clippy --version`，那里显示的才是本地闸门真正用的版本。
    **红点本身的修法**：① 大结构别塞进 `Result::Err`，照 `crates/hbot-code/src/dispatch.rs:34` 的 `pub error: Box<CodeError>` 先例装箱；
    ② 加一条**与 lint 同口径**的守卫单测（`gateway::tests::gateway_error_stays_within_the_large_err_budget`，
    `size_of::<GatewayError>() < 128` —— clippy 的判据是「至少 128 字节」就红，所以守卫必须用严格小于），
    这样本地就能提前红，不用等 push；
    ③ 判读 CI 红点时先看它停在**哪一步**：`fmt`/`clippy`/`test` 分步跑，停在 `clippy` 且本地绿 = 版本差，不是代码坏了。

116. **夹具里写死的像素尺寸会在别人的字体度量下失效 —— 尺寸要量出来，不要猜**（2026-11-20）。
    `crates/hbot-device/tests/windows_desktop.rs` 的按窗口 OCR 夹具把客户区写死 `1100 × 320`、字号 96px 粗体 Segoe UI。
    CI 的 `windows-latest` 字体度量比本机略宽，于是 `TextOutW(hdc, 40, 100, …)` 画 `"HBOT WIN OCR 7788"` 时末尾 `88` 落到客户区外被裁掉：
    OCR 只认出 `"HBOT WIN OCR 77"`，本机跑同一条测试全绿。判据：**凡是「画进去再读出来」的夹具，窗口/画布尺寸必须由实测值决定**
    （`GetTextExtentPoint32W` 之类；夹具里 `window_font()` 让画字与量字共用同一份 `HFONT`，`text_extent()` 给出宽高，
    `TestWindow::create` 按 `TEXT_LEFT + cx + TEXT_MARGIN` 开窗）。`TEXT_MARGIN` 不能为 0 —— OCR 对贴边字形不可靠。
    同族：这条测试还会偶发拿到空串（窗口刚 `ShowWindow`、DWM 未合成第一帧时 `PrintWindow` 返回成功但位图全零，见 `TestWindow::settle`），
    所以它带 5 次重试；**重试不放松断言**，仍要求认出完整的 `WINDOW_TEXT`。

117. **「同一时间戳内的两次写」不是理论问题 —— 只比 mtime 会静默漏改**（2026-11-20）。
    `crates/hbot-rpc/src/app/device/mod.rs` 的设置 watcher 原本拿 `device.json` 的 mtime 当变更判据。
    CI 的 `windows-latest` 上 `a_corrupt_external_edit_falls_back_to_deny_all_and_is_processed_once` 红过：
    夹具 `save` 与测试里 `fs::write` 落在**同一个 mtime 刻度**，`*last == mtime` 直接 `return None`，坏文件没被处理。
    实测（NTFS，同一文件连写 2000 次）：**41 次 mtime 完全相等**，最小差 5022 ticks = 502.2 µs；单次量到差 3.7 ms。
    所以「mtime 分辨率够细」和「两次写不会撞」是两件事 —— 负载高时撞的概率明显上升。
    判据：**变更判据要么比文件字节，要么比「mtime + 长度 + 内容地板」**。小配置文件（`device.json` 不到 1 KB、
    每秒最多读一次）直接读全文比字节最省心：`settings_bytes: Mutex<Option<Vec<u8>>>` + `settings_bytes_of()` 用 `fs::read`，
    仍然「先记账再读」所以坏文件只处理一次。大目录树别这么干 —— 那里用 `crates/hbot-rpc/src/app/runtime/fingerprint_cache.rs`
    的 `MetaSignature = Vec<(String, u64, u64)>`（路径/mtime 纳秒/长度）+ `DEFAULT_CONTENT_FLOOR = 5s` 兜同一类盲区。
    回归测试的写法：**不要赌「mtime 一定相等」**（不可控），构造两份**等长不同内容**的合法 JSON（等长替换一个应用名即可），
    断言第二份必须被处理 —— 它钉的是「判据不只看 mtime」，在任何 mtime 结果下都成立
    （`a_same_length_rewrite_is_never_swallowed`）。

118. **CI runner 磁盘耗尽会伪装成「链接器崩了」**（2026-11-20）。
    `.github/workflows/ci.yml` 的 `rust` job 跑 `cargo test --workspace --all-targets` 要链 ~120 个测试二进制，
    `Cargo.toml` 的 `[profile.dev] debug = 1` 让每个都带调试信息，于是 ubuntu runner 的空间被吃光。症状：
    `##[warning]You are running out of disk space ... Free space left: 19 MB` →
    `collect2: fatal error: ld terminated with signal 7 [Bus error], core dumped` →
    `error: could not compile `hbot-code` (test "s1_debug_instrument")`。看起来像编译/链接错误，**实际是磁盘**。
    修法两条一起上：① job 级 `env: CARGO_PROFILE_DEV_DEBUG: '0'` + `CARGO_PROFILE_TEST_DEBUG: '0'`
    （只影响 CI，`Cargo.toml` 不动、本地开发体验不变）；② 编译前删掉 runner 预装但本项目不用的大目录
    （`/usr/share/dotnet`、`/usr/share/swift`、`/opt/ghc`、`/usr/local/lib/android`、`/opt/hostedtoolcache/CodeQL`、
    `/usr/local/share/boost`，合计约 20 GB）。判读口诀：**看到 `signal 7 [Bus error]` / `IO failure` / `ENOSPC`，
    先看磁盘再看代码**（本地同族见坑 45）。

119. **`rlim_cur == rlim_max` 时 SIGKILL 抢先，`SIGXCPU` 永远不会出现**（2026-11-20，CI 实测）。
    `crates/hbot-command/src/containment_linux.rs` 的 `apply_limit` 原来把软硬上限设成同一个值
    （`RLimit { rlim_cur: value, rlim_max: value }`）。软上限到点该发 `SIGXCPU`、硬上限到点发 `SIGKILL`，
    两者相等时内核在同一刻两个都到，**实测结果直接是 `signal 9`**：CI 的
    `crates/hbot-command/tests/limits.rs` 里 `cpu_limit_terminates_a_busy_loop`（`cpu_ms: 200`）拿到
    `status=Exited { code: None, signal: Some(9) }`，而 `cpu_kill_hint()` 只认 `SIGXCPU`，于是
    `violation()` 恒 `None` → 「被限额杀掉」被当成**正常退出**返回（不是崩溃，是最坏的一种：静默）。
    判据：**软硬上限必须分开传**，硬上限留出宽限（`crates/hbot-command/src/limits.rs` 的
    `CPU_HARD_GRACE_SECONDS = 1`；`apply_limit(resource, soft, hard)`），并加一条
    「hard 恒 > soft」的纯逻辑单测（`cpu_soft_and_hard_limits_are_never_the_same_value`）。
    `RLIMIT_AS` 不需要宽限 —— 地址空间到顶是**分配失败**，没有信号层。
    同族的判读纪律：**本地 Windows 全绿 ≠ Linux 绿**。编译期问题能靠
    `cargo check -p hbot-command --target x86_64-unknown-linux-gnu --all-targets` 抓到（本机 9.62 s 能过，
    不需要 Linux C 工具链 —— 只有 `ring`/`psm` 这类需要 cc 的 crate 才卡住），**运行期语义只能靠 CI**。

120. **别把某一平台的实现细节写成跨平台测试的断言**（2026-11-20，CI 的 ubuntu 红）。
    `crates/hbot-device/tests/api.rs` 的 `the_backend_is_usable_as_a_trait_object` 断言
    「任何平台都该报告 `window` / `clipboard` 能力」，本机 Windows 全绿；
    CI 的 ubuntu 上 `platform_backend()` 是 `UnsupportedBackend`（`crates/hbot-device/src/unsupported.rs`：
    六个能力全 `false` + `detail` 写明「这个宿主没有设备能力实现」），断言当场红。
    判据：**测试的断言对象要和测试的名字一致**。这条测试钉的是「`DeviceBackend` 能对象化、能跨线程 `Arc` 共享」，
    能力内容必须按 `cfg!(windows)` 分支（Windows 钉能力位；其它宿主钉「全 false 且 `detail` 有值」——
    那才是「可诊断的拒绝」这条契约）。同理别断言 Windows 的 `detail` 为空：它在那里是**能力说明**
    （显示器清单、OCR 语言包、PrintWindow 限制），不是错误原因。

121. **`wait_until` 只等「执行完」不等于等「落库完」**（2026-11-20，CI 的 windows 红）。
    handler 链路是**先执行、后记账**：`crates/hbot-bot/src/supervisor.rs` 的 `RecordingHandler::on_message`
    先 `executor.on_message(ctx).await`（测试的 `TestExecutor` 在这里 push 进 `seen`），**再**
    `inbox.mark_processed(&message_id)` 写 `processed_at`。于是
    `crates/hbot-bot/tests/runtime.rs` 的 `unprocessed_inbound_is_replayed_on_start` 只等
    `seen().len() == 1`，下一行断言 `list_unprocessed_inbound(10).is_empty()` 就偶发为假（本机 15 次全绿、
    CI 上红）。**同一个文件里还有第二条同族**：`replay_continues_past_one_batch_without_losing_messages`
    只等 `seen().len() == total` 再断言未处理行为空，修完第一条的下一轮 CI（ubuntu）就红在它上面 ——
    说明「一处竞态 = 全仓扫同类」比逐条等 CI 报要省一轮。判据：**等待条件要覆盖后面所有断言的因果前置**，
    把「库里已空」并进 `wait_until`，而不是在等待之后断言一个还没发生的副作用；不放松语义（最终仍必须为空）。

122. **Windows 文件系统语义的用例，在 POSIX 上必须按平台门控**（2026-11-20，CI 的 ubuntu 红）。
    `crates/hbot-redteam/tests/a_path_escape.rs` 有四条用例无条件要求「必须拒绝」：`a04` 的
    `....\\secret.txt` / `..\..\secret.txt`、`a06` 的 `C:\Windows\win.ini` 文案含 `absolute`、
    `a10` 的 `a.` / `a `（尾随点/空格）、`a11` 的 `CON` / `NUL` / `COM1`（保留设备名）。
    实现是对的 —— `crates/hbot-workspace/src/project.rs` 的 `reject_windows_unsafe_name` **第一句就是**
    `if !cfg!(windows) { return Ok(()); }`（尾随点/空格、`<>:"|?*` 与控制字符、`WINDOWS_RESERVED_NAMES`
    全在这道门后面），因为那些名字**只在 Windows 上是逃逸面**：POSIX 上 `a.` 与 `a` 是两个文件、
    `CON` 只是三个字母、反斜杠不是分隔符。在 Linux 上拒绝它们反而是错的（合法的普通文件名）。
    另有一条更细的：`a06` 的 `C:\Windows\win.ini` 在 Linux 上命中的是 `has_drive_prefix`
    （`project.rs:523-526`，`bytes[1]==b':'`），文案是 `Windows drive paths are not allowed`，
    不是 `absolute paths are not allowed` —— 断言里必须两个词都接受。
    判据：① 表驱动用例里逐条问「这条输入在另一个平台上是不是同一个东西」，不是就分流；
    ② 分流后的弱断言**不是放水**：改成「拒绝 **或** 落点仍在根内」（`reject_or_contained_for_windows_names`），
    逃逸仍然必红，只是不再要求「合法名字也必须拒」；③ Windows 分支保持原样（必须拒绝 + 文案关键字），
    别为了两边都能过就把断言删成 `let _ =`。

123. **安全用例里的「解释器」不能写死某个平台的名字**（2026-11-20，CI 的 ubuntu 红）。
    `crates/hbot-redteam/tests/b_command_injection.rs` 的 `b03`（钉「命令层不判断名字危不危险」）与
    `b07` 第③段（钉「`run_command` 的 `cwd: ".."` 必须拒」）都把被测程序写成字面量 `"cmd"`。
    Windows 上它存在，用例绿；Linux 上没有 `cmd`，于是 `b03` 红在 `ProgramNotFound { program: "cmd" }`、
    `b07` 的③段还没走到 cwd 守卫就先撞「找不到可执行文件」（`b08` / `b09` 因为接受 `ProgramNotFound`
    才幸免）。**这两条要钉的都不是「这台机器有没有 cmd」**，所以判据跑偏了。
    做法：抽出 `platform_shell()` / `platform_shell_echo()` 返回 `(程序名, 参数)` ——
    Windows 给 `("cmd", ["/c", 脚本])`，其它给 `("sh", ["-c", 脚本])`；断言口径一字不改。
    判据：**安全用例的对照组要跨平台等价**，写死 `cmd`/`powershell`/`echo`/`/bin/sh` 都会在另一个平台
    变成「测这台机器装了什么」。

124. **夹具窗口「按文本量宽度」还不够，还要「整个落在屏幕内」**（2026-11-20，CI 的 windows 红，两轮）。
    `crates/hbot-device/tests/windows_desktop.rs` 的按窗口 OCR 夹具第一轮写死 `1100×320` → CI 上末尾两个
    字符被裁（认成 `"HBOT WIN OCR 77"`）；改成按 `GetTextExtentPoint32W` 实测文本宽度开窗（本机量到
    `1121` 宽）→ **仍然红，且 5 次重试稳定复现**，因为 `windows-latest` runner 的虚拟屏幕只有 1024 宽，
    窗口右边超出的部分**不会被合成进 `PrintWindow` 的位图**（`PrintWindow` 返回成功，但位图里就是没有）。
    做法：字号改成候选列表 `FONT_HEIGHT_CANDIDATES = [-96,-80,-72,-64,-48,-36,-28]`，新增
    `screen_extent()`（`GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)`）与 `current_layout()` —— 从大字号往小试，
    取第一个 `宽高都不超过屏幕` 的布局，**画字与建窗共用同一个 `Layout`**（否则「按 A 号字量、画 B 号字」），
    窗口贴 `(0,0)` 而不是 `(100,100)`。失败信息里带出 `screen_extent()` 与实际布局，下一轮红就能直接看出是尺寸问题。
    判据：**「稳定地少最后几个字」= 几何裁切，「偶发空串」= DWM 还没合成** —— 两者要分开处理（后者靠重试，
    前者重试一万次也没用）。同族：任何「按内容量出尺寸」的夹具都要再夹一层「不超过宿主可视区域」。

125. **同一个错误在 POSIX 与 Windows 上是不同的 `io::ErrorKind`，`io_error` 的兜底分支会把「用户输入错」变成 `Internal`**（2026-11-20，CI 的 ubuntu 红）。
    `crates/hbot-workspace/src/files.rs` 的 `create_dir("a.txt/sub")`（`a.txt` 是文件）在 Windows 上绿、
    在 Linux 上红：`got Internal { message: "path `a.txt/sub`: Not a directory (os error 20)" }`，而测试要求 `Invalid`。
    原因是 `crates/hbot-workspace/src/project.rs:278` 的 `resolve_new_dir` 用 `fs::symlink_metadata` 逐段上溯：
    POSIX 对 `a.txt/sub` 直接返回 `ENOTDIR`（`ErrorKind::NotADirectory`）→ 走最后一个 `Err(err) => io_error(...)`；
    Windows 返回 `ERROR_PATH_NOT_FOUND`（`ErrorKind::NotFound`）→ 走 `missing.push(name)` 继续上溯，
    最后在 `a.txt` 那一段命中 `Ok(_)` 且 `!canonical.is_dir()`，于是得到 `Invalid` 的 `is not a directory`。
    **同一个语义错误，两个平台走到两个分支**，而 `io_error`（`project.rs:521`）当时只有 `NotFound` / `PermissionDenied` / 兜底
    三条，`NotADirectory` 落到兜底就成了 `Internal`。
    做法：给 `io_error` 加 `io::ErrorKind::NotADirectory => HbotError::invalid(...)` —— 它是**输入问题**不是宿主故障，
    调用方（Agent 工具、UI）要能据此提示用户改路径；同时它把平台差异归一化，改一处覆盖所有调用点。
    配一条单测钉三件事：归 `Invalid`、文案带用户给的相对路径、**不带项目根的绝对路径**（坑 47）。
    判据：① `io_error` 这种「IO → 领域错误」的映射函数，每加一个 `ErrorKind` 分支都要问「另一个平台给的是不是同一个 kind」；
    ② 兜底 `_ => Internal` 只该接真正的宿主故障，**用户能改的输入不许落到这里**；
    ③ 这类红本地（Windows）永远看不见，只能靠 CI 或交叉编译 + 读码 —— 见坑 119 的判读纪律。
126. **「时间下界」类断言的前提，可能被启动路径自己悄悄满足掉**（2026-11-20，CI 的 ubuntu 红，差 12.78ms）。
    `crates/hbot-rpc/tests/hot_reload_e2e.rs` 的 `hot_reload_latency_after_saving_main_ts` 断言
    「保存 → 生效」的延迟 ≥ 由 metadata TTL 推出的下界，推导前提是「上一次 stat 就发生在预热那条消息里」。
    但每个 trial 的 `restart_ok()` 会调 `bot_start`，而 `crates/hbot-rpc/src/app/commands/bots.rs:279` 的
    `start_bot` 要跑 `run_lifecycle(Start)`，`run_lifecycle`（`sandbox_handler.rs:1742`）**第一句就是 `compiled()`**
    —— 也就是先算一次指纹、stat 一次。于是预热那条消息落在 TTL 内、**根本不 stat**，真正"上一次 stat"在
    `bot_start` 里，比 `warm_started` 早若干毫秒；下界因此被高估。本机 stat 快、预热回程 11–12ms，抵减后
    还留得住；CI 的预热回程 12.27ms 就把余量吃光了（`生效 1.9747s < 下界 1.9875s`），而且是 trial 1 绿、
    trial 2 红 —— 典型的"阈值卡在噪声上"。
    做法：预热之前先 `tokio::time::sleep(DEFAULT_METADATA_TTL + WARM_SETTLE)`，把 `bot_start` 那次 stat
    推到 TTL 之外，预热那条消息才成为真正"上一次 stat"。**不是**放宽断言、也不是删断言（它钉的是
    「指纹缓存真的按 TTL 才刷新」这一产品行为）。
    判据：① 任何"某件事必须发生在 X 之后"的时间断言，先把 X 之前所有会做同一件事的路径数一遍
    （启动钩子、预热、健康检查都爱顺手 stat / 编译一次）；② 本机绿 + CI 红、且差值只有几毫秒到几十毫秒时，
    先怀疑**前提**被满足，别先怀疑产品行为变了；③ 观测粒度（轮询间隔、日志落库）要显式写进推导，
    不要靠"反正够快"垫着。
127. **进程级测试计数器不能断言绝对值**（2026-11-20，CI 的 windows 红：`left: 6, right: 0`）。
    `crates/hbot-device/src/windows.rs` 的 `win32_probe` 是 `#[cfg(test)] static AtomicUsize`，
    用来证明「`allowed_apps` 为空时一个 Win32 调用都不许有」。但它是**进程级**的：同一个测试二进制里
    任何真走 Win32 的用例都在涨它，而 `cargo test` 默认并行跑。于是
    `empty_allow_list_is_rejected_before_any_win32_call` 的 `assert_eq!(count(), 0)` 会被邻居抬到 6 ——
    本机连跑 15 次全绿、CI 才红，典型的"串扰型 flake"。
    做法三条：① 断言改成比**增量**（`let before = count(); … assert_eq!(count(), before)`），
    「这次调用没有增加计数」才是这条测试真正要钉的语义，绝对值从来不是；② `bump()` 只放在**真的发起
    Win32 调用**的那一行 —— 它原来在 `virtual_key_of` 函数开头，于是纯查表的
    `combo_keys_map_to_their_virtual_key_codes` 也会抬 6，白送一次串扰；③ 没人用了的 `reset()` 删掉，
    留着只会诱使下一个人继续写"先归零再比 0"。
    判据：任何 `static Atomic*` / `static Mutex<Vec<_>>` 型测试探针，先问「谁会在我之外碰它」，
    再决定比绝对值还是比增量；同一进程里**没有任何**测试探针能靠 `reset()` 拿到独占。
128. **「副作用发生了」与「副作用被记账了」是两件事，等待条件必须等到记账**（2026-11-20，CI 的 ubuntu 红）。
    `crates/hbot-bot/tests/runtime.rs` 的 `outbound_is_persisted_then_retried_until_sent` 只等
    `fixture.adapter.sent().len() == 1`（假适配器 `send` 一返回就有记录），紧接着断言
    `supervisor.snapshot().outbox.sent == 1` —— 而 `crates/hbot-bot/src/outbox.rs` 的 `Ok(())` 分支是
    **先** `mark_outbound_sent` 落库、**再** `counters.sent.fetch_add(1, Ordering::AcqRel)`。于是存在一个
    窗口：消息真的发出去了，计数器还是 0，断言读到 `left: 0, right: 1`。本机 0.29s 的用例偶发，CI 才稳定红。
    做法：等待条件写成「已投递 **且** 已记账」两个条件的合取（`sent().len() == 1 && snapshot().outbox
    .is_some_and(|stats| stats.sent == 1)`），断言口径一字不改 —— 这是把等待补全，不是放宽。
    判据：① 凡是「先看 A 侧面（适配器 / 日志 / 文件）再断言 B 侧面（计数器 / 数据库 / 快照）」的测试，
    等待条件必须同时覆盖 B 侧面；② 排查时先看计数器的 `fetch_add` 在代码里排在哪一步之后 ——
    排在「落库成功」之后的计数器天然比副作用本身晚；③ 与坑 121 同族（那里是 `mark_processed` 晚于
    `executor.on_message`），一处竞态就要全仓扫同类读法（`grep "snapshot().outbox"` / `"list_unprocessed_inbound"`），
    别等 CI 一条一条报。

129. **文档里的「端到端在 `某文件`」不算证据，验收表必须写到能被 grep 到的用例名**（2026-11-20，P6 对账时发现）。
    `docs/07` §10 的验收表里，V2 写着「设备侧另有 approver 计数 = 0 的用例（`crates/hbot-rpc/tests/device_commands.rs`）」，
    V12–V16 一律写「端到端在 `tests/device_commands.rs`」—— 而那个文件里 `approv` / `HostLimit` / `toggle` / `audit`
    **全是 0 命中**：引用是空的，验收表看起来「已实测」，实际一条对应的断言都没有。同一次对账还发现 V13 的
    错误码写成了 `HOST_LIMIT`，真实实现给的是 `HBOT_DEVICE_NOT_ENABLED`（带 `toggle: device.enabled` / `device.capabilities`）。
    这类错误的危害不是「文档不精确」，而是**下一个人会拿它当"这块已经验过"的依据，于是不再验**。
    做法三条：① 验收表的每一格都要写**函数名**（`the_gateway_refuses_read_screen_at_the_capability_step`），
    不写「在 `x.rs` 里」；② 写完立刻 `grep` 一遍这些名字，0 命中就说明引用是编的；
    ③ 表格里的**错误码**与**字段名**也要对着实现核一遍（`toggle` 的取值、`HBOT_*` 的拼写），
    它们是契约的一部分，写错比缺失更容易误导。
    判据：任何「已实测 / 已覆盖」的字样，旁边必须有一个**能被执行器点名的测试**；
    只有实现没有自动化的（如 V15 真机 GUI）就明写「只有实现没有自动化」，不要混进「已实测」那一列。

130. **配额这类「跨会话共享的计数」必须由一条 SQL 判据，不能读-判-写三步**（2026-11-20，`C3.file` 配额落地时定）。
    同一 bot 的不同会话是**并行**的（每会话串行、跨会话并行，见 `docs/02-architecture.md` §4），
    而 `ctx.files.write` 在每个会话的 isolate 线程里同时发生。写成「先 `SELECT bytes`、再比 `limit`、
    最后 `UPDATE bytes + delta`」时，两个并发写会读到同一个旧值、都判定够用、都写进去 ——
    超出的量取决于并发度，而每一次写单独看都"通过了检查"。
    做法：把判据放进 `ON CONFLICT … WHERE`（`crates/hbot-store/src/repositories/files_usage.rs` 的 `reserve`），
    一条语句一个事务，判据不成立时 `changes() == 0`、一行都不改，调用方据此报 `HBOT_FILE_QUOTA`。
    同族：任何「先查余额/余量再扣」的记账（配额、额度、次数上限）都适用；
    测试要并发跑而不是串行跑，否则串行永远绿。
    另一个同批的口径：**账本减到 0 要删行**，否则「这个 bot 到底写过东西没有」不可判；
    反过来「删 bot 不删文件」是刻意的 —— 配额表是账本，不是文件清单。

131. **改动「记账」的测试里，最容易犯的错是把要验证的那个调用一起删掉**（2026-11-20，`C3.file` 配额单测）。
    `removing_a_directory_gives_back_the_whole_tree` 一开始红在 `left: 6, right: 0`（删完目录账本还是 6），
    看起来像 `release` 没生效；实际是我改断言时把 `files.remove("tmp", true)` 那一行整行替换掉了 ——
    「删完账本归零」这个断言从来没有执行过删除，于是它断言的是「没删也没记账」的初始状态。
    判据：① 一个断言变红时先读它**前后三行**，确认被测动作还在；② 「动作 + 断言」型测试改成
    先断言动作的返回值（`assert_eq!(files.remove(...), 4)`）再断言副作用 —— 返回值少了会立刻暴露；
    ③ 用「整段替换」改测试时，替换块里必须包含原块里的每一个调用，别只挑断言那行。

132. **「组件还活着但已经不再工作」是第三类状态，别把它塞进「运行中」**（2026-11-20，`onStop(crash)` 触发点）。
    `crates/hbot-bot/src/supervisor.rs` 的 `read_loop` 有五个退出点，之前**全部只是 `break`**：
    适配器订阅被关（`recv()` 返 `None`）、入站落库失败、入队失败之后，bot 在登记表里仍然是
    `Running`，面板显示绿灯，库里状态也是 running —— 唯一的现象是「它再也不回消息了」，
    而日志里什么都不一定有。更麻烦的是这类死亡**不改任何计数器**（没消息进来、handler 没失败），
    所以任何从外部观察的启发式（超时没活动、连续失败数）都只能猜。
    做法：退出原因作为 `read_loop` 的返回值 + 写进 `ReadExitSlot`（`Notify` 而不是通道：
    `notify_one` 会留 permit，先结束再等待也不会漏），`wait_read_loop_exit()` 是唯一读口，
    `hbot-rpc` 的 `start_inner` 起一个收尾任务等它、看到「意外」就按 `crash` 停机。
    判据：① 一个"常驻循环"必须能回答"它为什么停了"，不能只有 `break`；② 判据要由循环自己给，
    不要在外面猜；③ **别把"有兜底的局部失败"混进来** —— isolate 崩溃有 last-good 兜底
    （坑 24）、bot 还活着，把它算成 crash 会让用户的清理逻辑跑在一个活着的 bot 上。
    另一个必须记住的顺序：`Runtime::stop()` 会 `await` 读循环，所以测「正常停机不算意外」时
    **先 spawn 等待、再调 stop**，反过来会死锁。

133. **`cargo test --workspace` 返回 101 不一定是测试失败，可能是 rustc 自己被 OOM 打死**（2026-11-20 本机实测）。
    现象：日志末尾是 `alloc::alloc::handle_alloc_error` 的 backtrace，进程 `exit code: 0xc0000409`
    （`STATUS_STACK_BUFFER_OVERRUN`），紧接着一串 `error[E0463]: cannot find crate for hbot_adapters`。
    容易被误读成「有人把 Cargo.toml 改坏了」或「代码不编译」。真因是并行链接 ~120 个测试二进制时
    机器内存见底（本机 32 GB，实测那一刻只剩 7 GB），rustc 在写 rmeta 时分配失败。
    判据：① 先看日志尾部**有没有 `test result: FAILED` 或 `panicked at`** —— 没有就不是测试红；
    ② `cargo test --workspace` 单独复跑（不与其他 cargo 命令并发）通常立刻转绿；
    ③ 真红会指名某个用例，OOM 不会。别为这种假红去改断言或调低并发阈值。
134. **别用 `read` 的输出反向重写整个文件 —— 它有输出上限，会静默截断**（2026-11-20，代价是丢掉 316 行）。
    当时的动作是「`read` 全文 → 在末尾追加一条坑 → `write` 回去」，目标文件 `docs/pitfalls.md`
    约 90 KB / 678 行。`read` 为了控制上下文**按字节截断**返回内容，而返回值里仍然报着
    `totalLines: 678`，于是一写就把后 316 行抹掉了 —— `git commit` 的 stat 显示
    `316 +-------------` 才发现不对（幸好 commit 之后立刻看 stat 就能救回来：`git checkout <parent> -- <file>`）。
    规矩：① 给大文件追加内容用 `fs.appendFile`（或工具的 `edit`，用末几行当 anchor），
    **不要**走 read→write；② 改完任何文档/大文件，立刻看 `git diff --stat`，出现成百上千
    行删除就是截断而不是「整理」；③ 这类事故的恢复点是父提交，别急着重写。

## 135. 旁路参数塞不进一个「整个值」的通道（2026-11-21，C3.send 附件）

    `ctx.sendImage` 的宿主端口要把「发什么」送进 supervisor，而中间只有一条
    `mpsc<OutboundRequest>`（`crates/hbot-rpc/src/app/commands/bots.rs` 建通道、`sandbox_handler` 消费）。
    第一版为了少改十几处结构体字面量，把附件做成 `Outbox::enqueue_with_attachment(request, attachment)`
    的**第二个参数** —— 到接线时才发现 `OutboundRequest` 是这个通道唯一载荷，附件在那个位置没有落脚点，
    只能要么再开一条通道，要么在通道里塞一个 `Option` 假装是正文的一部分。

    最后改回「给 `OutboundRequest` 加 `attachment: Option<AttachmentRef>` 字段」，一次补完 10 处字面量。
    判据：**凡是穿过队列/通道的值，判据就是「接收端能不能只凭这个值完成工作」**；
    需要额外参数才能完成工作的，说明这个值不完整，加字段比加参数可靠（加参数时编译器不会
    提醒你还有第二个调用点没传）。

    两个附带的实测：① `#[serde(default)]` 不救结构体字面量，`E0063` 会逐个点名（这次漏了
    `sandbox_handler.rs` 的两处，因为它们的注释文本与别处不同，grep 时被跳过了）；
    ② 用 gzip 式「多次 `replace` 重构」后文件变小要复查 —— 这次 `send.rs` 从 ~8.6 KB 掉到 5.8 KB
    是整块函数合并（`enqueue_inner` 消失），不是截断；判断方法是 grep 关键行还在不在。

## 136. 附件的路径校验必须分两层：形状在 core，根内在宿主（2026-11-21）

    `validate_attachment`（`crates/hbot-core/src/bot_egress.rs`）只能判形状：非空、长度上限、
    无 NUL、不是绝对路径、没有 `..` 段。它**判不了**「解析到项目根内」—— 沙箱拿不到项目根、
    也没有 `fs`，而符号链接（`assets/link -> C:\Windows\win.ini`）只有 `canonicalize()` 之后
    才能看出越界。

    所以第二层在宿主：`StoreSendQueue::resolve_attachment`（`crates/hbot-rpc/src/app/runtime/send.rs`）
    做 `canonicalize()` → 比前缀 → `is_file()` → 比 5 MiB 上限，**在入队之前**做，失败当场返回错误。
    放到适配器那一刻才失败是错的：`ctx.sendImage` 早已返回成功，用户只看到「发了但对方没收到」，
    而且 sender 会把「文件不存在」当成可重试错误重试到死信。

    同族的两个细节：① 落库与穿越队列的路径是**绝对路径**（`attachment_json`），因为续发时项目根
    可能已经变了，相对路径会指向另一个文件；② 错误文案只带**文件名**不带目录（坑 47：这条 message
    会进 run log 面板与模型上下文）。

## 137. `ctx.sleep` 会被剩余预算夹短，所以它证明不了「预算生效」（2026-11-21）

    给作业/消息预算写端到端测试时，很自然会写「睡 3 秒，断言超时」。这条是**假的**：
    `crates/hbot-code/src/runtime/host_api.rs` 的 sleep 先取 `deadline.remaining()` 再夹短，
    3 秒的 sleep 在 1 秒预算下会**正常返回**，handler 接着往下跑、作业落 `done` —— 看起来「预算没生效」，
    实际是这层本来就有的语义（睡眠不该把预算变成异常）。

    要证明预算真的在拦，必须用**不夹预算**的忙等：`const started = Date.now(); while (Date.now() - started < 3000) {}`，
    超预算那一刻由 deadline 打断 handler，作业才会落 `failed`、结束行永远不出现。
    同一条理由在 `crates/hbot-code/tests/s2_ai.rs` 里已经用过（量模型抵账必须忙等）。
    对照组同样必要：不写 `limits.jobBudgetMs` 时 1.5 秒的作业必须跑满并落 `done`，
    否则「默认上限被误写小」会让所有长作业静默被掐断而掐断测试仍是绿的。

## 138. WAL 库的只读连接要用 `PRAGMA query_only`，不是 `SQLITE_OPEN_READ_ONLY`（2026-11-21）

    `crates/hbot-store/src/bot_sql.rs` 给 `ctx.sql.query` 单开一条连接（让长 SELECT 不占写锁）时，
    最直觉的写法是 `OpenFlags::SQLITE_OPEN_READ_ONLY`。库是 WAL 模式，这条会以
    `attempt to write a readonly database` 失败：只读打开一条 WAL 库需要能写 `-shm` 共享内存文件。

    正确写法是普通读写打开（不带 `CREATE`）+ `PRAGMA query_only = ON`：连接可以写 shm，
    而 SQL 层拒绝一切写。另外两条同族判据：① 库文件还不存在时**不要**把它记进「打不开」集合
    （否则文件建好后也永远走写连接），先 `path.exists()` 再决定；② 只读连接失败要**回退写连接**并记一行 warn ——
    这是性能优化，不是权限收紧，功能与错误码一律不变。

## 139. 日志行的时间戳是「落库时刻」，别拿它量耗时（2026-11-21）

    想用两条 `ctx.log` 的 `at` 之差证明「作业实际拿到了多少预算」是行不通的：
    `hbot-code` 先把日志收进 `EventOutcome.logs`，handler 结束后由宿主一次性落库，
    同一次执行的所有行共享**同一个**时间戳，差值恒为 0。

    要量耗时只有三条路：① 测试侧自己在派发前后取墙钟（会被 tick 间隔污染，需要宽余量）；
    ② 让 handler 把耗时写进 `ctx.kv` 或回复正文（那要额外能力，测预算时反而绕远）；
    ③ 改成断言**可分辨的结果**而不是时长 —— 例如「超预算 → 落 failed 且结束行不存在」，这正是
    坑 137 那条测试的形状。测量之前先确认那个数字真的是被测量的那一侧写下来的。

140. CI 的 job 一直 `queued` ≠ 它慢，是这台账号没有那种 runner 的容量（2026-11-21）

    `bundle (macos, dmg)` 在这仓库里一直是 `cancelled`，长期被当成「矩阵里没有 macOS runner，正常」。
    仓库 public 化之后第一次去读 `GET /repos/<owner>/<repo>/check-runs/<job_id>/annotations`，
    才看到真实原因：`The configuration 'macos-14-arm64-us-default' is not supported` —— 标签解析
    到的 runner 配置这台账号根本没有（一次都没真跑过）。

    换成 `macos-13`（Intel）之后不再报「不支持」，但 job **排队 35 分钟等不到 runner**
    （`runner_name` 一直是空的）。两者要分开判读：

    - **几秒内 `cancelled`/失败 + 有 annotation 说配置不支持** → 标签/配置不可用，换标签或换计划。
    - **一直 `queued`、`runner_name` 为空** → 容量问题（免费额度、机池紧张、镜像退役），等下去只会
      拖到 `timeout-minutes` 才失败，而且**每次 push 都会重演**，把整轮 CI 卡住。

    处理方式：把这类 job 改成手动触发（`if: github.event_name == 'workflow_dispatch' || vars.ENABLE_MACOS_BUNDLE == 'true'`），
    默认 push 时它是 `skipped` 而不是 `cancelled` —— 这两者的区别在报告里必须写清楚：
    `skipped` 是「没跑」，不是「通过」。别为了让整轮变绿把 `continue-on-error` 加上去。

## 141. Windows 的 DPI 虚拟化会让「同一块屏的 DPI 前后不变」变成假断言（2026-11-21）

    `crates/hbot-device/tests/windows_dpi.rs` 曾断言「声明 Per-Monitor V2 前后，每块屏的
    dpi_x/dpi_y/scale_percent 不变」，理由写着「DPI 是屏的属性，不是进程属性」。它在本机 100%
    缩放下连跑几个月都是绿的，直到某次远程会话把缩放改成 150% 才红：

    - 声明前（Unaware）：GetDpiForSystem 96、GetDpiForMonitor 96/96、rcMonitor 1280×720、位图 1280×720；
    - 声明后（PMv2）：144、144/144、1920×1080、1920×1080。

    这不是产品缺陷，是系统文档保证的 DPI 虚拟化：DPI-Unaware 进程调 `GetDpiForMonitor(MDT_EFFECTIVE_DPI)`
    被钉成 96，`GetMonitorInfoW` 的 `rcMonitor` 与 `CreateDCW` 抓的位图也一起被钉成逻辑像素。
    所以「不变」只在 **100% 缩放**时成立；恒成立的是两条：① 逻辑尺寸 / 逻辑缩放 == 物理尺寸 /
    真实缩放（交叉相乘 + 1% 容差，因为 `scale_percent` 是四舍五入过的）；② PMv2 下
    `rcMonitor` 恒等于物理像素（旧断言只在 scale==100 时才敢断，太弱）。

    同族坑：坑 116 与坑 124（夹具写死像素尺寸）那一类 —— **凡是把一个平台的默认配置写进断言，就要问一句
    「用户改了缩放/主题/DPI 之后它还成立吗」**。判据：断言应该是解方程的恒等式（换算关系）或
    单调性（只升不降），而不是「值相等」；真需要值相等就先断言前提（这里是「缩放比是 100%」）。
    产品侧不用改：`monitor_dpi`（`crates/hbot-device/src/windows.rs:963`）本来就该照系统说法报。

## 142. 追加 CSS 时把规则写进了上一个未闭合的块：样式静默「缩进到」别人的选择器里

2026-11-21 给 `apps/web/src/styles/app.css` 追加 bot 工作台样式时踩到：那一次是先 `read` 全文再 `write` 回去（坑 134 的复现），
`read` 有输出上限，**尾部 830 行被静默截断**，而我的插入点正好落在 `.device-panel__request {` 这个规则的声明之间 ——
于是这个规则既没有原始声明、也没有闭合花括号，后面整段工作台样式在构建时被 Lightning CSS 当成 **CSS 嵌套**，
全部编译成 `.device-panel__request .wb { … }` / `.device-panel__request .cap__row { … }`。

**症状是「样式全丢」，但没有任何报错**：构建 0 退出、`dist` 里能 grep 到类名、dev 服务器照样起。
只是渲染出来像没写 CSS —— 侧栏里 `状态 · KV未声明` 挤成一行、`.cap__row` 的 `display` 是 `list-item` 而不是 `grid`。

判读方法（比一张张截图猜快得多）：在浏览器/WebView 里 `getComputedStyle(document.querySelector('.cap__row'))`，
看 `display` 是不是你以为的值；不对就去 `dist/assets/*.css` 里 grep 类名，看它前面是不是被拼了一个父选择器。

规矩：

1. **给大文件追加一律 `fs.appendFile` 或 `edit`（加锚点）**，绝不 `read`→`write`（坑 134）。
2. 追加后**先做结构自检再看效果**：CSS 数花括号（`depth == 0 && neg == 0`）、Markdown 数标题、Rust 跑 `cargo fmt --check`。
3. 改完立刻 `git diff --stat`：行数变化与预期差太远（这次是 1046 行删成 214 行）说明被截断了，**先恢复再继续**。
   恢复手法：`git show HEAD:<path> > /tmp/head`，用「HEAD 里锚点之后的部分」重建尾部，再把新块拼到最后。

---

## 143. 设计稿里的「强调色」不能同时当文字色和实底按钮底色

2026-11-21 搬 Mino 的浅色主题时实测：Mino 的 `--accent:#e8742a` 直接做 `background` + `color:#fff`，
白字对比度只有 **3.02:1**（12–14px 正文要求 ≥4.5，AA 不达）。同一个值当文字色放在浅底 `#f6f3ee` 上是 4.69:1 ——
一头刚好、一头远不够。**一个色既当文字又当底色，必然有一头不达标**，这不是选值运气问题，是形状问题。

做法：拆两个令牌。

1. `--accent`：只当**文字/描边/图标**色（配浅底，取深到 4.7:1 左右）；
2. `--accent-fill` + `--accent-fill-hover` + `--accent-fg`：只当**实底按钮**（底色深到白字 ≥4.5:1，hover 再深一档）；
   深色主题下 `--accent-fg` 反过来是深字压浅橙底（10.4:1）。

品牌渐变、logo 这类**纯图形**可以继续用那个更鲜的原色 —— 文字不压在上面，不受对比度约束。

另两个同族教训（同一次改色）：

- **别只算一次对比度就落盘**。改成 `--fg-dim` 一档时它在 `--bg` 上 4.58:1、在 `--bg-elevated` 上 4.50:1，
  刚好卡在 4.5 边缘；换个面板底色就掉到 AA 以下。辅助文字色要么留 10% 余量，要么只在一种底上使用。
- **同一个信号不能用颜色独自承担**。`.status-dot--stopping` 与 `--degraded` 曾经都是 `--warn` 实心圆，
  色盲与灰度截图下完全不可分；改成「实心点 = 停止中 / 空心环 = 降级」，形状也参与编码。
- 验证手法：写个 10 行的 WCAG 相对亮度函数（sRGB 线性化 + `(L1+0.05)/(L2+0.05)`）本地算，别靠眼睛看截图；
  改完在 headless 浏览器里 `getComputedStyle` 取实际生效值再算一遍，防止变量没生效却以为算对了。

---

## 144. 加一个「整页面」要同批改五处，少一处就是「点了没反应」

2026-11-21 补作业看板 / 设计文档 / 开发日志时踩到。`SidebarView` 是联合类型，扩一个值编译器只会在
**部分**地方报错，剩下几处会静默降级（点了没反应、或侧栏渲染空白）：

1. `apps/web/src/lib/layout.ts` 的 `SidebarView` 联合类型；
2. `apps/web/src/lib/layout-prefs.ts` 的 `SIDEBAR_VIEWS`（存档迁移按这个白名单过滤，漏了会被 `PREFS_VERSION` 迁移丢掉，刷新回默认面）；
3. `apps/web/src/app/activity-bar.tsx` 的按钮表（aria-label 与 title 用同一个 label）；
4. `apps/web/src/app/App.tsx` 的 `renderSidebar()`（侧栏那一列）；
5. 同文件的 `renderPage()`（整页面正文）——**整页面不要塞进侧栏**：侧栏最大 420px，三列看板会折成三条竖线。

另两条同族：① 整页面的数据源不许另建一份（日志面直接用 `runStore` 的环形缓冲，文档面直接 `fs.read`/`fs.write`），
否则「底部面板显示 3 行、整页面显示 5 行」这种两套真相会立刻出现；② 浏览器 mock（`lib/ipc-mock.ts`）里
`bot_jobs` 是演示行、`run.log` 只在「试跑」注入后才有，截图时别把 mock 的空态当成功能没做。

## 145. `pnpm` 可能被 corepack 的空壳 shim 顶掉（与本仓无关，但会伪装成构建失败）

2026-11-21 实测：`pnpm --dir apps/web run check` 直接退出 1，日志唯一一行
`Error: Cannot find module 'C:\Users\Administrator\AppData\Local\node\corepack\v1\pnpm\12.21.0\bin\pnpm.mjs'`。
去看那个目录，里面只有一个 `pnpm-native.exe` —— corepack 装了一半。**这不是代码问题**，
但也别急着 `corepack prepare` 重装（要走网络）：先 `where.exe pnpm` 找另一个，
本机 `D:\node_global\pnpm.cmd`（12.14.0）可正常跑完整闸门。判据是**换一个 pnpm 后同一条命令转绿**，
而不是去改 `packageManager` 字段或删 `node_modules`。
