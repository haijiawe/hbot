# S5 桌面打包配置（Tauri 2 bundle）

- 状态：**配置就绪 + 部分实测**。已实测：配置文件结构经校验脚本 56 项通过、配置被 `tauri-build` 编译期解析（含反证）、release 二进制已构建。**未实测：任何安装包本体**（本机没有 `tauri-cli`，未跑 bundler）。
- 日期：2026-11-16
- 环境：Windows 11 专业版 10.0.26300 / Node v24.21.0 / rustc 1.96.0 (ac68faa20 2026-05-25) / release（`lto="thin"`、`codegen-units=1`、`strip="symbols"`，见根 `Cargo.toml` 的 `[profile.release]`）/ 窗口 1280×820
- 依赖版本（`Cargo.lock` 实测）：tauri 2.12.5、tauri-build 2.7.4、tauri-codegen 2.7.4、tauri-runtime 2.12.2、tauri-utils 2.10.2
- 关联：[S2 Tauri 同进程](S2-tauri.md)、[02 架构](../02-architecture.md) §14 性能预算、[ADR-0002](../adr/0002-agent-in-rust.md)

## 1. 目的

让 `crates/hbot-desktop` 的 Tauri 2 打包配置**成为可打包的形状**，并把“哪些已经验过、哪些必须等真跑 bundler”写清楚：

1. `bundle.active=true` + 三平台各自的目标格式（Windows `msi`/`nsis`、Linux `deb`/`appimage`、macOS `dmg`）；
2. 安装包元数据齐全（`publisher` / `copyright` / `category` / 长短描述 / 5 个图标）；
3. 配置**能被 Tauri 2 解析**（字段名对齐 schema，不是自己编的）；
4. 量出“配置就绪”离“安装包产出”还差什么（工具链、系统依赖、签名账号）。

**不在本 Spike 范围**：真跑 `cargo tauri build`、产出/安装/升级安装包、代码签名与公证、改 `.github/workflows/ci.yml`（属发布流程，需用户点头）。

## 2. 目标产物清单（文件名按 bundler 命名规则**推断，未实测**）

配置里 `productName=hbot`、`version=0.1.0`，产物落在 `<CARGO_TARGET_DIR>/release/bundle/<格式>/`。

| 平台 | `bundle.targets` | 预期产物 | 权限模型 |
|---|---|---|---|
| Windows x64 | `msi` | `hbot_0.1.0_x64_en-US.msi` | **per-machine**（MSI 默认装到 `Program Files`，需管理员；见 §6 B6） |
| Windows x64 | `nsis` | `hbot_0.1.0_x64-setup.exe` | **per-user**（配置里 `nsis.installMode="currentUser"`，装到 `%LOCALAPPDATA%`，不弹 UAC） |
| Linux x64 | `deb` | `hbot_0.1.0_amd64.deb` | 系统包（`/usr/lib/hbot`，`dpkg -i` 或 `apt install ./…`） |
| Linux x64 | `appimage` | `hbot_0.1.0_amd64.AppImage` | 免安装单文件（运行需 `libfuse2`，见 §5） |
| macOS Intel | `dmg` | `hbot_0.1.0_x64.dmg`（内含 `hbot.app`） | 拖入 `/Applications` |
| macOS Apple Silicon | `dmg` | `hbot_0.1.0_aarch64.dmg` | 同上；**需在 arm 机或加 `--target aarch64-apple-darwin` 才能产出** |

> 命名规则的“推断”依据：`productName`、`version` 与目标三元组是 tauri-bundler 的固定拼接位（`{product}_{version}_{arch}{,-setup}{.ext}`），本仓库未产出过任何安装包，**上表 6 个文件名一个都没被验证过**。第一次真跑 bundler 后应以实际文件名回填本表。

**安装包内含什么**：只含桌面壳（Rust 静态链接的 `hbot-desktop` 二进制）+ `apps/web/dist` 的 Web 资源；**不含 Node.js 运行时**（业务 JS 由 `rquickjs`/QuickJS 在进程内执行）。这是安装包能压在 70 MB 以内的前提。

## 3. 配置改动

### 3.1 `crates/hbot-desktop/tauri.conf.json`（整体重写，关键字段）

