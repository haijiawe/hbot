# 08 · 多宿主与手机端（Multi-Host Blueprint）

> 状态：**Draft / 设计稿**（2026-11-17）。配套 [`docs/07-bot-runtime-blueprint.md`](07-bot-runtime-blueprint.md)（能力面）与 [`docs/adr/0004-one-core-three-hosts.md`](adr/0004-one-core-three-hosts.md)（决策）。
>
> 这份文档回答：**同一份 bot 代码，怎么在桌面、服务器、手机上分别跑起来，以及手机上到底能做到什么。**
>
> 事实分级：`实测` / `官方文档逐字` / `推断` / `未核实`。手机端的平台规则我全部核过官方原文，出处见每节末尾。

---

## 1. 三个宿主，一个核心

```text
                 hbot-core（领域 + 端口）
                        │
      ┌─────────────────┼─────────────────┐
  桌面宿主           无头宿主           手机宿主
  hbot-desktop       hbot-cli           hbot-mobile
  （Tauri 2）        （hbot serve）      （Tauri 2 mobile）
  Rust 静态链接      Rust 静态链接       Rust 静态链接进 lib
```

三者的差别只有三样：**壳**（窗口/托盘 vs 无 GUI vs 手机 UI）、**能力矩阵**（§3）、**打包方式**（§6）。核心、契约、项目格式完全一样。

| | 桌面 | 无头（服务器） | 手机 |
|---|---|---|---|
| 进程形态 | 单进程，核心同进程 | 单进程，无窗口 | 单进程，核心编成 lib 同进程 |
| IPC | Tauri command + 事件 | JSON-RPC over WS | Tauri command + 事件（同桌面） |
| 谁在用 | 开发者写 bot | 24h 跑 bot | 跑 bot + 看日志 + 审批 |
| 现状 | `已实现` | `已实现` | `设计`（未写一行移动代码） |

**为什么不是"手机装个客户端连桌面"**：那是方案 B（`docs/adr/0004` §3），保留为降级路径。要让 bot 用**手机自己的**网络与登录态，核心必须跑在手机上。

---

## 2. 一份契约（三端唯一的缝）

- **命令**：`crates/hbot-rpc/src/dto/command.rs`，24 条，`type` 值 snake_case、字段 camelCase。
- **事件**：`dto/event.rs`，12 条。
- **生成物**：`cargo run -p xtask -- gen` → `apps/web/src/generated/`（42 文件，入库，`gen --check` 防漂移）。
- **项目格式**：`hbot.json` + `bot.ts` 多文件 + `.hbot/hbot.db`。

手机端要做的第一件事就是**接同一份契约**——不是另发明一套。这样"桌面写的 bot 拿到手机能跑"才是结构上成立的，而不是靠人工对齐。

---

## 3. 能力矩阵：同一份代码，三种上限

| 能力 | 桌面 | 无头 | 手机 | 说明 |
|---|---|---|---|---|
| `events.message` | ✅ | ✅ | ✅ | 三端一致 |
| `events.timer` | ✅ | ✅ | ⚠️ | 手机只在进程活着时准；见 §4 |
| `events.webhook` | ✅ | ✅ | ❌ | 手机没有公网入口（§4.4） |
| `events.lifecycle` | ✅ | ✅ | ✅ | |
| `state.kv` / `state.sql` | ✅ | ✅ | ✅ | SQLite 三端都能编（§7） |
| `state.history` | ✅ | ✅ | ⚠️ | 手机窗口更小（内存） |
| `state.session` | ✅ | ✅ | ⚠️ | 手机等待超时要短 |
| `egress.reply` | ✅ | ✅ | ✅ | |
| `egress.send`（主动） | ✅ | ✅ | ⚠️ | 受平台限额约束 |
| `egress.notify` | ✅ | ✅ | ✅ | 手机上就是本地通知 |
| `egress.http` | ✅ | ✅ | ✅ | 清单授权（策略 C） |
| `egress.file` | ✅ | ✅ | ⚠️ | 手机 fs 被限在应用目录 |
| `compute.jobs` | ✅ 10 min | ✅ 10 min | ⚠️ 30 s 量级 | 见 §4.3 |
| `compute.wasm` | ✅ | ✅ | ⚠️ | 内存更紧 |
| `ai` | ✅ | ✅ | ✅ | 走网络，注意流量与费用 |
| `device`（截图/键鼠） | ✅ 需确认 | ❌ | ❌ | 见 `docs/07` §9 |
| Agent 的 `run_command` | ✅ | ✅ | ❌ | 手机不能起子进程（§5） |

