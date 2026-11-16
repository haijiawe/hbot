# S6 桌面与后台运行（M5）

- 状态：**已实现，Windows 上真机验证；Linux/macOS 部署物未在真机运行**
- 日期：2026-11-16
- 写域：`crates/hbot-desktop/**`、`scripts/verify-*.ps1`、`packaging/**`、本文档、`docs/02-architecture.md` §17
- 一句话结论：五件事**代码都写了**，其中「单实例 / 关窗进托盘 / 优雅退出 / 开机自启开关 / 崩溃文件落盘 + 下次启动上报」在 Windows 上有**真进程、真退出码、真注册表、真日志**的证据；托盘三个菜单项的**菜单 id → 动作派发**（隐藏 CLI 入口，不是鼠标）已实测；但**鼠标真的点中菜单项**、托盘图标外观、Linux systemd/logrotate、macOS 全平台、重启后自启、以及「崩溃提示在界面上渲染」仍**没有端到端验证**，逐条列在 §9。

> 本文档遵守一条规矩：**"代码写了"不等于"功能可用"**。下面每个结论后面都跟着它是怎么被验证的，或者明确写「未验证」。

---

## 1. 交付物一览与验证状态

| # | 能力 | 实现位置 | 验证状态 |
|---|---|---|---|
| 1 | 单实例（第二次启动把已有窗口带到前台后 `exit 0`） | `tauri-plugin-single-instance` v2.5.5 + `lib.rs` 的 `.plugin(...)` | **已验证**（`scripts/verify-single-instance.ps1` 全绿；"真的在最前"见 §9） |
| 2 | 关窗进托盘 + 托盘菜单（显示 / 自启开关 / 退出） | `settings.rs` + `shell.rs` + `tauri.conf.json` 的 `app.trayIcon` | **部分验证**：关窗隐藏、第二次启动恢复窗口、`closeToTray=false` 时优雅退出 0 都实测；**三个菜单 id → 动作派发**（`scripts/verify-tray-actions.ps1`，show / autostart / quit 全绿，见 §3.3）；**鼠标真的点中菜单项**未验证（§9-2） |
| 3 | 开机自启（默认关，可切换，幂等） | `autostart.rs` + `cli.rs`（`--set-autostart`）+ `settings.rs` | **已验证**（`scripts/verify-autostart.ps1` 全绿，含注册表/幂等/删除）；Linux `.desktop`、macOS plist 只有单测 |
| 4 | Linux 后台服务部署物 | `packaging/systemd/hbot.service`、`packaging/logrotate.d/hbot`、`packaging/README.md` | **未验证**（从未在真实 Linux 上跑过，`hbot serve` 本身既有且不在本次范围） |
| 5 | 崩溃上报（只写本地文件 + 下次启动提示） | `crash.rs` | **已验证（日志/数据侧）**：真 panic 落盘、`text 脱敏`、下次启动 stderr 提示、`logs_tail` 读到 `source=agent` 的 `error` 行、报告改名 `.reported.log` 全实测（§6.2–§6.3）；**界面是否渲染这一行**未验证（§6.4） |

Windows 真机验证环境：Windows（`x86_64-pc-windows-msvc`），`rustc 1.96.0`，Tauri 2.12.5，WebView2 运行时；构建目录 `E:\hbot-target\dux`，`-j 2`。

---

## 2. 单实例

### 2.1 实现

官方插件 `tauri-plugin-single-instance = "2.5"`，在 `lib.rs` 里**第一个**注册：

```rust
tauri::Builder::default()
    .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
        tracing::info!("检测到第二次启动，把已有窗口带到前台");
        shell::show_main_window(app);
    }))
    .invoke_handler(tauri::generate_handler![dispatch])
    // ...
```

理由（读 vendored 源码确认，不是猜的）：

- 插件在**自己的 `setup` 钩子**里抢命名互斥量 `{identifier}-sim`。Tauri 的初始化顺序是：`Builder::build()` 里先 `initialize_plugins`，**之后**才按 `tauri.conf.json` 创建窗口，最后才跑应用级 `setup`。所以第二个实例在**任何 WebView 创建之前**就被 `exit(0)` 掉了 —— 这正是需求要的「不开第二个数据库写入者、不开第二个 WebView」。
- 第二个实例会 `FindWindowW("{id}-sic", "{id}-siw")` 找到第一个实例的隐藏事件窗口，`AllowSetForegroundWindow(pid)` 后 `SendMessageTimeoutW(WM_COPYDATA, ...)` 把 argv 与 cwd 递过去，然后 `app.cleanup_before_exit(); std::process::exit(0);` —— **退出码 0** 是插件行为，不是我另外补的。
- 第一个实例在自己的 UI 线程上跑回调（`shell::show_main_window`）。

`app.trayIcon` 的托盘由配置驱动（见 §3），所以 `shell.rs` **只往已存在的托盘上挂菜单**，不自己 `TrayIconBuilder`（否则会出现两个图标）。

### 2.2 实测（`scripts/verify-single-instance.ps1`，pwsh 7，EXIT=0）

脚本做的事：隔离子进程的 `APPDATA`/`USERPROFILE` 到临时目录 → 起第一个实例 → **确认主窗口可见且标题是 `hbot`** → 把窗口最小化 → 起第二个实例 → 断言。

