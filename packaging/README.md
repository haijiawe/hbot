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
> 本文只负责"怎么把它挂成服务"。

---

## 1. 前置条件

| 项 | 要求 | 说明 |
|---|---|---|
| `hbot` 二进制 | 放到 `/usr/local/bin/hbot` | 与 unit 里的 `ExecStart` 一致；换路径要同步改 unit |
| 专用用户 | `hbot:hbot`，无登录 shell | 不要用 root 跑 |
| 数据目录 | `/var/lib/hbot` | 由 `StateDirectory=hbot` 交给 systemd 创建（`0750`），**不要手工 `mkdir`** |
| 端口 | `hbot serve` 默认监听本地回环 | 需要外部访问时自己加 `--addr`，并同步改防火墙 |

`hbot serve` 已经能跑：它启动后往 **stdout 打一行 ready JSON**
（`{"type":"ready","addr":"…","pid":N}`），然后等 SIGTERM/SIGINT 优雅关闭
（停机器人 → `handle.shutdown()`）。本目录不改它的任何行为。

## 2. 安装

```bash
# 1) 建用户（无家目录、无登录 shell）
sudo useradd --system --no-create-home --shell /usr/sbin/nologin hbot

# 2) 装二进制
sudo install -Dm755 ./hbot /usr/local/bin/hbot

# 3) 装 unit 与轮转配置
sudo install -Dm644 packaging/systemd/hbot.service /etc/systemd/system/hbot.service
sudo install -Dm644 packaging/logrotate.d/hbot     /etc/logrotate.d/hbot

# 4) 起服务并设为开机启动
sudo systemctl daemon-reload
sudo systemctl enable --now hbot

# 5) 看状态与日志
systemctl status hbot
journalctl -u hbot -f
```

`--data-dir /var/lib/hbot` 是 unit 里写死的。数据目录里的内容是
`hbot.db`（SQLite，WAL）与 `logs/`（崩溃报告 `crash-<时间戳>.log`）。

### 需要 GUI 时

服务跑的是 **headless** `serve`。桌面壳（托盘/窗口/单实例）是另一个二进制入口，
它自己会连同一个数据目录；**不要**在服务器上同时跑两者 —— 单实例保护只覆盖桌面壳，
`serve` 与桌面壳是两个进程，会各开一个数据库写入者（SQLite WAL 能承受，但
`bots` 表的运行状态会分叉）。真要在同一台机器上切换：先 `systemctl stop hbot`。

## 3. 验证清单（真机上按顺序打勾）

```bash
systemctl is-enabled hbot        # 期望 enabled
systemctl is-active hbot        # 期望 active
journalctl -u hbot -n 50 --no-pager   # 期望看到一行 ready JSON 与"数据目录"日志
ls -la /var/lib/hbot            # 期望 hbot.db、logs/ 都是 hbot:hbot
sqlite3 /var/lib/hbot/hbot.db 'select count(*) from bots;'   # 期望不报错（0 也行）
```

## 4. 卸载

```bash
sudo systemctl disable --now hbot
sudo rm -f /etc/systemd/system/hbot.service /etc/logrotate.d/hbot
sudo systemctl daemon-reload
sudo systemctl reset-failed hbot 2>/dev/null || true
# 数据要不要删自己决定 —— 删了就没了（含机器人配置与聊天记录）
sudo rm -rf /var/lib/hbot
sudo userdel hbot
```

## 5. 加固与命令隔离：这里的取舍（重要）

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

## 6. logrotate 说明（它管的其实很少）

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

## 7. 已知缺失

- 没有打包成 `.deb` / `.rpm`：`packaging/` 只有 unit 与 logrotate，安装步骤是手工的。
  （Tauri 的 `bundle.targets` 里有 `deb`/`appimage`，但那是给**桌面壳**用的，
  不含这个 systemd unit。）
- 没有 TLS/鉴权前置：`serve` 的 token 由 `--token` 或 `HBOT_TOKEN` 给；unit 里**没有**
  写 token（写进 unit 等于写进 `/etc` 明文）。需要时用 `EnvironmentFile=` 指向一个
  `0600` 的文件。
- 没做 `hbot` 用户的资源限制（`LimitNOFILE`、cgroup 内存上限）。
- 没验证过重启风暴：`Restart=on-failure` + `RestartSec=5` 是最朴素的配置，
  没有 `StartLimitBurst` 调优。