# 09 — C6 设备能力：实现设计（窄版 → 完整版）

> 状态：**Draft**（设计稿；本文不含实现代码）｜2026-11-17
> 上位文档：[`07-bot-runtime-blueprint.md`](07-bot-runtime-blueprint.md) §9（能力与授权模型）、[`adr/0005-capability-ceiling-and-device-grant.md`](adr/0005-capability-ceiling-and-device-grant.md)（决策与备选）、[`08-multi-host.md`](08-multi-host.md) §4.6（手机端两条硬结论）
> 本文只回答**怎么落地**，不重述**为什么**。理由去 07 §9 与 ADR-0005。

---

## 0. 这份文档防止什么被忘掉

六条容易被时间冲掉的结论。它们不是"待讨论"，是**已经确定**的：

1. **设备能力在范围内。** 2026-11-17 用户原话：「都要，能力上限肯定越高越好……没有限制，当然也可以在设置修改，要么本地要么云端」。它曾是"建议晚做"，现在只是"排第几"。
2. **默认全关，逐个子能力授权。** 不做一个 `device: true` 总开关 —— 那会逼用户"为了截图交出点击权"。
3. **三层闸缺一层就拒**：清单声明 → 宿主设置（含应用白名单）→ 限时设备会话。**清单声明 ≠ 被允许；设置里开过 ≠ 永久授权。**
4. **设备授权与文件沙箱档正交。** `read-only` 约束的是文件与命令，**不代表**不能控制鼠标。UI 文案必须写这句话，否则用户会以为"只读档是安全的"。
5. **防护由宿主自己实现，不能指望系统。** 决定性事实：X11 两份规范都没有客户端授权要求 —— 能连上 X server 就能抓屏和注入。系统不拦，我们的闸就是唯一的闸。
6. **Android 上 Agent 驱动的设备自动化上不了 Google Play**（Play 禁 `autonomously initiate, plan, and execute`，只允许 `static, human-defined script`）→ 只能侧载；**iOS 上没有第三方注入 API** → 设备能力实际上为零。所以 C6 是**桌面三平台**的能力。

---

## 1. 决策与落点（2026-11-17）

| # | 决策 | 用户原话 / 依据 | 落在哪 |
|---|---|---|---|
| D1 | 能力上限不人为砍 | 「能力上限肯定越高越好，实现能力也能提升些不是吗，没有限制」 | 07 §9、ADR-0005 D1 |
| D2 | 默认全关、逐子能力、设置里改 | 「当然也可以在设置修改」 | 07 §9.2、ADR-0005 D2/D3 |
| D3 | 三道闸，缺一道就拒 | 设计 | 07 §9.2、本文 §4 |
| D4 | 与沙箱档正交，UI 必须说清 | 设计（避免"只读档=安全"的误解） | 07 §9.2、ADR-0005 D4 |
| D5 | 七条防护由宿主实现 | X11 无授权机制等事实 | 07 §9.3、本文 §6 |
| D6 | 运行位置可选（本机/局域网/云端） | 「要么本地要么云端。都可以的」 | 08 §9、ADR-0006 |
| D7 | 手机侧载 / iOS 无设备能力 | Play 与 Apple 官方原文 | 08 §4.6 |

> 还有一件**没定**的事：`M10` 是现在插主线，还是等 `M7` 能力底座做完（见本文 §11）。用户尚未回答。

---

## 2. 关键区分：两个消费者，一份闸

这是本文最重要的一节。漏掉它，实现必然长出第二份判据（旧项目 P0 的直接教训，见 `AGENTS.md` 坑 8）。

| 消费者 | 谁在用 | 走哪条路径 | 现在的闸 |
|---|---|---|---|
| **A. 生成的 bot 代码** | `ctx.device.click(...)`，在 QuickJS isolate 里 | `host_api.rs` 注入的宿主函数 → Rust 闭包 | **没有** —— 必须新建 |
| **B. Agent 自己的工具** | 模型决定"我要点一下"时调 `device_click` 工具 | `ToolGateway` 八步（`gateway.rs`） | 复用步骤 1 的 Capability 闸 |

