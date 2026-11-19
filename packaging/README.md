# HBot Linux 部署物（systemd / logrotate）

> ## ⚠️ 状态声明（先读这一段）
>
> **这套部署物从未在真实 Linux 主机上运行过。**
>
> 它是在 Windows 机器上写的，作者没有 Linux 主机、没有 `systemd`、没有 `journalctl`
> 可验。文件里的每一行都是照 systemd 文档写的，**不是**从一台跑通的机器上抄下来的。
> 因此：
>
> - `systemd-analyze verify packaging/systemd/hbot.service` 与 `logrotate -d` 都**没有**
>   在本轮跑过（Windows 上没有这两个命令）；
> - 加固选项与服务实际行为的冲突（见下面「加固与命令隔离」）是**推理**结论，
>   不是实测结论；
> - 第一次在真机上装的时候，请从「按最宽松的配置跑通 → 再一条一条加加固」的顺序来。
>
> 唯一被真实执行验证过的部分是 `hbot serve` 本身（它已有单测与真机启动记录），
> 以及本文件里引用的三个 CLI 行为：`hbot serve --allow-remote` 的拒启判据、
> `hbot token --write`、`GET /healthz`（各有单测/集成测试，见 §3 的括号）。
> 本文只负责"怎么把它挂成服务"。

---

## 1. 前置条件

| 项 | 要求 | 说明 |
|---|---|---|
| `hbot` 二进制 | 放到 `/usr/local/bin/hbot` | 与 unit 里的 `ExecStart` 一致；换路径要同步改 unit |
| 专用用户 | `hbot:hbot`，无登录 shell | 不要用 root 跑 |
| 数据目录 | `/var/lib/hbot` | 由 `StateDirectory=hbot` 交给 systemd 创建（`0750`），**不要手工 `mkdir`** |
| token 文件 | `/etc/hbot/hbot.env`，`0600 hbot:hbot` | unit 用 `EnvironmentFile=-` 读它；用 `hbot token --write` 生成，**别自己编一个** |
| 端口 | `127.0.0.1:8787`（unit 里写死） | 要换端口改 `ExecStart` 的 `--bind`，并同步改 §3 的验证命令 |

`hbot serve` 已经能跑：它启动后往 **stdout 打一行 ready JSON**
（`{"type":"ready","addr":"…","pid":N}`），然后等 SIGTERM/SIGINT 优雅关闭
（停机器人 → `handle.shutdown()`）。本目录不改它的任何行为。

## 2. 安装

手工按顺序来（或者用 `packaging/install.sh`，它做的正是下面这几步）：

```bash
# 1) 建用户（无家目录、无登录 shell）
sudo useradd --system --no-create-home --shell /usr/sbin/nologin hbot

# 2) 装二进制
sudo install -Dm755 ./hbot /usr/local/bin/hbot

# 3) 生成 token，写进 systemd 的 EnvironmentFile
#    内容是 `HBOT_TOKEN=<64 位十六进制>` 一行；stdout 只打路径，token 不进终端回滚。
sudo install -d -m 750 -o hbot -g hbot /etc/hbot
sudo -u hbot /usr/local/bin/hbot token --write /etc/hbot/hbot.env
sudo chmod 600 /etc/hbot/hbot.env

# 4) 装 unit 与轮转配置
sudo install -Dm644 packaging/systemd/hbot.service /etc/systemd/system/hbot.service
sudo install -Dm644 packaging/logrotate.d/hbot     /etc/logrotate.d/hbot

# 5) 起服务并设为开机启动
sudo systemctl daemon-reload
sudo systemctl enable --now hbot

# 6) 看状态与日志
systemctl status hbot
journalctl -u hbot -f
```

`--data-dir /var/lib/hbot` 是 unit 里写死的。数据目录里的内容是
`hbot.db`（SQLite，WAL）、`bot_sql/<botId>.db`（每个 bot 一个 SQL 库）与 `logs/`
（崩溃报告 `crash-<时间戳>.log`）。

### 需要 GUI 时

