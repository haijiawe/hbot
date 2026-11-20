# 五分钟上手（桌面端）

目标：从**装好软件**到**看到自己的 bot 回第一句话**，不超过五分钟，中间不需要手写任何配置文件、不需要 QQ 账号。

这一页的每一句都在 `crates/hbot-rpc/tests/quickstart_flow.rs` 里有对应的自动化断言（GUI 的点击没法在测试里点，钉住的是点击背后的命令链）。

## 0. 前提

- Windows 10/11 x64（当前唯一做完的桌面宿主；macOS 只出 dmg 不验证，手机端还没做）。
- 安装包：CI 的 `bundle (windows, nsis)` job 产物，或自己在仓库里 `pnpm --dir apps/web run build` 后 `cargo build -p hbot-desktop`。
- 不需要 QQ 账号，不需要 NapCat / LLOneBot：本地试跑平台（`local`）不联网，回复只进日志。

## 1. 安装（约 30 秒）

双击 `hbot_0.1.0_x64-setup.exe`。安装包**没有代码签名**，所以 Windows 会弹「已保护你的电脑」：点「更多信息」→「仍要运行」。

装完从开始菜单启动 hbot。首屏是一个空工作台：左侧栏、中间编辑器、底部面板（默认在「日志」页签）。

数据目录固定在 `%APPDATA%\hbot`（不是安装目录）：数据库、日志、会话现场都在那里。卸载不会删它。

## 2. 新建项目（约 30 秒）

1. 按 `Ctrl+Shift+P` 打开命令面板；
2. 输入 `新建项目`，选中「新建项目…（粘贴绝对路径）」回车；
3. 项目路径填一个**还不存在**的目录，例如 `C:\work\my-bot`；
4. 项目名称填 `我的第一个机器人`，点「创建」。

成功后状态栏会显示「已新建项目：C:\work\my-bot」，左侧文件树里出现：

```text
hbot.json        清单：入口、平台、权限、限额、要开的能力
src/main.ts      你的 bot 代码
hbot.d.ts        宿主 API 的类型声明（自动生成，别手改）
bots.json        连接配置：新建时自带一条 local 平台条目
AGENTS.md        给 IDE 内置 Agent 的项目说明
tsconfig.json / .gitignore
```

`bots.json` 里那条条目长这样（id 是创建时生成的 UUID）：

```json
{
  "bots": [
    { "id": "<UUID>", "platform": "local", "name": "我的第一个机器人 本地试跑" }
  ]
}
```

## 3. 启动试跑（约 15 秒）

1. 底部面板切到「试跑」页签；
2. 「选择 bot」下拉里已经出现「我的第一个机器人 本地试跑 · local」——**不需要先做任何配置**；
3. 点「开始试跑」，等状态变成运行中（本地平台会同步建好 `local:default` 会话）。

## 4. 说一句话（约 15 秒）

1. 在「模拟消息」输入框里输入 `你好`，回车；
2. 「回复」区出现 `收到：你好`；「日志」页签里同一时刻有一条 `[reply] 收到：你好`。

到这里就完成了：你的 JS 真的跑在 QuickJS isolate 里，消息走的是**与真实平台完全相同**的入站 → 队列 → handler → 出站链路，只是最后一跳不出网。

## 5. 改代码，立刻看到新行为

在左侧文件树里打开 `src/main.ts`，把 `收到：` 改成别的字，`Ctrl+S` 保存。

保存会触发重编译与新指纹：下一次发消息用的就是新代码（isolate 按项目指纹重建，不需要重启 bot）。如果新代码编译不过，**旧版本继续服务**，错误进「日志」面板——不会让 bot 直接停摆。

## 6. 接真的 QQ（可选，需要账号）

本地试跑不会把消息发到 QQ 上。要真机联调：

1. 装一个 OneBot 11 实现（NapCat / LLOneBot / Lagrange），开正向 WebSocket，记下地址（例如 `ws://127.0.0.1:3001`）；
2. 把 `bots.json` 里那条条目改成：

```json
{
  "bots": [
    {
      "id": "<同一个 UUID>",
      "platform": "onebot11",
      "url": "ws://127.0.0.1:3001",
      "token": "如果 OneBot 那边配了 access_token 就填，没有就删掉这一行",
      "selfId": 10001,
      "name": "我的第一个机器人"
    }
  ]
}
```

3. 回到「试跑」页签，先「停止试跑」再「开始试跑」（清单与连接配置参与热重载指纹，但换平台建议显式重启一次）。

字段全集与语义见 `crates/hbot-rpc/src/app/runtime/config.rs` 的模块文档：`platform` 只认 `onebot11` / `qq_official` / `local`，**写错一律 -32002 并列出允许值**，不会静默降档成 OneBot。QQ 官方机器人用 `platform: "qq_official"` + `appId` + `clientSecret`。

## 7. 常见问题

| 现象 | 原因 | 怎么办 |
| --- | --- | --- |
| 双击安装包弹「已保护你的电脑」 | 安装包没有代码签名 | 「更多信息」→「仍要运行」 |
| 新建项目报 `-32003 conflict` | 目标目录非空，或同一路径已经登记过项目 | 换一个不存在的目录；本项目从不覆盖用户文件 |
| 新建项目报 `-32002 invalid` | 路径为空 / 名称空 / 路径是个文件 | 粘贴绝对路径，例如 `C:\\work\\my-bot` |
| 「试跑」页签里没有 bot | 当前没打开项目，或项目的 `bots.json` 里没有条目 | 先打开项目；`bots.json` 删坏了就新建一条（见 §2） |
| 点「开始试跑」报 `HBOT_*_NOT_DECLARED` | `hbot.json` 的 `capabilities` 里没声明这个能力 | 按错误码补清单（声明层与装配层是两个码，`NOT_DECLARED` 改清单、`UNAVAILABLE` 去设置里配） |
| 发了消息没有回复 | handler 抛错 / 超时 | 看「日志」页签的 error 行，那里有完整的 `HBOT_*` 码 |
| 改了代码没生效 | 编译失败，旧版本在继续服务 | 「问题」面板或日志里找编译错误 |

## 8. 命令行等价物

不想开 GUI 也能跑：

```powershell
cargo run -q -p hbot-cli -- check examples/chat   # 只做清单校验 + 编译，不连平台
cargo run -q -p hbot-cli -- serve                 # 无头宿主，起 127.0.0.1 上的 RPC 服务
cargo run -q -p hbot-cli -- token                 # 打印/轮换无头宿主的访问令牌
```

无头宿主与桌面壳共用同一个数据目录与同一套核心（`crates/hbot-rpc`），差别只有「谁来画界面」。

## 9. 下一步读什么

| 想知道 | 读 |
| --- | --- |
| 能力面（`ctx.kv` / `ctx.ai` / `ctx.jobs` / `ctx.session` …）怎么用、边界在哪 | `docs/07-bot-runtime-blueprint.md` |
| 消息链、去重、每会话串行、沙箱 | `docs/02-architecture.md` |
| 示例项目 | `examples/chat`、`examples/game`、`examples/support` |
| 桌面端冒烟与压测怎么复跑 | `docs/spikes/S14-desktop-acceptance.md`、`docs/spikes/S15-soak.md` |

本页对应的自动化证据：`crates/hbot-rpc/tests/quickstart_flow.rs`（新建 → 列表 → 启动 → 注入 → `[reply]`）、`crates/hbot-rpc/tests/bot_list_declared.rs`（声明与库行不重复）。
