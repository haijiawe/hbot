/**
 * 宿主设备设置的卡片（挂在设备页签里，由 `device-panel.tsx` 引用）。
 *
 * 这一块管的是**这台机器**允许什么（`<data_dir>/device.json`，docs/09-device-capability.md
 * §4.2），不是"这次会话授权了什么"——那个在 `grant-dialog.tsx`。两者的判据不同：
 * 会话授权是"现在这一刻点不点头"，宿主设置是"长期允不允许"，所以两处各说各的话。
 *
 * 三条界面纪律：
 * 1. `capabilities` 空 = **全拒**，`allowedApps` 空 = **不设上限**：语义故意相反，
 *    文案里必须各写一遍，否则用户会把"没勾任何能力"当成"随便用"。
 * 2. 未知能力**不算错**：后端将来加 `audio` 之类，前端照原样显示、照原样提交，
 *    由核心决定收不收（`-32002`）。前端只拦那些**确定不合法**的（`maxMinutes > 30` 等）。
 * 3. 保存失败不许吞掉后端原文：`settingsError` 里已经是 `errorMessage()` 的结果，
 *    这里直接显示，不再二次包装成"操作失败"。
 */

import { useEffect, useState } from 'react'
import type { DeviceSettings } from '../../generated'
import { useDeviceStore } from '../../stores/device-store'
import { DEVICE_SUB_CAPABILITIES } from './grant-dialog'

/**
 * `maxMinutes` 的硬上限（docs/09 §4.2：核心写 999 也只取 30）。
 *
 * 前端先拦一次是为了省一次往返，也是为了让用户看到**为什么**不合法；
 * 真正的判据仍在核心，那边回 `-32002` —— 这里不是第二份权威。
 */
export const DEVICE_SETTINGS_MAX_MINUTES = 30

/**
 * 每个能力"让模型能做什么"的一句话说明。
 *
 * 为什么与授权对话框的 hint 不共用：对话框讲的是"这次会话授权它做什么"，
 * 这里是"长期允许它做什么"。同一句文案在两种语境下都会有一半不贴切。
 */
const SETTING_HINTS: Readonly<Record<string, string>> = {
  screen: '让模型看到屏幕画面（截图的像素会交给模型）',
  ocr: '让模型读到屏幕上的文字',
  input: '让模型点鼠标、打字（只对允许的应用生效）',
  window: '让模型列出窗口、按窗口操作',
  clipboard: '让模型读写剪贴板',
}

/** 表单态：数字用字符串存，才能表达"用户刚把这一格清空"（而不是悄悄变成 0）。 */
interface SettingsForm {
  enabled: boolean
  capabilities: string[]
  allowedAppsText: string
  defaultMinutes: string
  maxMinutes: string
  maxActionsPerSession: string
}

function formOf(settings: DeviceSettings): SettingsForm {
  return {
    enabled: settings.enabled,
    capabilities: [...settings.capabilities],
    allowedAppsText: settings.allowedApps.join('\n'),
    defaultMinutes: String(settings.defaultMinutes),
    maxMinutes: String(settings.maxMinutes),
    maxActionsPerSession: String(settings.maxActionsPerSession),
  }
}

/** 把"每行一个 / 逗号分隔"的文本拆成列表：去空白、去空项、按顺序去重。 */
function parseAllowedApps(text: string): string[] {
  const apps: string[] = []
  for (const raw of text.split(/[\n,]/)) {
    const app = raw.trim()
    if (app.length > 0 && !apps.includes(app)) apps.push(app)
  }
  return apps
}

/** 只接受非负整数字面量：`12abc` / `-1` / `1.5` / 空串都算"没填好"。 */
function parseCount(text: string): number | null {
  const trimmed = text.trim()
  if (!/^\d+$/.test(trimmed)) return null
  return Number(trimmed)
}

interface DraftResult {
  settings: DeviceSettings | null
  /** 本地拦下的原因（中文，直接给用户看）；`null` = 可以提交。 */
  error: string | null
}

/**
 * 本地校验。只拦**确定不合法**的几条（与核心 `-32002` 的判据同构），未知能力故意放行。
 */
