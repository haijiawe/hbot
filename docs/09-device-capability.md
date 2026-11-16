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

**已落地**（`crates/hbot-store/src/migrations/0004_device.sql`，2026-11-17）。下表是实际列，不是设想：

| 列 | 类型 | 说明 |
|---|---|---|
| `id` | TEXT PK | UUIDv4（调用方生成，存储层不造 ID） |
| `bot_id` | TEXT NOT NULL | 归属 bot，`REFERENCES bots(id) ON DELETE CASCADE`（删项目→级联删会话，不留孤儿授权） |
| `state` | TEXT NOT NULL | `requested` / `active` / `expired` / `revoked` / `denied`（`none` 不落行——"没有行"就是 none） |
| `sub_json` | TEXT NOT NULL | JSON 数组：`["screen","input"]`（**不含**未授权的；`[]` = 一个都没有） |
| `allowed_apps_json` | TEXT NOT NULL | JSON 数组；`[]` = 全拒 |
| `granted_at` | INTEGER NULL | unix 毫秒（`requested` 态可能还没批） |
| `deadline` | INTEGER NULL | unix 毫秒；默认 5 min，上限 30 min |
| `ended_at` | INTEGER NULL | 结束时刻；`NULL` = 还没结束 |
| `created_at` | INTEGER NOT NULL | unix 毫秒，**由调用方注入**（存储层不读时钟） |

索引：`idx_device_sessions_bot_created ON device_sessions (bot_id, created_at DESC)`。
`latest_device_session` 的读法是 `WHERE bot_id=?1 ORDER BY created_at DESC, id DESC LIMIT 1` ——
`id DESC` 只在同毫秒时做**确定性定序**，不是时间序（UUIDv4 不含时间）。

> **与初稿的两处差异（实现时定的，已改文档而不是改代码）**：
> 初稿写 `subcapabilities` 逗号分隔 + `allowed_apps` + `ended_reason`，索引 `(bot_id, state)`。
> 落地改成 JSON 两列（解析失败即坏数据，不用猜分隔符）、去掉 `ended_reason`（状态本身已经
> 表达了"为什么结束"）、索引改 `created_at DESC`（服务 `latest` 查询）。**0004 已发布，不可再改**，
> 要调就开 0005。

**为什么落库而不是只在内存**：① 崩溃/重启后必须能判定"上次那个会话已经不算数"，不能靠内存为空来"默认安全"（万一某天有人改成从库里恢复）；② 审计要能回看"上周二那个会话是谁开的、开了多久"。

**已知缺口（如实记下，未修）**：

- **没有部分唯一索引**保证"每 bot 至多一条 `active`"。当前靠上层语义（`device_grant` 先 `end_live_device_sessions` 再插）维持；`latest_device_session` 只取一条，所以真出现第二条 active 就会"活着但没人看得见"。加索引要先改上层，故留给 M10 收口时一起做。
- `state` **没有 CHECK 约束**，存储层也不校验：上层若写入 `"Active"`（大写），这行永远匹配不上 `WHERE state='active'`，撤销/到期都撤不掉它。当前用测试 `active_state_literal_matches_the_core_contract` 钉住"存储层常量 == 核心字面量"，但钉不住"上层传进来的就是它"。
- 排序键是调用方注入的 `created_at` 毫秒：系统时钟回拨 / NTP 跳跃会让"最新"不新。

### 3.2 `device_audit`

| 列 | 说明 |
|---|---|
| `id` | INTEGER 自增 |
| `at` | unix 毫秒（调用方注入的时钟） |
| `bot_id` | TEXT NOT NULL，`REFERENCES bots(id) ON DELETE CASCADE` |
| `actor` | `bot_code` / `agent_tool`（`hbot_core::DeviceActor::as_str()`）——**会话的两个消费者都记** |
| `action` | `screenshot` / `click` / `key` / `move` / `focus` / `clipboard_read` / `clipboard_write` |
| `target` | **只允许**进程名 / 窗口 id / 子能力名，**不含像素、不含剪贴板内容、不含 OCR 文本** |
| `outcome` | `ok` / `denied` / `failed` |

索引：`idx_device_audit_bot_at ON device_audit (bot_id, at DESC)`。

**落地现状（2026-11-17，与初稿的两处差异）**：

1. **审计只在放行时写**，`outcome` 今天恒为 `"ok"`；`denied` 那一列还没有写入点。要记拒绝就得在
   `authorize` 的每条 `Err` 前补一行写库，代价是"拒绝放大写库"（Agent 反复试错的路径会灌满表）。
   取舍未定，**别在 UI 上承诺"能看到被拒绝的操作"**。
2. **`action` 写的是能力名**（`screen`/`input`/`window`/`clipboard`），不是动作名（`screenshot`/`click`…）：
   `hbot_core::DeviceGate` 的签名只带"能力 + 目标"，拿不到"具体是点击还是输入文本"。动作级审计要等
   闸的签名扩出 `action` 字段（契约变更），或者由后端在 `DeviceBackend::input` 里补一行。