**这张表就是 `hbot.d.ts` 的生成输入**（`docs/07` §7.2）：手机上 `run_command` 不出现在 d.ts 里，Agent 就不会写它。⚠️ 项出现在 d.ts 但带 `@host-limited` 注释，运行时返回 `HOST_LIMIT` 而不是静默降级。

---

## 4. 手机端能不能跑 bot：平台真相

这是整份文档最要紧的一节。**结论先给：能跑，但"24 小时在线"在 iOS 上做不到，在 Android 上要用户明确同意且受商店审核。**

### 4.1 iOS：退到后台约 5 秒后挂起

**官方逐字**：`applicationDidEnterBackground(_:)`「Your implementation of this method has approximately **five seconds** to perform any tasks and return」，随后「the system puts your app into the **suspended state**」。

`UIBackgroundModes` 的全部取值（逐字）：`audio, bluetooth-central, bluetooth-peripheral, external-accessory, fetch, location, nearby-interaction, network-authentication, newsstand-content, processing, push-to-talk, remote-notification, screen-capture, voip`——**没有任何一项是"保持网络连接"**。指南 2.5.4 明确：「Multitasking apps may only use background services for their intended purposes」。

所以：

- **普通 iOS App 在后台保不住 WebSocket**。想靠静音音频/假定位保活，是审核拒绝与下架的高发点，不能作为产品方案。
- 唯一合规的"被唤醒"路径是 **APNs 推送 → 唤醒 → 重连 → 拉取**，主循环仍由服务端驱动。
- `BGTaskScheduler` 不保证执行时刻（官方原文只说 `earliestBeginDate`「doesn't guarantee launching the task at the specified date」），单次时限官方也未给数值。

**JIT 与审核**：指南全文**没有出现 "JIT"**；iOS 上**没有公开可申请的 JIT entitlement**（`com.apple.security.cs.allow-jit` 的 platforms 只有 macOS）。第三方 JS 引擎在 iOS 上只能解释执行——这正是我们选 QuickJS 的原因（`docs/adr/0004` D3）。

**2.5.2 与 4.7**：2.5.2 禁止「download, install, or execute code which introduces or changes features or functionality of the app」；4.7 则明确允许「HTML5 and JavaScript mini apps and mini games, streaming games, **chatbots**, and plug-ins」这类非内嵌软件，附带内容过滤/举报/不得暴露原生 API/软件索引/年龄分级五项义务。我们的形态（解释器内置、脚本在本地生成、能力受 d.ts 与网关约束）最接近 4.7 的"mini app"，**但"AI 现场生成的代码是否算 download/execute"是审核判断，不是技术判断——未核实，必须按 4.7 的五项义务准备。**

### 4.2 Android：可以，但必须"前台服务 + 常驻通知"

**官方逐字**：Android 14 起前台服务必须声明类型；Android 15 起 `dataSync` 类型「permit an app's dataSync services to run for a total of **6 hours in a 24-hour period**」，超时抛 `RemoteServiceException`。

类型选择（现实只有两条）：

| type | 能不能用于"常驻长连接" | 理由 |
|---|---|---|
| `dataSync` | ❌ | Android 15 起 6h/24h 硬上限，被系统杀掉 |
| `connectedDevice` | ⚠️ 牵强 | 需要 `CHANGE_NETWORK_STATE`/BT/USB 之一，语义是"外部设备" |
| `specialUse` | ✅ 唯一可控 | 无运行时前提，但必须在清单写 `PROPERTY_SPECIAL_USE_FGS_SUBTYPE`，并过 Play 人工审核 |

另有三条硬约束：

1. **必须有通知**：官方原文「Apps don't need to request the POST_NOTIFICATIONS permission in order to launch a foreground service. However, apps must include a notification when they start a foreground service」。
2. **不能从后台启动**：Android 12+「can't start foreground services while the app is running in the background」，违规抛 `ForegroundServiceStartNotAllowedException`；Android 15 还禁止 `dataSync` 从 `BOOT_COMPLETED` 起。
3. **Doze 会定期掐网**：系统「suspending network access」直到下一个维护窗口；要稳定只能申请电池优化白名单，而 Play 政策原文：「prohibit apps from requesting direct exemption from Power Management features—Doze and App Standby—in Android 6.0 and above **unless the core function of the app is adversely affected**」；官方进一步说明**能用 FCM 的即时通讯类不被接受豁免**，只有"技术上无法用 FCM"才可接受，且即便如此也只是 partially exempt（普通 AlarmManager 与 jobs 仍受限）。→ **我们申请豁免大概率被拒，应默认接受"息屏一段时间后连接会断、靠重连恢复"。**