**结论：闸的实现只能有一份。** 放在 `crates/hbot-code/src/device/gate.rs`，A 直接调它；B 的 `ToolGateway` 第一步（`Capability`）也调它。两条路都不许自己写一遍判断。

为什么 A **不能**顺带经过 `ToolGateway`：gateway 的语义是"模型请求一次工具调用，可能要问人"。而 bot 代码是**已经获批运行的代码**在事件处理里调宿主 API —— 没有"一次工具调用"这个东西，也不能在这条路径上弹审批（会把 handler 卡在 2 秒预算里）。所以 A 的能力授权必须在会话开始前就完成，运行时只做"有没有"的判定。

---

## 3. 数据模型

两张新表，落在现有的 `hbot.db`（单 writer 线程，`crates/hbot-store`）。

### 3.1 `device_sessions`

| 列 | 类型 | 说明 |
|---|---|---|
| `id` | TEXT PK | UUIDv4 |
| `bot_id` | TEXT | 归属 bot（**一个 bot 同时最多一个活跃会话**，见 07 §13 问 7） |
| `state` | TEXT | `requested` / `active` / `expired` / `revoked` / `denied` |
| `subcapabilities` | TEXT | 逗号分隔：`screen,input`（**不含**未授权的） |
| `allowed_apps` | TEXT | JSON 数组；空数组 = 全拒 |
| `granted_at` | INTEGER | unix 毫秒 |
| `deadline` | INTEGER | unix 毫秒；默认 5 min，上限 30 min |
| `ended_at` | INTEGER NULL | 结束时刻 |
| `ended_reason` | TEXT NULL | `timeout` / `revoked` / `bot_stop` / `shutdown` |

索引：`(bot_id, state)`。

**为什么落库而不是只在内存**：① 崩溃/重启后必须能判定"上次那个会话已经不算数"，不能靠内存为空来"默认安全"（万一某天有人改成从库里恢复）；② 审计要能回看"上周二那个会话是谁开的、开了多久"。

### 3.2 `device_audit`

| 列 | 说明 |
|---|---|
| `id`、`session_id`、`at` | 自增/时间 |
| `action` | `screenshot` / `click` / `key` / `move` / `focus` / `clipboard_read` / `clipboard_write` |
| `target` | 窗口标题或坐标，**不含像素、不含剪贴板内容、不含 OCR 文本** |
| `outcome` | `ok` / `denied` / `failed` |

**审计不记内容**（07 §9.3 第 5 条）：记了内容，日志本身就成了新的泄漏面 —— 密码框的像素、剪贴板里的 token 都会躺在 `logs` 里。

保留策略：随项目 DB，不做单独裁剪（与现有 `logs` 一致）；量级是每会话最多 10000 条（07 §9.3 的 `maxActionsPerSession`）。

---

## 4. 三层闸的落点

### 4.1 第一层：清单声明（Agent 写、你看得见）

`crates/hbot-code/src/manifest.rs:111` 的 `Permissions` 现在只有 `http: Vec<String>`。加：

```json
{
  "permissions": {
    "http": ["api.example.com"],
    "device": {
      "sub": ["screen", "input"],
      "apps": ["PathOfExile.exe"]
    }
  }
}
```

- `sub` 是**子能力白名单**，不是布尔。没写 = 没有。
- `apps` 是应用白名单的一部分（清单提议 + 设置必须同时允许，见下）。
- `deny_unknown_fields` 已在该结构上，打错字段名直接报 `HBOT_MANIFEST_INVALID` 而不是静默失效。

### 4.2 第二层：宿主设置（用户拍板）

**放 `<data_dir>/device.json`，不放 `desktop.json`。**

| | `desktop.json`（已存在） | `device.json`（新建） |
|---|---|---|
| 谁读 | `crates/hbot-desktop/src/settings.rs` | **核心**（`hbot-rpc` 的运行时装配） |
| headless 有没有 | **没有**（无头没有桌面壳） | 有 |
| 放这里会怎样 | 无头模式读不到自己的设置 → 闸在无头下形同虚设 | 三宿主同一份 |

这就是"闸必须在核心"的具体体现：**桌面壳可以写这个文件，但不能是唯一读者。**