**审计不记内容**（07 §9.3 第 5 条）：记了内容，日志本身就成了新的泄漏面 —— 密码框的像素、剪贴板里的 token 都会一直躺在库里，被导出、被 UI 展示、被喂进模型上下文。列清单由 `device_audit_rejects_content_columns`（`PRAGMA table_info` + 禁用子串表）钉死，**以后不许加** `content`/`text`/`png`/`image`/`ocr`/`clipboard`/`payload`/`bytes`/`pixel`/`blob`/`data` 之类的列。

> **已知缺口**：初稿有 `session_id` 外键，落地没有（只留 `bot_id`）。后果是"这次点击属于哪个授权"只能靠 `at` 时间窗猜。要按会话回放得开 0005 加列 —— 现在不需要，但**别在 UI 上承诺"按会话查审计"**。

保留策略：随项目 DB，不做单独裁剪（与现有 `logs` 一致）；量级是每会话最多 10000 条（07 §9.3 的 `maxActionsPerSession`）。

---

## 4. 三层闸的落点

### 4.1 第一层：清单声明（Agent 写、你看得见）

**已落地**（2026-11-17）：`crates/hbot-code/src/manifest.rs` 的 `Permissions` 原有 `http: Vec<String>`，现在多了 `device: Option<DevicePermissions>`：

```rust
pub struct DevicePermissions {
    pub sub: Vec<DeviceCapability>,  // 已解析成领域枚举，不是裸字符串
    pub apps: Vec<String>,
}
```

清单写法：

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

形状（**已落地，扁平、不是嵌套在 `device` 键下**；字段名就是 `DeviceSettings` 的 `camelCase`）：

```json
{
  "enabled": true,
  "capabilities": ["screen", "input"],
  "allowedApps": ["PathOfExile.exe", "Notepad.exe"],
  "defaultMinutes": 5,
  "maxMinutes": 30,
  "maxActionsPerSession": 10000
}
```

- `enabled` 默认 `false`（与 `DesktopSettings.autostart` 同款产品决策：首次启动不许偷偷开）。
- `capabilities` 空 = **全拒**（不是全放）。
- `allowedApps` 空 = **没有设上限**（**不是**全拒）—— 语义与 `capabilities` 故意相反，理由：
  能力是"开不开这一类"，开了一类就整类可用，所以空集必须全拒；应用是在"用户已经点头的那批"
  里再收窄的一层上限，若空集也当全拒，用户就得在 `device.json` 与授权对话框里各抄一遍同一份
  名单才能用一次输入，最后只会把名单写宽。真正的白名单是会话那一份（`device_grant` 的
  `allowedApps`，必填、非空）。
- 读不到/坏 JSON → 退回默认（全关）并 warn，启动不许失败（沿用 `DesktopSettings::load` 的容错口径）。
- 未知字段忽略；未知能力名只丢那一条（不整份作废）；`maxMinutes` 硬上限 30，写 999 取 30 并 warn。
- **写入入口（2026-11-17 落地）**：`device_settings_get` / `device_settings_set` 两条命令 + `device.settings` 事件。
  - `set` 是**全量覆盖**不是补丁；顺序固定为「校验 → 原子落盘 → 改内存 → 广播 → 返回新设置」，
    校验不过一律 `-32002` 且**磁盘与内存都不动**，落盘失败 `-32006` 且**内存也不动**（内存永远晚于磁盘）。
  - 这两条命令**不 `ensure_bot`**：宿主设置与 bot 无关，干净安装也要能读默认值。
  - 能力名走 `DeviceCapability` 的 parse/formater 反查，**rpc 里没有第二份字符串表**（手抄表迟早与 core 漂移）。
- **watcher**：没有引入 `notify`（会破 `xtask deps` 与体积预算），复用既有的 1 秒 `spawn_expiry_tick()`
  比 `device.json` 的 **mtime**；变了才 reload + 广播。判据是"先记账再读文件"，所以写坏/删掉文件
  只会被处理**一次**（退回全关 + warn），不会每秒重放事件。命令路径是立即生效，外部改文件最多 1 秒。
  mtime 的已知盲点：同一时间戳内连写两次、或文件系统不更新 mtime，那次改动不会被重载。
- **设置改变对已开会话的生效范围**：会话**时长与子能力不追溯**（那是用户在两个时刻做的两次决定）；
  但 `allowedApps` 与总开关的**收紧在下一次设备调用就生效**（`authorize`/`host_ceiling_allows` 每次重查），
  **放宽不追溯**，只影响之后开的会话。

### 4.3 第三层：运行时会话（撤销要立刻生效）

`AppState` 加 `device: Arc<DeviceGrants>`，内部 `Mutex<HashMap<BotId, Grant>>`。

**每一次宿主调用都查这个 `Arc`，不许在 spawn 时把授权快照塞进 isolate。** 这是"一键全局撤销"能成立的前提 —— 否则撤销只对下一个 isolate 生效，用户按下撤销键后鼠标还在动。

### 4.4 判定表（缺一层就拒）

**落地口径（2026-11-17，`crates/hbot-rpc/src/app/device/mod.rs` 的 `authorize`）**：闸门的四步顺序是
宿主总开关 → 清单声明 → 宿主点名的子能力 → 会话；**全部回 `-32002 invalid`**，不是 `-32005 denied`。
理由：这些不是"用户拒绝了这次操作"，而是"当前配置下这个能力根本不成立"，UI 要据此提示去改哪一处配置。

