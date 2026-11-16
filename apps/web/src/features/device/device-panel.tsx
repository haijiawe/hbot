/**
 * 底部面板的「设备」页签（lazy chunk，见 `features/run/bottom-panel.tsx`）。
 *
 * 它显示的是**核心告诉我们的**设备会话状态（docs/09-device-capability.md §7）：
 * - 状态一律等 `device.status` 事件；这个面板不推断"大概已经撤销了"。
 * - 倒计时只用契约里的 `remainingMs` 起步，本地每 1 秒减一格，**不去减 `deadline` 与本机
 *   时钟的差**（那个差会被系统时间/休眠拉扯，`DeviceStatus.remainingMs` 的文档写明了）。
 * - `state` / `sub` / `allowedApps` 都是开放字符串：不认识的值原样显示，绝不抛错。
 *
 * 待处理的 `device.request` 显示在最上面：`reason` 是 Agent 写的不可信文本，这里只当纯文本
 * 渲染（React 默认转义），**不许** `dangerouslySetInnerHTML`。
 */

import { useEffect, useState } from 'react'
import type { DeviceStatus } from '../../generated'
import { formatClock } from '../../lib/format'
import { useDeviceStore } from '../../stores/device-store'
import { DEVICE_SUB_CAPABILITIES, GrantDialog } from './grant-dialog'
import { DeviceSettingsCard } from './settings-card'

/** 倒计时刷新间隔：契约要求"每 1 秒本地减"，不重新请求核心。 */
const COUNTDOWN_TICK_MS = 1000
/** 剩余不足 1 分钟时用醒目样式（`remainingMs <= 60000`）。 */
const COUNTDOWN_URGENT_MS = 60_000

/**
 * `state` 的中文映射。
 *
 * 用 `Map` 而不是对象字面量：`state` 是**开放字符串**，`OBJECT[state]` 遇到
 * `constructor` / `toString` 这类名字会取到原型上的函数，`Map` 不会。
 */
const STATE_LABEL = new Map<string, string>([
  ['none', '未开始'],
  ['requested', '已请求'],
  ['active', '进行中'],
  ['expired', '已到期'],
  ['revoked', '已撤销'],
  ['denied', '被拒绝'],
])

/** 未知状态原样显示（后端将来可能加新状态，前端不能因此白屏）。 */
function stateLabel(state: string): string {
  return STATE_LABEL.get(state) ?? state
}

/** 把毫秒数格式化成 `分:秒`（倒计时展示用，不做本地时钟换算）。 */
function formatRemaining(ms: number): string {
  const totalSeconds = Math.max(Math.ceil(ms / 1000), 0)
  const minutes = Math.floor(totalSeconds / 60)
  const seconds = totalSeconds % 60
  return `${minutes}:${String(seconds).padStart(2, '0')}`
}

/**
 * 本地倒计时：`remainingMs` 是起点，之后每秒减 1 秒，到 0 就停在 0。
 *
 * 到 0 **不**改 `state` —— 到期由核心结算并推 `device.status`（docs/09 §7 规则 2），
 * 前端自己翻成"已到期"会造出第二份真相。
 */
function useCountdown(status: DeviceStatus | null): number | null {
  const active = status !== null && status.state === 'active'
  const seed = status?.remainingMs ?? null
  const [remaining, setRemaining] = useState<number | null>(seed)

  useEffect(() => {
    setRemaining(seed)
    if (!active || seed === null) return
    const timer = window.setInterval(() => {
      setRemaining((current) => (current === null ? null : Math.max(current - COUNTDOWN_TICK_MS, 0)))
    }, COUNTDOWN_TICK_MS)
    return () => window.clearInterval(timer)
  }, [active, seed])

  return active ? remaining : null
}