形状：

```json
{
  "device": {
    "enabled": true,
    "sub": ["screen", "input"],
    "apps": ["PathOfExile.exe", "Notepad.exe"],
    "maxSessionMinutes": 5
  }
}
```

- `enabled` 默认 `false`（与 `DesktopSettings.autostart` 同款产品决策：首次启动不许偷偷开）。
- `apps` 空 = 全拒（**不是**全放）。
- 读不到/坏 JSON → 退回默认（全关）并 warn，启动不许失败（沿用 `DesktopSettings::load` 的容错口径）。

### 4.3 第三层：运行时会话（撤销要立刻生效）

`AppState` 加 `device: Arc<DeviceGrants>`，内部 `Mutex<HashMap<BotId, Grant>>`。

**每一次宿主调用都查这个 `Arc`，不许在 spawn 时把授权快照塞进 isolate。** 这是"一键全局撤销"能成立的前提 —— 否则撤销只对下一个 isolate 生效，用户按下撤销键后鼠标还在动。

### 4.4 判定表（缺一层就拒）

| 清单 | 设置 | 会话 | 结果 | 错误码 | `data.toggle` |
|---|---|---|---|---|---|
| 没声明 | — | — | 拒 | `-32005 denied` | 无（**不提示开关**：代码本来就不该有这个能力） |
| 声明了 | 关 | — | 拒 | `-32005 denied` + `HOST_LIMIT` | `device.enabled` |
| 声明了 | 开但子能力缺 | — | 拒 | 同上 | `device.sub.screen` |
| 声明了 | 开 | 无会话/已过期 | 拒 | `-32005` | `start a device session` |
| 声明了 | 开 | 活跃 | 放行 | — | — |

**关键差别**：清单没声明时**连开关都不提示**（V12 要求 approver 调用次数 = 0）；声明了但设置没开时才告诉用户"去打开哪个开关"（V13 要求 `data.toggle` 存在）。两者混同会让用户以为"关掉开关能拦住任意代码"。

---

## 5. 宿主抽象

形状照抄 `crates/hbot-code/src/http.rs:130` 的 `trait HttpTransport`（已验证好用的三件套：trait + config + 错误枚举）：

```rust
pub trait DeviceBackend: Send + Sync {
    fn capabilities(&self) -> CapabilityReport;          // 探测：Wayland 上 input 不可用等
    fn screenshot(&self, req: &ScreenshotRequest) -> Result<Screenshot, DeviceError>;
    fn input(&self, action: &InputAction, allow: &AllowList) -> Result<(), DeviceError>;
    fn windows(&self) -> Result<Vec<WindowInfo>, DeviceError>;
    fn clipboard(&self, op: ClipboardOp) -> Result<ClipboardResult, DeviceError>;
}
```

三条硬约束：

1. **阻塞调用，绝不在 isolate 线程直接做。** 截图 + 编码一次 20–80 ms，而 handler 预算是 2 秒。走 `spawn_blocking` + 超时（沿用 `hbot-rpc` 的 `blocking()` 惯例）。
2. **不允许 backend 自己判断"该不该做"。** 它只执行；判定全在 `gate.rs`。理由同 `net.rs` 与 `permissions.rs` 的分工（`AGENTS.md` 坑 48：两道判据会打架）。
3. **坐标一律用"物理像素 + 显示器原点"**，逻辑坐标（DPI 缩放后的）转换放在 gate 之后再算，且**必须记录在审计里用的是哪个屏幕**。macOS 的 Retina、Windows 的多显示器负坐标、Linux 的缩放，三个都不一样。

---

## 6. Windows 实现（窄版唯一目标平台）

### 6.1 截图：三个候选，必须实测选（S14）

| 方案 | 优点 | 已知问题 | 状态 |
|---|---|---|---|
| `BitBlt` + `PW_RENDERFULLCONTENT`（`PrintWindow`） | 最简单、无新依赖、能只截单个窗口 | 硬件加速/DirectComposition 窗口可能全黑；独占全屏游戏基本不行 | 需实测 |
| `Windows.Graphics.Capture`（WinRT） | 官方推荐、能截 DComp 内容 | Win11 起有捕获指示边框；需要 WinRT 绑定（新依赖 `windows` crate 的 `Graphics_Capture` feature）；在部分全屏游戏上仍黑 | 需实测 |
| DXGI Desktop Duplication | 最快、全屏游戏可用 | 只能整屏（不能单窗口）；出错要重建；代码量最大 | 需实测 |