| 清单 | 设置 | 会话 | 结果 | 错误码 | `DeviceError` |
|---|---|---|---|---|---|
| 没声明 | — | — | 拒 | `-32002` | `NotDeclared{capability}` |
| 声明了 | 关 | — | 拒 | `-32002` | `NotEnabled{toggle:"device.enabled"}` |
| 声明了 | 开但子能力缺 | — | 拒 | `-32002` | `NotEnabled{toggle:"device.capabilities"}` |
| 声明了 | 开 | 无会话/已过期 | 拒 | `-32002` | `NotGranted{capability}` / `Expired` |
| 声明了 | 开 | 活跃 | 放行 | — | — |

**应用白名单是三层求交，落点在 `grant`**：`请求 ∩ 清单 permissions.device.apps ∩ 宿主 allowedApps`
（宿主没写上限则不参与求交），交集为空直接 `-32002` 并列出三边。`authorize` 每次调用还会
**再查一遍宿主上限**（`host_ceiling_allows`）——因为设置可能在会话中途被改窄，旧会话必须跟着收紧。

**关键差别**：清单没声明时错误文案里**没有** `toggle`（V12 要求 approver 调用次数 = 0）；声明了但设置
没开时才带上"去打开哪个开关"（V13 要求 `data.toggle` 存在）。两者混同会让用户以为"关掉开关能拦住任意代码"。

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
| `device_status` | `{ botId }` | `DeviceStatus`（**从不返回 `null`**；无会话时是 `state:"none"` 的快照，见 8.4） | `-32001` bot 不存在 |
| `device_grant` | `{ botId, sub, allowedApps?, minutes? }` | `DeviceStatus` | `-32001` bot 不存在；`-32002` 子能力非法 / 三边交集为空 / 设置未开（**设置未开也是 `-32002`，不是 `-32005`**，见 4.4） |
| `device_revoke` | `{ botId }` | `null` | `-32001`；无活跃会话时**成功**（幂等） |
| `device_request` | `{ botId, sub, reason }` | `null`（请求已投递给 UI） | `-32001`；`-32002` 子能力非法 / 清单没声明 |

`minutes` 超上限**不报错**：截到 `maxMinutes` 并 warn（善意超限，收紧而不是拒绝；见 `effective_minutes`）。`reason` 超 500 字符按字符截断。

`DeviceStatus`（`dto/common.rs`，字段全 camelCase）：

```
{ botId, sessionId, state, sub: [String], allowedApps: [String],
  grantedAt, deadline, remainingMs }
```

`state` / `sub` / `allowedApps` 是**开放字符串**，不是枚举（与 `ConversationSummary.kind` 同一个理由：设备能力是产品面最容易继续长的一处）。前端遇到不认识的 `state` 必须原样显示、不认识的 `sub` 必须忽略而不是 throw。

### 8.2 事件（`dto/event.rs`，现有 12 个 → 14 个）

| `type` | 用途 | 通道 |
|---|---|---|
| `device_status` | 会话开始/到期/撤销都发，UI 靠它画指示条 | **critical**（用户必须立刻看到，丢一条等于指示条骗人） |
| `device_request` | Agent 请求授权 → UI 弹卡片 | **critical** |

`device_status` 必须进 `crates/hbot-rpc/src/ws.rs` 的 `classify()` 的 critical 分支（与 `ApprovalRequest`/`DebugPaused` 同级）。理由：这个事件丢了，用户会以为"没有会话在跑"，而鼠标正在被控制。

### 8.3 生成代码面（`hbot.d.ts`）

```ts
interface DeviceApi {
  /** 截主屏。pngBase64 是 base64 字符串（JS 侧没有 Node Buffer）。 */
  screen(options?: { monitor?: number; maxBytes?: number }):
    Promise<{ width: number; height: number; pngBase64: string; monitor: number }>;
  windows(): Promise<Array<{ id: number; title: string; process: string; focused: boolean }>>;
  click(x: number, y: number, button?: 'left' | 'right' | 'middle'): Promise<void>;
  key(text: string): Promise<void>;        // 文本注入（不是剪贴板粘贴）
  keyCombo(keys: string[]): Promise<void>; // ["ctrl","c"]；键名大小写不敏感
  clipboardRead(): Promise<string>;
  clipboardWrite(text: string): Promise<void>;
}
```

两处与初稿不同（实现时定的，已按实现改文档）：

- `png` 是 **`pngBase64: string`** 而不是 `Uint8Array`：QuickJS 里没有 Node 的 `Buffer`，宿主把像素编成 PNG 后以 base64 交给 JS，JS 侧要发出去就放进 `http` 请求体。
- `windows()` 的 `id` 是 **number**（`hbot_core::WindowInfo.id` 是 `u64`，Windows 上是 `HWND as usize as u64`）。实测 HWND 远小于 2^53，JSON number 不会丢精度；**不要**为了"看起来更安全"改成字符串，那会让 `device-win` 的实现和契约打架。