```json
{
  "productName": "hbot",
  "version": "0.1.0",
  "identifier": "com.hbot.app",
  "build": { "frontendDist": "../../apps/web/dist" },
  "app": {
    "windows": [{ "title": "hbot", "width": 1280, "height": 820, "minWidth": 960, "minHeight": 600, "resizable": true, "fullscreen": false }],
    "security": { "csp": null }
  },
  "bundle": {
    "active": true,
    "targets": ["msi", "nsis"],
    "publisher": "HBot",
    "copyright": "Copyright © 2026 HBot contributors",
    "license": "MIT OR Apache-2.0",
    "category": "DeveloperTool",
    "shortDescription": "QQ 机器人低代码框架桌面端",
    "longDescription": "HBot 是 QQ 机器人的低代码开发框架：内置 Monaco 编辑器、Flow 可视化画布与 Agent 面板；业务能力统一经 hbot-rpc 的 dispatch 暴露给前端，数据落在 SQLite（hbot-store）。本安装包只含桌面壳与 Web 资源，不含 Node 运行时。",
    "createUpdaterArtifacts": false,
    "icon": ["icons/32x32.png", "icons/128x128.png", "icons/128x128@2x.png", "icons/icon.png", "icons/icon.ico"],
    "windows": { "nsis": { "installMode": "currentUser" } },
    "linux": { "deb": { "section": "devel" } },
    "macOS": { "minimumSystemVersion": "10.15", "hardenedRuntime": true }
  }
}
```

改动前后差异：

| 字段 | 改前 | 改后 | 为什么 |
|---|---|---|---|
| `bundle.active` | `false` | `true` | 不开就永远不出安装包 |
| `bundle.targets` | `"all"` | `["msi","nsis"]`（+ 平台覆盖） | `"all"` 在 Windows 上会试图出 `deb`/`dmg` 然后报“不支持的平台”；见 §3.2 |
| `bundle.icon` | `[]`（空） | 5 个文件全列 | 空数组会让 bundler 拿不到图标；`icon.ico` 还是 `tauri-build` 编译 Windows 资源时的硬需求 |
| `publisher`/`copyright`/`category`/`shortDescription`/`longDescription` | 全缺 | 已补 | 缺失时 MSI/deb/dmg 的元数据是空的（`category` 缺失 deb 会落到默认分类，Windows 开始菜单分组也不对） |
| `version` | 缺（随 crate 0.1.0） | `"0.1.0"` | 显式写死，被校验脚本钉住与根 `Cargo.toml` 的 `[workspace.package] version` 一致 |
| `createUpdaterArtifacts` | 缺（默认 false） | `false` | 显式声明“不出自动更新包”，避免以后误开又没人验签名链 |
| `windows.nsis.installMode` | 缺（默认 `currentUser`） | `currentUser` | 写明产品意图：NSIS 装到用户目录、不弹 UAC |
| `linux.deb.section` | 缺（默认 `utils`） | `devel` | 这是开发工具，不该进 `utils` |
| `macOS.minimumSystemVersion` | 缺（默认 `10.13`） | `10.15` | WebView2 的对位物是系统 WKWebView，10.15 是 Tauri 2 的合理下限 |
| `macOS.hardenedRuntime` | 缺（默认 true） | `true` | 公证（notarization）的硬前提，显式写出来 |
| `identifier` | `com.hbot.app` | **未动** | 已写进 `AGENTS.md` 与 ADR，改了等于换应用身份（安装路径、注册表键、macOS bundle id 全变） |

### 3.2 为什么要三个**平台覆盖文件**（本 Spike 的核心设计决定）

`bundle.targets` 在 base config 里只有一个位置。三平台要各出不同格式，只能靠 Tauri 的**平台覆盖**机制：

- `crates/hbot-desktop/tauri.windows.conf.json` → `{"bundle": {"targets": ["msi", "nsis"]}}`
- `crates/hbot-desktop/tauri.linux.conf.json` → `{"bundle": {"targets": ["deb", "appimage"]}}`
- `crates/hbot-desktop/tauri.macos.conf.json` → `{"bundle": {"targets": ["dmg"]}}`