```
== 1. 启动第一个实例
  PID: 28508
  [PASS] 第一个实例建出可见主窗口: hwnd=0x3230AD0 标题='hbot'
  [PASS] 主窗口标题是 hbot: 标题='hbot'（期望 'hbot'）
  [PASS] 第一个实例进入了 setup（打了数据目录日志）: stderr 里找 "桌面壳数据目录"

== 2. 先把主窗口最小化（稍后看它是否被恢复）
  [PASS] 窗口已最小化（IsIconic=True）—— 给"带到前台"设好了前置条件
  此刻的前台窗口: 'RelayOne - Google Chrome'

== 3. 启动第二个实例（期望：把第一个带到前台，然后自己退出 0）
  PID: 36736
  [PASS] 第二个实例在超时内退出: 等待 30s；HasExited=True
  [PASS] 第二个实例退出码 = 0: ExitCode=0
  [PASS] 第二个实例没有进入 setup（= 没有第二个 WebView / DB 写入者）: stderr 里不应出现 "桌面壳数据目录"
  第二个实例 stderr 全文（0 字符）:
  [PASS] 第一个实例执行了单实例回调（日志里有"检测到第二次启动"）: 证明第二个进程把启动事件交给了第一个

== 4. 第一个实例仍在，且同可执行文件只剩一个进程
  [PASS] 第一个实例仍在运行: PID=28508
  [PASS] 同可执行文件的进程数 = 1: 实际 1 个（PID: 28508）
  主窗口标题: 'hbot'；进程名: hbot-desktop

== 5. 窗口是否被带到前台
  此刻：前台窗口='RelayOne - Google Chrome'（hwnd=0x2370C32），主窗口 IsIconic=False
  [PASS] 窗口从最小化被恢复（IsIconic 由 True 变 False）: IsIconic=False
  [WARN] 主窗口没能成为前台窗口（Windows 前台锁允许拒绝）: 前台=0x2370C32 主窗口=0x3230AD0 ...
```

**踩过的坑（写脚本时最容易错的一处）**：单实例插件的辅助窗口（类 `com.hbot.desktop-sic`、标题 `com.hbot.desktop-siw`）**是可见窗口**（`SetWindowLongPtrW(..., WS_VISIBLE | WS_POPUP)`），所以「取该进程第一个可见有标题的顶层窗口」会抓到它 —— 然后对它 `ShowWindow(SW_MINIMIZE)` 跟主窗毫无关系，「从最小化被恢复」就假失败。脚本因此必须**按标题精确匹配**（`-WindowTitle`，默认 `hbot`，来自 `tauri.conf.json`），并且**不能**用 `-like '*hbot*'`（`com.hbot.desktop-siw` 也含 `hbot`）。另有一个 PowerShell 陷阱：参数 `$WindowTitle` 会被局部变量 `$windowTitle` 悄悄清空（变量名大小写不敏感），这也是同一轮修掉的。

**关键证据怎么读**：

- 「第二个实例 stderr 全文（0 字符）」是**最硬的一条**：那条数据目录日志在 `setup` 的第一行就打，第二个实例一个字都没打，说明它连 `setup` 都没进 —— 没有第二个 `AppState::open`（SQLite 写者）、没有第二个 WebView。剩下的进程数 = 1 也印证了没有第二个主进程活着。
- 「第一个实例日志里有"检测到第二次启动"」证明回调真的被调用、`show_main_window` 真的执行（窗口从 `IsIconic=True` 变 `False` 只能由它造成）。
- `[WARN]` 那条是**诚实的失败面**，见 §9 第 1 条。

### 2.3 为什么不用「文件锁 / 命名事件」自己写

- 插件已经在做这块最容易做错的部分：并发双启动时的重试（40×25ms）、`AllowSetForegroundWindow` 的交接、`cleanup_before_exit` 里释放锁。自己写一遍只会多一份要维护的竞态代码。
- crate 级依赖即可（`Cargo.toml` 里加一行），**没动根 `Cargo.toml`**，符合写域。

---

## 3. 托盘（关窗最小化 + 菜单）

### 3.1 实现

三块：

1. **托盘本身由配置创建**：`tauri.conf.json` 新增

   ```json
   "trayIcon": {
     "id": "main",
     "iconPath": "icons/32x32.png",
     "iconAsTemplate": false,
     "showMenuOnLeftClick": false,
     "tooltip": "hbot（关闭窗口后仍在后台运行）"
   }
   ```

   读 `tauri-codegen-2.7.4` 的 `context.rs` 确认：`iconPath` 在**构建期**就被嵌进二进制（`CachedIcon`），运行时 `Builder::build()` 里直接建出托盘。所以 `Cargo.toml` 只需要 `tauri = { version = "2", features = ["tray-icon"] }` 这一个 feature（`image-png` 不需要，图标由 codegen 解）。**这也意味着不能同时在 Rust 里再建一个托盘**，否则桌面上会多一个图标。

2. **菜单与关窗行为在 `shell.rs`**（`install()`，在应用级 `setup` 里调用）：
   - `Menu`：`显示主窗口` / 分隔线 / `开机自启`（`CheckMenuItem`，勾选状态取自 `desktop.json`）/ `退出`。
   - `app.tray_by_id("main")` → `set_menu` + `set_tooltip`；拿不到时 `warn` 并退回代码里建托盘（兜底，正常路径不会走到）。
   - `WindowEvent::CloseRequested` → **一律 `api.prevent_close()`**，然后：
     - `closeToTray=true`（默认）：`window.hide()` —— 进程留着，机器人继续跑；
     - `closeToTray=false`：走 `request_quit()`。
   - 左键单击托盘图标（`TrayIconEvent::Click` + `MouseButton::Left` + `MouseButtonState::Up`）→ `show_main_window()`。