function buildDraft(form: SettingsForm): DraftResult {
  const maxMinutes = parseCount(form.maxMinutes)
  if (maxMinutes === null) {
    return { settings: null, error: '会话时长上限请填一个整数（单位：分钟）。' }
  }
  if (maxMinutes < 1) {
    return { settings: null, error: '会话时长上限至少 1 分钟。' }
  }
  if (maxMinutes > DEVICE_SETTINGS_MAX_MINUTES) {
    return {
      settings: null,
      error: `会话时长上限最多 ${DEVICE_SETTINGS_MAX_MINUTES} 分钟（核心的硬上限，超过会直接拒绝）。`,
    }
  }

  const defaultMinutes = parseCount(form.defaultMinutes)
  if (defaultMinutes === null) {
    return { settings: null, error: '默认时长请填一个整数（单位：分钟）。' }
  }
  if (defaultMinutes < 1) {
    return { settings: null, error: '默认时长至少 1 分钟。' }
  }
  if (defaultMinutes > maxMinutes) {
    return {
      settings: null,
      error: `默认时长（${defaultMinutes} 分钟）不能超过会话时长上限（${maxMinutes} 分钟）。`,
    }
  }

  const maxActionsPerSession = parseCount(form.maxActionsPerSession)
  if (maxActionsPerSession === null) {
    return { settings: null, error: '每次会话的动作上限请填一个整数。' }
  }
  if (maxActionsPerSession < 1) {
    return { settings: null, error: '每次会话的动作上限至少 1 次。' }
  }

  return {
    settings: {
      enabled: form.enabled,
      capabilities: [...form.capabilities],
      allowedApps: parseAllowedApps(form.allowedAppsText),
      defaultMinutes,
      maxMinutes,
      maxActionsPerSession,
    },
    error: null,
  }
}