**倾向**：窄版先 `BitBlt`+`PrintWindow`（零新依赖、能过 V15 白名单测试），把"游戏黑屏"作为已知限制写进文案；`DXGI` 留给"玩游戏"这个用例真做的时候。**在 S14 实测三种在同一台机器同一游戏上的结果之前，不写进验收标准。**

### 6.2 输入：`SendInput` + UIPI

- 用 `SendInput`（不是 `keybd_event`/`mouse_event`，那两个已废弃）。
- **UIPI 是硬墙**：官方原文「permitted to inject input only into applications that are at an equal or lesser integrity level」。我们的进程是 Medium，所以**提权窗口、UAC 对话框、任务管理器一律点不动**。这不是 bug，写进文案。
- **锁屏期间失败**：`OpenInputDesktop` 拿不到时直接返回 `denied`，不要重试。
- 绕 UIPI 只有 `UIAccess`（manifest + Authenticode 签名 + 装在安全位置），**窄版不做**：它要求代码签名证书，且仍进不了 system IL。

### 6.3 前台窗口与白名单（V15）

顺序：`GetForegroundWindow` → `GetWindowThreadProcessId` → 进程映像名 → 与 `allowed_apps` 交集。

- 白名单匹配用**进程映像名**（`PathOfExile.exe`），不用窗口标题（标题可变、可伪造）。
- 判定在**每次注入前**做，不是会话开始时做一次 —— 用户切窗口的瞬间就必须停。
- 判定失败时**一个按键都不许发**（V15 断言"B 未收到按键"，不是"返回了错误"）。

### 6.4 已知坑（写在这里省一次返工）

- **DPI 感知**：进程 manifest 必须声明 Per-Monitor V2，否则在 150% 缩放下坐标错位。`hbot-desktop` 的 Tauri 壳已处理，但 `hbot serve` 无头路径**没有窗口**，抓屏走的是 session 桌面 —— 两边 DPI 语义不同，别共用一份换算。
- **多显示器负坐标**：虚拟屏幕原点可能在 (0,0) 左边，裁剪时必须用 `SM_XVIRTUALSCREEN` 而不是假设 (0,0)。
- **管理员窗口**：见 UIPI。
- **RDP/无会话**：无交互桌面时 `OpenInputDesktop` 全失败，返回 `HOST_LIMIT` 并在 UI 说明"远程会话下设备能力不可用"（未核实具体表现，S14 里测）。

---

## 7. 会话状态机

```
            用户开 / Agent 请求获批
  (无) ─────────────────────────────► active{deadline}
   ▲                                      │
   │                     ┌────────────────┼────────────────┐
   │                到期 │            撤销 │            bot_stop / 进程退出
   │                     ▼                ▼                ▼
   │                  expired          revoked          revoked
   └───────── 下次要重新开（不自动续） ──────────┘
```

四条规则：

1. **不自动续期。** 到期就得用户再点一次。自动续期等于永久授权。
2. **到期由定时器结算，不靠"下次调用时才发现过期"**（旧项目 `wait_input` 超时永不结算的 P0 教训，见 `docs/00-assessment.md` P0-3）。复用 M7 要建的定时器骨架；M7 没做完时用一个 1 秒 tick 的独立任务，但**必须在库里写 `ended_at`**，否则审计里会出现"永远活跃"的会话。
3. **两条入口**：用户主动（设置页 / 快捷键 / 托盘菜单）+ Agent 请求（Agent 调 `device_request` 工具 → UI 弹卡片 → 用户点"允许 5 分钟"）。**bot 代码没有请求入口** —— 它会卡住 handler，且"代码自己申请权限"是个坏模式。
4. **撤销是全局失效，不是暂停**：清 `DeviceGrants` 里的条目 + 库置 `revoked` + 发事件 + 停所有正在进行的注入（注入是短调用，天然容易停）。

