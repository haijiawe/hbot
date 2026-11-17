// @vitest-environment jsdom
/**
 * 行内编辑浮层（`features/editor/inline-edit-popover.tsx`）。
 *
 * 它是"就地浮层"而不是模态框：没有遮罩、没有 aria-modal，用户还能继续滚代码。
 * 这里钉住四件事：说清改哪一段、提交只交非空指令、取消不发请求、失败原因留在浮层里
 * （`role="alert"` —— 关掉浮层等于让用户以为请求已经发出去了）。
 */

import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { InlineEditPopover } from './inline-edit-popover'

const selection = {
  path: 'src/plugins/ping.ts',
  startLine: 3,
  endLine: 5,
  text: 'function ping() {\n  return true\n}',
}

function renderPopover(patch: Partial<Parameters<typeof InlineEditPopover>[0]> = {}) {
  const onSubmit = vi.fn()
  const onCancel = vi.fn()
  render(
    <InlineEditPopover
      selection={selection}
      dirty={false}
      error={null}
      busy={false}
      left={12}
      top={34}
      onSubmit={onSubmit}
      onCancel={onCancel}
      {...patch}
    />,
  )
  return { onSubmit, onCancel }
}

afterEach(cleanup)

describe('InlineEditPopover', () => {
  it('标题里写清文件与行范围（用户要确认改的是哪一段）', () => {
    renderPopover()
    expect(screen.getByRole('dialog', { name: '行内编辑' })).toBeTruthy()
    expect(screen.getByText(/src\/plugins\/ping\.ts/).textContent).toContain('第 3–5 行')
  })

  it('它不是模态框：没有 aria-modal', () => {
    renderPopover()
    expect(screen.getByRole('dialog', { name: '行内编辑' }).getAttribute('aria-modal')).toBeNull()
  })

  it('输入指令后提交，把去掉首尾空白的指令交出去', async () => {
    const user = userEvent.setup()
    const { onSubmit } = renderPopover()

    await user.type(screen.getByLabelText('行内编辑指令'), '  改成早返回  ')
    await user.click(screen.getByRole('button', { name: '发送行内编辑请求' }))

    expect(onSubmit).toHaveBeenCalledTimes(1)
    expect(onSubmit).toHaveBeenCalledWith('改成早返回')
  })

  it('没有输入时发送按钮禁用，提交也不发请求', async () => {
    const user = userEvent.setup()
    const { onSubmit } = renderPopover()

    const submit = screen.getByRole('button', { name: '发送行内编辑请求' }) as HTMLButtonElement
    expect(submit.disabled).toBe(true)
    await user.click(submit)
    expect(onSubmit).not.toHaveBeenCalled()
  })

  it('取消按钮只关浮层，不发请求', async () => {
    const user = userEvent.setup()
    const { onSubmit, onCancel } = renderPopover()

    await user.type(screen.getByLabelText('行内编辑指令'), '随便改改')
    await user.click(screen.getByRole('button', { name: '取消行内编辑' }))

    expect(onCancel).toHaveBeenCalledTimes(1)
    expect(onSubmit).not.toHaveBeenCalled()
  })

  it('Esc 取消（焦点在浮层里时不用去点按钮）', () => {
    const { onCancel } = renderPopover()
    fireEvent.keyDown(screen.getByRole('dialog', { name: '行内编辑' }), { key: 'Escape' })
    expect(onCancel).toHaveBeenCalledTimes(1)
  })

  it('发送中：按钮禁用并显示"发送中…"，避免连点发两条', () => {
    renderPopover({ busy: true })
    const submit = screen.getByRole('button', { name: '发送行内编辑请求' })
    expect((submit as HTMLButtonElement).disabled).toBe(true)
    expect(submit.textContent).toContain('发送中')
  })

  it('有未保存改动时给出 role=note 提示（Diff 按磁盘内容生成）', () => {
    renderPopover({ dirty: true })
    const note = screen.getByRole('note')
    expect(note.textContent).toContain('未保存')
    expect(note.textContent).toContain('磁盘')
  })

  it('没有未保存改动时不显示那条提示', () => {
    renderPopover()
    expect(screen.queryByRole('note')).toBeNull()
  })

  it('发送失败：原因留在浮层里用 role=alert 显示', () => {
    renderPopover({ error: '发送失败：核心未就绪' })
    expect(screen.getByRole('alert').textContent).toContain('发送失败：核心未就绪')
  })

  it('底部提示里给出快捷键（唯一真源里读出来，不在这里手写 Ctrl+K）', () => {
    renderPopover()
    expect(screen.getByText(/Esc 取消/).textContent).toContain('Ctrl+K')
  })
})