# docs

这个目录放**设计与证据**，不放运行日志、临时文件、生成物。

## 目录

- [先读哪一份](#先读哪一份)
- [文件表](#文件表)
- [子目录](#子目录)
- [什么该进这里，什么不该](#什么该进这里什么不该)
- [写法约定](#写法约定)

## 先读哪一份

| 你想干什么 | 读这份 |
|---|---|
| 接手这个仓库 | [HANDOFF.md](HANDOFF.md) —— 快照：已完成 / 剩余 / 闸门现状 / 外部依赖 |
| 补一条踩过的坑 | [pitfalls.md](pitfalls.md) —— 146 条，编号稳定，别重排 |
| 五分钟装上并跑起来 | [quickstart-desktop.md](quickstart-desktop.md) |
| 改代码前确认落点 | [03-directory.md](03-directory.md) + 各目录的 README.md |
| 改能力面（6 组能力 / 4 类 bot） | [07-bot-runtime-blueprint.md](07-bot-runtime-blueprint.md) |
| 加插件式扩展 | [10-plugin-model.md](10-plugin-model.md) |

规则冲突时以 `AGENTS.md` 与 [04-implementation-plan.md](04-implementation-plan.md) 为准，`HANDOFF.md` 只是快照。

## 文件表

| 文件 | 讲什么 |
|---|---|
| [00-assessment.md](00-assessment.md) | 旧项目代码评估。**证据不是规范**：读它是为了知道哪些坑不能再踩 |
| [01-requirements.md](01-requirements.md) | 需求与范围：产品做什么、不做什么 |
| [02-architecture.md](02-architecture.md) | 消息链路、Agent 循环、沙箱、存储、安全、多宿主 |
| [03-directory.md](03-directory.md) | 目录与命名、依赖方向、改 X 去哪 |
| [04-implementation-plan.md](04-implementation-plan.md) | 里程碑 M0–M11、每步的出口判据 |
| [05-quality-gates.md](05-quality-gates.md) | 六道闸门、性能预算、测试数量基线 |
| [06-ide.md](06-ide.md) | IDE 布局、编辑器、Agent Diff、运行与调试面板 |
| [07-bot-runtime-blueprint.md](07-bot-runtime-blueprint.md) | 能力面蓝本：6 组能力、每组的判据层与设计决定 |
| [08-multi-host.md](08-multi-host.md) | 桌面 / 无头 / 手机三宿主的差异与手机端限制 |
| [09-device-capability.md](09-device-capability.md) | C6 设备能力：闸只有一份、撤销要即时、审计不记内容 |
| [10-plugin-model.md](10-plugin-model.md) | 插件模型：编译期 seam + 沙箱槽位，与 DSH-Cordis 的对照，为什么不做运行时 dlopen |
| [HANDOFF.md](HANDOFF.md) | 交接快照：当前 HEAD、闸门数字、剩余清单、卡住的外部条件 |
| [pitfalls.md](pitfalls.md) | 踩过的坑全表（146 条）。每条都是实测过的，不是推测 |
| [quickstart-desktop.md](quickstart-desktop.md) | 五分钟上手：装包 → 新建项目 → 试跑 → 看到回复 |

## 子目录

| 目录 | 放什么 |
|---|---|
| [adr/](adr/) | 架构决策记录（0001–0007）：为什么是 Rust、为什么无 Node、为什么三宿主、能力给不给、跑在哪 |
| [spikes/](spikes/) | 技术验证与实测报告（S0–S15）：每份写清结论、实测数字与**没测到什么** |
| [plans/](plans/) | 阶段性执行计划（按日期命名）。做完就归档，不删 |
| [security/](security/) | 红队发现与处置记录 |

## 什么该进这里，什么不该

**该进**：

- 一个决定了之后很难改的选择（进 `adr/`，带"为什么不是另一个方案"）；
- 一次实测的数字（进 `spikes/`，写清机器、日期、测法、以及**没有测到的部分**）；
- 一条踩过的坑（进 `pitfalls.md`，编号递增，写清判据与同族坑）。

**不该进**：

- 运行日志、临时脚本、一次性排查记录（放 `_recovery/` 或根本不留）；
- 会过期的进度表（那种东西进 `HANDOFF.md`，且只保留最近一次）；
- 与代码重复的清单（能力清单在 `07`，别在别处再抄一份 —— 两份必然漂移）。

## 写法约定

- **中文正文**，代码标识符保持原样；标题层级只在 `#` / `##` / `###` 三级内。
- **结论先行**：每节第一段就能回答问题，细节放后面。
- **代码是唯一真源**：引用代码时给 `路径:行号`，不复制整段实现。
- **不确定就写不确定**：没测到的写成"未实测"，别让下一个人以为已经验过（`spikes/` 里这条尤其重要）。