**这些文件不是我们自己发明的约定**，是 `tauri-utils` 内建行为（`tauri-utils-2.10.2/src/config.rs` 的 `Config` 文档段：*“…read a platform-specific configuration from `tauri.linux.conf.json`, `tauri.windows.conf.json`, `tauri.macos.conf.json`…”*），且**在编译期就被合并**：本机 `cargo build -p hbot-desktop --release` 的 `cargo:rerun-if-changed` 输出里出现了 `E:\HBot-Next\crates\hbot-desktop\tauri.windows.conf.json`，说明 `tauri-build` 确实读了它。

三个覆盖文件**只写 `targets`**（校验脚本会拒绝在覆盖文件里放多余字段）：一旦把 `icon`、`windows`、`macOS` 也抄进去，同一份事实就有两处真源，改一处忘另一处立刻分叉。

> 副作用（已知）：base config 的 `["msi","nsis"]` 只是“默认值”，真正生效的是覆盖文件。Linux/macOS 的 CI 上如果覆盖文件缺失，就会退化成 base 的 `msi/nsis` 并在非 Windows 上失败 —— 所以覆盖文件是**入仓的必需文件**，不是可选优化。

### 3.3 `frontendDist` 前置条件（必须先有 `apps/web/dist`）

`build.frontendDist = ../../apps/web/dist`（相对 config 文件所在目录，即 `E:\HBot-Next\apps\web\dist`）。**该目录在 `.gitignore` 里**（第 4 行 `dist/`），干净检出后不存在。缺了它，`tauri-build` 直接编译失败，错误原文（`tauri-codegen-2.7.4/src/context.rs:185-189`）：

```
The `frontendDist` configuration is set to "<path>" but this path doesn't exist
```

所以打包顺序**只能是**：

```powershell
pnpm --dir apps/web install --frozen-lockfile   # 干净环境
pnpm --dir apps/web build                       # 产出 apps/web/dist
cargo tauri build                               # 才能打包
```

`scripts/build-desktop.mjs` 把上面三步串起来（默认**不跑** bundler，只到 `cargo build --release` 为止）。

## 4. 实测证据

### 4.1 配置结构校验（本单元新增脚本，可重复跑）

```powershell
cd E:\HBot-Next
node scripts/check-tauri-conf.mjs
```

退出码 **0**，`56 项通过 / 1 条警告`，末行 `结论：通过 56 项检查（警告 1 条）— 配置可被 Tauri 2 解析`。校验内容分五组：

1. **结构**：`$schema`、`productName`、`version`（semver，且与根 `Cargo.toml` 的 workspace version 一致）、`identifier`（冻结为 `com.hbot.app`）、`build.frontendDist`、`app.windows`、`app.security`；
2. **bundle 必需字段**：`active=true`、`targets` 数组且取值在 `deb/rpm/appimage/msi/nsis/app/dmg` 内、`publisher`/`copyright`/`license`/`category`（按 Tauri 的 `AppCategory` 清单比）/`shortDescription`（≤80 字、单行）/`longDescription`（≥40 字）/`createUpdaterArtifacts=false`；
3. **图标**：5 个路径全部存在；PNG 读 IHDR 校验实际像素与文件名一致；`icon.ico` 校验 6 字节 ICO 头 + 目录项数；缺 `.icns` 只警告（macOS 打包才需要）；
4. **前端产物**：`frontendDist` 目录存在、`index.html` 存在且含 `<script>` 且不是 dev 入口、`assets/` 非空；报告体积；
5. **crate 接线**：`Cargo.toml` 有 `tauri = "2"` 与 `build-dependencies.tauri-build = "2"`；`build.rs` 调用了 `tauri_build::build()`；三份平台覆盖各自 `targets` 集合正确且**只覆盖 `bundle.targets`**。

唯一警告：`缺少 icons/icon.icns（macOS 打包需要，Windows/Linux 不影响）`。

### 4.2 反证：字段名写错会被**编译期**拒绝（不是我们脚本的一面之词）

为了证明“配置真的被 Tauri 解析”而不只是被我们的脚本认可，往 `bundle` 里临时塞一个不存在的字段 `"totallyUnknownField": true`，再跑 release 构建：

