# features/workbench

> bot 工作台的八个面：六个侧栏面（概览、Bot、能力、会话、运行态、渠道）+ 两个整页面（作业看板、设计文档）。开发日志面已于 2026-11-21 删除（与底部日志页签重复），筛选搬进 `features/run/run-log.tsx`。
>
> 整页面的正文渲染在编辑区位置（`App.tsx` 的 `renderPage()`），侧栏只放清单与统计 —— 三列看板在 420px 侧栏里会折成三条竖线。

## 职责

把这个项目「是什么、声明了什么、谁在跑、跑到哪」摆在侧栏里，而不是要求用户先知道命令面板。
形态借自 Mino（侧栏文字导航 + topbar 大标题 + stat 大数字 + list-item 两行），
颜色与间距用本仓的令牌 —— 暖色亮底、大圆角、大留白那一套在 IDE 里不适用，见
`docs/11-ui-language-bot-workbench.md` 的黑名单。

## 面与文件

| 面（`SidebarView`） | 文件 | 读什么 |
|---|---|---|
| `overview` | `overview-view.tsx` | 清单（项目名 / 平台 / 能力数）+ `run-store` 的运行状态；两个按钮（启动 / 停止）与一步上手清单 |
| `bots` | `bots-view.tsx` | `run-store.bots`（`bot.list` 的完整结果，含已声明未启动的） |
| `capabilities` | `capabilities-view.tsx` | `workspace-store.manifest`，12 行固定顺序 |
| `conversations` | `conversations-view.tsx` | `bot_conversations` 命令 |
| `runtime` | `runtime-view.tsx` | `bot_jobs` / `bot_waits` 各 20 条，三个计数 + 最近几条 |
| `channels` | `channels-view.tsx` | `workspace-store.botsFile` 的原文，解析在 `lib/bots-file.ts` |
| `board` | `jobs-board-view.tsx` | `bot_jobs` 只读快照（`BOARD_LIMIT=60`），分列逻辑在 `lib/job-board.ts` |
| `docs` | `docs-view.tsx` | `workspace.tree` 挑文档扩展名（`lib/doc-list.ts`）+ `fs.read` / `fs.write` |
| 共用 | `bot-status.ts` | 五态标签与状态点类名（颜色不是唯一信号：降级是空心环） |

清单的解析在 `lib/bot-manifest.ts`，bots.json 的解析在 `lib/bots-file.ts` ——
两者都是纯函数，测试直接喂字符串，不需要渲染。

## 数据来源

- **只经 `lib/ipc.ts` 的 `dispatch`**：不直接 `fetch` / WebSocket。
- **不新增命令**：清单与渠道都读项目文件（`fs.read`），因为核心没有「返回清单」的命令；
  加命令要动 `dto` → `xtask gen` → 生成物三处，而这里只需要只读展示。
- **`workspace-store` 存原文 / 视图，`run-store` 存运行态**：两个 store 各管一段，面板只读。

## 约定

1. **只显示声明层**。能力面底部那句话不能删：声明 ≠ 装配 ≠ 授权（ADR-0005 三层求交）。
   用户看到 `ai` 打了勾就以为能用，是这条文案要防的事。
2. **凭据不上界面**。`lib/bots-file.ts` 压根不读 `token` / `clientSecret`。
3. **没有打开项目时不发命令**。核心对「没有项目」回 `-32002`，面板会把这条显示成红字，
   而用户什么都没做 —— 每个面的 `useEffect` 第一句都是 `projectRoot === null` 就返回。
4. **样式只加 `.wb__*` / `.cap__*`**，复用 `app.css` 的按钮与令牌；不引入卡片与大圆角。

## 测试

| 文件 | 钉什么 |
|---|---|
| `src/lib/bot-manifest.test.ts` | 12 行顺序、计划类 detail、坏 JSON / 缺 name 的人话、HTTP 源去重、limitsSummary |
| `src/lib/bots-file.test.ts` | 两种 platform 形状、凭据不出现在结果里、坏结构退化成空数组 |
| `src/features/problems/*.test.tsx` | 「没有打开项目时不发命令」这条同族判据（问题面板那条先落地） |

面板本身没有组件测试：它们的形状是「一组只读行」，判据都在纯函数里；
要验渲染时用真机（`_recovery` 下的截图）或加 jsdom 用例，别只断言类名。
