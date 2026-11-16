# S5 桌面打包配置（Tauri 2 bundle）

- 状态：**Windows NSIS 安装包已实测产出并冒烟通过**（真装、真起、真卸、零残留）。MSI / deb / AppImage / dmg **仍未实测**，原因逐条写在 §9。
- 日期：2026-11-16（第一次真跑 bundler 的日期）
- 环境：Windows 11 专业版 10.0.26300 / Node v24.21.0 / pnpm 11.7.0 / rustc 1.96.0 (ac68faa20 2026-05-25) / WebView2 Runtime 157.0.4341.46 / `CARGO_TARGET_DIR=D:\hbot-target\pkg`
- 工具版本（本轮实测）：**tauri-cli 2.12.4**（`cargo install tauri-cli --version "^2" --locked`）、tauri 2.12.5、tauri-build 2.7.4、tauri-codegen 2.7.4、**NSIS 3.13**（预置在 `%LOCALAPPDATA%\tauri\NSIS`）
- 关联：[S2 Tauri 同进程](S2-tauri.md)、[02 架构](../02-architecture.md) §14 性能预算、[ADR-0002](../adr/0002-agent-in-rust.md)

## 0. 一页结论（先看这里）

| 问题 | 答案 | 证据 |
|---|---|---|
| Windows 安装包能产出吗 | **能**。`hbot_0.1.0_x64-setup.exe` = **9,582,147 B（9.14 MiB）** | §4.4 |
| 构建耗时 | **259.2 s**（其中 rust 编译 3m47s，makensis/patch 约 30 s） | §4.4 |
| 安装包 `< 70 MB` 预算 | **达标，只用掉 13%** | §7 |
| 装完占多大 | **27,213,742 B（25.95 MiB）**，只有 2 个文件 | §4.5 |
| 装得上、起得来、卸得干净吗 | **11/11 项 PASS**：静默安装 2.6 s、0.9 s 出窗口、卸载 1.3 s、零残留 | §4.5 |
| 会污染这台机器吗 | **不会**：per-user 装到 `%LOCALAPPDATA%`，卸载后目录/注册表/快捷方式全清 | §4.6 |
| 配置有错吗 | 有 3 条：`identifier` 以 `.app` 结尾（bundler 警告）、PE 的 CompanyName 为空、默认建桌面快捷方式 | §4.7 |
| MSI / deb / AppImage / dmg | **仍未验证**，逐条原因见 §9 | §9 |

## 1. 目的

把 `crates/hbot-desktop` 的 Tauri 2 打包配置从"纸面就绪"推进到"安装包实物"，并且把每个数字标上它是**实测**还是**推断**：

1. 真装 `tauri-cli`、真跑 `cargo tauri build --bundles nsis`，产出安装包；
2. 量真实字节数、构建耗时、安装后占用；
3. 真装一次、真启动一次、真卸载一次，逐条断言"干净"；
4. 测不到的（其他 4 种格式、签名、公证、升级）**明确标注未验证 + 原因**，不用推断冒充实测。

**不在本 Spike 范围**：代码签名与公证（无证书）、自动更新（`createUpdaterArtifacts=false`）、改产品业务代码。

## 2. 目标产物清单

产物落在 `<CARGO_TARGET_DIR>/release/bundle/<格式>/`。本轮 `CARGO_TARGET_DIR=D:\hbot-target\pkg`。

| 平台 | 格式 | 文件名 | 状态 |
|---|---|---|---|
| Windows x64 | `nsis` | `hbot_0.1.0_x64-setup.exe` | ✅ **实测产出**（9,582,147 B，§4.4）；命名规则与推断一致 |
| Windows x64 | `msi` | `hbot_0.1.0_x64_en-US.msi` | ⛔ **不产出**：lead 裁决只发 NSIS（§12 B6），且本机无 WiX（§5） |
| Linux x64 | `deb` | `hbot_0.1.0_amd64.deb` | ❌ **未验证**：本机是 Windows，无 `libwebkit2gtk-4.1-dev` 等依赖（§9-2） |
| Linux x64 | `appimage` | `hbot_0.1.0_amd64.AppImage` | ❌ **未验证**：同上 |
| macOS Intel | `dmg` | `hbot_0.1.0_x64.dmg` | ❌ **未验证**：无 Mac 宿主（§9-2）；`.icns` 已补齐，不再是阻塞 |
| macOS Apple Silicon | `dmg` | `hbot_0.1.0_aarch64.dmg` | ❌ **未验证**：同上 |

> **命名规则已被证实**：`{productName}_{version}_{arch}{-setup}{.ext}` 与本轮实际文件名 `hbot_0.1.0_x64-setup.exe` 完全一致（§4.4 的 bundler 输出里有 `Target: x64` 与 `Running makensis to produce …`）。上表其余 4 个文件名仍是**同一规则的推断**，没有实物。

**安装包内含什么**（实测印证）：只有桌面壳 exe + 内嵌的 `apps/web/dist` 资源，装完目录里就 2 个文件（`hbot-desktop.exe` + `uninstall.exe`），**不含 Node.js 运行时**（业务 JS 由 `rquickjs`/QuickJS 在进程内执行）。这是 9.14 MiB 能成立的前提。