```
error: failed to run custom build command for `hbot-desktop v0.1.0 (E:\HBot-Next\crates\hbot-desktop)`

unknown field `totallyUnknownField`, expected one of `active`, `targets`, `createUpdaterArtifacts`,
`publisher`, `homepage`, `icon`, `resources`, `copyright`, `license`, `license-file`, `licenseFile`,
`category`, `fileAssociations`, `short-description`, `shortDescription`, `long-description`,
`longDescription`, `use-local-tools-dir`, `useLocalToolsDir`, `external-bin`, `externalBin`,
`windows`, `linux`, `macOS`, `macos`, `iOS`, `ios`, `android`
```

退出码 **101**（构建失败）。这条错误信息本身就是 Tauri 2 的 **`BundleConfig` 权威字段清单**，也证明 `deny_unknown_fields` 生效 —— 拼错字段名不会静默忽略，会红。随后已删除该字段并用备份校验 SHA256 一致。

> 结论：`tauri.conf.json` 的字段名是被 `tauri-utils 2.10.2` 的 `Deserialize` 强校验的；我们的校验脚本只是把这个反馈提前到“不编译也能查”，不是另立一套标准。

### 4.3 release 二进制构建（验收命令）

```powershell
cd E:\HBot-Next
$env:CARGO_TARGET_DIR='D:\hbot-target\pkg'
cargo build -p hbot-desktop --release
```

结果：**退出码 0**，耗时 **253.6 s（4 分 12 s）**，产物：

| 文件 | 字节 | MiB | MB（十进制） |
|---|---|---|---|
| `D:\hbot-target\pkg\release\hbot-desktop.exe` | 34,992,128 | **33.37 MiB** | 34.99 MB |

本次会话里同一条命令**成功过两次**：06:06 那次 34,800,640 B（33.19 MiB，320.3 s）、06:24 那次 34,992,128 B（33.37 MiB，253.6 s）。两次之间**只差 191,488 B**，来源是并行单元同时在改 `crates/hbot-desktop/src` 与 `crates/hbot-rpc`，**与打包配置无关**（打包配置只影响 Windows 资源与前端内嵌，量级是 KB~MB，不会两次不同）。

与既有实测对齐：`hbot.exe` 16.2 MB、`hbot-desktop.exe` 33.0 MB（[S2](S2-tauri.md) 记录 34,644,480 B = 33.04 MiB）。本轮 34,992,128 B 比 S2 那次大 347,648 B（+0.33 MiB）—— 同期 `crates/hbot-desktop/src/lib.rs` 被并行单元加了数百行，属正常增长。

> 这 320 s 不是冷编译：依赖已在 §4.2 反证那轮编进同一个 `CARGO_TARGET_DIR`，本轮实际只重编了 `tauri` → `wry` → `hbot-desktop` 的尾段。**根 `Cargo.toml` 的 `[profile.release]` 是 `lto="thin"` + `codegen-units=1` + `strip="symbols"`**，链接段是主要耗时来源；换机器或改 profile 后这个数字不具参考性。
>
> **重跑提示**：本仓库当时是并行开发树，其他单元正在改 `crates/hbot-rpc` / `crates/hbot-code`。本文写作期间对同一条命令重跑过两次：一次撞上 `crates/hbot-rpc/src/app/debug/mod.rs` 的 `E0716`、一次撞上 `crates/hbot-code/src/instrument.rs` 的 `E0308/E0599`（都是**别人在途的半成品**）；第三次编译到链接阶段报 `rustc-LLVM ERROR: IO failure on output stream: no space on device` —— 那是 **D: 盘被并行构建塞满**（当时 `D:\hbot-target` 下 31 个单元目录共 85.72 GB、可用仅 2.84 GB），也不是配置问题。判定“本单元的验收是否通过”，只看 `tauri.conf.json` 这一侧：先 `node scripts/check-tauri-conf.mjs`（exit 0），再 `cargo build -p hbot-desktop --release`（磁盘要有富余空间，链接期临时占用以 GB 计）。

