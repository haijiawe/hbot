/**
 * 问题面板的两个动作：**Fix**（机械修复，不经模型）与 **Ask Agent to fix**（走既有 Agent 通道）。
 *
 * 三条硬约束（docs/06-ide.md §5.2 / §5.3 + 任务 C）：
 * 1. **不直接写盘**。机械修复只改编辑器缓冲区（`editorModel` 存在就直接改模型，让
 *    `onDidChangeContent` 回写 `workspace-store`），磁盘上的文件仍由用户按保存键写 ——
 *    这与"接受 Diff"不同：那条路径由核心 `edit_apply` 写盘，机械修复没有核心参与。
 * 2. **不绕过 dirty 语义**。改完缓冲区后 `updateContent` 自己会算出 `dirty`（文件变脏是
 *    正确结果），这里绝不碰 `saved`。
 * 3. **未保存改动要说出来**。请求文本里带 `unsavedBufferNote`，面板上另有一行可见提示；
 *    接受 Diff 时 `diff-card` 也会逐文件标「有未保存改动」。
 *
 * 返回值约定：`null` = 成功；字符串 = 给用户看的原因（面板用既有的 `role="alert"` 显示）。
 * 不用 throw —— 这些失败都是"预期内的用户可见状态"，不是异常。
 */

import { editorModel } from '../../lib/editor-model-bridge'
import {
  problemFixBatchPrompt,
  problemFixPrompt,
} from '../../lib/agent-requests'
import { applyEditsAsOneUndoGroup, splitDocumentLines } from '../../lib/edit-apply'
import type { Diagnostic } from '../../lib/ipc'
import {
  applyMechanicalFix,
  canFixMechanically,
  planMechanicalFix,
  planToLineEdit,
} from '../../lib/quick-fix'
import { useAgentStore } from '../../stores/agent-store'
import { useWorkspaceStore } from '../../stores/workspace-store'

/** 这些文件里有未保存改动（用于请求文本与面板上的可见提示）。 */
export function dirtyPathsOf(paths: readonly string[]): string[] {
  const tabs = useWorkspaceStore.getState().tabs
  return paths.filter((path) => tabs.some((tab) => tab.path === path && tab.dirty))
}

/**
 * 把一条诊断机械修好（只改缓冲区）。
 *
 * 文件没打开过就先 `openFile` —— 诊断可能来自一个没在编辑器里打开过的文件，
 * 而"能修"的前提是拿到它的文本。打开失败时 `workspace-store` 已经把原因写进 `error`，
 * 这里原样交回去，不另造一套文案。
 */
export async function applyMechanicalFixToBuffer(item: Diagnostic): Promise<string | null> {
  if (!canFixMechanically(item.code)) {
    return `${item.code ?? '这条问题'}没有机械修法，请用「请求 Agent 修复」。`
  }
  if (item.path === '') return '项目级问题没有可改的文件，请用「请求 Agent 修复」。'
  if (item.line <= 0) return '这条问题没有行号，定位不到要改的那一行。'

  const store = useWorkspaceStore.getState()
  if (typeof store.contents[item.path] !== 'string') {
    await store.openFile(item.path)
  }
  const opened = useWorkspaceStore.getState()
  const buffered = opened.contents[item.path]
  if (typeof buffered !== 'string') {
    return opened.error ?? `读不到 ${item.path} 的内容，无法机械修复。`
  }

  // 编辑器里有 model 就以模型为准（那才是用户眼前的文本）；否则用 store 里的副本。
  const model = editorModel(item.path)
  const content = model === null ? buffered : model.getValue()
  const plan = planMechanicalFix(item, splitDocumentLines(content))
  if (plan === null) {
    return `在 ${item.path} 的这一行里找不到可替换的模块名，请用「请求 Agent 修复」。`
  }

  if (model !== null) {
    // 与接受 Diff 同一套整组撤销：一次 Ctrl+Z 退回这一步，不散成多段。
    applyEditsAsOneUndoGroup(model, [planToLineEdit(plan)])
    return null
  }
  opened.updateContent(item.path, applyMechanicalFix(content, plan))
  return null
}

/** 发一条请求前的共同检查：Agent 正在忙就别发（`send` 会静默丢弃）。 */
function prepareAgent(): string | null {
  const agent = useAgentStore.getState()
  if (agent.sending) return 'Agent 正在处理上一条消息，等它回来再发。'
  if (agent.error !== null) agent.dismissError()
  return null
}

/** 把发送结果翻译成"成功 = null / 失败 = 原因"，并在成功时把 Agent 栏展开。 */
async function finishAsk(): Promise<string | null> {
  const after = useAgentStore.getState()
  if (after.error !== null) return after.error
  // 展开 Agent 栏：Diff 卡片回来时用户第一眼能看到（docs/06-ide.md §5.2）。
  useWorkspaceStore.getState().toggleAgent(false)
  return null
}

/** 单条「请求 Agent 修复」：文件 / 行 / 错误码 / 消息都进请求。 */
export async function askAgentToFix(item: Diagnostic): Promise<string | null> {
  const blocked = prepareAgent()
  if (blocked !== null) return blocked
  await useAgentStore
    .getState()
    .send(problemFixPrompt(item, dirtyPathsOf([item.path])))
  return finishAsk()
}

/**
 * 批量请求**只在全部为 error 级**时可点：一条 warning 混进来时"全部修掉"会让模型
 * 顺手改掉用户还没打算改的东西（docs/06-ide.md §5.3 只承诺"报错列表"可批量）。
 */
export function canAskAgentToFixAll(items: readonly Diagnostic[]): boolean {
  return items.length > 0 && items.every((item) => item.severity === 'error')
}

/** 批量「全部请求 Agent 修复」：一条请求里逐条列出（序号与面板顺序一致）。 */
export async function askAgentToFixAll(items: readonly Diagnostic[]): Promise<string | null> {
  if (items.length === 0) return '当前没有可修复的问题。'
  if (!canAskAgentToFixAll(items)) {
    return '只有全部是错误级问题时才能一次性请求修复；先单独处理警告与提示。'
  }
  const blocked = prepareAgent()
  if (blocked !== null) return blocked
  const paths = Array.from(new Set(items.map((item) => item.path)))
  await useAgentStore.getState().send(problemFixBatchPrompt(items, dirtyPathsOf(paths)))
  return finishAsk()
}