> **隐性契约（实现时撞出来的，2026-11-17）**：`MessageCtx` 里新增**对象属性**（`readonly device: DeviceApi;`）会被两个独立扫描器当成"入站事件字段"——`crates/hbot-code/src/dts.rs` 的 `message_ctx_properties()` 和 `crates/hbot-rpc/tests/event_shape_dts.rs` 的同名函数（后者直接与 `hbot_bot::event_json` 的键集合 `assert_eq!`）。两边都跳过"名字里含空格"的行，所以这里必须保持 `readonly device: DeviceApi;` 这种写法。**把它"规范化"成 `device: DeviceApi;` 会同时红两个 crate，而报错信息完全看不出原因。**
>
> 同一条精神也适用于防漂移闸门本身：`installed_device_keys()` 扫的是 `let entries: [(&str, Function<'js>); N]` 这张**结构化表**，不是 `object.set("...")` 的行首字面量——后者会被 rustfmt 的 `chain_width` 折行，静默漏掉键（第一版就这么漏了 `clipboardRead`/`clipboardWrite`）。

**没在清单里声明的子能力，这个对象上就不出现对应方法**（V1：生成 d.ts 后断言不含该符号）。`dts.rs` 的 `host_bindings_match_the_module_and_the_dts` 四向防漂移测试必须扩到 `device`，否则 d.ts 会承诺运行时不存在的符号。

### 8.4 写 UI 时被问出来的六件事（口径在此，别再各自猜）

1. **`device_status` 永远不会 `data: null`**（初稿写错过，已改）。UI 无条件按对象处理：`state:"none"` 就是"从没有过会话"，此时 `sessionId`/`grantedAt`/`deadline`/`remainingMs` 为 `null`，但 `sub`/`allowedApps` 是数组（设置页要拿它画当前配置）。
2. **没有 `device_deny` 命令，而且不需要**：`device_request` 只发一条事件、**不创建任何服务端状态**（`device-cmd` 的实现约束）。所以 UI 的「拒绝」= 清掉本地 `pending`，服务端无事可做。副作用是 `DeviceSessionState::Requested` 这个变体今天**没有任何代码会写**——它是为 M7「请求也要落库、可跨重启恢复」预留的，别在别处假装它有用。
3. **UI 拿不到 bot 列表**（真缺口）：四条命令都要 `botId`，界面只能取当前运行中的那一个。要支持多 bot 授权得让 UI 用现成的 `bot_list` 命令（它早就有），并在 `ipc.ts` 补上——不在本轮范围，记在 §11.1 的待办里。
4. **`-32005` 的 `data.toggle` 被前端丢掉了**：`IpcError` 只保留 `message`，所以「设置没开」无法给"去打开设置"的跳转按钮，只能显示一行错误。要修得让 `IpcError` 保留 `data`（跨所有命令的改动），不在本轮。
5. **`allowedApps` 的大小写口径**：**比较前双方都 `trim` + 转小写**（`device-win` 取前台进程的映像名后也转小写），UI 侧**原样显示用户写的东西**，不回写、不归一化。这样 `PathOfExile.exe` 与 `pathofexile.exe` 都能匹配，而用户看到的还是自己写的。
6. **`device.status` 不带 `requestId`**：因为请求不落状态（见第 2 条），"这条状态是否消化了那个待处理请求"由前端在 `device_grant` 成功时自己清 `pending`。不要试图从事件里反推。

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
| 子能力 | 五类全有（`screen`/`ocr`/`input`/`window`/`clipboard`），OCR 走本地引擎、按窗口 OCR 已通 | 同（另加跨平台 OCR 与视觉输入） |
| 应用白名单 | ✅ 进程映像名 | ✅ 同 |
| 限时会话 + 撤销 | ✅ | ✅ 同 |
| 审计 | ✅（不含内容） | ✅ 同 |
| 让模型看懂屏幕 | **OCR**（含中文，实测） | 视觉输入（A 路线） |
| 坐标/多显示器 | 全显示器可枚举、可按序号抓整屏；注入坐标用虚拟桌面绝对坐标 | 每屏 DPI 归一、跨屏拼图、按窗口截图 |
| 出口标准 | V12–V16 中的 V12/V13/V15/V16 + 一条"手动跑通点按钮" | V12–V16 全绿 + 三平台 |

### 9.3 工作量（粗估，仅供参考）

- 窄版：骨架（gate + grants + 库表 + 4 命令 2 事件 + d.ts）**约 3–4 天**；Windows backend（抓屏 + 注入 + 白名单 + 锁屏检测）**约 3–4 天**；OCR 接线 **约 2 天**；测试与真机 **约 2 天** → **10–12 天**。
- 完整版另需：macOS（屏幕录制授权流程 + 重启提示）、Linux X11、Wayland portal（ScreenCast + RemoteDesktop 两套）、三平台 OCR、每屏 DPI 归一与跨屏拼图、按窗口截图、视觉输入路线 → **再 3–4 周**。
- **实际进度已超出窄版的估算**（2026-11-17）：OCR、按窗口 OCR、中文、多显示器、限速、设置读写都已落地，窄版估的 10–12 天里"只做 screen + input"那半边被提前拆掉了。

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