**Play 的前台服务政策**还要求：对用户有益且与核心功能相关、由用户发起或用户可感知、用户可停止、只在必要时长运行；且要在 Play Console 逐类型声明并提供演示视频。

### 4.3 三端"在线"能力对照

| 场景 | iOS | Android | 无头/桌面 |
|---|---|---|---|
| 打开应用时跑 bot | ✅ | ✅ | ✅ |
| 切走 / 息屏后继续 | ❌（~5 s 后挂起） | ✅（FGS + 通知） | ✅ |
| 息屏后稳定联网 | ❌ | ⚠️（需电池白名单） | ✅ |
| 24 h 在线 | ❌ | ⚠️（政策+ROM 双重不确定） | ✅ |
| 被推送唤醒后处理一条 | ✅（APNs） | ✅（FCM） | 不需要 |
| 收 webhook | ❌ | ❌ | ✅ |

### 4.4 于是"手机跑 bot"的产品形态只有三种

| 形态 | 做什么 | 适用 | 代价 |
|---|---|---|---|
| **A 前台常驻**（默认） | 打开 App 才跑；切走/息屏即停（iOS 必须这样） | 个人试玩、现场演示 | 不能收离线消息 |
| **B 前台服务**（Android 进阶） | FGS + 常驻通知 + 电池白名单 | 安卓当"小服务器"用 | 通知栏一直有图标；Play 审核；ROM 杀后台 |
| **C 云端/家用机跑，手机只做遥控** | bot 在桌面或服务器上跑，手机连过去 | 要 24 h 在线 | 依赖一台常开的机器 |

**推荐**：默认 A，Android 提供 B 作为显式开关（文案要写清"会有常驻通知、可能被系统杀"），C 作为"我要 24 小时"的答案（也是 `docs/adr/0004` 备选 B 的形态）。

**webhook 不要指望手机**：手机没有公网入口。要么走平台推送（QQ 官方/微信这类本来就是长连接，不需要 webhook），要么经一台中转机。这一条要写进 `docs/07` C1.webhook 的能力矩阵：手机 ❌。

### 4.5 由此产生的两条产品承诺（必须写进文案，不能含糊）

1. **"手机 24 小时在线"不承诺**。iOS 结构性做不到；Android 需要用户开前台服务 + 白名单，且可能被厂商 ROM 杀掉。
2. **"手机能跑 bot"承诺的是**：打开 App 就能跑、能看日志、能注入消息、能审批；Android 可另开常驻。

**出处**（本节全部为官方原文核对）：
- iOS 后台：<https://developer.apple.com/documentation/uikit/uiapplicationdelegate/applicationdidenterbackground(_:)>、<https://developer.apple.com/documentation/uikit/extending-your-app-s-background-execution-time>、<https://developer.apple.com/documentation/bundleresources/information-property-list/uibackgroundmodes>
- iOS 审核：<https://developer.apple.com/app-store/review/guidelines/>（2.5.2、2.5.4、4.7）
- iOS JIT entitlement：<https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.allow-jit>（platforms 仅 macOS）
- 本地网络：<https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy>
- Android FGS 类型：<https://developer.android.com/develop/background-work/services/fgs/service-types>、<https://developer.android.com/about/versions/14/changes/fgs-types-required>、<https://developer.android.com/about/versions/15/behavior-changes-15>
- Doze：<https://developer.android.com/training/monitoring-device-state/doze-standby>
- 后台启动限制：<https://developer.android.com/develop/background-work/services/fgs/restrictions-bg-start>
- Play 前台服务政策：<https://support.google.com/googleplay/android-developer/answer/13315670>

**未核实**（不要当结论用）：`beginBackgroundTask` 的具体延长秒数（官方只写「extra time」）；`BGTaskScheduler` 的具体时限；iOS 回环 `127.0.0.1` 是否触发本地网络授权（TN3179 未点名 loopback，按"非广播接口"定义推断不触发）；Android 明文策略对 `127.0.0.1` 是否豁免；Android 16 起的 local network permission 具体范围。

---

## 5. 生态：手机上没有 Node，怎么用 npm

`docs/07` §6 的 E1 档落地方式：

