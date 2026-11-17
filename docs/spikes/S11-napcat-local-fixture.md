# S11 · 本机 NapCat 当 OneBot 11 夹具（只读验证，未完成）

> 状态：**未完成（卡在扫码登录）**。2026-11-17。
> 这是一份**本机一次性夹具**的记录，不是"支持的协议端"清单，也不是 CI 的一部分。

## 1. 为什么要它

OneBot 11 适配器到今天只对**假网关**测过（`crates/hbot-adapters/src/platforms/onebot/` 的单元测试 +
`crates/hbot-parity/tests/parity_15_onebot_single_accept.rs`）。真协议端才能验到测试看不见的四件事：

1. 真实事件 JSON 形状（字段拼错、类型不对、`post_type` 分支与文档不一致）；
2. 断线重连与心跳超时的真实节奏（45 秒看门狗是否够、重连时事件是否会丢）；
3. `message_id` 语义（回显、撤回、`reply_to` 是否真能对上）；
4. 群/私聊的 at 规则、消息段（`image`/`face`/`reply`）与去重键。

**不提供它也能继续**：QQ 官方适配器已经真机端到端验过（`S9`），OneBot 这条链路属于"降风险"。

## 2. 本机实物（外部资产，不入库）

| 项 | 值 |
|---|---|
| NapCat 目录 | `D:\机器人\QQ JQR\QQ JQR\napcat\NapCat.Shell.Windows.OneKey\`（OneKey 版，QQ 9.9.26-44498） |
| 启动器 | `bootmain\NapCatWinBootMain.exe`（配 `NapCatWinBootHook.dll` + `bootmain\loadNapCat.js`） |
| OneBot 11 配置 | `bootmain\config\onebot11_2974154439.json` |
| WS 服务 | `{name:"hbot", enable:true, host:"127.0.0.1", port:3001, messagePostFormat:"array", token:"", heartInterval:30000}` |
| WebUI | `http://[::]:6099/webui?token=…` |

`docs/08-multi-host.md` 与 `docs/02-architecture.md` 都**不**引用这个目录：它是本机资产，
换机器就没有。上游仓库已被作者删除，所以这份副本没有升级路径。

## 3. 三个启动坑（都已绕过，记下来省下一次排查）

1. **路径含中文会让启动器把参数截断成 `D:\`**，报 `Error Code: 2`。
   绕过：建 ASCII 目录联接 `mklink /J E:\hbot-target\napcat-link "D:\机器人\…\OneKey"`，
   并把 `bootmain\loadNapCat.js` 里的路径重写成 `file:///E:/hbot-target/napcat-link/bootmain/napcat.mjs`。
2. **`cmd /c` + `Start-Process -WorkingDirectory` 不生效**（报"不是内部或外部命令"）。
   绕过：直接 `Start-Process <bootmain>\NapCatWinBootMain.exe -ArgumentList @(<qq.exe>,<hook.dll>,'<QQ号>') -WorkingDirectory <bootmain>`。
3. **快速登录态会失效**（日志 `快速登录错误： 登录态已失效，请重新登录。`）→ 只能扫码。
   另外 QQ 官方客户端（`d:\Program Files\Tencent\QQNT\QQ.exe`，9.9.26.44343）与 NapCat 自带的
   44498 构建**版本不一致**，必须用 NapCat 目录里那份 `QQ.exe`，否则钩子挂不上。

## 4. 卡在哪

二维码能出（`bootmain\cache\qrcode.png`），扫码那一步需要用户本人操作，且：

- 那是一个**真实 QQ 账号**（2974154439）。**往外发消息**必须先问用户；只读验证（测试 bot 只记日志
  不回话）不需要。
- 本机同时有用户自己的 QQ 在跑，NapCat 自带的 QQ 与它有单实例冲突风险，重启前要先确认。
- 07:38 那次启动的二维码早已过期（19:06 复查：进程已退、3001 未监听）。

## 5. 一旦登录成功，要验的四件事

1. `bot_start` 后 `logs_tail` 里出现一条真实 `lifecycle`/心跳记录（证明 45 秒看门狗不是空转）。
2. 群里 @ 一次，确认入站 `payload_json` 的事件名与判据（**QQ 官方是 `GROUP_MESSAGE_CREATE`，
   OneBot 是 `message`/`message_type:group`，两套别混**）。
3. 一次断网重连（拔网线或 `Stop-Process` 网关），确认没有"重连丢入站"（旧项目 P0 之一）。
4. `send` 的 `reply_to` 是否等于入站 `message_id`（去重与被动回复都依赖它）。

## 6. 明确不做

- 不写进任何 `Cargo.toml`、不进 CI、不写进 README 的"支持平台"表；
- 不为它改适配器代码（只用 `127.0.0.1:3001` 这个 OneBot 11 接口）；
- 不依赖它长期可用（作者已删库，随时可能彻底跑不起来）。