---

## 8. 契约增量（精确到名字，供冻结）

按 `docs/02-architecture.md` §9 的口径写，**表与实现必须一致**（`project_open` 漂移的教训，commit `4014610`）。

### 8.1 命令（`crates/hbot-rpc/src/dto/command.rs`，`type` 值 snake_case、字段 camelCase）

| `type` | 入参 | 成功 data | 失败 |
|---|---|---|---|
| `device_status` | `{ botId }` | `DeviceStatus`；无会话时为 `null` | `-32001` bot 不存在 |
| `device_grant` | `{ botId, sub, allowedApps?, minutes? }` | `DeviceStatus` | `-32002` 子能力非法/`minutes` 超 30；`-32005` 设置未开 |
| `device_revoke` | `{ botId }` | `null` | `-32001`；无活跃会话时**成功**（幂等） |
| `device_request` | `{ botId, sub, reason }` | `null`（请求已投递给 UI） | `-32002`、`-32005` 清单没声明 |

`DeviceStatus`（`dto/common.rs` 新增）：

```
{ sessionId, botId, state, sub: [String], allowedApps: [String],
  grantedAt, deadline, remainingMs }
```

### 8.2 事件（`dto/event.rs`，现有 12 个 → 14 个）

| `type` | 用途 | 通道 |
|---|---|---|
| `device_status` | 会话开始/到期/撤销都发，UI 靠它画指示条 | **critical**（用户必须立刻看到，丢一条等于指示条骗人） |
| `device_request` | Agent 请求授权 → UI 弹卡片 | **critical** |

`device_status` 必须进 `crates/hbot-rpc/src/ws.rs` 的 `classify()` 的 critical 分支（与 `ApprovalRequest`/`DebugPaused` 同级）。理由：这个事件丢了，用户会以为"没有会话在跑"，而鼠标正在被控制。

### 8.3 生成代码面（`hbot.d.ts`）

```ts
interface DeviceApi {
  screen(): Promise<{ width: number; height: number; png: Uint8Array; monitor: number }>;
  windows(): Promise<Array<{ id: number; title: string; process: string; focused: boolean }>>;
  click(x: number, y: number, button?: 'left' | 'right'): Promise<void>;
  key(text: string): Promise<void>;                         // 文本注入
  keyCombo(keys: string[]): Promise<void>;                  // "ctrl+c"
  clipboardRead(): Promise<string>;
  clipboardWrite(text: string): Promise<void>;
}
```

**没在清单里声明的子能力，这个对象上就不出现对应方法**（V1：生成 d.ts 后断言不含该符号）。`dts.rs:388` 的 `host_bindings_match_the_module_and_the_dts` 四向防漂移测试必须扩到 `device`，否则 d.ts 会承诺运行时不存在的符号。

---

## 9. 窄版 vs 完整版

### 9.1 一个必须先承认的事实

**窄版躲不开"模型怎么看屏幕"。**

`crates/hbot-agent/src/provider/mod.rs:245` 是 `pub content: Option<String>` —— **消息内容只有文本**。两个 SSE 客户端（`provider/openai.rs`、`provider/anthropic.rs`）都不发图像块。所以"截一张图给模型看"这条路今天走不通。

两条路：

| 路线 | 要做什么 | 代价 | 建议 |
|---|---|---|---|
| **A. 补视觉输入** | `ChatMessage` 支持 image 块；OpenAI 走 `image_url` + base64、Anthropic 走 `image` + `source.base64`；UI 要能显示 Agent 看到的那张图 | 契约改动（provider + 两个 SSE 客户端 + 可能 UI）；成本高（一次截图 1080p PNG 约 200–400 KB，一张图 ≈ 数万 token） | 长版正路 |
| **B. OCR 转文字** | 截图 → 本地 OCR → 文本给模型 | 三平台各一套 OCR 依赖（体积 +3–10 MB）；**OCR 会把屏幕文字带进模型上下文 → 提示注入新入口** | 窄版更省，但要在 07 §9.3 第 6 条基础上加"OCR 文本打标" |

