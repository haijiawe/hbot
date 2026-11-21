# features/device

> 设备能力面板：截图 / 注入 / 窗口 / 剪贴板 / OCR 的开关、授权与设置。

## 职责

展示宿主侧 `DeviceGate` 的三层判据（清单声明 → 宿主设置 → 限时会话），并提供"临时授权"入口。判据不在这里，在 Rust 侧。

## 入口与文件

| 文件 | 做什么 |
|---|---|
| `device-panel.tsx` | 能力列表、当前授权状态、最近审计 |
| `grant-dialog.tsx` | 限时会话授权（带到期时间与范围） |
| `settings-card.tsx` | 宿主级设备设置（哪几个能力整体开 / 关） |

## 数据来源

| 来源 | 用途 |
|---|---|
| `stores/device-store.ts` | 能力表、授权、审计行 |
| `lib/ipc.ts` | `device.capabilities` / `device.grant` / `device.revoke` |

## 约定

- "存在不等于授权"：`capabilities()` 全 true 也不代表某个 bot 能用，必须看声明与设置。
- 撤销要即时生效：撤销后正在跑的动作也要中断，不能等下一次调用。
- 审计**不记内容**（截图内容、剪贴板文本一律不进审计）。

## 测试

`device-panel.test.tsx` / `settings-card.test.tsx`。