export function DeviceSettingsCard() {
  const settings = useDeviceStore((state) => state.settings)
  const busy = useDeviceStore((state) => state.settingsBusy)
  const storeError = useDeviceStore((state) => state.settingsError)
  const loadSettings = useDeviceStore((state) => state.loadSettings)
  const saveSettings = useDeviceStore((state) => state.saveSettings)

  /** `null` = 还没读到设置（表单不渲染，免得用户在一份假默认值上改）。 */
  const [form, setForm] = useState<SettingsForm | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    // 只读一次：设置是宿主级的，页面级订阅由 `device.settings` 事件负责刷新。
    void loadSettings()
  }, [loadSettings])

  useEffect(() => {
    // 读到设置、或别的窗口推了 `device.settings`：重建表单。
    // 事件是唯一真相 —— 用户在这里的未保存修改会被覆盖，这是"设置已被别人改掉"的正确表现。
    if (settings !== null) setForm(formOf(settings))
  }, [settings])

  function patch(next: Partial<SettingsForm>): void {
    setForm((current) => (current === null ? current : { ...current, ...next }))
    setError(null)
  }

  function toggleCapability(id: string): void {
    setForm((current) => {
      if (current === null) return current
      const capabilities = current.capabilities.includes(id)
        ? current.capabilities.filter((item) => item !== id)
        : [...current.capabilities, id]
      return { ...current, capabilities }
    })
    setError(null)
  }

  async function save(): Promise<void> {
    if (form === null || busy) return
    const draft = buildDraft(form)
    if (draft.settings === null) {
      // 本地拦下就不发命令：多一次往返只会让用户多等一次，且后端回的也是同一句理由。
      setError(draft.error)
      return
    }
    setError(null)
    await saveSettings(draft.settings)
  }

  const message = error ?? storeError

  if (form === null) {
    return (
      <section className="device-panel__section" aria-label="宿主设备设置">
        <h3 className="device-panel__section-title">宿主设备设置</h3>
        <p className="panel-hint">
          {busy ? '正在读取设备设置…' : '还没读到设备设置（核心可能还没实现这个命令）。'}
        </p>
        {busy ? null : (
          <div className="device-panel__actions">
            <button type="button" className="button" onClick={() => void loadSettings()}>
              重试
            </button>
          </div>
        )}
        {message === null ? null : (
          <p className="inline-error" role="alert">
            {message}
          </p>
        )}
      </section>
    )
  }

  // 认识的五个能力按契约顺序排，后端新加的原样接在后面（不认识也要能看见、能取消勾选）。
  const extra = form.capabilities.filter(
    (id) => !DEVICE_SUB_CAPABILITIES.some((option) => option.id === id),
  )

  return (
    <section className="device-panel__section device-settings" aria-label="宿主设备设置">
      <h3 className="device-panel__section-title">宿主设备设置</h3>
      <p className="device-panel__hint">
        这是这台机器的设置，对 Agent 与 bot 代码都生效；改的是"允不允许"，不是"这次会话授权了什么"。
      </p>

      <label className="device-settings__switch">
        <input
          type="checkbox"
          aria-label="允许设备控制（总开关）"
          checked={form.enabled}
          disabled={busy}
          onChange={(event) => patch({ enabled: event.target.checked })}
        />
        <span className="device-settings__switch-label">允许设备控制</span>
      </label>
      <p className="device-panel__hint">
        关掉之后 Agent 与 bot 代码的所有设备调用都会被拒。
      </p>

      <fieldset className="device-settings__group">
        <legend className="device-panel__section-title">允许哪些能力（默认全不勾）</legend>
        {DEVICE_SUB_CAPABILITIES.map((option) => (
          <label className="device-settings__option" key={option.id}>
            <input
              type="checkbox"
              aria-label={option.label}
              checked={form.capabilities.includes(option.id)}
              disabled={busy}
              onChange={() => toggleCapability(option.id)}
            />
            <span className="device-settings__option-label">{option.label}</span>
            <code className="device-panel__code">{option.id}</code>
            <span className="device-settings__option-hint">
              {SETTING_HINTS[option.id] ?? option.hint}
            </span>
          </label>
        ))}
        {extra.map((id) => (
          <label className="device-settings__option" key={id}>
            <input
              type="checkbox"
              aria-label={id}
              checked
              disabled={busy}
              onChange={() => toggleCapability(id)}
            />
            <span className="device-settings__option-label">未知能力（按原样保留）</span>
            <code className="device-panel__code">{id}</code>
            <span className="device-settings__option-hint">
              界面还不认识这个能力；取消勾选才会把它从设置里去掉。
            </span>
          </label>
        ))}
        <p className="device-panel__hint">
          一个都不勾 = <strong>全拒</strong>（与下面应用列表的空值语义故意相反）。
        </p>
      </fieldset>

      <label className="device-settings__field">
        <span className="device-panel__section-title">允许的应用（每行一个，逗号分隔也行）</span>
        <textarea
          className="device-settings__textarea"
          aria-label="允许的应用（每行一个，逗号分隔也行）"
          rows={3}
          spellCheck={false}
          autoComplete="off"
          placeholder={'PathOfExile.exe\nchrome.exe'}
          value={form.allowedAppsText}
          disabled={busy}
          onChange={(event) => patch({ allowedAppsText: event.target.value })}
        />
        <span className="device-panel__hint">
          空 = <strong>不设上限</strong>（写进程名，例如{' '}
          <code className="device-panel__code">PathOfExile.exe</code>；不是窗口标题）。
        </span>
      </label>

      <div className="device-settings__numbers">
        <label className="device-settings__field device-settings__field--number">
          <span className="device-panel__section-title">默认时长（分钟）</span>
          <input
            className="device-settings__number"
            type="number"
            min={1}
            step={1}
            aria-label="默认时长（分钟）"
            value={form.defaultMinutes}
            disabled={busy}
            onChange={(event) => patch({ defaultMinutes: event.target.value })}
          />
        </label>
        <label className="device-settings__field device-settings__field--number">
          <span className="device-panel__section-title">会话时长上限（分钟）</span>
          <input
            className="device-settings__number"
            type="number"
            min={1}
            max={DEVICE_SETTINGS_MAX_MINUTES}
            step={1}
            aria-label="会话时长上限（分钟）"
            value={form.maxMinutes}
            disabled={busy}
            onChange={(event) => patch({ maxMinutes: event.target.value })}
          />
          <span className="device-panel__hint">硬上限 {DEVICE_SETTINGS_MAX_MINUTES} 分钟。</span>
        </label>
        <label className="device-settings__field device-settings__field--number">
          <span className="device-panel__section-title">每次会话动作上限</span>
          <input
            className="device-settings__number"
            type="number"
            min={1}
            step={1}
            aria-label="每次会话动作上限"
            value={form.maxActionsPerSession}
            disabled={busy}
            onChange={(event) => patch({ maxActionsPerSession: event.target.value })}
          />
        </label>
      </div>

      {busy ? (
        <p className="device-panel__hint" role="status">
          保存中…
        </p>
      ) : null}

      {message === null ? null : (
        <p className="inline-error" role="alert">
          {message}
        </p>
      )}

      <div className="device-panel__actions">
        <button type="button" className="button button--primary" disabled={busy} onClick={() => void save()}>
          {busy ? '保存中…' : '保存设置'}
        </button>
        <span className="device-panel__hint">保存后立刻对下一次设备调用生效（已开的会话按原授权走完）。</span>
      </div>
    </section>
  )
}