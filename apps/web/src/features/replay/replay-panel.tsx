/**
 * 事件回放面板（底部面板的「回放」页签，lazy chunk）。
 *
 * 回放 = 用**当前代码**把历史入站事件重跑一遍，再逐条比对「历史回复」与「重跑回复」。
 * 它不联网、不写库（核心另起 isolate，回复只收集在内存里），所以这是个纯排查工具：
 * 改完 handler 想确认"只有这一类消息变了"，跑一次看差异即可。
 *
 * bot 选择的来源：优先 `bot.list`（页面上唯一可信的 bot 清单），读不到时退回
 * `run-store` 里当前选中的 bot —— 面板在"列表接口还没回来/不可用"时也仍然可用。
 */

import { useEffect, useMemo, useState, type FormEvent } from 'react'
import type { ReplayReport } from '../../generated'
import { errorMessage, formatCount } from '../../lib/format'
import type { BotSummary } from '../../lib/ipc'
import { dispatch } from '../../lib/ipc'
import { useRunStore } from '../../stores/run-store'
import { ReplayEntryRow } from './replay-entry-row'

/**
 * 最多渲染多少条 entry。
 *
 * 不做虚拟化（回放是人工排查，列表通常几十条）；但历史上限是"全量"，一条 SQL 级
 * 的误配就能拉出几万条 —— 超过这个数只画前面这些并明示，别让 DOM 把标签页拖死。
 */
export const MAX_RENDERED_ENTRIES = 200

/** 输入框 id：`<label htmlFor>` 与 `aria-label` 都指向它，读屏与测试都靠它定位。 */
const BOT_SELECT_ID = 'replay-bot'
const FROM_INPUT_ID = 'replay-from'
const LIMIT_INPUT_ID = 'replay-limit'

/** 解析结果：`ok: false` 时 `reason` 直接给用户看（中文、说明怎么改）。 */
type Parsed<T> = { ok: true; value: T } | { ok: false; reason: string }

/**
 * `from` 输入 → Unix 毫秒。空 = 不限（`null`）。
 *
 * `<input type="datetime-local">` 的 value 是 `YYYY-MM-DDTHH:mm`（本地时区、无后缀），
 * `Date.parse` 对这个形状按**本地时区**解释 —— 正好是用户看到的那个时刻。
 * 纯 ISO 文本也走同一条路径，所以两种输入不用分叉。
 */
function parseFrom(text: string): Parsed<number | null> {
  const trimmed = text.trim()
  if (trimmed.length === 0) return { ok: true, value: null }
  const at = Date.parse(trimmed)
  if (!Number.isFinite(at)) {
    return { ok: false, reason: '起始时间看不懂：请用日期时间选择器，或填 ISO 文本；留空表示不限。' }
  }
  return { ok: true, value: at }
}

/**
 * `limit` 输入 → 条数上限。空 = 全量（`null`）。
 *
 * **`0` 必须在前端拦住**：契约里 `0` 是 `-32002`（不是"全量"），放过去只会让用户
 * 等一次往返换回一条参数错误；这里直接说清"留空才是全量"。
 */
function parseLimit(text: string): Parsed<number | null> {
  const trimmed = text.trim()
  if (trimmed.length === 0) return { ok: true, value: null }
  const value = Number(trimmed)
  if (!Number.isInteger(value)) {
    return { ok: false, reason: '条数上限必须是整数；留空表示全量。' }
  }
  if (value === 0) {
    return { ok: false, reason: '条数上限不能是 0：0 会被后端按 -32002 拒绝，留空表示全量。' }
  }
  if (value < 0) return { ok: false, reason: '条数上限必须是正数；留空表示全量。' }
  return { ok: true, value }
}