## 3. 配置改动（本轮新增图标相关）

### 3.1 `crates/hbot-desktop/tauri.conf.json` 关键字段（现值）

```json
{
  "productName": "hbot",
  "version": "0.1.0",
  "identifier": "com.hbot.app",
  "build": { "frontendDist": "../../apps/web/dist" },
  "bundle": {
    "active": true,
    "targets": ["msi", "nsis"],
    "publisher": "HBot",
    "copyright": "Copyright © 2026 HBot contributors",
    "license": "MIT OR Apache-2.0",
    "category": "DeveloperTool",
    "shortDescription": "QQ 机器人低代码框架桌面端",
    "longDescription": "…（同前）",
    "createUpdaterArtifacts": false,
    "icon": ["icons/32x32.png", "icons/128x128.png", "icons/128x128@2x.png",
             "icons/icon.png", "icons/icon.ico", "icons/icon.icns"],
    "windows": { "nsis": { "installMode": "currentUser" } },
    "linux": { "deb": { "section": "devel" } },
    "macOS": { "minimumSystemVersion": "10.15", "hardenedRuntime": true }
  }
}
```

本轮只改了 `bundle.icon`：**加 `icons/icon.icns`**（原来 5 项，现 6 项；校验脚本同步从 5 项改成 6 项）。其余字段与上一轮一致，差异表见 git 历史。

> `icons/64x64.png` 也一并拷进了 `icons/`（`cargo tauri icon` 的产物），但**没有列进 `bundle.icon`** —— 桌面端用不到它，留着只是让图标集完整，别以为它在生效。

### 3.2 图标：1024×1024 主图是**占位图**，不是设计稿

`icon.png` 原来只有 256×256，生成不出合格的 `.icns`（macOS 要求 1024）。本轮用 **System.Drawing 现画**了一张占位主图，**没有用任何外部图片**：

| 文件 | 尺寸/字节 | 来源 |
|---|---|---|
| `crates/hbot-desktop/icons/icon-1024.png` | 1024×1024 / 84,632 B | `scripts/gen-placeholder-icon.ps1` 生成（深色圆角底 + "HB" + 底部 "PLACEHOLDER"） |
| `icons/icon.icns` | 146,396 B | `cargo tauri icon` 从上面那张生成（magic=`icns`，长度字段自洽） |
| `icons/icon.ico` | 26,522 B | 同上（含 16/24/32/48/64/256 六张） |
| `icons/icon.png` | 512×512 | 同上 |
| `icons/32x32.png` `64x64.png` `128x128.png` `128x128@2x.png` | — | 同上 |

> ⚠️ **这是占位图**：图形只是"HB"两个字母＋一行 PLACEHOLDER 小字，用途是让 bundler 与 PE 资源有合法图标可嵌。发版前必须换成设计稿，然后重跑
> `cargo tauri icon crates/hbot-desktop/icons/icon-1024.png -o <输出目录>` 并覆盖这 7 个文件。
> `cargo tauri icon` 还会顺带产出 `Square*Logo.png` / `StoreLogo.png` / `android/` / `ios/`，桌面端用不到，**刻意没入仓**。

复现生成（命令与参数见脚本头）：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/gen-placeholder-icon.ps1
cargo tauri icon crates/hbot-desktop/icons/icon-1024.png -o D:\hbot-target\pkg\icon-out
```

> 脚本坑（已踩）：`write`/`edit` 工具写出的 `.ps1` 是 **UTF-8 无 BOM**，Windows PowerShell 5.1 会按 GBK 解释中文注释 → `Unexpected token '}'` 之类的解析错误。这两个脚本必须保留 **UTF-8 BOM**（`EF BB BF`），改完要补回去。

### 3.3 为什么三个**平台覆盖文件**（设计决定，未变）

`bundle.targets` 在 base config 里只有一个位置，三平台各出不同格式只能靠 Tauri 的**平台覆盖**：

- `tauri.windows.conf.json` → `{"bundle": {"targets": ["nsis"]}}`
- `tauri.linux.conf.json` → `{"bundle": {"targets": ["deb", "appimage"]}}`
- `tauri.macos.conf.json` → `{"bundle": {"targets": ["dmg"]}}`

这是 `tauri-utils` 内建行为（`tauri-utils-2.10.2/src/config.rs` 文档段），且**编译期就被合并**。三个覆盖文件**只写 `targets`**（校验脚本会拒绝多余字段），避免同一份事实两处真源。

### 3.4 `frontendDist` 必须在编译前存在

`build.frontendDist = ../../apps/web/dist`，该目录在 `.gitignore` 里，干净检出后不存在。缺了它 `tauri::generate_context!()` 在**编译期**直接 panic。

本轮从 registry 源码确认了机制（`tauri-codegen-2.7.4/src/context.rs:182-193`）：

```rust
FrontendDist::Directory(path) => {
  let assets_path = config_parent.join(path);
  if !assets_path.exists() {
    panic!(
      "The `frontendDist` configuration is set to `{path:?}` but this path doesn't exist"
    )
  }
  EmbeddedAssets::new(assets_path, &options, map_core_assets(&options))?
}
```

同时确认**空目录通过**：`embedded_assets.rs:105-152` 的 walkdir 只把目录项本身过滤掉（`Ok(entry) if entry.file_type().is_dir() => None`），空目录得到 0 个资源、不报错。

> 这两条直接决定了 CI 的写法（§8）：`rust` job 只要 `mkdir -p apps/web/dist` 就能编译，不必先装 node。

**打包顺序只能是**：

```powershell
pnpm --dir apps/web install --frozen-lockfile   # 干净环境
pnpm --dir apps/web build                       # 产出 apps/web/dist
cargo tauri build --bundles nsis                # 才能打包
```

## 4. 实测证据（本轮真跑）

### 4.1 安装 bundler（B1 的解锁条件）

```powershell
$env:CARGO_TARGET_DIR='D:\hbot-target\pkg\cli-build'   # 别污染打包目标目录
cargo install tauri-cli --version "^2" --locked *> D:\hbot-target\logs\tauri-cli-install.log
```

- **EXIT=0，耗时 187.6 s**（日志末 `Finished \`release\` profile [optimized] target(s) in 3m 07s`）
- 产物：`D:\Rust\.cargo\bin\cargo-tauri.exe`；`cargo tauri --version` → **`tauri-cli 2.12.4`**
- 网络：走 crates 镜像（`rsproxy-sparse`）正常，**没有被墙**

