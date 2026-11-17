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
import {
  EDIT_APPLY_CONFLICT_CODE,
  applyEditsAsOneUndoGroup,
  captureApplySnapshot,
  planEditApply,
  restoreModelAsOneUndoGroup,
} from '../../lib/edit-apply'
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

    async afterApply({ editId }) {
      const current = plan
      if (current === null) return

      // 快照只在**应用这一刻**拍一次，而且必须在改缓冲区之前：`decision.before` 是 precheck
      // 时读到的应用前文本，下面每个文件都会被写成应用后的内容 —— 之后再想拿"应用前"就只能靠
      // 历史版本，而 store 里没有历史。
      useWorkspaceStore
        .getState()
        .recordApplySnapshot(captureApplySnapshot(editId, current.decisions))

      const applied = current.decisions.filter(
        (decision): decision is Extract<FileDecision, { action: 'apply' }> =>
          decision.action === 'apply',
      )

      for (const decision of applied) {
        const model = editorModel(decision.path)
        if (model === null) {
          // 文件没在编辑器里打开（或编辑器还没挂载）：直接改 store 里的缓冲区，
          // 之后模型按 `contents` 建出来就是新文本。
          //
          // 但绝不能覆盖这期间用户又敲进去的字：`precheck` 之后隔着一次
          // `edit.apply` 往返回核心，缓冲区可能已经变了。变了就只报问题、
          // 不写缓冲区（磁盘已由核心落盘，用户手上那份留给他自己保存/重试）。
          const latest = useWorkspaceStore.getState().contents[decision.path]
          if (latest !== undefined && latest !== decision.before) {
            useWorkspaceStore.getState().reportProblems([
              mismatchProblem(
                decision.path,
                `已落盘 ${decision.path}，但缓冲区在这期间又有改动：没有覆盖你的输入，先保存或刷新再重试。`,
              ),
            ])
            continue
          }
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

    async undo({ editId }) {
      const workspace = useWorkspaceStore.getState()
      const snapshot = workspace.lastApply
      if (snapshot === null || snapshot.editId !== editId) {
        throw new Error('这次应用没有留下可撤销的编辑器快照')
      }

      // 先退模型、再退缓冲区：模型变更会经 `onDidChangeContent` 回写 `contents`，
      // 顺序反了会被随后的缓冲区恢复盖成中间态（多文件时尤其明显）。
      for (const file of snapshot.files) {
        if (file.before === null) continue
        const model = editorModel(file.path)
        if (model !== null) restoreModelAsOneUndoGroup(model, file.before)
      }

      // 缓冲区与标签（新建文件）由 store 一次退完 —— 多文件的"一次 Ctrl+Z"只能这么实现。
      if (workspace.undoLastApply(editId) === null) {
        throw new Error('这次应用没有留下可撤销的编辑器快照')
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

/**
 * 组件里调的那个入口：「撤销这次应用」。
 *
 * 效果与 Ctrl+Z 一致，但范围是**这次应用碰过的所有文件**（Monaco 的撤销栈不跨 model，
 * 所以多文件只能靠快照一起退）。状态改动仍走 store。
 */
export async function undoEditInEditor(): Promise<void> {
  await useAgentStore.getState().undoEdit(createEditApplyHooks())
}