这 0.19–0.35 MiB 的差异**未做归因实验**（可能来自本次真正编进 exe 的 Windows 资源，也可能只是并行单元改动与不同 `CARGO_TARGET_DIR` 下的代码布局差异）。不把它写成结论。

### 4.4 `bundle` 元数据确实进了二进制（不只是进了 JSON）

光“配置文件合法”不等于“元数据生效”。对产物做了两项只读探测（都在 `D:\hbot-target\pkg\release\hbot-desktop.exe` 上）：

| 探测 | 方法 | 结果 |
|---|---|---|
| VERSIONINFO 字符串 | 按 UTF-16LE 在文件字节里找 `Copyright` / `hbot` / `HBot` / `0.1.0` | **四项全部命中** → 编译期写进了 PE 资源 |
| 可执行文件图标 | `[System.Drawing.Icon]::ExtractAssociatedIcon($exe)` | 取到 **32×32** 图标 → `bundle.icon` 里的 `icon.ico` 被编成了 PE 图标资源（ico 内只有一张 256×256，32×32 是 Windows 自己缩的） |

依据：`tauri-build-2.7.4/src/lib.rs` 在 Windows 上取 `bundle.icon` 里第一个 `.ico`（找不到才退回 `icons/icon.ico`）去编译资源。这条链路**本轮有实测支撑**；而“MSI/NSIS 里的图标怎么呈现”仍属 §9 未验证项。

### 4.5 前端产物体积（打包会把整份 `dist` 内嵌进二进制）

`apps/web/dist`（在本机已存在，来自一次 vite 构建）：

| 项 | 数值 |
|---|---|
| 文件总数 | 197 |
| 总大小 | **58.16 MiB** |
| `.map`（sourcemap） | **96 个 / 44.20 MiB（76%）** |
| `.js` | 96 个 / 13.61 MiB |
| `index.html` | 445 B |

> 数值会随前端构建浮动：写作过程中另一个单元重建过一次 `dist`，同一台机器两次测得 58.11 MiB / 44.16 MiB 与 58.16 MiB / 44.20 MiB。**看十位就够了**，别把它当基线钉死。

**这是打包体积的最大隐患**：44.20 MiB 的 sourcemap 会被 brotli 压缩后原样塞进安装包（Tauri 用 `EmbeddedAssets` 内嵌 `frontendDist` 全目录）。生产打包应在 vite 侧关掉 sourcemap —— 但那要改 `apps/web` 的 vite 配置，**不在本单元可改范围**（见 §11 跨单元问题 ①）。

## 5. 平台前置工具（三平台各缺什么）

### Windows（本轮唯一有构建能力的平台）

| 需要 | 用途 | 现状 |
|---|---|---|
| `tauri-cli` 2.x | 唯一能跑 bundler 的入口（`cargo tauri build`） | **本机未装**：`cargo tauri` → `error: no such command: 'tauri'`。装法 `cargo install tauri-cli --version "^2" --locked` |
| WiX Toolset 3.14 | 出 MSI | 由 `tauri-cli` 首次打包时**自动下载**到 `%LOCALAPPDATA%\tauri`（离线环境需自己预置） |
| NSIS 3.11 + `nsis_tauri_utils` 插件 | 出 NSIS `.exe` | 同上，自动下载 |
| WebView2 运行时 | 应用运行必需 | 配置用默认 `webviewInstallMode` = `downloadBootstrapper`（`tauri-utils` 的 `WebviewInstallMode::default()` 实测就是这个值）：安装器**联网**拉 bootstrapper；目标机没网就得改离线安装器（+~127 MB，见 §7） |
| 代码签名证书 | 消 SmartScreen 警告 | **无**（未配 `certificateThumbprint`/`signCommand`，产物是未签名 exe） |

### Linux

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev \
  patchelf libayatana-appindicator3-dev build-essential curl wget file