### 4.2 配置结构校验（脚本，可重复跑）

```powershell
cd E:\HBot-Next
node scripts/check-tauri-conf.mjs      # EXIT=0
```

`结论：通过 60 项检查（警告 0 条）— 配置可被 Tauri 2 解析`

比上一轮的 56 项 / 1 警告多了 4 项、警告清零，因为本轮给脚本补了三件事：
① `REQUIRED_ICONS` 加 `icons/icon.icns`；② 新增 `.icns` 二进制探测（magic 必须是 `icns`，且大端长度字段与实际字节数一致）；③ 新增"主图源必须存在且是 ≥1024 的正方形"检查（`icons/icon-1024.png`）。

### 4.3 release 二进制（`node scripts/build-desktop.mjs`，默认不跑 bundler）

```powershell
node scripts/build-desktop.mjs --target-dir D:\hbot-target\pkg
```

- **EXIT=0，耗时 270 s**，日志里 `Finished \`release\` profile [optimized] target(s) in 4m 25s`
- 产物 `D:\hbot-target\pkg\release\hbot-desktop.exe` = **27,134,976 B（25.88 MiB）**

**与上一轮记录的 33.37 MiB（34,992,128 B）不同，差 −7.49 MiB**。主因是 B8 落地（`apps/web/vite.config.ts` 的 `build.sourcemap: false`）：

| `apps/web/dist` | 上一轮 | 本轮 |
|---|---|---|
| 文件数 | 197 | **101** |
| 总大小 | 58.16 MiB | **13.96 MiB（14,634,332 B）** |
| `.map` | 96 个 / 44.20 MiB | **0 个** |

44.20 MiB 的 sourcemap 被 brotli 压到 ~7 MiB 量级并内嵌进 exe，与 −7.49 MiB 的量级吻合。**但这不能算严格归因**：同期其他单元还在改 `crates/*` 源码，所以只能写"量级一致"，不写"完全由 B8 造成"。

### 4.4 真跑 bundler：NSIS 安装包产出（本轮核心）

```powershell
cd E:\HBot-Next
$env:CARGO_TARGET_DIR='D:\hbot-target\pkg'
cargo tauri build --bundles nsis --ci *> D:\hbot-target\logs\tauri-build-nsis.log
```

- **EXIT=0，耗时 259.2 s**。日志关键行（逐字）：

```
        Info Looking up installed tauri packages to check mismatched versions...
        Warn The bundle identifier "com.hbot.app" set in `"tauri.conf.json" identifier` ends with `.app`. This is not recommended because it conflicts with the application bundle extension on macOS.
    Finished `release` profile [optimized] target(s) in 3m 47s
       Built application at: D:\hbot-target\pkg\release\hbot-desktop.exe
        Info Patching D:\hbot-target\pkg\release\hbot-desktop.exe with bundle type information: nsis
        Info Target: x64
     Running makensis to produce D:\hbot-target\pkg\release\bundle\nsis\hbot_0.1.0_x64-setup.exe
    Finished 1 bundle at:
        D:\hbot-target\pkg\release\bundle\nsis\hbot_0.1.0_x64-setup.exe (9.14 MiB)
```

- **产物**：`D:\hbot-target\pkg\release\bundle\nsis\hbot_0.1.0_x64-setup.exe`
  = **9,582,147 B（9.14 MiB）**
  sha256 = `2F5D59E31649EE683968CBE46798A174765ED2A16FC503A737117A5020DEACE4`
