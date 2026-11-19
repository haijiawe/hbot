# HBot-Next 剩余工作总计划（2026-11-20）

> 这是工作排期，不是规范。规范仍是 `AGENTS.md` + `docs/04-implementation-plan.md`。
> 批准后我按 P0 → P6 推进；每段自己过六道闸门、按 crate 拆 commit、回填文档，中途不再回来问。

---

## 1. 起点快照（实测）

| 项 | 值 |
|---|---|
| HEAD | `27a503b`（144 个 commit，工作树干净） |
| Rust 测试 | **1866 passed / 0 failed / 120 target** |
| web | 46 文件 417 用例 |
| 六道闸门 | 全绿（2026-11-20 02:35 实测） |
| git remote | **无** —— 144 个 commit 只在本地磁盘，一次没备份 |
| 能力面 | C1.message/timer/webhook/lifecycle + C2.kv/history/session/sql + C3.reply/send/notify/http + C4.jobs + C5.ai + C6.device（Windows 窄版 + 按窗口截图） |

结论：**能力底座（桌面侧）已经齐了**，缺的是「验收收口」和「要外部条件的里程碑」。

---

## 2. 我替你拍的六个决定

| # | 决定 | 理由 |
|---|---|---|
| D1 | **`onSchedule` 进 MVP，本轮做** | 它与 `onTimer` 的差别只是「清单里的具名计划 + `onSchedule(ctx, name)`」，而 `schedules` 表与 tick 已经存在，成本是小件；不做的话 `timer` 只能表达「固定间隔」，与 `docs/07` §4.1 的钩子清单不一致 |
| D2 | **`E1` npm 打包不做，改写成一条 ADR 结论** | 本项目的多文件 TS 由宿主 SWC 编译、只用相对 import，没有 npm 依赖解析的需求；引入 npm 就要把 Node 工具链放进构建期，与 §0 硬规则 1（运行时不许 Node）与 9（禁止裸包名）的边界极难划清。结论与理由写进 `docs/07` §6 |
| D3 | **`C3.file` 做窄版** | 三个示例项目里有真实的「存档 / 导出 / 知识库文件」需求，而 Agent 有文件工具、bot 代码没有 —— 这是明确的能力缺口。形状：项目根内 `read/write/list/exists`，realpath 必须落在项目根内，单文件 1 MiB，禁绝对路径与 `..` |
| D4 | **先做 GitHub 备份（P0）** | 144 个 commit 只有一份本地副本，是当前最大的单点风险。仓库里 `grep -r "sk-"` 必须为空（已核对过历史提交无密钥） |
| D5 | **`M9` 云端宿主只做本地可写的一半** | `token issue/revoke`、非 loopback 必须显式确认、未配 TLS 拒绝启动、`GET /healthz`、systemd unit + 日志轮转 + 备份脚本 —— 这些不需要服务器也能写完并测；「真云机 24h 不断线」等有机器再验，不写假结论 |
| D6 | **平台移植（M8 Android / M11 iOS / M10 剩余 / M6 影子）本轮不做** | 缺手机、缺 macOS/Linux 机器、缺独立测试账号。没有验证手段的移植代码是负债，不是进度 |

另外两个原本在清单里的东西，我决定**本轮也不做**，理由写在下面第 6 节：

- **UI 等待状态面板 / 跨 bot 转交**：都要新增 `Command` / `Event` 契约并跑 `cargo xtask gen`，属中等契约改动；比它们更值钱的是先让三个示例项目跑通。
- **混合 DPI 跨屏拼图**：必须在 Tauri 壳里做（宿主没声明 Per-Monitor V2 时库层拿到的尺寸是被虚拟化的），要真机多屏，本机只有一块屏。

---

## 3. 阶段划分（每段都能独立验收）