## 11. 顺序：与 M7 的关系（用户已选 B，2026-11-17）

`M10` 的最小前置链：

1. **定时器骨架**（M7 第 3 步 `C1.timer` 的一部分）—— 没有它，"到期自动结束"只能靠 lazy 检查，而那正是旧项目 `wait_input` 的病。
2. **设置文件读取**（`device.json`）—— 已有 `DesktopSettings` 的写法可抄。
3. **`Capability` 闸的位置** —— `ToolGateway` 第一步现在不存在，要先加空壳（只判定"清单声明"这一层）并把现有 8 个工具全部登记为"无能力要求"。

| 选择 | 何时能跑 | 风险 |
|---|---|---|
| A. 等 M7 | M7 之后 | 晚，但骨架长在正地方 |
| **B. 现在做窄版（已选）** | 10–12 天后有 demo | 定时器与 Capability 闸会被设备需求"推着长"，可能与 M7 的设计打架，届时返工 |

用户 2026-11-17 说「开始吧」，按 **B** 开工。本轮的处置原则（写下来是为了将来不返工时能一眼看出当时怎么想的）：

- **不写"临时实现"**。需求被拆成两部分：本次做"形式正确、范围窄"的实现；M7 的 `C1.timer` / `Capability` 闸到 M7 时**替换实现而不是替换形状**。
- 具体地：到期结算用 `AppState` 里一个 1 秒 tick（`docs/09` §7 已定"由定时器结算，不 lazy 检查"）；M7 的 timer 落地后，这个 tick 应变成"往统一调度器注册一个任务"，而不是删掉——**所以 tick 的入口必须是一个可被替换的 `fn sweep_expired(&self)`**，不是内联在 `AppState::open` 里。
- `ToolGateway` 的 `Capability` 第一步**当时判"本次不做"**，2026-11-17 当天做了（见 §11.1）：设备能力有两个消费者（bot 代码走 `ctx.device`、Agent 走 `ToolGateway`），只做前者等于"限时会话与撤销管不住 IDE 里的 Agent"。落地时是**加了一个实现者**（`ManifestCapabilityGate`），`DeviceGate` trait 一个字没改 —— 这正是当初留形状而不是留坑的回报。

### 11.1 落地状态（2026-11-17）

| 层 | 状态 | 落点 |
|---|---|---|
| 契约（DTO/事件/错误码/trait） | **已冻结并提交**（`da97da6`） | `crates/hbot-core/src/device.rs`、`crates/hbot-rpc/src/dto/{common,command,event}.rs` |
| 存储（两表 + 6 方法） | **已完成** | `crates/hbot-store/src/migrations/0004_device.sql`、`src/repositories/device.rs` |
| 宿主闸 + `ctx.device.*` | **已完成**（`hbot-code` 158 passed） | `crates/hbot-code/src/device/`、`src/runtime/device_api.rs` |
| 设置 + 四条命令 + 到期 tick | **已完成**（`hbot-rpc` lib 253 passed） | `crates/hbot-rpc/src/app/device/`、`src/app/commands/device.rs` |
| Windows 后端 | **已完成**（34 passed / 2 ignored，clippy 0） | `crates/hbot-device/` |
| IDE 面板 + 授权弹窗 | **已完成**（web 30 文件 / 230 用例） | `apps/web/src/features/device/`、`src/stores/device-store.ts` |
| 注入沙箱（装配） | **已完成**（2026-11-17） | `SandboxHandler::with_device(gate, backend)`，在 `crates/hbot-rpc/src/app/commands/bots.rs` 的 `start_bot` 里调；每次新建 isolate 时 `set_device` + `set_device_apps(device_apps(project_root))` |
| 切项目时刷新清单 | **已完成** | `AppState::open_project` 末尾 `self.device.note_project_root(Some(root))` |
| 限速（20 次/秒、2 帧/秒） | **已完成（2026-11-17）** | 判据只有一份：`hbot_core::DeviceRateWindow`（一秒滑动窗口，先判后记，被拒的那次不占配额）。两处各持一个实例：bot 代码在 `hbot-code` 的 `DeviceSlot`（一个 `Sandbox` = 一个 bot），Agent 在 `hbot-rpc` 的 `DeviceRegistry`。参数错误在闸之前返回，所以传错参数不占配额、也不会被限速文案盖住。Agent 侧调用同样吃该会话的 `maxActionsPerSession` 动作配额（与 bot 代码共用一个计数，否则一个 `while` 循环里的 Agent 能一直读到会话到期）。超时已拆成 `HBOT_DEVICE_TIMEOUT`（见 §13 问 8） |
| OCR | **已完成（窄版）**（`hbot-device` 26 lib + 7 api + 14 desktop，clippy 0） | §9.1 的 B 路线：`Windows.Media.Ocr` → 文本。宿主函数 `__hbot.device.ocr`、Agent 工具 `read_screen` |
| OCR 按窗口 | **已完成（2026-11-17）** | `DeviceTarget::Window{process}` → `PrintWindow`/`BitBlt`；判据是「内容全零」不是 `PrintWindow` 的返回值；找不到可读窗口 → `AppNotAllowed{process}`（见 §11.2 ④） |
| OCR 中文 | **已完成（2026-11-17 实测）** | 本机 `["en-US","zh-Hans-CN"]`，画 `你好，世界` → `你 好 ， 世 界`；测试自门控（无语言包/字体回退时打印理由跳过，不伪造通过） |
| 多显示器截图 | **已完成（2026-11-17）** | `ScreenshotRequest::monitor: Some(i)` 抓第 i 块整屏（`CreateDCW(szDevice)`，不是 `GetDC(NULL)`）；序号排序 `primary` → 左上角 → `szDevice`；越界 `HostLimit` 带可用范围；清单由 `hbot_device::displays()` 暴露（**本机只有一块屏，第二块屏只验了对称路径**） |
| 设置写入 + watcher | **已完成（2026-11-17）** | `device_settings_get` / `device_settings_set` / `device.settings`；1 秒 mtime 轮询（不引入 `notify`），坏文件只处理一次（见 §4.2） |
| IDE 设备设置卡片 | **已完成**（web 31 文件 / 246 用例） | `apps/web/src/features/device/settings-card.tsx`；`maxMinutes>30` 与 `defaultMinutes>maxMinutes` 前端就拦，后端仍独立校验 |
| `ToolGateway` 的 Capability 闸 | **已完成**（B2，2026-11-17） | `GatewayStep::Capability` 作第一步；实现者 `app/device/capabilities.rs` 的 `ManifestCapabilityGate`（每次重读清单，不快照） |
| Agent 侧的会话判据 | **已完成**（B2，2026-11-17） | `app/device/agent_backend.rs` 的 `SessionScopedBackend`：撤销/到期对 Agent 立刻生效（见下） |

