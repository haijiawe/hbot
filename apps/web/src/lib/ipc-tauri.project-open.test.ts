// @vitest-environment jsdom
/**
 * `project_open` / `project_create` 的桌面壳映射（`lib/ipc-tauri.ts`）。
 *
 * 这里钉的是**协议形状**，因为核心与 wire 契约在这一点上不一致、且很容易改错：
 * `crates/hbot-rpc/src/app/commands/system.rs` 的 `project_open` 成功时返回
 * `Response::with_data(Value::String(root))` —— data 是**规范化后的路径字符串**，
 * 不是 `ProjectSummary`（`project_create` 才返回摘要）。web 侧必须把它归一成
 * `{ path }`，并且对 `{ path }` 这种对象形状也要宽容（上层/未来版本若换成对象，
 * UI 不该整条链路炸掉）。
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
  it('核心返回路径字符串时归一成 { path }，并把 wire 命令原样发出', async () => {
    const invoke = installFakeCore({ type: 'ok', data: 'C:\\work\\demo' })

    const result = await dispatchViaTauri({ type: 'project.open', path: 'C:/work/demo' })

    expect(result).toEqual({ path: 'C:\\work\\demo' })
    expect(invoke).toHaveBeenCalledWith('dispatch', {
      command: { type: 'project_open', path: 'C:/work/demo' },
    })
  })

  it('核心返回 { path } 对象时也接受（不因为形状变化就整条链路失败）', async () => {
    installFakeCore({ type: 'ok', data: { path: 'C:/work/demo' } })

    await expect(dispatchViaTauri({ type: 'project.open', path: 'C:/work/demo' })).resolves.toEqual({
      path: 'C:/work/demo',
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