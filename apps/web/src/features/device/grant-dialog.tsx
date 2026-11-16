/**
 * 「开始设备会话」对话框（`device-panel` 的「开始会话」与待处理请求的「允许」都打开它）。
 *
 * 为什么这些默认值是"全不勾 / 空应用列表"：设备控制是**能力**而不是开关，默认放开任何一项
 * 都等于替用户做决定（docs/09-device-capability.md §4.2 的"最小授权"）。空应用列表在核心
 * 那边等于全拒，所以这里**直接拦住提交** —— 否则用户会得到一个"开始了但点什么都被拒"的会话。
 *
 * 那句必须读的话（`MUST_READ`）不是装饰：只读沙箱与设备控制是两套闸门，用户很容易以为
 * "我把文件设成只读了，Agent 就动不了我的电脑"（docs/09 §1）。
 */

import { useEffect, useRef, useState } from 'react'
import { useDeviceStore } from '../../stores/device-store'

/** 子能力清单（`sub` 是开放字符串：不在这张表里的值照样显示，只是没有中文说明）。 */
export interface DeviceSubCapability {
  /** 契约里的 `sub` 取值。 */
  id: string
  /** 一句中文说明（面板与对话框共用，避免两处各写一份）。 */
  label: string
  hint: string
}

export const DEVICE_SUB_CAPABILITIES: readonly DeviceSubCapability[] = [
  { id: 'screen', label: '截屏', hint: '截取屏幕画面（像素会交给当前会话）' },
  { id: 'ocr', label: '读屏幕文字', hint: '把屏幕上的文字识别出来' },
  { id: 'input', label: '点击与输入', hint: '控制鼠标与键盘（只对允许的应用生效）' },
  { id: 'window', label: '窗口列表', hint: '列出可见窗口的标题与进程名' },
  { id: 'clipboard', label: '剪贴板', hint: '读取与写入剪贴板' },
]

/** 时长档位（分钟）。上限 30 是契约（`device_grant` 超过 30 → `-32002`）。 */
export const DEVICE_MINUTE_OPTIONS: readonly number[] = [1, 5, 10, 30]
export const DEVICE_DEFAULT_MINUTES = 5

/**
 * 提交前必须让用户看到的一句话。
 *
 * 文案与 docs/09-device-capability.md §1 的表述一致，**不要改写**：它同时说明了两件事
 * ——设备控制能做什么、以及它不受文件沙箱档位约束。
 */
export const DEVICE_MUST_READ =
  '会话期间生成代码可以截取屏幕并控制鼠标键盘。这跟文件沙箱是两件事——只读档仍然允许设备控制（若已授权）。'

interface GrantDialogProps {
  /** 预勾的子能力（来自 `device.request` 事件的 `sub`）。默认空 = 一个都不勾。 */
  initialSub?: readonly string[]
  onClose: () => void
}

/** 把"每行一个应用"的文本拆成列表：去空白、去空行、按顺序去重。 */
function parseAllowedApps(text: string): string[] {
  const apps: string[] = []
  for (const line of text.split('\n')) {
    const app = line.trim()
    if (app.length > 0 && !apps.includes(app)) apps.push(app)
  }
  return apps
}