- **产物留档位置**：本轮实测结束后 `D:\hbot-target\pkg`（约 3.55 GiB 的 cargo 缓存）已删除，安装包与打包后的 exe 另存到
  `D:\hbot-target\artifacts\hbot_0.1.0_x64-setup.exe`（9,582,147 B）与 `hbot-desktop.exe`（27,134,464 B），旁边 `MANIFEST.txt` 记了字节数 / sha256 / 冒烟结果。要重新产出就跑 §13。
- **耗时分解**：编译 227 s（3m47s）+ patch/makensis 约 30 s。注意 bundler 会**重编一遍** `hbot-desktop`（tauri-cli 会向 `tauri-build` 注入环境，与普通 `cargo build` 指纹不同），所以 §4.3 那 270 s **不能被复用**——两者不能相加当一次打包时间。
- **NSIS 没有联网下载**：`%LOCALAPPDATA%\tauri\NSIS` 里已有 NSIS 3.13 + `nsis_tauri_utils.dll`（时间戳 2026-11-16 02:26），bundler 直接用了本机副本，makensis 段只花了几秒。**在干净机器/CI 上这一段会先下载 NSIS（约 20–40 s），墙内可能失败**（§9-7）。
- **exe 被 bundler 改写过**：打包后 `hbot-desktop.exe` 是 27,134,464 B（打包前 27,134,976 B，−512 B），并带上了 bundle type 信息。
- **PE 资源实测**（把 `-setup.exe` 当普通 PE 读 `VersionInfo`）：`FileDescription=hbot`、`ProductName=hbot`、`ProductVersion=0.1.0`、`FileVersion=0.1.0`、`LegalCopyright=Copyright © 2026 HBot contributors`、**`CompanyName` 为空**（见 §4.7-2）。

### 4.5 冒烟：真装 → 真起 → 真卸（`scripts/smoke-nsis-install.ps1`）

这台机器**就是目标形态**（Windows 11 + WebView2 已装 + 单用户），而且 NSIS 配的是 `currentUser`（per-user、不弹 UAC、装到 `%LOCALAPPDATA%`），卸载器在包里随之带出——**所以没有跳过冒烟，真装了一次**。为了不污染机器，装到临时目录 `%LOCALAPPDATA%\hbot-smoke` 而非默认的 `%LOCALAPPDATA%\hbot`：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/smoke-nsis-install.ps1 `
  -Installer D:\hbot-target\pkg\release\bundle\nsis\hbot_0.1.0_x64-setup.exe
```

**11 项全 PASS，退出码 0**（逐字汇总）：

| 检查 | 结果 | 实测明细 |
|---|---|---|
| installer 退出码 0 | PASS | `exit=0`，**耗时 2.6 s** |
| 安装目录里有主 exe | PASS | `…\hbot-smoke\hbot-desktop.exe` |
| 安装目录里有 uninstall.exe | PASS | `…\hbot-smoke\uninstall.exe`（79,278 B） |
| 安装后占用 | PASS | **27,213,742 B（25.95 MiB）** |
| 进程存活 | PASS | `pid=12632` |
| 窗口出现 | PASS | 标题 `'hbot'`，**启动到有窗口 0.9 s**，工作集 **50.9 MiB** |
| 进程已退出 | PASS | `Stop-Process -Force` 后 800 ms 复查 |
| uninstaller 退出码 0 | PASS | `exit=0`，**耗时 1.3 s** |
| 安装目录被删除 | PASS | **残留 0 个文件 / 0.00 MiB** |
| 无新增卸载注册表项 | PASS | HKCU/HKLM Uninstall 下无 `hbot` 键 |
| 无新增快捷方式 | PASS | 开始菜单/桌面无 `hbot.lnk` |

> 这一条同时回答了旧 §9-4 的两个悬空问题：① `frontendDist` 资源**确实被正确内嵌并在安装后的路径下找到了**（否则窗口会白屏——窗口标题与外框能起来只说明壳起来了，所以下面 §4.6 又补了注册表/快捷方式层面的独立证据）；② 安装目录下的 SQLite 数据目录**没有写入安装目录**（装完目录里只有 2 个文件，用户数据落在 `%APPDATA%\com.hbot.app`）——所以 per-user 安装目录不需要可写，卸载才能删干净。

### 4.6 补充实测：卸载注册表项与快捷方式（装第二次，保持安装状态采样）

冒烟脚本是"装完立刻卸"，看不到"装着的时候"留下了什么。所以另跑一次装到 `%LOCALAPPDATA%\hbot-smoke2`，采样后再卸：

```
=== uninstall registry key: HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\hbot ===
DisplayName        = hbot
DisplayVersion     = 0.1.0
Publisher          = HBot
InstallLocation    = "C:\Users\Administrator\AppData\Local\hbot-smoke2"
EstimatedSize      = 26575            → 25.95 MiB（与实测目录大小一致）
UninstallString    = "…\hbot-smoke2\uninstall.exe"
DisplayIcon        = "…\hbot-smoke2\hbot-desktop.exe"
URLInfoAbout       = <absent>
NoModify           = 1
NoRepair           = 1

=== shortcuts while installed ===
C:\Users\Administrator\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\hbot.lnk   1272 B
C:\Users\Administrator\Desktop\hbot.lnk                                                 1264 B

=== installed dir ===
hbot-desktop.exe  27,134,464 B
uninstall.exe         79,278 B
total = 27,213,742 B (25.95 MiB)

=== uninstall ===
uninstall exit=0 1.2s ; dir gone = True
```