```

- `deb`：`dpkg-deb` / `ar`（`binutils` 自带）；
- `AppImage`：`tauri-cli` 自动下 `linuxdeploy`，另需 `squashfs-tools`；**运行时需 `libfuse2`**（Ubuntu 22.04+ 默认没有），或用 `--appimage-extract-and-run`；
- 未在本机验证：本机是 Windows，Linux 一列**全部未实测**。

### macOS

| 需要 | 用途 |
|---|---|
| Xcode Command Line Tools | 编译 + `hdiutil`/`codesign` |
| **`icons/icon.icns`** | `.app` 图标。**当前仓库没有这个文件**（只有 png/ico），macOS 打包会缺图标（甚至报错） |
| Apple Developer ID（`Developer ID Application` 证书） | 签名，否则 Gatekeeper 拦 |
| App Store Connect API Key / Apple ID + 应用专用密码 | `notarytool` 公证，否则用户首次打开要手动绕过 |

生成 icns 的方法（需 1024×1024 主图，当前 `icon.png` 只有 256×256，**不够**）：

```bash
# 有 tauri-cli 时一条命令生成全套（含 icns）
cargo tauri icon path/to/icon-1024.png
```

## 6. 已知阻塞

| ID | 阻塞 | 影响 | 解法 |
|---|---|---|---|
| B1 | **本机没有 `tauri-cli`，bundler 从未跑过** | 5 类安装包一个都没产出；`bundle` 段里除字段名外的语义（图标怎么进 MSI、deb 依赖怎么填）全部未经检验 | `cargo install tauri-cli --version "^2" --locked`（约 3–5 min） |
| B2 | Linux 缺系统库（`libwebkit2gtk-4.1-dev` 等） | 在没装这些的容器/机器上 `cargo build` 就失败，谈不上打包 | §5 的 `apt-get` 一行 |
| B3 | **缺 `icons/icon.icns`**，且主图仅 256×256（`icon.png`） | macOS `.app`/`.dmg` 图标缺失或打包报错；高 DPI 与商店展示也吃亏 | 提供 1024×1024 主图 → `cargo tauri icon` 重生成全套 |
| B4 | macOS 公证需要 Apple Developer 账号（$99/年） | `.dmg` 未签名未公证 → 用户首次打开被 Gatekeeper 拦 | 买账号 + 配 `APPLE_CERTIFICATE`/`APPLE_ID`/`APPLE_TEAM_ID` 环境变量 |
| B5 | Windows 无代码签名证书 | SmartScreen 报“未知发布者”，用户需点“仍要运行” | 买 OV/EV 证书；或用 `bundle.windows.signCommand` 接自建签名 |
| B6 | **权限模型不一致**：MSI 默认 per-machine（要管理员），NSIS 配成 `currentUser`（不要管理员） | 同一版本两个安装器装到不同位置，用户混装会出现两份应用 | 产品决策：要么给 MSI 也做 per-user（Tauri 的 `WixConfig` 没有 per-user 开关，只能改模板），要么对外只发 NSIS |
| B7 | WebView2 靠 `downloadBootstrapper` **联网下载** | 内网/离线机器装不上 | 换 `offlineInstaller`（+~127 MB，破 70 MB 预算）或随包分发 |
| B8 | `frontendDist` 内嵌 44.20 MiB sourcemap | 安装包虚胖（估算 +10–15 MB 未压缩，压缩后 +3~6 MB） | `apps/web` 的 vite 关 sourcemap（跨单元） |
| B9 | MSI 的 `upgradeCode` 未固定 | 现在由 `productName` 派生；一旦改 `productName`，MSI 会被当成**另一个应用**，升级链断 | 首次发版前用 `cargo tauri inspect wix-upgrade-code` 取默认值并写进 `bundle.windows.wix.upgradeCode`（**不要手算 UUID 猜**） |

## 7. 体积预算（安装包 < 70 MB）

| 项 | 数值 | 性质 |
|---|---|---|
| 安装包预算 | **< 70 MB** | 需求给定 |
| release 二进制 `hbot-desktop.exe` | **33.37 MiB（34,992,128 B）** | **本轮实测**（同日另一次构建 33.19 MiB） |
| 参照：`hbot.exe` / `hbot-desktop.exe` | 16.2 MB / 33.0 MB | 既有实测（[S2](S2-tauri.md)） |
| 内嵌前端资源 | 58.16 MiB（未压缩；brotli 后估 12–18 MiB） | `dist` 实测，压缩后为估算 |
| NSIS `.exe` | 约 **24–33 MB** | **估算**（LZMA 对已内嵌压缩的资源增益有限，5%–20%） |
| MSI | 约 **25–34 MB** | **估算**（cabinet LZX） |
| deb | 约 **24–32 MB** | **估算**（xz） |
| AppImage | 约 **33–36 MB** | **估算**（squashfs，几乎不额外压缩） |
| dmg | 约 **33–36 MB** | **估算**（UDZO） |

**判断**：按估算全部落在 70 MB 内，**但一个都没实测**。最容易破预算的两种情况：① 换 WebView2 离线安装器（+~127 MB，直接破）；② 全量 sourcemap 内嵌（若生产构建没关，B8）。第一次真跑 bundler 后必须用 `Get-Item … | Select Length` 回填上表。

## 8. CI 提示（**仅文档，未改 `.github/workflows/ci.yml`**）

现有 `ci.yml` 有 `rust`（ubuntu+windows）、`web`、`gates` 三个 job，**没有任何打包 job**。加打包属发布流程变更，需用户点头，这里只给草案：

```yaml
  bundle:
    needs: [rust, web]
    strategy:
      fail-fast: false
      matrix:
        include:
          - { os: windows-latest, target: '' }
          - { os: ubuntu-22.04,   target: '' }
          - { os: macos-14,       target: 'aarch64-apple-darwin' }
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Linux 系统依赖
        if: runner.os == 'Linux'
        run: |
          sudo apt-get update
          sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev \
            patchelf libayatana-appindicator3-dev build-essential
      - uses: pnpm/action-setup@v4
      - uses: actions/setup-node@v4
        with: { node-version: 22, cache: pnpm }
      - run: pnpm install --frozen-lockfile
      - run: pnpm --dir apps/web build          # 必须先有 dist
      - run: node scripts/check-tauri-conf.mjs  # 配置闸门，几十毫秒
      - uses: tauri-apps/tauri-action@v0        # 会装 tauri-cli + WiX/NSIS
        with:
          args: ${{ matrix.target && format('--target {0}', matrix.target) || '' }}
      - uses: actions/upload-artifact@v4
        with: { name: 'bundle-${{ matrix.os }}', path: 'target/*/release/bundle/**' }