export function GrantDialog({ initialSub = [], onClose }: GrantDialogProps) {
  const grant = useDeviceStore((state) => state.grant)
  const busy = useDeviceStore((state) => state.busy)
  const storeError = useDeviceStore((state) => state.error)
  const [selected, setSelected] = useState<string[]>(() => [...initialSub])
  const [appsText, setAppsText] = useState('')
  const [minutes, setMinutes] = useState<number>(DEVICE_DEFAULT_MINUTES)
  const [error, setError] = useState<string | null>(null)
  const firstBox = useRef<HTMLInputElement | null>(null)

  // 未知的子能力（比如后端新加的 `audio`）也要出现在清单里：请求里带了它，
  // 用户就得能看见自己正在授权什么；藏起来再提交等于悄悄丢掉一项。
  const options: DeviceSubCapability[] = [...DEVICE_SUB_CAPABILITIES]
  for (const sub of initialSub) {
    if (!options.some((option) => option.id === sub)) {
      options.push({ id: sub, label: '未知能力（按原样显示）', hint: '这次请求里出现的新能力，界面还不认识它' })
    }
  }

  useEffect(() => {
    firstBox.current?.focus()
  }, [])

  function toggle(id: string): void {
    setSelected((current) =>
      current.includes(id) ? current.filter((item) => item !== id) : [...current, id],
    )
  }

  async function submit(): Promise<void> {
    if (busy) return
    const allowedApps = parseAllowedApps(appsText)
    if (allowedApps.length === 0) {
      // 空列表在核心那里是"全拒"，不是"无限制"：拦住提交并说清代价。
      setError('必须至少一个应用；留空等于全拒（点击与输入会被拒绝）。')
      return
    }
    setError(null)
    // 子能力保持用户在勾选框里看到的顺序（而不是集合顺序），便于人读。
    const sub = options.map((option) => option.id).filter((id) => selected.includes(id))
    const ok = await grant({ sub, allowedApps, minutes })
    if (ok) onClose()
  }

  const message = error ?? storeError

  return (
    <div className="palette-backdrop">
      <form
        className="palette device-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="device-dialog-title"
        onSubmit={(event) => {
          event.preventDefault()
          void submit()
        }}
        onKeyDown={(event) => {
          if (event.key === 'Escape') {
            event.stopPropagation()
            onClose()
          }
        }}
      >
        <h2 className="device-dialog__title" id="device-dialog-title">
          开始设备会话
        </h2>

        {/* 必须读的一句话：放在勾选框之前，提交按钮之前也一直可见。 */}
        <p className="device-dialog__warning" role="note">
          {DEVICE_MUST_READ}
        </p>

        <fieldset className="device-dialog__group">
          <legend className="device-dialog__label">允许哪些设备能力（默认一个都不勾）</legend>
          {options.map((option, index) => (
            <label className="device-dialog__option" key={option.id}>
              <input
                ref={index === 0 ? firstBox : undefined}
                type="checkbox"
                checked={selected.includes(option.id)}
                onChange={() => toggle(option.id)}
              />
              <span className="device-dialog__option-label">{option.label}</span>
              <code className="device-dialog__option-id">{option.id}</code>
              <span className="device-dialog__option-hint">{option.hint}</span>
            </label>
          ))}
          <p className="device-dialog__hint">
            一个都不勾也能开始会话，但那时没有任何设备能力可用（只会浪费一次授权）。
          </p>
        </fieldset>

        <label className="device-dialog__field">
          <span className="device-dialog__label">允许的应用（每行一个）</span>
          <textarea
            className="device-dialog__textarea"
            value={appsText}
            spellCheck={false}
            autoComplete="off"
            rows={4}
            placeholder={'PathOfExile.exe\nchrome.exe'}
            aria-label="允许的应用"
            onChange={(event) => setAppsText(event.target.value)}
          />
          <span className="device-dialog__hint">
            必须至少一个应用；留空等于全拒。写进程名（例如{' '}
            <code className="device-dialog__option-id">PathOfExile.exe</code>），不是窗口标题。
          </span>
          {/* 说明而不是重做列表：对话框当前的 textarea 没有"可勾选项"，改成按宿主设置的清单
              渲染会牵动 8 条既有断言（见 device-panel.test.tsx ⑦ "恰好 5 个 checkbox"）。 */}
          <span className="device-dialog__hint">
            「宿主设备设置」里也能设「允许的应用」，那是长期上限；这里填的是这次会话的范围。
          </span>
        </label>

        <label className="device-dialog__field device-dialog__field--inline">
          <span className="device-dialog__label">时长</span>
          <select
            className="device-dialog__select"
            value={minutes}
            aria-label="会话时长（分钟）"
            onChange={(event) => setMinutes(Number(event.target.value))}
          >
            {DEVICE_MINUTE_OPTIONS.map((option) => (
              <option key={option} value={option}>
                {option} 分钟
              </option>
            ))}
          </select>
          <span className="device-dialog__hint">到点自动到期（不会自动续）；最长 30 分钟。</span>
        </label>

        {message === null ? null : (
          <p className="inline-error" role="alert">
            {message}
          </p>
        )}

        <div className="device-dialog__actions">
          <button type="button" className="button" onClick={onClose} disabled={busy}>
            取消
          </button>
          <button type="submit" className="button button--primary" disabled={busy}>
            {busy ? '正在授权…' : '开始会话'}
          </button>
        </div>
      </form>
    </div>
  )
}