export function DevicePanel() {
  const botId = useDeviceStore((state) => state.botId)
  const status = useDeviceStore((state) => state.status)
  const pending = useDeviceStore((state) => state.pending)
  const busy = useDeviceStore((state) => state.busy)
  const error = useDeviceStore((state) => state.error)
  const refresh = useDeviceStore((state) => state.refresh)
  const revoke = useDeviceStore((state) => state.revoke)
  const clearPending = useDeviceStore((state) => state.clearPending)
  /** `null` = 对话框关着；数组 = 预勾的子能力（来自待处理请求）。 */
  const [dialogSub, setDialogSub] = useState<readonly string[] | null>(null)

  const remaining = useCountdown(status)

  const requestBlock =
    pending === null ? null : (
      <section className="device-panel__request" role="alert" aria-label="待处理的设备请求">
        <h3 className="device-panel__request-title">Agent 请求设备能力</h3>
        {/* 不可信数据：纯文本渲染，绝不拼 HTML。 */}
        <p className="device-panel__request-reason">{pending.reason}</p>
        <p className="device-panel__hint">
          请求的能力：
          {pending.sub.length === 0 ? '（请求里没有列出子能力）' : pending.sub.join('、')}
        </p>
        <div className="device-panel__actions">
          <button type="button" className="button" onClick={clearPending}>
            拒绝
          </button>
          <button
            type="button"
            className="button button--primary"
            onClick={() => setDialogSub(pending.sub)}
          >
            允许…
          </button>
        </div>
        <p className="device-panel__hint">
          理由由 Agent 撰写，界面只展示、不当指令；「拒绝」不会调用任何设备命令。
        </p>
      </section>
    )

  const dialog =
    dialogSub === null ? null : (
      <GrantDialog initialSub={dialogSub} onClose={() => setDialogSub(null)} />
    )

  if (botId.length === 0) {
    return (
      <div className="device-panel">
        {requestBlock}
        <p className="panel-empty">先启动一个 bot。</p>
        <p className="panel-hint">设备会话按 bot 记：选一个运行中的 bot 之后才能开始会话。</p>
        {/* 设置是宿主级的：没有 bot 也要能改，否则第一个 bot 起来之前连"允许什么"都设不了。 */}
        <DeviceSettingsCard />
        {dialog}
      </div>
    )
  }

  const state = status?.state ?? null
  const urgent = remaining !== null && remaining <= COUNTDOWN_URGENT_MS
  const sub = status?.sub ?? []
  const allowedApps = status?.allowedApps ?? []

  return (
    <div className="device-panel">
      {requestBlock}

      <section className="device-panel__status" aria-label="设备会话状态">
        <div className="device-panel__status-row">
          <span className={`device-panel__state${state === 'active' ? ' device-panel__state--active' : ''}`}>
            {status === null ? '还没有读到设备状态' : stateLabel(status.state)}
          </span>
          {remaining === null ? null : (
            <span
              className={`device-panel__countdown${
                urgent ? ' device-panel__countdown--urgent' : ''
              }`}
              role="timer"
              aria-label="会话剩余时间"
            >
              剩余 {formatRemaining(remaining)}
              {urgent ? '（即将到期）' : ''}
            </span>
          )}
          <span className="device-panel__spacer" />
          <button
            type="button"
            className="button"
            onClick={() => void refresh(botId)}
            disabled={busy}
          >
            {busy ? '刷新中…' : '刷新'}
          </button>
          <button
            type="button"
            className="button button--danger"
            aria-label="立即撤销设备会话"
            onClick={() => void revoke()}
            disabled={busy}
          >
            立即撤销
          </button>
        </div>

        {state === 'active' && remaining === 0 ? (
          <p className="device-panel__hint">
            本地倒计时已到 0，但到期由核心确认：状态会在{' '}
            <code className="device-panel__code">device.status</code> 事件之后才变成已到期。
          </p>
        ) : null}
        {state === 'expired' ? (
          <p className="device-panel__hint">会话已到期（不会自动续），要再控制设备需要重新开始一次会话。</p>
        ) : null}
        {state === 'revoked' ? (
          <p className="device-panel__hint">会话已撤销：核心已经停止接受这个会话的任何设备调用。</p>
        ) : null}
        {state === 'denied' ? (
          <p className="device-panel__hint">请求被拒绝：这次没有开始会话。</p>
        ) : null}
        {error === null ? null : (
          <p className="inline-error" role="alert">
            {error}
          </p>
        )}
      </section>

      <section className="device-panel__section" aria-label="能力清单">
        <h3 className="device-panel__section-title">能力清单</h3>
        {sub.length === 0 ? (
          <p className="panel-hint">这次会话没有授权任何设备能力。</p>
        ) : (
          <ul className="device-panel__list">
            {sub.map((item) => {
              const known = DEVICE_SUB_CAPABILITIES.find((capability) => capability.id === item)
              return (
                <li className="device-panel__capability" key={item}>
                  <code className="device-panel__code">{item}</code>
                  <span className="device-panel__capability-label">
                    {known === undefined ? '未知能力（按原样显示）' : known.label}
                  </span>
                  <span className="device-panel__capability-hint">
                    {known === undefined ? '界面还不认识这个能力，因此不能替它做说明' : known.hint}
                  </span>
                </li>
              )
            })}
          </ul>
        )}
      </section>

      <section className="device-panel__section" aria-label="允许的应用">
        <h3 className="device-panel__section-title">允许的应用</h3>
        {allowedApps.length === 0 ? (
          // 空数组 = 全拒，**不是**"无限制"。这条文案是安全语义，别改成"全部允许"。
          <p className="device-panel__deny">不允许任何应用（点击与输入会被拒绝）</p>
        ) : (
          <ul className="device-panel__list">
            {allowedApps.map((app) => (
              <li className="device-panel__app" key={app}>
                <code className="device-panel__code">{app}</code>
              </li>
            ))}
          </ul>
        )}
        <p className="panel-hint">
          只对进程名命中的窗口生效；提权窗口（任务管理器、UAC 提示）由系统拦着，控制不了。
        </p>
      </section>

      {status !== null && status.sessionId !== null ? (
        <section className="device-panel__section" aria-label="会话信息">
          <h3 className="device-panel__section-title">会话信息</h3>
          <p className="panel-hint">
            会话 <code className="device-panel__code">{status.sessionId}</code>
            {status.grantedAt === null ? null : ` · 授权于 ${formatClock(status.grantedAt)}`}
            {status.deadline === null ? null : ` · 到期于 ${formatClock(status.deadline)}`}
          </p>
        </section>
      ) : null}

      <DeviceSettingsCard />

      <div className="device-panel__footer">
        <button
          type="button"
          className="button button--primary"
          onClick={() => setDialogSub([])}
          disabled={busy}
        >
          开始会话
        </button>
        <span className="device-panel__hint">
          「立即撤销」随时可点：撤销是全局失效（不是暂停），对已经结束的会话重发也安全。
        </span>
      </div>

      {dialog}
    </div>
  )
}