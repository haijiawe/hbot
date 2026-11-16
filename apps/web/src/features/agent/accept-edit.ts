/**
 * 「接受 Diff」在界面侧的那一半：按缓冲区校验 → 交给核心落盘 → 落到编辑器（一个 undo 组）。
 *
 * 分工：
 * - `stores/agent-store.ts` 只管调 `edit.apply` 与自己的状态（不 import workspace-store）；
 * - 这里（组件层）负责需要两个 store + Monaco 模型的部分，通过 `EditApplyHooks` 注入；
 * - `lib/edit-apply.ts` 是纯逻辑（可单测），`lib/editor-model-bridge.ts` 供模型。
 *
 * 一个文件被拒不影响别的文件：`precheck` 只剔掉那一个文件的 hunk，其余照常落盘。
 */

import type { EditApplyHooks, EditApplyPlan, FileDecision } from '../../lib/edit-apply'
import { EDIT_APPLY_CONFLICT_CODE, applyEditsAsOneUndoGroup, planEditApply } from '../../lib/edit-apply'
import { editorModel } from '../../lib/editor-model-bridge'
import { errorMessage } from '../../lib/format'
import { dispatch, type Diagnostic } from '../../lib/ipc'
import { useAgentStore } from '../../stores/agent-store'
import { useWorkspaceStore } from '../../stores/workspace-store'

/** 应用后模型内容与预期不一致（外部改动 / 行尾差）时给用户看的一句话。 */
function mismatchProblem(path: string, message: string): Diagnostic {
  return {
    path,
    line: 0,
    column: 1,
    severity: 'warning',
    message,
    code: EDIT_APPLY_CONFLICT_CODE,
  }
}

/**
 * 建一次「接受」用的 hooks。
 *
 * 每次接受都新建一份：`plan` 是这个闭包私有的，两个文件同时接受也不会串。
 */
export function createEditApplyHooks(): EditApplyHooks {
  let plan: EditApplyPlan | null = null

  return {
    precheck({ files, acceptHunks }) {
      const workspace = useWorkspaceStore.getState()
      plan = planEditApply(
        files,
        {
          bufferOf: (path) => workspace.contents[path],
          diskOf: (path) => workspace.saved[path],
          dirtyOf: (path) => workspace.tabs.find((tab) => tab.path === path)?.dirty ?? false,
        },
        acceptHunks,
      )
      if (plan.problems.length > 0) workspace.reportProblems(plan.problems)

      const accepted = new Set(acceptHunks)
      const rejected = new Set(
        plan.decisions.filter((item) => item.action === 'reject').map((item) => item.path),
      )
      // 只保留"没被拒的文件里、用户这次同意接受的" hunk。
      const kept = files
        .filter((file) => !rejected.has(file.path))
        .flatMap((file) => file.hunks.map((hunk) => hunk.index))
        .filter((index) => accepted.has(index))

      // 全被拒时返回空数组：`agent-store` 会因此**不派发** `edit.apply`
      // （核心收到空 accept_hunks 的含义是"丢弃这次编辑"，那会连重试机会都没有）。
      return { acceptHunks: kept }
    },

    async afterApply() {
      const current = plan
      if (current === null) return
      const applied = current.decisions.filter(
        (decision): decision is Extract<FileDecision, { action: 'apply' }> => decision.action === 'apply',
      )

      for (const decision of applied) {
        const model = editorModel(decision.path)
        if (model === null) {
          // 文件没在编辑器里打开（或编辑器还没挂载）：直接改 store 里的缓冲区，
          // 之后模型按 `contents` 建出来就是新文本。
          useWorkspaceStore.getState().updateContent(decision.path, decision.after)
          continue
        }
        // 一次应用 = 一个 undo 组：整批 hunk 放进一个 pushStackElement 边界里。
        applyEditsAsOneUndoGroup(model, decision.edits)
        const actual = model.getValue()
        if (normalizeEol(actual) !== normalizeEol(decision.after)) {
          // 不静默：模型内容与预期不一致说明有别的写入插进来了，用户该知道。
          useWorkspaceStore.getState().reportProblems([
            mismatchProblem(
              decision.path,
              `已按一个 undo 组应用 ${decision.path}，但编辑器内容与预期不一致（可能有别的改动同时写了这个文件），请检查。`,
            ),
          ])
        }
      }

      // 磁盘基线以**重新读回来的**内容为准：核心落盘后的结果只有磁盘知道，
      // 前端拿 `disk + hunk` 猜一遍会在"核心的 apply_hunks 与前端行替换口径不同"时把
      // 脏标记算错（那会让用户以为改动已经存过，进而丢字）。
      for (const decision of applied) {
        try {
          const disk = await dispatch({ type: 'fs.read', path: decision.path })
          useWorkspaceStore.getState().markDiskContent(decision.path, disk.content)
        } catch (error) {
          useWorkspaceStore.getState().reportProblems([
            mismatchProblem(
              decision.path,
              `已应用 ${decision.path}，但没能重新读取磁盘内容（${errorMessage(error)}）；保存前请先刷新。`,
            ),
          ])
        }
      }
    },
  }
}

function normalizeEol(text: string): string {
  return text.replace(/\r\n/g, '\n')
}

/**
 * 组件里调的那个入口：带缓冲区校验 + 编辑器 undo 分组的"接受”。
 *
 * 与 `useAgentStore.getState().acceptEdit()` 的区别只有 hooks；状态改动仍走 store。
 */
export async function acceptEditInEditor(): Promise<void> {
  await useAgentStore.getState().acceptEdit(createEditApplyHooks())
}