3. **优雅退出复用既有路径**（没有新写一条）：

   ```rust
   async fn shutdown(app: &AppHandle) {
       // 取 Arc<DesktopHost> → host.core().await
       core.state.bots().stop_all().await;          // 既有：BotRegistry::stop_all（每台 ≤5s 宽限）
       tokio::task::spawn_blocking(move || {
           let bots = state.store().list_bots()?;
           // 空库：没有写内容可排；有 bot：插一行作为写屏障
           state.append_log(bot.id, "info", "桌面端退出：机器人已停，写队列已排空")?;
       }).await;
       app.exit(0);
   }
   ```

   **为什么一次 `append_log` 就算 flush**：`Store` 只有一个写线程，`submit()` 把操作塞进有界队列后会**阻塞等回执**（`crates/hbot-store/src/db.rs`），因此一次成功返回的写等于「排在它前面的所有写都已落盘」。`hbot-store` 没有公开的 `flush()/close()`，`StoreOp::Shutdown` 只由 `Store` 自己的 `Drop` 发 —— 这是唯一可用的写屏障。
   `logs.bot_id` 有外键，所以空库时没有可写的行，只记日志（`空库（没有机器人），写队列无内容可排`），这是已知的、刻意的行为。

   `request_quit` 用 `AtomicBool` 防重入（连点两次"退出"不会跑两遍 shutdown）。

### 3.2 实测（`scripts/verify-tray-close.ps1`，pwsh 7，EXIT=0）

```
== A. 默认（closeToTray=true）：发 WM_CLOSE，窗口应消失、进程应仍在
  [PASS] 主窗口出现: hwnd=0xD30EB8
  [PASS] 日志确认托盘已建出并挂上菜单: stderr 里找 托盘已就绪 与 source=config
  [PASS] WM_CLOSE 投递成功: PostMessage 返回值
  [PASS] 窗口被隐藏（IsWindowVisible=false）: Visible=False
  [PASS] 进程仍然存活（后台继续跑）: HasExited=False

== B. 第二个实例应把窗口重新显示出来（= 托盘"显示主窗口"同一条路径）
  [PASS] 第二个实例退出码 = 0: HasExited=True ExitCode=0
  [PASS] 窗口重新可见（被带到前台）: Visible=True

== C. closeToTray=false：关窗应优雅退出（停 bot → 写屏障 → exit 0）
  [PASS] 第三个实例的主窗口出现: hwnd=0x8F09D2
  [PASS] WM_CLOSE 投递成功: PostMessage 返回值
  [PASS] 进程自行退出（不是被杀的）: HasExited=True
  [PASS] 退出码 = 0: ExitCode=0
  [PASS] 日志证明走了优雅退出路径: stderr 里找 "桌面端开始优雅退出" 与 "空库（没有机器人）"
```

C 段第三个实例的完整 stderr（说明顺序真的是「先停机器人 → 再排空写队列 → 再退」）：

```
 INFO hbot_desktop::shell: 桌面端开始优雅退出：先停机器人
 INFO hbot_desktop::shell: 空库（没有机器人），写队列无内容可排
 INFO hbot_desktop::shell: 桌面端优雅退出完成
```

A 段日志里的 `托盘已就绪，菜单已挂上 source="config"` 是「配置驱动的托盘真的建出来了、菜单真的挂上去了」的证据（`source="config"` 表示走的是 `tray_by_id("main")` 命中分支，不是兜底分支）。

`WM_CLOSE` 而不是模拟点击，是因为托盘菜单项没法用脚本点（要 `TrackPopupMenu` 级别的 UI 自动化）。`WM_CLOSE` 打的是**同一个** `CloseRequested` 处理器，所以"关窗隐藏"这条真的验到了；「鼠标点菜单」见 §9。

### 3.3 托盘菜单三个动作的派发实测（`scripts/verify-tray-actions.ps1`，pwsh 7，EXIT=0）

真实鼠标点击 Windows 通知区域里的托盘菜单不可靠，所以这里验证的是**菜单 id → 动作**这一段，不是鼠标：第二个进程带隐藏参数 `--tray-action <id>` 启动，单实例插件把 argv 转交给已运行的实例，第一个实例用与 `on_menu_event` **完全相同**的 `shell::handle_menu` + `shell::dispatch_menu_action` 执行。脚本只按结果断言：

```
== A. 托盘动作 show：主窗口隐藏后应被隐藏入口重新显示
  [PASS] 前置条件：窗口已隐藏: Visible=False
  [PASS] show 转发进程退出码 = 0: HasExited=True ExitCode=0
  [PASS] 派发 show 后窗口重新可见（IsWindowVisible=true）: Visible=True
  [PASS] 日志证明动作来自隐藏入口（source=cli / menu_id / action=show）: ...
== B. 托盘动作 autostart：第一次应开（写注册表），第二次应关（删注册表）
  [PASS] 第 1 次 desktop.json.autostart = True: 实际=True
  [PASS] 第 1 次注册表 HKCU Run\hbot 已写入且指向当前 exe: 实际="E:\hbot-target\dux\debug\hbot-desktop.exe" 期望="E:\hbot-target\dux\debug\hbot-desktop.exe"
  [PASS] 第 2 次 desktop.json.autostart = False: 实际=False
  [PASS] 第 2 次注册表 HKCU Run\hbot 已删除: 实际=
== C. 托盘动作 quit：第一个实例应优雅退出（退出码 0）
  [PASS] 第一个实例自行退出（不是被杀的）: HasExited=True
  [PASS] 第一个实例退出码 = 0: ExitCode=0
  [PASS] 日志证明走了优雅退出路径: stderr 找 "桌面端开始优雅退出" 与 "空库（没有机器人）"
```