**装配方必须调两个方法，少一个就是静默失败（`hbot-code` 实测出来的，2026-11-17）**：

| 方法 | 不调的后果 | 为什么签名分成两个 |
|---|---|---|
| `Sandbox::set_device(gate, backend)` | 每次设备调用都报 `HBOT_DEVICE_UNAVAILABLE: 设备能力尚未装配` | 闸与后端都是 `Arc<dyn …>`，与 `Sandbox::spawn*` 的签名解耦（`spawn` 签名冻结不动，`hbot-rpc`/`hbot-bench` 在调） |
| `Sandbox::set_device_apps(apps)` | 设备能力装好了，但 `input` 永远被拒（`HBOT_DEVICE_APP_NOT_ALLOWED`）——**安全方向对，但是静默的** | `Sandbox` 没有 manifest、`CompiledProject` 没有项目根，白名单只能由装配方喂 |

另外两条由实现定下来的事实：

- **设备授权的 bot 身份来自事件 payload 的 `botId`**（`build_event` 里取 `payload.botId`）。手写 payload 的测试/基准里设备能力会报 `HBOT_DEVICE_FAILED: 本次事件没有 botId`。这也是为什么"能构造事件交给 `dispatch` 的路径"等于"能决定对哪个 bot 的会话做授权"。
- **`read-only` 档连闸都不问**（`authorize` 在第一步就返回 `HBOT_DEVICE_NOT_GRANTED`，假 gate 的 `authorize` 调用计数为 0）：因为 `DeviceGate::authorize` 的实现方自己要读 `device.json`、查会话表，那些也是"碰外部资源"。这比"后端没被调"更强。

### 11.2 两个前置落地时改掉了什么（2026-11-17）

**① 定时器骨架** —— 按 §11 的要求做成可替换入口而不是内联：`DeviceRegistry::spawn_expiry_tick()` 里跑一个 1 秒 `interval`（`MissedTickBehavior::Skip`，`Drop` 时置 cancel），每次只调 `sweep_expired()`。M7 的 `C1.timer` 落地时，把"注册一个周期性任务"这一句换掉即可，`sweep_expired` 的判据不用动。

**② `Capability` 闸** —— 从"明确不做"改成"做"，原因是需求把消费者数从 1 变成 2：

| 消费者 | 走哪条路 | 原来的判据 | 现在 |
|---|---|---|---|
| bot 代码 | `ctx.device.*` → `hbot-code` 的 `device::gate` | read-only → 装配 → botId → `DeviceGate` | 不变 |
| Agent | `read_screen` → `ToolGateway` | 只有"清单声明 + 逐次审批" | 多一步 `GatewayStep::Capability`，且执行后端换成 `SessionScopedBackend` |

`GatewayStep::Capability` 是八步里的**第一步**（`能力声明 → 参数校验 → 工具存在 → 沙箱档 → 路径 realpath → 审批 → 执行 → 审计`）。它是**条件性**的：只有 `Tool::required_capabilities()` 非空才推入轨迹，所以既有 8 个工具（`required_capabilities` 全是 `&[]`）的步进断言一条都没改。

**③ Agent 侧的第四层判据（本轮发现并修掉的真实缺口）** —— 只加 `Capability` 步**不够**：那一步只答"项目清单声明了没有"，是**静态**的。撤销与到期是**运行时**状态，只管住 bot 代码那条路，就等于"用户点了撤销，IDE 里的 Agent 还能继续读屏"。修法是不改 `hbot-agent` 的形状，在 `hbot-rpc` 侧包一层：