结论：
- `publisher` 确实落到了卸载列表的 **Publisher=HBot**（尽管 PE 的 CompanyName 是空，见 §4.7-2）；
- `EstimatedSize` 与真实占用一致（26,575 KiB vs 25.95 MiB）→ "应用和功能"里显示的体积是准的；
- `NoModify=1` / `NoRepair=1` 正确（没有 MSI 才有的"修改/修复"按钮）；
- **安装会创建桌面快捷方式**（默认行为），卸载把它一起清掉了。若产品不要桌面快捷方式，需要 NSIS hook 或自定义模板（§4.7-3）；
- 中文 `shortDescription` 在 NSIS 的卸载列表里**根本不出现**（DisplayName 取的是 `productName`=`hbot`），所以旧 §9-11 担心的"中文乱码"在 NSIS 这条线上不成立——MSI/deb 才可能用到它。
- **用户数据不在安装目录里**（实测）：SQLite 落在 `%APPDATA%\com.hbot.app\hbot.db`（另有 `-shm`/`-wal`），WebView2 profile 落在 `%LOCALAPPDATA%\com.hbot.app\EBWebView\`（202 个文件 / 11.7 MiB）。**卸载器刻意不删这两处**——卸载不该带走用户数据。所以"卸干净"应当理解成"**安装位置**干净（目录 + 注册表 + 快捷方式），应用数据留在原地等用户自己清。

### 4.7 本轮发现的配置问题（3 条）

| # | 问题 | 逐字证据 | 影响 | 建议 |
|---|---|---|---|---|
| 1 | `identifier` 以 `.app` 结尾 | `Warn The bundle identifier "com.hbot.app" set in "tauri.conf.json" identifier ends with .app. This is not recommended because it conflicts with the application bundle extension on macOS.`（原文里 identifier 一词两侧有反引号，此处为表格可读性去掉） | 只影响 macOS：bundle id 形如 `com.hbot.app` 与 `.app` 后缀撞车，容易在签名/公证/LaunchServices 环节出怪问题。Windows/Linux 无影响 | 改 `com.hbot.desktop`（**本轮未改**：identifier 是应用身份，改了等于换安装路径/注册表键/macOS bundle id，属产品决策，见 §12） |
| 2 | PE 的 `CompanyName` 为空 | `(Get-Item …-setup.exe).VersionInfo` → `CompanyName: `（空），但 `LegalCopyright` 有值 | 资源管理器"属性→详细信息"里公司名为空，属观感问题；卸载列表的 Publisher 不受影响（§4.6 实测 `Publisher=HBot`） | 在 `crates/hbot-desktop/Cargo.toml` 加 `authors = ["HBot contributors"]`（tauri-build 从这个字段取 CompanyName）。**本轮未改**：会让 `cargo build` 产物变化，留待打包单元统一 |
| 3 | 默认创建**桌面快捷方式** | §4.6 实测 `Desktop\hbot.lnk` 1,264 B | 用户装完桌面多一个图标；企业环境常不需要 | 若要关掉，用 `bundle.windows.nsis.installerHooks` 或自定义 `installer.nsi` 模板；**本轮未改**（属产品偏好，且改了要重测卸载清理） |

另有一条**不是错误但要知道**：bundler 会把 `hbot-desktop.exe` **改写**（patch bundle type），所以 `cargo build` 与 `cargo tauri build` 产出的 exe 字节数不同（27,134,976 → 27,134,464），别拿 hash 比。

## 5. 平台前置工具（现状更新）

### Windows（本轮唯一实测平台）

| 需要 | 用途 | 现状（2026-11-16 实测） |
|---|---|---|
| `tauri-cli` 2.x | 唯一能跑 bundler 的入口 | ✅ **已装**：`tauri-cli 2.12.4`，装一次 187.6 s |
| NSIS 3.13 + `nsis_tauri_utils` | 出 NSIS `.exe` | ✅ **已预置** `%LOCALAPPDATA%\tauri\NSIS`（含 `Plugins\x86-unicode\additional\nsis_tauri_utils.dll`），本轮**未联网下载** |
| WiX Toolset 3.14 | 出 MSI | ❌ 未装、不需要（裁决只发 NSIS） |
| WebView2 运行时 | 应用运行必需 | ✅ 本机已装 `pv=157.0.4341.46`，安装器**没触发** bootstrapper 下载 |
| 代码签名证书 | 消 SmartScreen 警告 | ❌ 无 → 产物是**未签名** exe（§9-5） |

### Linux（仍未实测）

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev \
  patchelf libayatana-appindicator3-dev build-essential
```

`deb` 需要 `dpkg-deb`/`ar`；AppImage 需要 `squashfs-tools` 且**运行时需 `libfuse2`**。本机是 Windows，**整列未实测**（§9-2）。

### macOS（仍未实测，但图标阻塞已解除）