```text
桌面/CI 构建时：  项目里的 dependencies
                  → esbuild/rolldown 打包成单文件 bundle
                  → .hbot/bundle.js（随项目走，入库或不入库可选）
手机上运行时：     QuickJS 只跑 bundle.js（一个文件）
```

- **好处**：`dayjs`/`zod`/`lodash` 这类纯 JS 包三端都能用；内存不变（还是 0.30 MB/isolate 量级）。
- **硬限制**：① 有原生 addon 的包（`sharp`、`better-sqlite3`）**不行**；② 依赖 `node:fs`/`net`/`process` 的包**不行**；③ bundle 体积计入项目（要有预算）。
- **代价**：供应链攻击面。闸门：`pnpm audit`/OSV + 依赖清单人工过一遍 + bundle 里禁止 `eval`/动态 `require`。
- 现状：`设计`。今天 `hbot-code` 只允许裸模块名 `hbot`（`modules.rs` 的 `HOST_MODULE`），没有任何第三方包通路。

---

## 6. 打包与分发

| 宿主 | 产物 | 现状 |
|---|---|---|
| 桌面 Windows | NSIS `hbot_0.1.0_x64-setup.exe` **9.14 MiB** | `实测`（安装冒烟 11/11） |
| 桌面 macOS | `.dmg` | `未验证`（无机器） |
| 桌面 Linux | `.deb`/AppImage | `未验证` |
| 无头 | 单二进制 `hbot` **18.05 MiB** + systemd 单元 + logrotate | `实测`（二进制）/ `未验证`（systemd 真跑） |
| Android | APK / AAB，需 keystore 签名 | `设计` |
| iOS | IPA，**只能 macOS + Xcode 构建**，需 Apple Developer 计划（约 99 美元/年） | `设计` |

移动端体积：按桌面静态库（`libquickjs.a` 2.96 MB、`libsqlite3.a` 4.61 MB）+ `swc` 的 rlib 体量外推，**单 ABI strip+LTO 后约 8–15 MB**——`估算，未实测`（本机没有 NDK/Xcode）。

---

## 7. 工程风险（来自 `stack-portability` 的 registry 源码核查）

| # | 风险 | 具体 | 处置 |
|---|---|---|---|
| R1 | **`rquickjs` 没有 Android/iOS 预生成 bindings** | `rquickjs-sys/src/lib.rs:19` 按 target 文件名 `include!`，`src/bindings/` 只有 20 个 target，**既无 `aarch64-linux-android` 也无 `aarch64-apple-ios`** → 默认配置直接编译失败 | 开 `bindgen` feature（需 libclang + `BINDGEN_EXTRA_CLANG_ARGS` 带 `-isysroot`），或**一次性 vendor 两份 bindings 进仓库**（推荐，避免每台 CI 装 libclang） |
| R2 | **`hbot-desktop` 自己挡住移动端编译** | `src/shell.rs:22` 无条件 `use tauri::tray::…`、`lib.rs:543` `tauri_plugin_single_instance::init`、`lib.rs:558` `.on_tray_icon_event(…)`；而 `tauri::tray` 是 desktop-only | 抽 `hbot-desktop`（桌面壳）与 `hbot-mobile`（移动壳）两个 crate，共用 `hbot-rpc`；或全部加 `cfg(desktop)` 门控（更省 crate，但一个文件里两套壳更难读） |
| R3 | **三处 C 依赖各自的工具链** | SQLite（`cc`）、QuickJS（`cc` + bindgen）、ring（预生成 `.S`，**不需要 perl/nasm**） | Android：NDK + `cargo-ndk`；iOS：Xcode + `rustup target add aarch64-apple-ios` |
| R4 | 移动端工程化零起点 | 无 target、无 `.cargo/config.toml`、CI 无移动任务 | M8 第一件事就是把这条链跑通 |
| R5 | 两套 WebView | Android 系统 Chromium、iOS WKWebView（版本随系统） | 前端只用两边都稳定的 API；E2E 至少跑一台 Android 真机 |
| R6 | **Android 明文 HTTP 默认禁用** | targetSdk ≥ 28 起明文默认关，且 network security config **对所有主机统一生效、没有 `localhost`/`127.0.0.1` 白名单**（AOSP 源码核对） | 手机壳用 loopback 服务时，要么在 NSC 里显式 `domain-config cleartextTrafficPermitted="true"` 放行该域，要么走 https/wss；桌面那条 `http://localhost` 不能直接照搬 |