服务跑的是 **headless** `serve`。桌面壳（托盘/窗口/单实例）是另一个二进制入口，
它自己会连同一个数据目录；**不要**在服务器上同时跑两者 —— 单实例保护只覆盖桌面壳，
`serve` 与桌面壳是两个进程，会各开一个数据库写入者（SQLite WAL 能承受，但
`bots` 表的运行状态会分叉）。真要在同一台机器上切换：先 `systemctl stop hbot`。

## 3. 验证清单（真机上按顺序打勾）

```bash
systemctl is-enabled hbot        # 期望 enabled
systemctl is-active hbot        # 期望 active
journalctl -u hbot -n 50 --no-pager   # 期望看到 ready JSON（含 addr 127.0.0.1:8787）与"数据目录"日志

# 活体探针：不需要 token、不需要 WebSocket。给 systemd/反向代理/监控用。
curl -s http://127.0.0.1:8787/healthz        # 期望 {"status":"ok","version":"…"}
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8787/   # 期望不是 200（那是 WS 入口）

ls -la /var/lib/hbot            # 期望 hbot.db、logs/ 都是 hbot:hbot
sudo -u hbot test -r /etc/hbot/hbot.env && echo token-ok
sqlite3 /var/lib/hbot/hbot.db 'select count(*) from bots;'   # 期望不报错（0 也行）
```

前两条 CLI 行为在仓库里有测试钉住：`/healthz` 见 `crates/hbot-rpc/src/ws.rs` 的
`healthz_answers_a_plain_http_get_without_a_token`；`token --write` 见
`crates/hbot-cli/tests/cli.rs` 的 `token_subcommand_writes_an_environment_file`。

## 4. 卸载

```bash
sudo systemctl disable --now hbot
sudo rm -f /etc/systemd/system/hbot.service /etc/logrotate.d/hbot /etc/hbot/hbot.env
sudo systemctl daemon-reload
sudo systemctl reset-failed hbot 2>/dev/null || true
# 数据要不要删自己决定 —— 删了就没了（含机器人配置与聊天记录）
sudo rm -rf /var/lib/hbot
sudo userdel hbot
```

## 5. 对外暴露：必须显式确认（这里不提供 TLS）

`hbot serve` **不实现 TLS**：它的 token 在 WebSocket 握手的第一帧里明文传输。
因此绑定非回环地址时它会**拒绝启动**，除非显式加 `--allow-remote`：

```bash
# 会被拒绝（错误里会写清楚为什么、以及怎么补救）
sudo -u hbot /usr/local/bin/hbot serve --bind 0.0.0.0:8787
# 拒绝绑定非回环地址 0.0.0.0:8787：本服务不提供 TLS，直接暴露等于把 token 明文发出去。
# 确实由前置反向代理终结 TLS 时，加 --allow-remote 显式确认。

# 正确的对外形状：反向代理（nginx/caddy）终结 TLS，回源到 127.0.0.1:8787
# —— 这种情况下根本不需要 --allow-remote，因为绑定地址仍是回环。
```

只有一种情况该加 `--allow-remote`：**代理与 hbot 不在同一台机器**、且这段网络你已经
自己保护好了（内网/VPN/专用网段）。加之前先想清楚：这一段链路上 token 是明文。

```ini
# 那种情况下 unit 里改成（示例，按需替换地址）
ExecStart=/usr/local/bin/hbot serve --bind 10.0.0.5:8787 --allow-remote --data-dir /var/lib/hbot
```

`--allow-remote` 在仓库里也有测试：`binding_a_public_address_is_refused_without_confirmation`
（`crates/hbot-cli/tests/cli.rs`）。

## 6. 加固与命令隔离：这里的取舍（重要）

unit 里开了这一串加固：

`NoNewPrivileges` `ProtectSystem=strict` `ProtectHome=read-only` `PrivateTmp`
`PrivateDevices` `ProtectKernel*` `ProtectControlGroups` `ProtectClock`
`ProtectHostname` `ProtectProc=invisible` `RestrictSUIDSGID` `LockPersonality`
`CapabilityBoundingSet=`（清空）

它们与「让 Agent 在目标项目里跑命令」这件事存在**结构性冲突**，取舍如下：