三个动作的完整日志（第一个实例 stderr）：

```
INFO hbot_desktop::shell: 托盘动作派发 source="cli" menu_id="desktop/tray/show" action="show"
INFO hbot_desktop::shell: 托盘动作派发 source="cli" menu_id="desktop/tray/autostart" action="toggleAutostart"
INFO hbot_desktop::shell: 开机自启已切换 autostart=true
INFO hbot_desktop::shell: 托盘动作派发 source="cli" menu_id="desktop/tray/autostart" action="toggleAutostart"
INFO hbot_desktop::shell: 开机自启已切换 autostart=false
INFO hbot_desktop::shell: 托盘动作派发 source="cli" menu_id="desktop/tray/quit" action="quit"
INFO hbot_desktop::shell: 桌面端开始优雅退出：先停机器人
INFO hbot_desktop::shell: 空库（没有机器人），写队列无内容可排
```

**这条证据的边界（非常重要）**：它验证的是「菜单 id → `MenuAction` → 动作」的派发，**不是**「鼠标点中系统托盘图标/菜单项」这一步；后者仍列在 §9-2，未验证。

---

## 4. 开机自启（默认关）

### 4.1 实现

- 偏好文件 `<data_dir>/desktop.json`，`data_dir` 来自 **`hbot_core::data_dir()`**（不自己拼路径，AGENTS 第 39 条）。字段：`{"autostart": false, "closeToTray": true}`，camelCase，未知字段忽略、缺失字段取默认。
- 写盘用「临时文件 + rename」，对 Windows 上 `PermissionDenied/WouldBlock` 重试 5 次 × 40ms（`rename` 被 Defender/别的句柄拿住会 EPERM，AGENTS 第 7 条）；**不写 BOM**（serde_json 会拒收）。
- 平台实现（`autostart.rs`）：
  - **Windows**：`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 的 `hbot` 值；数据是**带引号**的 exe 绝对路径（装到 `C:\Program Files\...` 后路径有空格，Windows 会在空格处切开）。`windows-registry` crate（微软官方），**不碰 HKLM、不要管理员**。
  - **Linux**：`~/.config/autostart/hbot.desktop`（尊重 `XDG_CONFIG_HOME`），desktop-entry 规范，`Exec` 按规范转义（`"` `` ` `` `$` `\` 加反斜杠、`%` → `%%`）。
  - **macOS**：`~/Library/LaunchAgents/com.hbot.desktop.plist`（`RunAtLoad`，不设 `KeepAlive`）。
  - 关闭 = 删值 / 删文件；`NotFound` 视为成功 ⇒ **幂等**。
- **切换入口有两个，走同一份函数**：托盘里的 `开机自启` 勾选项，以及命令行 `hbot --set-autostart on|off|toggle`（`cli.rs`，无 GUI，脚本可驱动）。两者都是「先改系统、再写 desktop.json」，写盘失败会把系统状态**回滚**到原样并弹错误对话框。
- 首次启动**不会**自动开：`desktop.json` 默认 `autostart=false`；只有 `reconcile_autostart` 在「系统里已有、偏好是关」时**采纳系统状态**（把偏好改成开，而不是偷偷删掉用户的注册表项），以及「偏好是开、系统里没有」时补写（失败则把偏好翻回关）。

### 4.2 实测（`scripts/verify-autostart.ps1`，pwsh 7，EXIT=0）

脚本会**备份并在结束时还原**真实的 HKCU 值与 `desktop.json`（所以它动的是真注册表，跑完不留痕）。

```
== 1. 关闭自启，并把状态记成"关"
  [PASS] 关闭后注册表里没有 hbot 值: 读到: ''
  --print-settings: {"dataDir":"C:\\Users\\Administrator\\AppData\\Roaming\\hbot",...,"systemAutostart":"disabled"}

== 2. 开启自启
  [PASS] 开启后注册表里有 hbot 值: 读到: '"E:\hbot-target\dux\debug\hbot-desktop.exe"'
  [PASS] 值的数据是加引号的 exe 路径: "E:\hbot-target\dux\debug\hbot-desktop.exe"

== 3. 幂等性：再开一次
  [PASS] 枚举 Run 键，hbot 只有一条: 匹配条数: 1

== 4. desktop.json 内容
  [PASS] desktop.json 存在: C:\Users\Administrator\AppData\Roaming\hbot\desktop.json
  [PASS] autostart 字段是 true / closeToTray 字段存在且是 true
  [PASS] 文件没有 BOM（serde_json 会拒收 BOM）: 首字符不是 U+FEFF

== 5. --set-autostart toggle 应当把开翻成关
  [PASS] toggle 之后注册表里没有 hbot 值: 读到: ''

== 6. 关闭自启（幂等：再关一次也不报错）
  [PASS] 连关两次都退出码 0: ExitCode=0/0

== 7. 参数处理
  [PASS] --help 打印用法 / [PASS] 未知参数以退出码 2 结束（不会误开 GUI）: ExitCode=2
```