**已经确认没问题的**（省得以后重查）：`rusqlite` + `bundled` 支持 Android/iOS（Android 有上游特判 `-DSQLITE_TEMP_STORE=3`，iOS 无需额外配置）；`ring` 有 android/ios 预生成汇编；`reqwest` + `rustls-tls` + `webpki-roots` 可用（**不要换 native-tls**，那反而要在 Android 编 OpenSSL）；`swc_ecma_*`、`ignore`、`grep-*`、`blake3` 全部纯 Rust 无 `build.rs`；`hbot-command` 在移动端能编（`cfg` 兜底到 `confinement_unsupported.rs`）。

---

## 8. 数据与同步

- 项目是**目录**：`hbot.json` + 源码 + `.hbot/`（`hbot.db`、日志、trash）。
- 手机沙盒：`fs` 被限制在应用目录（官方文档原文「restricted to the application folder by default」）。所以"手机上的项目"必须在 `AppData`/`Documents` 内部，不能任意指路径。
- 三端同步**不做自动同步**（会引入冲突合并、鉴权、多写者——那是另一个产品）。做法是显式两条路：
  1. **git**：项目目录本来就是 git 友好的（旧项目已验证这条路），手机端提供 `.zip` 导入导出；
  2. **局域网直连**：桌面 IDE 通过 RPC 连手机上的 daemon 直接编辑（这才是"在手机上跑、在桌面写"的正解）。
- 冲突原则：**同一时刻只有一个写者**。手机跑 bot 时桌面不要改同一个项目（或者只读连接）。

---

## 9. 里程碑（接在 `docs/04` 的 M6 之后）

| 里程碑 | 内容 | 出口标准 |
|---|---|---|
| **M7 能力底座** | `docs/07` §11 的 1–5 步：kv / history / ai / timer / jobs / session | 四类 bot 的示例项目各一个，全部能在桌面跑通；V1–V11 验收全绿 |
| **M8 Android 宿主** | 拆 `hbot-mobile` crate；vendor rquickjs bindings；APK 能装能跑；前台服务与常驻通知；能力矩阵裁剪 | 真机装上、`local` bot 能跑、`onebot11` 能连局域网协议端、`bot_inject` 有回复；体积 ≤ 25 MB |
| **M9 iOS 宿主**（可选） | macOS + Xcode 构建；IPA；`WKWebView` 控制台；仅前台运行 | 真机 TestFlight 可装、前台跑通同一条链路；审核按 4.7 准备材料 |

**顺序理由**（`docs/adr/0004` §6）：手机端 90% 的工作量是"宿主移植"，10% 是"能力裁剪"。能力底座没做就移植，等于把四种半成品复制到三个平台。

---

## 10. 验收标准

| # | 验收 | 怎么测 |
|---|---|---|
| H1 | 同一份 bot 项目在桌面与 Android 都能编译并通过 `onMessage` | 同一个 fixture 两端各跑一次 |
| H2 | 手机上 `run_command` 不出现在 d.ts、调用返回 `denied` | 生成 d.ts 断言 + 调用断言 |
| H3 | 能力受限项返回 `HOST_LIMIT` 且消息里带上限值 | 手机上跑 `jobs`，断言错误码 |
| H4 | iOS 切后台后 bot 明确停止并记一条"已挂起"日志，不假装在跑 | 真机切后台，断言日志与状态 |
| H5 | Android 前台服务停止后通知消失、服务不留残 | 真机停服务，查通知栏与进程 |
| H6 | 三端 RPC 契约一致（同 `gen --check` 产物） | CI 比对三端生成物哈希 |
| H7 | 手机导出的项目 zip 在桌面能直接打开并跑 | 真机导出 → 桌面 `project_open` |

---

## 11. 未决问题

1. 移动壳是**新 crate**（`hbot-mobile`）还是 `hbot-desktop` 里 `cfg` 门控？（倾向新 crate：托盘/单实例/更新这些桌面专属逻辑不该和移动共处一个文件）
2. rquickjs bindings 是**开 bindgen**（CI 装 libclang）还是**vendor 两份**（体积换来确定性）？（倾向 vendor）
3. Android 的 `specialUse` 申报文案怎么写才能过审？（"运行用户自己编写的自动化脚本"——要真实描述，不能含混）
4. 手机端要不要做"本地通知"能力（`egress.notify` 在手机上是系统通知，语义与桌面托盘通知不同）。
5. 手机端 `hbot.db` 的 WAL 在 iOS 沙盒里是否有已知问题？（未核实）
6. M8 要不要同时支持 Android 平板/横屏（IDE 在小屏上用不了，但平板可能能用）。