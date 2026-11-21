# features/settings

> 设置页。

## 职责

宿主级设置的渲染壳：模型 provider、沙箱档、命令白名单、设备开关、Webhook 端口等。

## 入口与文件

| 文件 | 做什么 |
|---|---|
| `settings-view.tsx` | 设置分组与表单（每个分组一个卡片） |

## 数据来源

| 来源 | 用途 |
|---|---|
| `lib/ipc.ts` | `settings.read` / `settings.write`、`agent.config` |

## 约定

- 写错档位字符串要报错，**不许静默退回默认档**（`AGENTS.md` 坑 35）。
- 密钥字段只显示"已配置 / 未配置"，不回显明文。

## 测试

没有独立测试文件；设置项的行为由 Rust 侧的配置单测与 `docs/quickstart-desktop.md` 的常见问题覆盖。