`--print-settings` 的输出（供排查用，也是开关状态的单一真源）：

```json
{"dataDir":"C:\\Users\\Administrator\\AppData\\Roaming\\hbot",
 "execPath":"E:\\hbot-target\\dux\\debug\\hbot-desktop.exe",
 "settings":{"autostart":false,"closeToTray":true},
 "settingsPath":"C:\\Users\\Administrator\\AppData\\Roaming\\hbot\\desktop.json",
 "systemAutostart":"disabled"}
```

`systemAutostart` 是**去系统里实际读的**（注册表/文件），不是回显偏好文件 —— 这样「偏好说开、系统里其实没有」能被看出来。

Linux/macOS 的实现只有单测（`desktop_entry_quotes_paths_with_reserved_characters`、`launch_agent_is_valid_xml_with_the_expected_keys` 等），**没在真机上写过一个文件**。

---

## 5. Linux 后台服务（部署物，不改 `serve`）

- `packaging/systemd/hbot.service`：`Type=simple`、`ExecStart=/usr/local/bin/hbot serve --data-dir /var/lib/hbot`、`Restart=on-failure`、`RestartSec=5`、`User=hbot`/`Group=hbot`、`StateDirectory=hbot`、`KillSignal=SIGTERM`、`TimeoutStopSec=30`，以及 `NoNewPrivileges=true`、`ProtectSystem=strict`、`ProtectHome=read-only`、`PrivateTmp=true` 等 hardening。
- `packaging/logrotate.d/hbot`：`weekly` / `rotate 4` / `copytruncate`，只覆盖 `/var/lib/hbot/logs/*.log`（**崩溃报告**）；运行日志走 journald，由 journald 自己转。
- `packaging/README.md`：建用户、装 unit 与 logrotate、`systemctl enable --now`、`journalctl -u hbot -f`、卸载步骤，以及**显式声明「这套部署物从未在真实 Linux 主机上运行过」**。

**hardening 与命令隔离的冲突（没有盲抄，写在 README 的表里）**：

| 选项 | 对 bot 的实际影响 |
|---|---|
| `ProtectSystem=strict` | 整个文件系统只读，**只有** `StateDirectory`（`/var/lib/hbot`）可写 ⇒ 用户项目必须待在 `/var/lib/hbot` 下，否则 Agent 写文件会 `EROFS` |
| `ProtectHome=read-only` | `$HOME` 只读 ⇒ 工具链缓存（cargo/npm）必须重定向到可写目录，否则命令隔离里的缓存写入会失败 |
| `PrivateTmp=true` | `/tmp` 变成命名空间私有 ⇒ 宿主上看不到 bot 的临时文件（排查时要进 `systemd-private-*`） |
| `NoNewPrivileges=true` | 任何需要 sudo/提权的工具直接失效 |
| `CapabilityBoundingSet=`（空） | 绑不了 <1024 端口（`hbot serve` 默认端口 >1024，没问题） |

刻意**没加**的：`MemoryDenyWriteExecute`（会让 JIT/某些运行时崩溃）、`RestrictAddressFamilies`（bot 要连 QQ 网关与 LLM API，白名单一收紧就断）、`DynamicUser`（数据目录 owner 会漂移，`StateDirectory` 的属主与 `User=hbot` 不一致时起不来）。

`hbot serve` 本身**不在本次范围**：它已经能跑（`crates/hbot-cli` 的 `serve` 子命令，stdout 打一行 ready JSON，`SIGTERM` 后优雅关闭），我只产出部署物。

---

## 6. 崩溃上报（只做本地）

### 6.1 实现

`crash.rs`，三件事：

1. **panic hook**（`install()`，`run()` 里在 `init_tracing()` 之后立刻装）：
   - 先用 `take_hook()` 拿到原 hook，再 `set_hook`；新 hook 里 `catch_unwind` 包住整个写文件过程（写崩溃报告本身 panic 不能把进程再搞死一次），最后**永远调用原 hook**（保持默认的 stderr 输出与退出行为）。
   - 文件名 `<data_dir>/logs/crash-YYYYMMDDTHHMMSSZ.log`（UTC，**不含冒号** —— Windows 文件名不允许）。时间格式化是手写的（不想为一个时间戳引 chrono）。
   - 内容：标题（"本机文件，未上传"）、时间、版本、平台、线程名、panic 文本、位置、backtrace（`Backtrace::force_capture()`）。**不 dump 环境变量**。
   - **脱敏**：整份文本过一遍既有的 `hbot_rpc::app::agent::log::redact_text`（它组合了 `redact_bearer` / `redact_named_values` / `redact_hex_runs`）。
2. **下次启动时提示**（`report_pending()`，在核心装配完成、事件泵挂好之后）：
   - 找 `logs/` 下最新的、没被标记过的 `crash-*.log`；
   - 先 `tracing::error!(path=..., "{line}")` 打到 stderr（**空库也能看到**），`line` = `上次运行异常退出：<panic 摘要>（报告：<路径>）`；
   - 通过 `hbot_rpc::app::agent::log::StoreRunLogSink` 写 `logs` 表：`level='error'`、`bot_id=NULL`、`source='agent'`（迁移 `0003_agent_logs.sql` 已把 `logs.bot_id` 改为可空，并新增 `source` 列）；
   - 最多等 5s（50 × 100ms）轮询 `Store::list_logs(None, 100)` 确认这一行已落库；确认成功才把文件改名成 `crash-*.reported.log`，失败则保留文件下次重试（不假装上报成功）；
   - **不再发 `Event::RunLog`**：该 DTO 仍要求 `bot_id: BotId`，崩溃场景没有 bot 行可指。