- `crates/hbot-rpc/src/app/device/agent_backend.rs` 的 `SessionScopedBackend { inner, registry: Weak<DeviceRegistry> }`：每个能力方法第一句先问 `DeviceRegistry::authorize_tool(capability)`，判据顺序 = 设备总开关 → 清单声明 → 宿主设置的子能力 → **只允许无目标能力**（`screen`/`ocr`/`clipboard`；`input`/`window` 一律拒，因为它们必须有窗口目标，而 Agent 无法表达"对哪个窗口"）→ 存在一条 live 且含该能力且未到期的会话 → 写审计。
- **不构造时快照**：`Weak` + 每次重查，撤销后下一次调用就拒（测试 `an_agent_tool_call_uses_a_live_session_and_revocation_stops_the_next_one` 钉住）。
- **审计记"按谁的授权放行"**：`device_audit.bot_id` 外键不可空，所以写那条 live 会话的 bot id、`actor='agent_tool'`，而不是编一个空值。
- 装配缺失 = 什么都不装：`ToolGateway` 没有 `with_device`，`read_screen` 报"没有装配设备后端"。这也是 `AppState::open` 里 `device` 必须**先于** `agent` 构造的原因。

**④ OCR 的三个实测事实**（写在这里是因为它们决定"OCR 能不能算完成"）：

| 事实 | 证据 |
|---|---|
| 真能认出字 | GDI 画 `HBOT OCR TEST 12345` → 识别得 `H BOT OCR TEST 12345`（引擎把 `HBOT` 拆开，断言用「去空白转大写后包含 `HBOTOCRTEST12345`」） |
| `windows-core` 的 WinRT 工厂缓存会导致 `0xC0000005` | `factory_cache.rs` 把激活工厂缓存在进程级静态表，而 MTA 引用计数按线程；线程退出拆 MTA → 静态表悬空。修法：`CoIncrementMTAUsage` 调一次、**永不 decrement、绝不 `CoUninitialize`** |
| 中文能认（2026-11-17 实测） | 本机语言包 `["en-US","zh-Hans-CN"]`；`CreateFontW` 请求 `Microsoft YaHei`、`GetTextFaceW` 读回实际选中"微软雅黑"（证明画出了中文不是方框）、暗像素 17414 个，画 `你好，世界` → 识别得 `你 好 ， 世 界` |
| **空格是不稳定的** | `HBOT OCR TEST 12345` → `H BOT OCR…`，中文 `你好` → `你 好`。比较一律 squash，不能 `==`（这条是调用方最容易踩的坑） |

OCR 的硬限制（同 `crates/hbot-device/src/lib.rs:33`）：只认系统已装语言包（**没装就 `HostLimit`**，不静默降级）；
图标/艺术字/旋转超过 45° 的文字识别不了；**空串是合法结果**（屏幕上真没字），但后端不许拿空串冒充"识别成功"。

**按窗口 OCR（2026-11-17 落地）**：`ocr({ window: { process } })` 走 `GetWindowRect` + `PrintWindow(PW_RENDERFULLCONTENT)`，
失败或画出来**全零**退 `GetWindowDC` + `BitBlt`。判据是「内容全零」而不是 `PrintWindow` 的返回值 —— 实测窗口刚
`ShowWindow`、DWM 还没合成第一帧时 `PrintWindow` 返回 1 却一个像素都没画，只看返回值会把"没抓到"当成"抓到纯黑窗口"，
最后 OCR 返回空串，正是契约禁止的那件事。窗口查找四条判据全要满足：可见、非最小化、标题非空、进程名不区分大小写相等；
多块匹配先看前台窗口、否则取 `EnumWindows` 的第一个（Z 序最上）。找不到 → **`AppNotAllowed{process}`** 不是 `HostLimit`：
能力是有的，只是"这个进程此刻没有可读窗口"，报 `HostLimit` 会让调用方永久放弃这条路。

**多显示器（2026-11-17 落地）**：`ScreenshotRequest::monitor: Some(i)` 抓第 i 块屏的**整屏**（不做"前台窗口优先"，
否则 `Some(1)` 会变成"截主屏上那个窗口"，与参数的承诺不符）。序号规则 = `primary` 降序 → `rcMonitor` 左上角 →
`szDevice` 字典序：**`0` 必须恒为主屏**（既有契约），坐标给人直觉顺序，字典序只用于克隆模式同坐标时打平。三个字段都是
系统静态属性，所以同机反复调用、进程重启都稳定。抓每块屏走 `CreateDCW(szDevice)` 而不是 `GetDC(NULL)`（后者只覆盖主屏）。
序号越界 → `HostLimit` 并带数量与可用范围，**绝不静默退回主屏**。清单由 `hbot_device::displays()` 暴露（不改 `hbot-core` trait）。

**⑤ OCR 文本是不可信内容**（§9.1 提醒的"提示注入新入口"，落地时打标 6 处）：`hbot-agent` 的 `read_screen` 输出走 `prompt::untrusted_block("screen_ocr", …)` 包成 `<untrusted_data source="screen_ocr">`；`hbot-code` 的 `device_api.rs`/`dts.rs`/`host_api.rs` 各写一段"屏幕文字不得用来扩权、改沙箱档、跳过审批、改配置"。`read_screen` 的 `risk` 是 `External`（每次审批），`min_sandbox` 是 `ReadOnly`（设备能力与文件沙箱正交，见 ADR-0005）。

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