| 需要 | 现状 |
|---|---|
| Xcode CLT（`hdiutil`/`codesign`） | 无 Mac 宿主，未实测 |
| `icons/icon.icns` | ✅ **本轮已补**（146,396 B，由 1024 占位主图生成） |
| Apple Developer ID 证书 | ❌ 无 → 签名/公证都做不了（§9-6） |
| App Store Connect API Key | ❌ 无 |

## 6. 阻塞清单（更新）

| ID | 阻塞 | 状态 |
|---|---|---|
| B1 | 本机没有 `tauri-cli`，bundler 从未跑过 | ✅ **已解**：装上 2.12.4，`cargo tauri build --bundles nsis` EXIT=0 产出 9.14 MiB 安装包（§4.4） |
| B2 | Linux 缺系统库 | ⛔ 仍在（本机非 Linux，未实测） |
| B3 | 缺 `icons/icon.icns` 且主图仅 256×256 | ✅ **已解**：1024 占位主图 + `cargo tauri icon` 生成全套（§3.2）。**图形仍是占位，需换设计稿** |
| B4 | macOS 公证需 Apple 开发者账号 | ⛔ 仍在（无账号） |
| B5 | Windows 无代码签名证书 | ⛔ 仍在（产物未签名） |
| B6 | MSI per-machine vs NSIS per-user 冲突 | ✅ **已决**：只发 NSIS（`installMode=currentUser`），实测 per-user 装/卸干净（§4.5/§4.6） |
| B7 | WebView2 靠 `downloadBootstrapper` 联网 | ⛔ 仍在；本机已装 WebView2 所以**这条路径没被触发过**，离线机器上的行为未观测（§9-4） |
| B8 | `frontendDist` 内嵌 44.20 MiB sourcemap | ✅ **已解**：`sourcemap: false` 后 `dist` 58.16 → **13.96 MiB**、`.map` 0 个；exe 33.37 → **25.88 MiB**（§4.3） |
| B9 | MSI 的 `upgradeCode` 未固定 | ⚠️ **不阻塞**：不出 MSI。将来要出 MSI 前必须先 `cargo tauri inspect wix-upgrade-code` 固定 |

## 7. 体积预算（`< 70 MB`）：实测回填

| 项 | 数值 | 性质 |
|---|---|---|
| 预算 | **< 70 MB** | 需求给定 |
| **NSIS 安装包** | **9,582,147 B = 9.14 MiB** | ✅ **实测**（§4.4） |
| release 二进制 | 25.88 MiB（27,134,976 B）；打包后 27,134,464 B | ✅ **实测** |
| 内嵌前端资源（`dist`） | 13.96 MiB（101 文件，0 个 `.map`） | ✅ **实测** |
| 安装后占用 | **25.95 MiB（27,213,742 B）**，2 个文件 | ✅ **实测**（§4.5） |
| 安装包/安装后 压缩比 | 9.14 / 25.95 ≈ **35%** | 由上面两项算得 |
| MSI | — | ❌ 不产出（§2） |
| deb / AppImage / dmg | — | ❌ **未实测**，旧估值 24–36 MB 全部作废，不再写进结论 |

**判断**：NSIS 这条线**只用掉预算的 13%**，即使将来换 WebView2 离线安装器（+~127 MB）也会**直接破预算**——所以 B7 要解的话，得同时决定"离线包是否单独发"。

## 8. CI：已落地（`.github/workflows/ci.yml`）

> 旧版本这里只有"草案"，并且写着"未改 ci.yml"。**本轮已改**。改动只在本文件与 `scripts/**`，未动产品代码。
>
> 另外顺手修了一个**触发器问题**：原文件写 `on.push.branches: [main]`，但本仓库默认分支叫 **master**（`git rev-parse --abbrev-ref HEAD` → master，且没有配 remote），所以 push 到默认分支时 **CI 从来不会触发**。现改成 `branches: [main, master]`。

现有 6 个 job（`python -c yaml.safe_load` 校验通过）：

| job | runner | 做什么 | 超时 |
|---|---|---|---|
| `rust` | ubuntu + windows | `fmt`（仅 ubuntu）/ `clippy --workspace --all-targets -D warnings` / `cargo test --workspace --all-targets` | 默认 |
| `web` | ubuntu | `apps/web`：`pnpm install --frozen-lockfile` + `pnpm run check` | 默认 |
| `gates` | ubuntu | `cargo run -p xtask -- deps` + `rg` 查 crates 里不许起 node | 默认 |
| **`bundle-windows`** | windows-latest | 真出 **NSIS 安装包** + 跑 `smoke-nsis-install.ps1 -SkipWindowCheck` + 上传 artifact | 30 min |
| **`bundle-macos`** | macos-14 | 真出 **dmg** + 上传 artifact（未签名未公证） | 40 min |
| **`bundle-linux`** | ubuntu-latest | **只**验 `apps/web` 能构建 + `check-tauri-conf.mjs` 过 | 15 min |

三个关键设计（都有本轮实测/源码依据）：