3. **不联网**：没有任何 HTTP 调用、没有遥测 SDK、没有 Sentry。

### 6.2 实测证据（真 panic，不是我构造的）

验证托盘 C 段时踩到一个环境冲突（前一个实例被 `taskkill` 后 WebView2 用户数据目录还占着），新实例在 `setup` 阶段真的 panic 了 —— 于是拿到了**真实 panic 被 hook 捕获并落盘**的证据：

```
ERROR hbot_desktop::crash: 已写入崩溃报告 path=...\appdata\hbot\logs\crash-20261116T014904Z.log
thread 'main' (20356) panicked at ...\tauri-2.12.5\src\app.rs:1444:11:
Failed to setup app: runtime error: failed to create webview: WebView2 error:
WindowsError(Error { code: HRESULT(0x800700AA), message: "请求的资源在使用中。" })
```

落盘文件 12,103 字节，开头是：

```
hbot 崩溃报告（本机文件，未上传）
时间: 20261116T014904Z (1794793744743)
版本: 0.1.0
平台: windows x86_64
线程: main
panic: Failed to setup app: runtime error: failed to create webview: ...
位置: ...\tauri-2.12.5\src\app.rs:1444:11
backtrace:
   0: std::backtrace_rs::backtrace::win64::trace
             at /rustc/<redacted>/library\std\src\....\backtrace\src\backtrace\win64.rs:85
   ...
  40: hbot_desktop::run
             at .\crates\hbot-desktop\src\lib.rs:393
  41: hbot_desktop::main
             at .\crates\hbot-desktop\src\main.rs:4
```

（`/rustc/<redacted>/` 是**脱敏的副作用**：`redact_hex_runs` 把 rustc 的 40 位 commit hash 当成"疑似密钥"抹了。功能上无害，可读性上略损失，记在这里以免下次有人以为文件被改坏了。）

### 6.3 崩溃两次启动端到端实测（`scripts/verify-crash-report.ps1`，pwsh 7，EXIT=0）

`HBOT_TEST_PANIC_ON_START=1` 只在**同时满足**「`debug_assertions` 编译期」+「显式环境变量」时触发（见 `lib.rs` 的 `maybe_trigger_test_panic`），release 构建里这个函数体被 `#[cfg(debug_assertions)]` 整个编掉。脚本用隔离的 `APPDATA`/`USERPROFILE` 跑两次启动：

```
== A. 崩溃第一次启动（HBOT_TEST_PANIC_ON_START=1）
  [PASS] 进程已退出（panic 没有被吞掉）: HasExited=True
  [PASS] 退出码 = 101（Rust panic 默认）: ExitCode=101
  [PASS] crash-*.log 已落盘: 未上报报告数: 1
  [PASS] 文件名形如 crash-YYYYMMDDTHHMMSSZ.log（无冒号）: crash-20261116T092019Z.log
  [PASS] 报告含稳定标记 S6-CRASH-E2E: 报告里查找 marker
  [PASS] 报告里密钥被写成 <redacted>: 报告里查找 api_key=<redacted>
  [PASS] 报告不含测试密钥原文: 报告里不得出现 SecretLiteral

== B. 第二次正常启动（不设 HBOT_TEST_PANIC_ON_START）
  [PASS] stderr 出现「上次运行异常退出」+ 标记: ...
  [PASS] stderr 出现「已上报上次崩溃（logs.source=agent）」: ...
  [PASS] 原报告改名为 *.reported.log: reported 文件数: 1

== C. --logs-tail 50：logs 表里必须查到 source=agent 的 error 行
  [PASS] --logs-tail 进程退出码 = 0: HasExited=True ExitCode=0
  [PASS] logs_tail 里找到崩溃行: 匹配行数: 1
  [PASS] 该行 level = error: level=error
  [PASS] 该行 source = agent（bot_id=NULL 的 Agent 行）: source=agent
  [PASS] 该行同样不含测试密钥原文: logs_tail 行检查 SecretLiteral
  [PASS] 第一个实例收到 quit 后自行退出: HasExited=True ExitCode=0
```

A 段报告里的 panic 行（真实落盘内容）：

```
panic: HBOT_TEST_PANIC_ON_START=1：启动期自检 panic（marker=S6-CRASH-E2E；api_key=<redacted>
位置: crates\hbot-desktop\src\lib.rs:491:13
```

C 段 `--logs-tail 50` 的返回片段：

```json
[{"at":1794820820500,"level":"error","line":"上次运行异常退出：panic: HBOT_TEST_PANIC_ON_START=1：启动期自检 panic（marker=S6-CRASH-E2E；api_key=<redacted> / 位置: crates\\hbot-desktop\\src\\lib.rs:491:13 / ...（报告：...\\logs\\...reported.log）","source":"agent"}]
```