| 选项 | 冲突在哪 | 本轮的取舍 |
|---|---|---|
| `ProtectSystem=strict` | 整个文件系统只读，只有 `StateDirectory=` 推导出的 `/var/lib/hbot` 与 `/tmp` 可写。若把**项目放在 `/srv/project` 或 `/home/…`**，Agent 的写入/`git`/构建全部会 `EROFS`。 | 保留。要求项目放在 `/var/lib/hbot` 之下（也就是 `--data-dir` 里）。要跑外部项目就加 `ReadWritePaths=/srv/project`，**并想清楚这等于把那个目录的写权限交给模型**。 |
| `ProtectHome=read-only` | `$HOME` 只读。`hbot` 用户没有家目录（`--no-create-home`），所以正常情况下不触发；但**工具链缓存**（`~/.cargo`、`~/.npm`、`~/.cache`）会写失败。 | 保留。让工具链把缓存放到 `CARGO_HOME`/`npm_config_cache` 指向 `/var/lib/hbot/cache`（与原 Windows 沙箱同一思路），或改 `ProtectHome=true` 让家目录彻底不可见。 |
| `PrivateTmp=true` | 进程看到的 `/tmp` 是私有的，**重启即清空**。命令行沙箱、构建中间产物、`sqlite` 的临时文件都在这里。好处是别的服务看不到、也改不了；坏处是崩溃现场里指向 `/tmp` 的路径重启后不存在（能读，但文件没了）。 | 保留。`/tmp` 私有且可写，正好是沙箱要的"可写但隔离"。 |
| `NoNewPrivileges=true` | 任何 setuid 提权都失效 —— 包括 Agent 跑 `sudo` 的场景。 | 保留。这是**特性不是缺陷**：模型不该能在服务器上提权。 |
| `CapabilityBoundingSet=` 清空 | 进程没有任何 capability，绑定 <1024 端口会失败。 | 保留。`serve` 默认不用特权端口；要 80/443 就在前面放反向代理。 |
| `PrivateDevices=true` | 看不到真实设备节点。 | 保留。`serve` 不需要设备。 |

**刻意没有加的**（有意为之，别以为是漏了）：

- `MemoryDenyWriteExecute=true`：会连带禁掉任何 JIT；QuickJS 是解释执行，暂时没冲突，
  但一旦引入 JIT 就会莫名 `EPERM`，先不加。
- `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`：看起来很美，但 `serve` 还要
  连 LLM 的 HTTPS / WebSocket，漏一个族就是"能起但发不出消息"，本轮不冒这个险。
- `DynamicUser=true`：与 `StateDirectory` 里的持久数据搭配容易出现属主漂移，
  本轮用固定系统用户。

## 7. logrotate 说明（它管的其实很少）

`packaging/logrotate.d/hbot` 只轮转 `/var/lib/hbot/logs/*.log`：

- **运行日志不在这里**。`hbot serve` 的 tracing 写 stderr，进 journald，由 journald
  按 `SystemMaxUse` 自己管（`journalctl --vacuum-size=…`）。
- 这个目录里目前只有**崩溃报告**（`crash-<时间戳>.log`，见
  `crates/hbot-desktop/src/crash.rs`）。用 `copytruncate` 是因为应用侧没有
  "收到信号重开文件"的逻辑。
- 想立刻验一遍轮转规则（不改文件）：

  ```bash
  sudo logrotate -d /etc/logrotate.d/hbot     # -d = dry run，只打印计划
  ```

## 8. 已知缺失

- 没有打包成 `.deb` / `.rpm`：`packaging/` 只有 unit、logrotate 与一个安装脚本，
  手工程序仍要按 §2 走一遍。（Tauri 的 `bundle.targets` 里有 `deb`/`appimage`，
  但那是给**桌面壳**用的，不含这个 systemd unit。）
- `install.sh` 也是草稿：它只在 Windows 上做过 shellcheck 级的人工审阅，没有在真机跑过。
- 没做 `hbot` 用户的资源限制（`LimitNOFILE`、cgroup 内存上限）。
- 没验证过重启风暴：`Restart=on-failure` + `RestartSec=5` 是最朴素的配置，
  没有 `StartLimitBurst` 调优。