**建议：窄版走 B（OCR），完整版走 A。** 理由：窄版的目标是"证明设备链路能通"，OCR 的依赖与体积都比较可控；视觉输入会碰 provider 契约，应该和能力底座（M7 的 `C5.ai`）一起设计，而不是在设备里程碑里顺手改协议。

### 9.2 范围表

| 项 | 窄版（Windows only） | 完整版 |
|---|---|---|
| 平台 | Windows 10/11 | + macOS + Linux（X11 直连、Wayland 走 portal） |
| 子能力 | `screen` + `input` | + `ocr` / `window` / `clipboard` |
| 应用白名单 | ✅ 进程映像名 | ✅ 同 |
| 限时会话 + 撤销 | ✅ | ✅ 同 |
| 审计 | ✅（不含内容） | ✅ 同 |
| 让模型看懂屏幕 | **OCR** | 视觉输入（A 路线） |
| 坐标/多显示器 | 单显示器 + 主屏缩放 | 全显示器、负坐标、每屏 DPI |
| 出口标准 | V12–V16 中的 V12/V13/V15/V16 + 一条"手动跑通点按钮" | V12–V16 全绿 + 三平台 |

### 9.3 工作量（粗估，仅供参考）

- 窄版：骨架（gate + grants + 库表 + 4 命令 2 事件 + d.ts）**约 3–4 天**；Windows backend（抓屏 + 注入 + 白名单 + 锁屏检测）**约 3–4 天**；OCR 接线 **约 2 天**；测试与真机 **约 2 天** → **10–12 天**。
- 完整版另需：macOS（屏幕录制授权流程 + 重启提示）、Linux X11、Wayland portal（ScreenCast + RemoteDesktop 两套）、三平台 OCR、多显示器、视觉输入路线 → **再 3–4 周**。

（这两个数字是估算，不是承诺；实际以 S14 的真机结论为准。）

---

## 10. 验收映射（V12–V16 → 测试）

| 验收 | 测试放哪 | 断言要点 |
|---|---|---|
| V12 没声明 → `denied` 且 approver 调用 0 次 | `crates/hbot-code/tests/s3_device.rs` | 计数用 `crates/hbot-agent/src/gateway.rs` 测试里 `approver.count()` 同款（`trait Approver` 在 `crates/hbot-agent/src/approvals.rs:320`）；**不能只断言返回错误码** |
| V13 声明了但设置没开 → `-32005` + `data.toggle` | `crates/hbot-rpc/tests/device_commands.rs` | 断言 `data.toggle == "device.enabled"`，且**不是空对象** |
| V14 到期后立即 `denied`；撤销后新会话起不来 | `crates/hbot-rpc/tests/device_session.rs` | 注入 clock（`crates/hbot-rpc/src/app/ids.rs:66` 的 `ManualClock` 已有）；不要 `sleep` 真等 5 分钟 |
| V15 前台不在白名单 → 拒绝且**零输入** | `crates/hbot-command` 式的真进程测试不适用；用 `FakeBackend` 记录调用 | 断言 backend 的 `input` **未被调用**（不是"返回了错误"） |
| V16 截图/OCR 原文不进日志 | `crates/hbot-redteam/tests/` 新增 `d01`–`d03` | 塞一个明显的哨兵串进假截图/OCR 文本，断言 `logs` 与审计里都不含它 |

**红队归属**：设备能力必须进 `crates/hbot-redteam`（六面里的 D 审批绕过 + F 密钥泄漏都要扩）。设备是最大的新攻击面，不能只有正向测试。

---

## 11. 顺序：与 M7 的关系（待用户拍板）

`M10` 的最小前置链：

1. **定时器骨架**（M7 第 3 步 `C1.timer` 的一部分）—— 没有它，"到期自动结束"只能靠 lazy 检查，而那正是旧项目 `wait_input` 的病。
2. **设置文件读取**（`device.json`）—— 已有 `DesktopSettings` 的写法可抄。
3. **`Capability` 闸的位置** —— `ToolGateway` 第一步现在不存在，要先加空壳（只判定"清单声明"这一层）并把现有 8 个工具全部登记为"无能力要求"。