| 阶段 | 内容 | 谁做 | 规模 | 验收判据 |
|---|---|---|---|---|
| **P0** | GitHub 私有仓备份 | 我 | 小 | `git remote -v` 有地址、`git ls-remote` 能看到 144 个 commit |
| **P1** | `C3.file` 全链路（core 契约 → code 槽位与 JS 桥 → dts → rpc 装配 → e2e） | 我 | 中 | 新增 e2e ≥6 条：未声明 / 越界路径 / 超限 / 读回 / 覆盖 / 不存在 |
| **P2** | `onSchedule` 全链路（core 计划类型 → store 复用 `schedules` → code 钩子 → rpc tick → e2e） | 我 | 中 | 新增 e2e ≥4 条：具名计划触发 / 未声明 / 缺钩子点名 / 重启后仍按 deadline |
| **P3** | 三个示例项目 + 1 个打包件（**4 个子代理并行**） | 子代理 | 中 | 每个示例 `hbot check` 退出 0；P6 我再加一条冒烟 e2e 真派发 |
| **P4** | `M9` 本地可写部分（token 子命令、bind 确认、TLS 拒绝、`/healthz`） | 我 | 大 | 新 e2e：非 loopback 无 `--allow-remote` 拒启、无 TLS 拒启、`/healthz` 200、token 撤销后握手失败 |
| **P5** | session 等待超时的**可配置提示**（超时结算时往会话发一句话 + run log 记一行） | 我 | 小 | e2e：超时后会话收到提示、提示文案可被清单关掉 |
| **P6** | 验收对账 + 文档收口（`docs/07` §10 的 V1–V11 逐条填证据、docs/11` 决策记录、`README` / `AGENTS.md` / `HANDOFF`） | 我 | 中 | 六道闸门全绿 + V1–V9 每条都有指向测试的名字 |

顺序：**P0 → P1 → P2 → P3（并行，与 P4 交错）→ P4 → P5 → P6**。
P3 的子代理一启动我就能同时写 P4，两条线不碰同一批文件。

---

## 4. 子代理任务表（写域互不重叠，这是硬约束）

| 名字 | 独占写域 | 交付 | 验收命令（必须自己跑通） | 明令禁止 |
|---|---|---|---|---|
| `example-chat` | `projects/example-chat/**` | 聊天 bot：人设 + `ctx.history` 多轮 + `ctx.kv` 记忆 + `ctx.ai.complete` 兜底。含 `hbot.json` / `bots.json` / `src/main.ts` / `README.md` | `cargo run -q -p hbot-cli -- check projects/example-chat` 退出 0 且 `ok:true` | 改 `crates/**` / `apps/**` / `docs/**`；跑 workspace 级 cargo |
| `example-game` | `projects/example-game/**` | 氪金抽卡：`timer` 每日签到 + `sql` 排行榜 + `jobs` 长结算 + `send`。含 `migrations/0001_init.sql` | 同上（换目录） | 同上 |
| `example-support` | `projects/example-support/**` | 工单客服：`webhook` 收工单 + `sql` 存单 + `ctx.session.prompt` 等用户补充 + `notify` 通知 | 同上 | 同上 |
| `packaging-systemd` | `packaging/**` | `hbot.service`（systemd unit）、日志轮转配置、备份脚本（sqlite `.backup` + 保留 N 份）、`README` 安装说明 | `cargo run -q -p xtask -- deps` 退出 0；脚本逐行自审（本机无 systemd，**不许**在报告里写「已验证启动」） | 改 `crates/**`；声称在 Linux 上跑过 |

### 派单纪律（上一批子代理的教训）

2026-11-19 派的两个子代理，一个没交终报、一个留下编译不过的残骸（`crates/hbot-device/src/windows.rs` 引用不存在的类型，得 `git checkout` 回滚）。所以这次：

1. **写域绝不重叠**，四个任务各占一个目录，我一行都不碰；
2. 每个任务自带**一条能自己跑的验收命令**，跑不通就不许交；
3. 明确禁止改 `crates/**`（示例项目只需要 `hbot.d.ts` 的声明，由 `hbot check` 自动落地）；
4. 每个子代理的报告必须是「改了什么 / 证据（命令+退出码）/ 没做什么 / 风险」四段；
5. 我收工时逐个验收：读文件 + 自己跑一遍验收命令，**不信报告**。

---

## 5. 每段的纪律（不重复解释）

- 六道闸门：`cargo fmt --all -- --check` / `xtask gen --check` / `xtask deps` / `clippy --workspace --all-targets -D warnings` / `cargo test --workspace` / `pnpm --dir apps/web run check`。
- 按 crate 拆 commit：`feat(core)` → `feat(store)` →（需要时 `feat(code)`）→ `feat(rpc)` → `docs`。
- 新增能力照 `AGENTS.md` §8「新增宿主 API」的四处同批改 + §8「新增清单能力」的六步走，不另创形状。
- **不许为了让闸门变绿改断言、删测试、放宽上限**；失败就报失败。
- 每段结束回填 `docs/07` 对应小节（设计决定 + 理由 + 实测条数）、`README` 数字、`AGENTS.md` 新坑、`HANDOFF` §3/§4/§7。

---

## 6. 本轮明确不做（含理由，避免下次重新讨论）

| 项 | 为什么不做的 | 什么时候能做 |
|---|---|---|
| UI 等待状态面板 | 需要新的 `Command`（列 pending 等待）+ `Event` + `xtask gen`，属契约改动；先让示例项目跑通更有价值 | 有人真用 `session` 之后 |
| 跨 bot 转交 | 同上，且要定「转交后原会话怎么回」的语义（现在没定） | 有真实客服场景后 |
| 混合 DPI 跨屏拼图 | 必须改 Tauri 壳的 Per-Monitor V2 声明，且要两块不同 DPI 的真机屏；本机只有一块 | 有双屏机器 |
| macOS / Linux 设备实现 | 没有机器，写完也验证不了 | 有机器 |
| `E1` npm 打包 | 见 D2 | 真有人要「一行 npm i 装依赖」时再谈 |
| `M8` Android / `M11` iOS | 缺手机、缺 Xcode、缺 Android SDK | 有条件时 |
| `M6` 切换与影子 | 缺一份独立测试账号 + 要跑一周 | 有账号时 |

---

## 7. 外部阻塞（拿不到就别硬等）

| 需要什么 | 卡住什么 | 现在怎么办 |
|---|---|---|
| **GitHub 仓库（或授权我建）** | P0 备份 | 我等你一句话；建私有仓 + 推 master，一分钟的事 |
| 一台云主机 / VPS | `M9` 的「24h 不断线」验收 | 只做本地可写的一半，验收口径写清「未在真机验证」 |
| 真 NapCat 扫码登录 | `M2` 的 OneBot 真机证据 | 需要你先把自己的 QQ 退出客户端（二维码会过期） |
| macOS / Linux 机器 | `M10` 平台移植、`M4` 的 Landlock 真跑 | 只在文档里标「仅交叉编译」 |

---

## 8. 风险与回滚

| 风险 | 应对 |
|---|---|
| 子代理又留下编译不过的残骸 | 写域 = 独立目录（`projects/**`、`packaging/**`），最坏情况 `git checkout -- <该目录>` 即可，不影响 crates |
| `C3.file` 越界读写 | realpath 校验一律在宿主侧、在打开文件之前；e2e 必含「`..` 越界」「符号链接逃逸」两条拒绝路径 |
| `M9` 的安全改动开放了远程面 | 默认只绑 127.0.0.1；非 loopback 必须 `--allow-remote` + 必须配 TLS，两者缺一拒绝启动（fail-closed，不留降级路径） |
| 示例项目写出来是「为了跑通而写」 | 每个项目必须用满它对应那类 bot 的能力组合（`docs/07` §3.1 的表），否则不算验收 |

---

## 9. 我需要你给的（只有一件）

**GitHub 仓库**：给我一个仓库名（默认建 `haijiawe/hbot` 的私有仓），或者你自己在 GitHub 上建好空仓库把 SSH/HTTPS 地址给我。

其余全部我自己决策并推进。批准后我从 P0 开始，每完成一个阶段给你一张「单元 / 状态 / 证据 / 未过原因」的表。
