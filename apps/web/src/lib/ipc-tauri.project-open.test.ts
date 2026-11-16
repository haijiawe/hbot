// @vitest-environment jsdom
/**
 * `project_open` / `project_create` 的桌面壳映射（`lib/ipc-tauri.ts`）。
 *
 * 这里钉的是**协议形状**：`crates/hbot-rpc/src/app/commands/system.rs` 的
 * `project_open` 成功时返回 `Response::with_data(ProjectOpenResult)` ——
 * data 是 `{ "path": "<规范化后的绝对路径>" }`，不是裸字符串、也不是 `ProjectSummary`
 * （`project_create` 才返回摘要）。生成物见 `src/generated/ProjectOpenResult.ts`。
 *
 * 两种形状都必须**归一成 `{ path }`**：
 * - `{ path }` 对象是当前契约（主路径）；
 * - 裸字符串是旧核心 / 旧快照的形状，桌面壳与 web 产物不是同一刻升级的，判死会让
 *   "打开项目"在过渡期整条链路失败。
 *
 * 其余形状（数字、null、空串）必须抛 `IpcError`：项目根是 journal 的项目身份，
 * 猜一个路径就会把 A 项目没保存的正文写进 B 项目。
 */

import { afterEach, describe, expect, it, vi } from 'vitest'
import { IpcError, dispatchViaTauri } from './ipc-tauri'

/** 装一个假桌面壳：`invoke('dispatch', { command })` 一律返回给定的 Response。 */
function installFakeCore(response: unknown) {
  const invoke = vi.fn(async (command: string) => {
    if (command !== 'dispatch') throw new Error(`未知命令：${command}`)
    return response
  })
  Object.defineProperty(window, '__TAURI_INTERNALS__', {
    configurable: true,
    writable: true,
    value: { invoke, transformCallback: () => 1 },
  })
  return invoke
}

afterEach(() => {
  delete (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__
})

describe('dispatchViaTauri：project.open', () => {
  it('核心返回 { path } 对象时归一成 { path }（当前契约的主路径）', async () => {
    const invoke = installFakeCore({
      type: 'ok',
      data: { path: 'C:\\work\\demo' },
    })

    const result = await dispatchViaTauri({ type: 'project.open', path: 'C:/work/demo' })

    expect(result).toEqual({ path: 'C:\\work\\demo' })
    expect(invoke).toHaveBeenCalledWith('dispatch', {
      command: { type: 'project_open', path: 'C:/work/demo' },
    })
  })

  it('{ path } 里的多余字段被忽略（结果只带 path，不把 name 等串进项目根）', async () => {
    installFakeCore({
      type: 'ok',
      data: { path: 'C:/work/demo', name: '不该被采纳', entry: 'src/main.ts' },
    })

    await expect(dispatchViaTauri({ type: 'project.open', path: 'C:/work/demo' })).resolves.toEqual({
      path: 'C:/work/demo',
    })
  })

  it('旧核心返回裸路径字符串时仍能归一（兼容路径，不是主路径）', async () => {
    const invoke = installFakeCore({ type: 'ok', data: 'C:\\work\\demo' })

    const result = await dispatchViaTauri({ type: 'project.open', path: 'C:/work/demo' })

    expect(result).toEqual({ path: 'C:\\work\\demo' })
    expect(invoke).toHaveBeenCalledWith('dispatch', {
      command: { type: 'project_open', path: 'C:/work/demo' },
    })
  })

  it('返回无法识别的形状时抛 IpcError（而不是把 undefined 当项目根写进 store）', async () => {
    installFakeCore({ type: 'ok', data: 42 })

    await expect(dispatchViaTauri({ type: 'project.open', path: 'C:/work/demo' })).rejects.toThrow(
      IpcError,
    )
    await expect(
      dispatchViaTauri({ type: 'project.open', path: 'C:/work/demo' }),
    ).rejects.toThrow('无法识别的形状')
  })

  it('返回空字符串时也抛 IpcError（空路径不是项目根）', async () => {
    installFakeCore({ type: 'ok', data: '' })

    await expect(dispatchViaTauri({ type: 'project.open', path: 'C:/work/demo' })).rejects.toThrow(
      IpcError,
    )
  })

  it('核心回错误时把错误码带出来（-32002 空路径）', async () => {
    installFakeCore({
      type: 'error',
      error: { code: -32002, message: 'project_open: path 不能为空', data: null },
    })

    await expect(dispatchViaTauri({ type: 'project.open', path: '' })).rejects.toThrow('-32002')
  })
})

describe('dispatchViaTauri：project.create', () => {
  it('把 path/name 发给 project_create，并返回核心的项目摘要', async () => {
    const invoke = installFakeCore({
      type: 'ok',
      data: { path: 'C:/work/new-bot', name: 'new-bot', entry: 'src/main.ts' },
    })

    const result = await dispatchViaTauri({
      type: 'project.create',
      path: 'C:/work/new-bot',
      name: 'new-bot',
    })

    expect(result).toEqual({ path: 'C:/work/new-bot', name: 'new-bot', entry: 'src/main.ts' })
    expect(invoke).toHaveBeenCalledWith('dispatch', {
      command: { type: 'project_create', path: 'C:/work/new-bot', name: 'new-bot' },
    })
  })
})

describe('dispatchViaTauri：workspace.tree 的落点', () => {
  // 打开项目的动作链最后一步是 `loadTree()`；它必须真的变成核心的 `file_list`，
  // 否则"打开成功但树是空的"在真机上无法解释。
  it('workspace.tree 落到 file_list（带项目根）', async () => {
    const invoke = installFakeCore({ type: 'ok', data: [] })

    await dispatchViaTauri({ type: 'workspace.tree', root: 'C:/work/demo' })

    expect(invoke).toHaveBeenCalledWith('dispatch', {
      command: { type: 'file_list', path: 'C:/work/demo' },
    })
  })
})