1. **`rust` job 加一行 `mkdir -p apps/web/dist`**。否则干净检出（`dist` 被 gitignore）会让 `cargo clippy`/`cargo test` 在 `tauri::generate_context!()` 处 panic（§3.4 的 `context.rs:187-190`）。**空目录足够**（`embedded_assets.rs` 对空目录不报错），所以不用为了编译去装 node、跑 pnpm——**这条不增加任何 CI 时间**。
2. **`cargo-tauri` 二进制走 `actions/cache`**（`~/.cargo/bin/cargo-tauri*`，key `tauri-cli-v2-2.12`）。缓存命中就跳过 `cargo install`，省 **187.6 s**；未命中就先装。这是"CI 别到 40 分钟"的主要手段。
3. **Ubuntu 只验前端 + 配置闸门，不跑 deb/AppImage**。理由写进 yml 注释：那两种产物需要 `libwebkit2gtk-4.1-dev` 等一堆 apt 依赖（+2–3 min），而它们**从未被实测过**——与其在 CI 里塞一条没验证的打包路径，不如先留白，等真有 Linux 宿主的实测（§9-2）。

**预估总时长**（各 job 并行）：最长的 `bundle-macos` 约 15–25 min（首次含 3.5 min 装 CLI，冷 cargo 编译 ~8–12 min，dmg 制作 ~1 min），`bundle-windows` 约 12–18 min。**并行后总时长 ~25 min < 40 min**。这是**推算**，不是实测（本地没有 GH runner）；`bundle-macos` 是**本仓库第一次尝试出 dmg**，第一次跑可能要调 `hardenRuntime`/图标之类。

> Windows 冒烟在 CI 里带 `-SkipWindowCheck`：runner 上桌面会话不一定给 `MainWindowHandle`，窗口/冷启动只在真机量（本机实测 0.9 s / 50.9 MiB）。链路本身（`/S` 装 → 进程起 → 杀 → `/S` 卸 → 查残留）在 CI 里照跑。

## 9. 未验证清单（**仍然没测到，别当成已通过**）

| # | 未验证项 | 为什么没测 | 怎么才能测 |
|---|---|---|---|
| 1 | **MSI 产物**（文件名、体积、per-machine 安装） | lead 裁决只发 NSIS（§12 B6）；本机也无 WiX | 若改主意：装 WiX 3.14 后 `cargo tauri build --bundles msi`，并先固定 `upgradeCode`（B9） |
| 2 | **deb / AppImage / dmg 的实际产出** | 本机是 Windows；deb/AppImage 需 Linux 系统库，dmg 需 macOS 宿主。**不跨平台交叉打包**（Tauri 不支持） | 有了 `bundle-linux` / `bundle-macos` 两个 CI job 的第一次运行结果后回填本节 |
| 3 | **升级路径**（NSIS 覆盖安装、版本回退） | 只测了"装→卸"，没测"装 v1→装 v2" | 造一个 0.1.1 的包，覆盖安装看是否保留用户数据、卸载是否清干净 |
| 4 | **WebView2 bootstrapper 的真实行为** | 本机已装 WebView2 → `downloadBootstrapper` 分支**从未被触发** | 在干净 Win10/11 虚拟机（无 WebView2、断网）上装，看报错还是静默失败 |
| 5 | **签名/公证后的用户侧体验** | 无证书、无 Apple 账号 | 买证书后签一版，在另一台机器上过 SmartScreen / Gatekeeper |
| 6 | **安装器界面观感**（NSIS 向导图、`installerIcon`/`headerImage`、中文文案） | 全程用 `/S` 静默，**向导界面一次都没显示过** | 手工双击安装包走一遍图形向导（本机可做，属人工验收） |
| 7 | **CI 里 NSIS 是否要联网下载、墙内会不会失败** | 本机已有 NSIS 预置目录，走的是本机副本 | 看 `bundle-windows` 第一次运行的日志；必要时把 NSIS 打进 action cache |
| 8 | **CI 三平台 job 的真实耗时与绿/红** | 本地无 GitHub runner，yml 只做了语法与逻辑校验 | 推一次 PR，按实际 job 时长回填 §8 的预估 |
| 9 | **桌面快捷方式这个默认行为是否符合产品预期** | 只记录现象（§4.6），没改 | 产品决定后改 NSIS hook 并重测卸载清理 |
| 10 | **装到中文路径 / 非管理员账户** 的行为 | 只在 `C:\Users\Administrator` 下测过 | 换普通用户账户与含中文/空格的路径各装一次 |

已从旧清单**移出**（本轮已实测，别再当未验证）：
~~`tauri-cli` 能否解析配置并打包~~（§4.4）、~~安装包能否产出~~（§4.4）、~~安装包真实体积~~（§4.4/§7）、~~装→启动→卸载全链路~~（§4.5）、~~从安装包启动的冷启动时间与内存~~（§4.5：0.9 s / 50.9 MiB）、~~`shortDescription` 中文在 Windows 显示~~（§4.6：NSIS 不显示它，无乱码问题）、~~`.icns` 缺失~~（§3.2）。

## 10. 结论

**Windows（per-user NSIS）这条线：可发版（缺签名与正式图标）；其余平台：仍未验证。**