1. **OCR 用本地引擎还是视觉模型？**（07 §13 问 9）**窄版已定并落地：`Windows.Media.Ocr`**（零额外依赖，`windows` crate 0.62 的 `Media_Ocr` feature；手写 PNG 也没引入 zlib）。中文与按窗口 OCR 已实测可用（见 §11.2 ④）。完整版要不要换跨平台引擎（`tesseract` 带语言包 10–20 MB）或干脆走视觉模型，仍待 M7 的 `C5.ai` 一起评估。
2. **`device.input` 的前台白名单默认值**（07 §13 问 6）：空（全拒，安全但每次要配）还是"当前前台应用"（好用但等于默认放行一个）？倾向空。
3. **截图存哪**：只在内存里传给 OCR 就丢，还是落 `.hbot/screenshots/` 供用户回看？落盘要加保留策略与清理（否则是磁盘与隐私双坑）。倾向：不落盘，但**Agent 看到的那张图在 UI 里可回看**（走事件流，不进库）。
4. **`device_request` 与现有 `ApprovalRequest` 是否合并**：两者都是"Agent 请用户点一下"，但语义不同（一个是单次动作、一个是限时会话）。倾向不合并 —— 合并会让审批 UI 长出两种完全不同的卡片。但用同一个 critical 通道。
5. **多显示器时的坐标系**：**已定并落地（2026-11-17）**：`screen()` 默认主屏（`monitor: None` 保持历史语义"先抓前台窗口、失败退主屏"），显式 `Some(i)` 抓第 i 块**整屏**；注入坐标用虚拟桌面绝对坐标（`SM_XVIRTUALSCREEN`），所以跨屏点击本来就是对的。仍欠：**每屏 DPI 归一**（宿主没声明 Per-Monitor V2 manifest 时，150% 缩放下 `rcMonitor` 与 `CreateDCW` 的像素都会被虚拟化，`width/height` 与实际 PNG 尺寸会不符 —— 必须在壳里解决，库做不到）与**跨屏拼一张虚拟桌面图**（未做）。
6. **无头模式下谁批准设备会话**：`hbot serve` 没有 UI。是拒绝（最安全）、还是从 `device.json` 里预置一个"永久允许"（危险）？倾向**拒绝**：无头模式没有人在场，设备能力应该返回 `HOST_LIMIT` 并说明"需要桌面端批准"。
7. **`ctx.device` 的调用是否要限速在 gate 层还是 backend 层**（07 §9.3 给了 20 次/秒、2 帧/秒的数字）——**已定并落地（2026-11-17）：放闸层**，因为后端可能被换、被绕过；实现只有一份 `hbot_core::DeviceRateWindow`，两个装配点（`hbot-code` 的 `DeviceSlot`、`hbot-rpc` 的 `DeviceRegistry`）各持一个实例。两个细节写进了代码注释，别改：① 被拒的那次**不占配额**（`admit` 先判后记），否则一个 handler 循环重试会把窗口彻底堵死；② 限速排在授权**之后**，否则"没授权"会被报成 `RATE_LIMITED`，用户会以为等一秒就好。
8. **`HBOT_DEVICE_HOST_LIMIT` 不再一词两义（2026-11-17 收口）**：它只表示"宿主做不到 / 不支持"（无显示器、无语言包、按窗口 OCR 未实现、非 Windows）。后端调用超时**已经拆成独立的 `HBOT_DEVICE_TIMEOUT`**（`hbot_core::DeviceError::Timeout { ms }`），否则用户看到 `HOST_LIMIT` 会以为"这台机器不支持设备能力"。限速另有独立的 `HBOT_DEVICE_RATE_LIMITED`（带 `retry_after_ms`，文案是"调得太频繁，N 毫秒后再试"）。三种码对应三种补救动作：换机器 / 加预算 / 等一等——不要合并。
9. **`clipboardWrite` 的"不回显"只管住了闸这一侧（第 11 节）**：`crates/hbot-code/src/runtime/device_api.rs` 从不把写入内容放进异常与日志（有测试钉住），但它对后端返回的 `DeviceError::Failed { message }` 是**原样透传**的。所以 `DeviceBackend` 的实现方也不许把剪贴板内容/截图字节塞进 `message` —— 这条约束同时属于 V16 的两侧（闸 + 后端），不只管闸。

---

## 14. 与现有文档的关系

- `docs/07` §9：能力模型、三道闸的**产品语义**、平台事实与来源（本文是它的实现稿）。
- `docs/08` §4.6、§9：手机端结论与运行位置（设备能力在云端/手机上都不可用）。
- `ADR-0005`：为什么默认全关、为什么按子能力拆（决策与备选）。
- `docs/02` §11.1：威胁模型的两行（C6 与远程中间人）。
- `docs/04`：里程碑 `M10` 的出口标准（本文 §10 是它的细化）。