这证明：崩溃文件**真的落盘**并经过脱敏；第二次正常启动**真的发现并上报**到 `logs` 表（`source=agent`、`bot_id=NULL`、`level=error`），且 `logs_tail`（`bot_id=None` 的全项目视图）**真的能读到**；最后文件改名避免重复上报。

> `lib.rs` 的精确行号会随并发单元对启动追踪的改动漂移（上面是本次实测输出）；定位本任务的 panic 入口以 `maybe_trigger_test_panic` 函数为准。

### 6.4 未验证的部分（重要）

- **「崩溃提示在界面上真的渲染出来」没有验证。** 目前验证到的是数据层与 CLI 读取层：`logs_tail` 的 DTO 里有这一行（§6.3 C 段），但负责显示它的 `apps/web` 面板不在本任务写域，且没有 tauri-driver / 截图类 UI 自动化可断言「屏幕上出现了这一行」。**不要把它理解成已看到 UI。**
- 崩溃发生在 `AppState` 打开**之前**（比如 WebView 创建失败）时，`report_pending` 也不会跑（它挂在核心装配成功之后）。这是取舍：没有核心就没有 UI 可提示，此时文件仍然留在磁盘上，用户能手动找到。

---

## 7. 验收命令（真实输出）

本轮（2026-11-16 托盘与崩溃补验）实际跑过的：

| # | 命令 | 结果 |
|---|---|---|
| 1 | `cargo test -j 2 -p hbot-desktop` | **EXIT=0**，`39 passed; 0 failed; 0 ignored` |
| 2 | `cargo clippy -j 2 -p hbot-desktop --all-targets -- -D warnings` | **EXIT=0** |
| 3 | `cargo fmt -p hbot-desktop -- --check` | **EXIT=0** |
| 4 | `cargo build -p hbot-desktop`（debug） | **EXIT=0** |
| 5 | `pwsh -File scripts/verify-tray-actions.ps1` | **EXIT=0**（§3.3，三个菜单 id 全绿） |
| 6 | `pwsh -File scripts/verify-crash-report.ps1` | **EXIT=0**（§6.3，两次启动全绿） |
| 7 | `node scripts/check-tauri-conf.mjs` | **EXIT=0**（`通过 61 项检查（警告 0 条）`） |
| 8 | `cargo run -q -p xtask -- deps` | **EXIT=0**（`deps: 0 violation(s)（15 个 crate，47 条内部边）`） |

既有真机脚本（历史记录，本轮未复跑）：`verify-single-instance.ps1`（§2.2）、`verify-tray-close.ps1`（§3.2）、`verify-autostart.ps1`（§4.2）均为 **EXIT=0**。

> `xtask deps` 曾经是红的：一开始为了写 `logs` 表给 `hbot-desktop` 加了 `hbot-store` 依赖，而闸门只允许壳依赖 `hbot-core` + `hbot-rpc`。**没有放宽规则**，改成走 `hbot_rpc::app::agent::log::StoreRunLogSink`（rpc 侧封装 `NewAgentLog`，桌面壳不直接依赖 store），依赖删掉，闸门恢复绿。这条记在这里，是因为"加个依赖省事"是这个仓库里最容易犯的错。

---

## 8. 踩过的坑（免得下一个人再踩）

1. **Windows PowerShell 5.1 的 `Start-Process -RedirectStandardError <file>` 会丢 `.ExitCode`**（实测 `cmd /c exit 3` 也返回空）。三个验证脚本因此 `#Requires -Version 7.0`，必须用 `pwsh` 跑；`powershell.exe -File` 会被版本要求挡住。
2. **读子进程正在写的日志文件**：`[System.IO.File]::ReadAllText` 默认 `FileShare.Read` → 共享冲突 → 返回空串（断言全假失败）；PS 5.1 的 `Get-Content -Raw` 又按 GBK 解 UTF-8 中文 → 乱码。正确姿势是自己开 `FileStream(..., FileShare.ReadWrite)` + `UTF8Encoding(false)`。
3. **不能太早发 `WM_CLOSE`**：窗口是 `Builder::build` 里创建的，`setup` 里的关窗拦截器要晚几百毫秒。抢在前面发 `WM_CLOSE` 会走 Tauri 默认关窗（最后一个窗口没了就退出），测到的不是优雅退出。判据：等日志里出现「托盘已就绪」再发。
4. **`Get-Process` 的 `MainWindowHandle` 会指到单实例插件的隐藏辅助窗口**（标题 `com.hbot.desktop-siw`，`IsWindowVisible=true`）。窗口句柄必须自己 `EnumWindows` + 按 PID + 可见 + **标题精确匹配**。
5. **`taskkill /T /F` 之后立刻重启会撞 WebView2 用户数据目录占用**（`0x800700AA 请求的资源在使用中`），新实例在 `setup` panic、退出码 101。这是「强杀后立刻重启」的环境冲突，不是产品 bug；验证脚本用「换隔离目录 + 等 3s」绕开。副作用是**意外验证了 panic hook**（见 §6.2）。
6. **`scripts/*.ps1` 必须是 UTF-8 with BOM**（本仓库 AGENTS 第 41 条）：PS 5.1 会把无 BOM 的 UTF-8 当 GBK 读，中文注释直接语法错误。`write`/`edit` 工具不会写 BOM，每次改完要自己 `WriteAllText(..., UTF8Encoding($true))` 重打。
7. **`tauri.conf.json` 的 `app.trayIcon` 会自动建托盘**：别再在 Rust 里 `TrayIconBuilder`，否则两个图标；菜单要在 `setup` 里通过 `tray_by_id("main")` 挂。
8. **`CheckMenuItem` / `Menu` 这些 handle 不是 `Send + Sync`**：不能塞进 `app.manage()` 的状态里（`tauri::State` 要求 `Send + Sync`）。改成"菜单重建"（`refresh_menu()` 拿 `app` 重新 build 一遍）而不是缓存 handle。