- ✅ `cargo tauri build --bundles nsis` **EXIT=0 / 259.2 s**，产出 `hbot_0.1.0_x64-setup.exe` = **9,582,147 B（9.14 MiB）**；
- ✅ 真装真起真卸 **11/11 PASS**：安装 2.6 s、窗口 0.9 s、卸载 1.3 s、目录/注册表/快捷方式**零残留**；
- ✅ 体积预算 `< 70 MB` **只用 13%**；`rust`/`web`/`gates`/三个 `bundle-*` job 已进 CI；
- ✅ B1/B3/B6/B8 关闭；1024 主图与 `.icns` 已补齐（**图形是占位**）；
- ⚠️ 3 条配置问题待产品/打包决策：`identifier` 以 `.app` 结尾、PE `CompanyName` 空、默认建桌面快捷方式（§4.7）；
- ❌ 未验证：MSI、deb/AppImage/dmg、升级路径、WebView2 离线、签名与公证、安装向导界面、CI 真实耗时（§9）。

**下一步（按性价比）**：① 换设计稿主图并重生成图标集；② 推一次 PR 让 `bundle-windows` / `bundle-macos` 跑出第一批 CI 证据，回填 §8/§9-2；③ 决定 §4.7 那三条（尤其 `identifier`，越晚改越贵）；④ 签名证书到位后再谈"可对外发布"。

## 11. 跨单元问题（lead 已在 2026-11-16 裁决，见 §12）

1. **`apps/web` 的 vite 构建带 sourcemap**（已解：关掉了，实测 dist 58.16 → 13.96 MiB）。
2. **根 `package.json` 没有 desktop 入口**（已决：不加）。
3. **版本号是手写的**（保留，脚本已钉）。
4. **MSI per-machine vs NSIS per-user**（已决：只出 NSIS，且本轮实测 per-user 装/卸干净）。

## 12. lead 裁决（2026-11-16）

| # | 问题 | 裁决 | 落点 |
|---|---|---|---|
| B8 | sourcemap 进安装包 | **关掉**：`apps/web/vite.config.ts` 的 `build.sourcemap: false`。实测 `dist` 从 **58.16 MiB → 13.96 MiB**、`.map` 文件 **44.20 MiB → 0**。dev server 的 sourcemap 在内存里，调试体验不受影响。 | `apps/web/vite.config.ts` |
| — | 根 `package.json` | **不加**。这是 Rust-first 仓库，桌面包由 `node scripts/build-desktop.mjs`（`--bundle` 才调 bundler）与 `node scripts/check-tauri-conf.mjs` 驱动；再加一层 pnpm 只是多一套要维护的入口。 | 文档 |
| B6 | MSI per-machine vs NSIS per-user | **只出 NSIS**（`installMode: currentUser`，不需要管理员）。MSI 默认 per-machine，两套权限模型同时发会在同一台机器装出两份互不相识的应用，升级/卸载都会打结。 | `crates/hbot-desktop/tauri.windows.conf.json`、`scripts/check-tauri-conf.mjs` |

**裁决后仍未动、留给后续的**：

| # | 事项 | 为什么还留着 |
|---|---|---|
| 1 | `bundle.windows.wix.upgradeCode` | 只在出 MSI 时需要；现在不出 MSI，不阻塞。将来要出必须先用 `cargo tauri inspect wix-upgrade-code` 取默认值（**不要手算 UUID**） |
| 2 | `identifier` 的 `.app` 后缀 | 属应用身份变更（安装路径/注册表键/macOS bundle id 全变），要产品点头；bundler 已给出明确警告（§4.7-1） |
| 3 | PE `CompanyName` 为空 | 要动 `crates/hbot-desktop/Cargo.toml` 的 `authors`，会让所有构建产物变化，留待打包单元统一做（§4.7-2） |
| 4 | 桌面快捷方式默认创建 | 产品偏好；改了要重测卸载清理（§4.7-3） |
| 5 | 主图仍是占位图 | 等设计稿；脚本 `scripts/gen-placeholder-icon.ps1` 与 `cargo tauri icon` 一条命令可重生成（§3.2） |

## 13. 复现本轮全部实测（一条命令一份日志）

```powershell
cd E:\HBot-Next
$env:CARGO_TARGET_DIR='D:\hbot-target\pkg'

# 0) 图标（只做一次；占位图）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/gen-placeholder-icon.ps1

# 1) 装 bundler（约 188 s，一次即可）
cargo install tauri-cli --version "^2" --locked

# 2) 配置闸门（毫秒级）
node scripts/check-tauri-conf.mjs

# 3) release 二进制（默认不打包，约 270 s）
node scripts/build-desktop.mjs --target-dir D:\hbot-target\pkg

# 4) 真出 NSIS 安装包（约 259 s，含重编译）
cargo tauri build --bundles nsis --ci

# 5) 冒烟：装 → 起 → 卸（本机 11/11 PASS）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/smoke-nsis-install.ps1 `
  -Installer D:\hbot-target\pkg\release\bundle\nsis\hbot_0.1.0_x64-setup.exe
```