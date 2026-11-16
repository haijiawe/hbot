/**
 * 项目的两个动作：打开 / 新建（UI 的唯一入口）。
 *
 * 为什么单独一层而不是直接写进 store：切项目必须走
 * `lib/session-persistence.ts` 的 `switchProject`（先 flush journal，失败**不许切**），
 * 而 store 之间、store 与持久化层之间不许互相引用（`stores/workspace-store.ts` 文件头
 * 的铁律）。放 `lib/` 是唯一没有环的位置：`lib → stores` 单向，`switchProject` 再回写 store。
 *
 * 失败约定：**不改变当前项目**，把原因交给 `reportError`（状态栏 `role="alert"` 那一行），
 * 然后把错误抛给调用方（对话框据此留在原地，用户可以改路径重试）。
 * 门闩拦下的那种失败（`ProjectSwitchBlockedError`）已由 `switchProject` 上报过，这里
 * 不重复报一句 —— 状态栏只留一条可读的原因。
 */

import { errorMessage } from './format'
import { dispatch } from './ipc'
import { ProjectSwitchBlockedError } from './session-journal'
import { switchProject } from './session-persistence'
import { useWorkspaceStore } from '../stores/workspace-store'

/** 把失败原因写进状态栏，并把同一个错误返回给调用方 `throw`。 */
function failure(message: string): Error {
  useWorkspaceStore.getState().reportError(message)
  return new Error(message)
}

/**
 * 打开一个已存在的项目。
 *
 * 走门闩：先 flush 未保存缓冲区，写盘失败就**不切**（`project_open` 一次都不会发出）。
 * 成功后 `projectRoot` 变成核心规范化后的路径、旧项目的标签被清空、文件树重读、
 * 目标项目的 journal 现场尝试恢复。返回规范化后的项目根。
 */
export async function openProject(path: string): Promise<string> {
  const target = path.trim()
  if (target.length === 0) throw failure('打开项目失败：项目路径不能为空。')

  let opened = target
  try {
    await switchProject(target, async (chosen) => {
      const summary = await dispatch({ type: 'project.open', path: chosen })
      opened = summary.path
      return summary.path
    })
  } catch (error) {
    // 门闩已经把原因放进状态栏了（见 switchProject 的 catch），别再盖一条。
    if (!(error instanceof ProjectSwitchBlockedError)) {
      useWorkspaceStore.getState().reportError(`打开项目失败：${errorMessage(error)}`)
    }
    throw error
  }
  return opened
}

/**
 * 新建一个项目。
 *
 * 语义按核心 `project_create`：目录不存在就连父目录一起建、空目录直接用、非空目录
 * （或同路径已登记）回 `-32003`、`name` 为空回 `-32002` —— 这些都由核心判定，这里只做
 * "别把空字符串发出去"的本地拦截（少一次往返，且错误文案更早出现）。
 * 返回新项目的规范化根。
 */
export async function createProject(path: string, name: string): Promise<string> {
  const target = path.trim()
  const projectName = name.trim()
  if (target.length === 0) throw failure('新建项目失败：项目路径不能为空。')
  if (projectName.length === 0) throw failure('新建项目失败：项目名称不能为空。')

  let created = target
  try {
    await switchProject(target, async (chosen) => {
      const summary = await dispatch({
        type: 'project.create',
        path: chosen,
        name: projectName,
      })
      created = summary.path
      return summary.path
    })
  } catch (error) {
    if (!(error instanceof ProjectSwitchBlockedError)) {
      useWorkspaceStore.getState().reportError(`新建项目失败：${errorMessage(error)}`)
    }
    throw error
  }
  return created
}