---

## 9. 未验证清单（**不要当成已验证**）

1. **「第二个实例把窗口真的抬到最前」**：脚本在**有另一个应用占着前台**时观测到 `[WARN]`（两次实测的前台分别是 DSH 窗口与 Chrome）：窗口确实从最小化恢复、确实可见，但 `GetForegroundWindow()` 仍是别的窗口。这符合 Windows 前台锁的规则（后台进程不允许随便抢焦点，插件只是调了 `AllowSetForegroundWindow`），但我们**没有**在"没有别的窗口抢焦点"的干净桌面上复测过 `-StrictForeground` 全绿。**结论：窗口会被恢复并请求聚焦；"一定在你眼前"不保证。**
2. **托盘菜单项真的被鼠标点击**：仍未验证。`scripts/verify-tray-actions.ps1` 验证的是「菜单 id → `MenuAction` → 动作」的派发（§3.3，A/B/C 全绿），事件来源是单实例插件转交的隐藏 `--tray-action` argv，**不是鼠标**；`TrackPopupMenu` 级别的 UI 自动化没有做。
3. **托盘图标在真实桌面上长什么样**（是否被 Windows 折叠进"隐藏的图标"、tooltip 是否可读）：没有截图、没有人工确认。
4. **Linux**：systemd unit、logrotate、`hbot serve` 在 systemd 下的实际行为 **全部未验证**，从未在真实 Linux 主机上运行。`.desktop` 自启文件只有单测。
5. **macOS**：全部未验证（plist 内容有单测；`tauri.macos.conf.json` 的 dmg 构建没跑过；命令隔离在 macOS 上不可用这件事也与本任务无关但与桌面壳共存）。
6. **崩溃提示出现在界面上**（见 §6.4）：数据层已验证（`logs_tail` 能查到 `source=agent`、`level=error` 的崩溃行），但 **UI 是否渲染这一行仍未验证**。
7. **开机自启真的在下次登录时拉起应用**：只验证了"注册表项写对/删对/幂等"，**没有重启机器**去确认它真的自启。
8. **`closeToTray=false` 且**有**机器人在跑时的优雅退出**：C 段是空库（`空库（没有机器人），写队列无内容可排`），"停真实 bot → 等 ≤5s 宽限 → 写屏障"这条**有 bot 的分支**没有真机验证（要连真 QQ 平台）。代码路径是既有的 `BotRegistry::stop_all()`，但从没在这个壳里跑过真实 bot。
9. **多显示器 / 高 DPI 下的托盘与窗口恢复**：未测。

---

## 10. 已知缺失（本任务范围之外）

- **没有自动更新**（`createUpdaterArtifacts: false` 保持不动），没有签名/公证流水线。
- **没有服务器端崩溃上报**：崩溃只落本地文件，用户得自己找到 `<data_dir>/logs/`。也没有"崩溃后一键打包诊断包"的入口。
- **没有 `desktop.json` 的设置界面**：只能改文件或用 `--set-autostart`；`closeToTray` 目前**只能手改 JSON**（没有 UI 开关，也没做 CLI 参数）。
- **单实例只识别 `--tray-action`**：第二个进程的 argv 会被插件递过来，但回调只处理隐藏的 `--tray-action`（测试/运维入口），其余参数仍忽略（比如 `hbot --open-project X` 在已运行时不会真的打开项目）。
- **没有托盘气泡通知**（崩溃/崩溃提示只走 RunLog，不弹系统通知）。
- **没有"最小化到托盘"的动画/首次提示**：用户第一次关窗口可能会以为程序退出了（只有一个托盘图标）。产品上通常需要一次性提示。
- **systemd unit 没有 `ExecReload`**、没有配套的 `hbot.socket`、没有 SELinux/AppArmor profile。

---

## 11. 怎么复现本文档里的证据

```powershell
# 前置：E 盘构建目录 + 只开两个 job（D 盘曾被打爆，rustc 也怕并发）
$env:CARGO_TARGET_DIR = 'E:\hbot-target\dux'
cargo build -p hbot-desktop -j 2

# 1~4：静态门
cargo clippy -p hbot-desktop --all-targets -j 2 -- -D warnings
cargo fmt -p hbot-desktop -- --check
node scripts/check-tauri-conf.mjs

# 5~8：真机行为（必须 pwsh 7；脚本自己隔离 APPDATA，不动你的真实数据目录）
pwsh -File scripts/verify-single-instance.ps1
pwsh -File scripts/verify-tray-close.ps1
pwsh -File scripts/verify-autostart.ps1      # 会备份/还原你的 HKCU Run 值与 desktop.json
pwsh -File scripts/verify-tray-actions.ps1   # 三个菜单 id 的派发（不是鼠标点击）
pwsh -File scripts/verify-crash-report.ps1   # 崩溃两次启动 + --logs-tail 断言
```

每个脚本失败时打印「失败项: …」并以 1 退出；日志目录会打印在最后一行。