export function ReplayPanel() {
  // run-store 的 `botId` 是"当前选中的 bot"：列表读不到时拿它兜底。
  const currentBotId = useRunStore((state) => state.botId)

  const [bots, setBots] = useState<BotSummary[]>([])
  const [botId, setBotId] = useState(currentBotId)
  const [fromText, setFromText] = useState('')
  const [limitText, setLimitText] = useState('')
  const [running, setRunning] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [report, setReport] = useState<ReplayReport | null>(null)

  // 只读一次清单；组件卸载后到达的响应丢掉（避免对已卸载组件 setState）。
  useEffect(() => {
    let disposed = false
    void dispatch({ type: 'bot.list' })
      .then((list) => {
        if (!disposed) setBots(list)
      })
      .catch(() => {
        // 清单读不到不算致命：下面会退回 run-store 的当前 bot，面板照常能跑。
        if (!disposed) setBots([])
      })
    return () => {
      disposed = true
    }
  }, [])

  // 默认选中：先等清单（第一个 bot），清单为空再用 run-store 的当前 bot。
  useEffect(() => {
    if (botId !== '') return
    const first = bots[0]
    if (first !== undefined) {
      setBotId(first.botId)
      return
    }
    if (currentBotId !== '') setBotId(currentBotId)
  }, [bots, botId, currentBotId])

  const options = useMemo(() => {
    if (bots.length > 0) {
      return bots.map((bot) => ({ id: bot.botId, label: `${bot.name} · ${bot.platform}` }))
    }
    // 清单不可用：把兜底的 bot 也列成一个选项，否则 `<select>` 的 value 会指向不存在的 option。
    return botId === '' ? [] : [{ id: botId, label: botId }]
  }, [bots, botId])

  async function submit(event: FormEvent<HTMLFormElement>): Promise<void> {
    event.preventDefault()
    const from = parseFrom(fromText)
    if (!from.ok) {
      setError(from.reason)
      return
    }
    const limit = parseLimit(limitText)
    if (!limit.ok) {
      setError(limit.reason)
      return
    }
    const target = botId.trim()
    if (target === '') {
      setError('还没有可回放的 bot：先打开一个带 bot 的项目。')
      return
    }

    setRunning(true)
    setError(null)
    try {
      const result = await dispatch({
        type: 'replay_run',
        botId: target,
        from: from.value,
        limit: limit.value,
      })
      setReport(result)
    } catch (failure) {
      // 后端 message 原文照抄（`IpcError` 已经把 `[code] message` 渲染成一句话），
      // 不要在外面再包一层"回放失败"——用户要能直接把这句话贴给开发者。
      setReport(null)
      setError(errorMessage(failure))
    } finally {
      setRunning(false)
    }
  }

  const visibleEntries = report === null ? [] : report.entries.slice(0, MAX_RENDERED_ENTRIES)
  const truncated = report !== null && report.entries.length > visibleEntries.length

  return (
    <section className="replay" aria-label="事件回放">
      <p className="panel-hint replay__warning">
        回放会用当前代码把历史事件重跑一遍：不联网、不写库，只在内存里对账。
      </p>

      {/* `noValidate`：校验全在 `parseFrom` / `parseLimit` 里做，中文理由也写在它们那儿。
          `min={1}` 交给浏览器只会弹一句原生气泡并**吞掉 submit**，用户看不到真正的原因。 */}
      <form className="replay__form" noValidate onSubmit={(event) => void submit(event)}>
        <div className="replay__field">
          <label className="replay__label" htmlFor={BOT_SELECT_ID}>
            bot
          </label>
          <select
            id={BOT_SELECT_ID}
            className="replay__select"
            aria-label="选择要回放的 bot"
            value={botId}
            disabled={running}
            onChange={(event) => setBotId(event.target.value)}
          >
            {options.length === 0 ? <option value="">（还没有 bot）</option> : null}
            {options.map((option) => (
              <option key={option.id} value={option.id}>
                {option.label}
              </option>
            ))}
          </select>
        </div>

        <div className="replay__field">
          <label className="replay__label" htmlFor={FROM_INPUT_ID}>
            起始时间
          </label>
          <input
            id={FROM_INPUT_ID}
            className="replay__input"
            type="datetime-local"
            aria-label="起始时间（留空表示不限）"
            value={fromText}
            disabled={running}
            onChange={(event) => setFromText(event.target.value)}
          />
        </div>

        <div className="replay__field replay__field--limit">
          <label className="replay__label" htmlFor={LIMIT_INPUT_ID}>
            条数上限
          </label>
          <input
            id={LIMIT_INPUT_ID}
            className="replay__input"
            type="number"
            min={1}
            step={1}
            placeholder="留空 = 全量"
            aria-label="条数上限（留空表示全量）"
            value={limitText}
            disabled={running}
            onChange={(event) => setLimitText(event.target.value)}
          />
        </div>

        <button type="submit" className="button button--primary replay__run" disabled={running}>
          {running ? '回放中…' : '开始回放'}
        </button>
      </form>

      {error === null ? null : (
        <p className="inline-error replay__error" role="alert">
          {error}
        </p>
      )}

      {report === null ? (
        <p className="panel-empty replay__empty" role="status">
          {running ? '正在回放…' : '还没有回放结果：选好范围和条数后点「开始回放」。'}
        </p>
      ) : (
        <div className="replay__report">
          <div className="replay__meta">
            {report.project} · {report.bot} · 起点{' '}
            {report.from === null ? '不限' : new Date(report.from).toLocaleString('zh-CN')} · 上限{' '}
            {report.limit === null ? '全量' : formatCount(report.limit)}
          </div>

          <div className="replay__summary" role="status" aria-label="回放统计">
            <span className="replay__summary-item">事件 {formatCount(report.events)}</span>
            <span className="replay__summary-item replay__summary-item--match">
              一致 {formatCount(report.matchedEvents)}
            </span>
            <span className="replay__summary-item replay__summary-item--diff">
              有差异 {formatCount(report.differingEvents)}
            </span>
            <span className="replay__summary-item replay__summary-item--error">
              出错 {formatCount(report.failedEvents)}
            </span>
          </div>

          <div className="replay__reply-summary" aria-label="逐条回复统计">
            逐条回复：一致 {formatCount(report.same)} · 多出 {formatCount(report.extra)} · 缺少{' '}
            {formatCount(report.missing)} · 顺序不同 {formatCount(report.reordered)}
          </div>

          {visibleEntries.length === 0 ? (
            <p className="panel-empty replay__empty" role="status">
              这个范围里没有可回放的事件。
            </p>
          ) : (
            <ol className="replay__entries" aria-label="回放条目">
              {visibleEntries.map((entry, index) => (
                <ReplayEntryRow key={`${index}-${entry.inboundId}`} entry={entry} index={index} />
              ))}
            </ol>
          )}

          {truncated ? (
            <p className="panel-hint replay__truncated">
              只显示前 {MAX_RENDERED_ENTRIES} 条（共 {formatCount(report.entries.length)} 条）。
            </p>
          ) : null}
        </div>
      )}
    </section>
  )
}