```

要点：
- **必须在 `cargo tauri build` 之前 build 前端**，否则 `frontendDist` 不存在直接失败（§3.3）；
- Linux runner 必须装 §5 那行系统依赖；
- `ubuntu-22.04` 而非 `latest`：24.04 的 `libwebkit2gtk` 包名/ABI 更麻烦，22.04 是 Tauri 官方文档靶子；
- macOS 交叉出双架构要分别 `--target`，或直接用 `macos-13`(x64) + `macos-14`(arm) 两个 runner；
- 签名/公证所需的 secret 未配 → 产物是未签名包（能产出，用户侧会被拦）。

## 9. 未验证清单（**必须等真跑 bundler 才知道，别当成已通过**）

1. **`tauri-cli` 能否解析本配置并成功打包** —— 本机无 CLI，`bundle` 段只被 `tauri-build`（编译期配置合并）校验过，**未经 bundler 校验**；
2. **6 个安装包文件能否真的产出**（§2 表格里的文件名全是推断）；
3. **安装包真实体积**（§7 整张表是估算）；
4. **安装 → 从开始菜单/`/Applications` 启动 → 卸载**全链路（窗口能否起来、`frontendDist` 资源能否被正确内嵌并找到、SQLite 数据目录在安装位置下是否可写）；
5. **升级路径**：MSI `upgradeCode` 稳定性、NSIS 覆盖安装、`deb` 版本比较；
6. **安装器界面**：NSIS 的 `installerIcon`/`headerImage` 未配（用默认），debian 的 desktop 图标与分类是否显示正确；
7. **签名与公证**：B5/B4 全未做，产物在用户机器上的实际拦截行为未观测；
8. **WebView2 bootstrapper 行为**：目标机无 WebView2 且离线时安装器的表现未观测（B7）；
9. **Linux / macOS 上的实际构建**：本机是 Windows，两个平台连编译都没跑过；
10. **从安装包启动的冷启动时间与内存**：S2 只测了“直接跑 `target/release/*.exe`”，安装后的路径/资源布局不同；
11. **`shortDescription` 含中文在 Windows 开始菜单/文件属性里的显示**（编码问题只在真机安装后能看到）；
12. **AppImage 在 `libfuse2` 缺失时的报错**与 `--appimage-extract-and-run` 兜底。

## 10. 结论

**配置就绪（部分实测），未到“可发版”**。

- ✅ `bundle.active=true`、三平台 targets 经平台覆盖文件表达、元数据与 5 个图标齐全、`identifier` 未动；
- ✅ 配置结构经脚本 56 项校验通过，且**反证过**字段名由 Tauri 2 编译期强校验（写错即 101）；
- ✅ `cargo build -p hbot-desktop --release` 退出码 0，产物 **33.37 MiB（34,992,128 B）**，且探测到 VERSIONINFO 与图标已编入；
- ❌ **没有任何安装包产出**：本机无 `tauri-cli`（B1），5 类格式、真实体积、安装/卸载/升级、签名、Linux/macOS 全部在 §9 的未验证清单里。

**下一步（按性价比）**：① `cargo install tauri-cli --version "^2" --locked` 后真跑一次 `cargo tauri build --bundles nsis`（最快、无需签名证书也能出包），用实测值回填 §2 与 §7；② 补 1024×1024 主图 + `icon.icns`（B3）；③ 与 web 单元收敛 sourcemap（B8）；④ 定 B6 的权限模型与 B4/B5 的账号。

## 11. 跨单元问题（lead 已在 2026-11-16 裁决，见 §12）

1. **`apps/web` 的 vite 构建带 sourcemap**（已解）：关掉了。
2. **根 `package.json` 没有 desktop 入口**（已决：不加）。
3. **版本号是手写的**（保留，脚本已钉）。
4. **MSI per-machine vs NSIS per-user**（已决：只出 NSIS）。

## 12. lead 裁决（2026-11-16）

| # | 问题 | 裁决 | 落点 |
|---|---|---|---|
| B8 | sourcemap 进安装包 | **关掉**：`apps/web/vite.config.ts` 的 `build.sourcemap: false`。实测 `dist` 从 **58.16 MiB → 13.96 MiB**、`.map` 文件 **44.20 MiB → 0**。dev server 的 sourcemap 在内存里，调试体验不受影响。 | `apps/web/vite.config.ts` |
| — | 根 `package.json` | **不加**。这是 Rust-first 仓库，桌面包由 `node scripts/build-desktop.mjs`（`--bundle` 才调 bundler）与 `node scripts/check-tauri-conf.mjs` 驱动；再加一层 pnpm 只是多一套要维护的入口。命令写进 `README.md` 与本文件的 §4。 | 文档 |
| B6 | MSI per-machine vs NSIS per-user | **只出 NSIS**（`installMode: currentUser`，不需要管理员）。MSI 默认 per-machine，两套权限模型同时发会在同一台机器装出两份互不相识的应用，升级/卸载都会打结。要恢复 MSI 必须先定 per-machine 的升级与卸载策略，不是改一行 targets 的事。`scripts/check-tauri-conf.mjs` 的期望值同步改成 `["nsis"]`，并把理由写在断言旁边（不许有人"顺手补回 msi"）。 | `crates/hbot-desktop/tauri.windows.conf.json`、`scripts/check-tauri-conf.mjs` |

仍然**没有**动、留给真跑 bundler 的：`bundle.windows.wix.upgradeCode`（只在出 MSI 时需要，现在不出 MSI，所以不阻塞）、`.icns`（macOS 打包前必须补）、`ci.yml` 的三平台 bundle job（需要 `tauri-cli`，本机与 CI 都还没装）。
5. **WiX `upgradeCode` 应在首版前固定**（B9），需 CLI 的 `tauri inspect wix-upgrade-code`，**不要手算 UUID**。
6. **`ci.yml` 未改**：打包仍不在 CI 里（§8 只有草案），加 job 属发布流程变更，需用户点头。
7. **缺 1024×1024 主图标**：当前 `icon.png` 仅 256×256，生成不了合格的 `icns`；建议设计侧提供主图后统一重生成图标集。