所以有两个选择：

| 选择 | 何时能跑 | 风险 |
|---|---|---|
| **A. 等 M7**（推荐） | M7 之后 | 晚，但骨架长在正地方 |
| **B. 现在做窄版** | 10–12 天后有 demo | 定时器与 Capability 闸会被设备需求"推着长"，可能与 M7 的设计打架，届时返工 |

如果选 B，**把第 1、3 项写成"临时实现 + 明确 TODO 迁移点"**，不要假装它们是 M7 的最终形态。

---

## 12. 明确不做（避免以后被当成遗漏）

| 不做 | 为什么 |
|---|---|
| `device.process`（启动/杀进程） | 与 `run_command` 完全重叠。同一种能力开两个口子，等于让用户以为关掉了一个就安全了 |
| `UIAccess`（绕 UIPI） | 需要代码签名证书 + 装进安全位置，且仍进不了 system IL。收益小、门槛高 |
| UI Automation（读界面树） | 比像素稳，但每个应用的可访问性实现质量不一（游戏基本没有）。留给"完整版之后再评估" |
| 设备会话嵌套（一个 bot 开两个） | "一键撤销"与审计都会变复杂（07 §13 问 7） |
| 跨机设备控制（云上 bot 点你本地的鼠标） | 那是另一个安全模型（要在用户机器上跑受控 agent），见 07 §13 问 8 |
| 手机端设备能力 | Play 禁自主型自动化（只能侧载）、iOS 无注入 API。见 08 §4.6 |

---

## 13. 未决问题

继承 07 §13 中与设备相关的三条，加四条新的：

1. **OCR 用本地引擎还是视觉模型？**（07 §13 问 9）本文 §9.1 建议窄版本地 OCR，但**本地 OCR 的引擎选型没定**（Windows 自带 `Windows.Media.Ocr` 可用且零依赖；跨平台的 `tesseract` 要带语言包 10–20 MB）。倾向：窄版先用 `Windows.Media.Ocr`，完整版再评估。
2. **`device.input` 的前台白名单默认值**（07 §13 问 6）：空（全拒，安全但每次要配）还是"当前前台应用"（好用但等于默认放行一个）？倾向空。
3. **截图存哪**：只在内存里传给 OCR 就丢，还是落 `.hbot/screenshots/` 供用户回看？落盘要加保留策略与清理（否则是磁盘与隐私双坑）。倾向：不落盘，但**Agent 看到的那张图在 UI 里可回看**（走事件流，不进库）。
4. **`device_request` 与现有 `ApprovalRequest` 是否合并**：两者都是"Agent 请用户点一下"，但语义不同（一个是单次动作、一个是限时会话）。倾向不合并 —— 合并会让审批 UI 长出两种完全不同的卡片。但用同一个 critical 通道。
5. **多显示器时的坐标系**：`screen()` 返回"当前主屏"还是"虚拟桌面全图"？注入坐标是绝对还是相对某屏？倾向：默认主屏 + 显式 `monitor` 参数，绝对坐标。
6. **无头模式下谁批准设备会话**：`hbot serve` 没有 UI。是拒绝（最安全）、还是从 `device.json` 里预置一个"永久允许"（危险）？倾向**拒绝**：无头模式没有人在场，设备能力应该返回 `HOST_LIMIT` 并说明"需要桌面端批准"。
7. **`ctx.device` 的调用是否要限速在 gate 层还是 backend 层**（07 §9.3 给了 20 次/秒、2 帧/秒的数字）：倾向 gate 层，因为 back 层可能被绕过（未来加第二个 backend 时）。

---

## 14. 与现有文档的关系

- `docs/07` §9：能力模型、三道闸的**产品语义**、平台事实与来源（本文是它的实现稿）。
- `docs/08` §4.6、§9：手机端结论与运行位置（设备能力在云端/手机上都不可用）。
- `ADR-0005`：为什么默认全关、为什么按子能力拆（决策与备选）。
- `docs/02` §11.1：威胁模型的两行（C6 与远程中间人）。
- `docs/04`：里程碑 `M10` 的出口标准（本文 §10 是它的细化）。