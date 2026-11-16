/**
 * 试跑面板（底部面板的「试跑」页签，lazy chunk）。
 *
 * 一个最小闭环：选 bot → 开始试跑（拿到会话）→ 注入一条模拟消息 → 看 bot 的回复 → 停止。
 * 回复**不是**第二份日志缓冲：日志只有 `run-store` 一份（`Event::RunLog` 的唯一订阅点
 * 在 `app/use-core-events.ts`），本组件从 `useRunStore` 读已归约的 `logs`，用
 * `repliesOfBot` 投影出 `[reply] ` 行。这样多 bot 时也不会把别人的回复显示成本 bot 的。
 */

import { useEffect, useMemo, useState, type FormEvent } from 'react'
import { useRunStore } from '../../stores/run-store'
import { useTryRunStore } from '../../stores/tryrun-store'
import { repliesOfBot } from './replies'

/** 输入框的 id：`<label htmlFor>` 与 `aria-label` 都指向它，读屏与测试都稳。 */
const MESSAGE_INPUT_ID = 'tryrun-message'

export function TryRunPanel() {
  const bots = useTryRunStore((state) => state.bots)
  const botsLoaded = useTryRunStore((state) => state.botsLoaded)
  const selectedBotId = useTryRunStore((state) => state.selectedBotId)
  const conversationId = useTryRunStore((state) => state.conversationId)
  const starting = useTryRunStore((state) => state.starting)
  const sending = useTryRunStore((state) => state.sending)
  const stopping = useTryRunStore((state) => state.stopping)
  const notice = useTryRunStore((state) => state.notice)
  const error = useTryRunStore((state) => state.error)
  const loadBots = useTryRunStore((state) => state.loadBots)
  const selectBot = useTryRunStore((state) => state.selectBot)
  const start = useTryRunStore((state) => state.start)
  const send = useTryRunStore((state) => state.send)
  const stop = useTryRunStore((state) => state.stop)

  // 日志的唯一来源：run-store 已经归约好的缓冲（本面板不订阅事件、不另存一份）。
  const logs = useRunStore((state) => state.logs)
  const replies = useMemo(() => repliesOfBot(logs, selectedBotId), [logs, selectedBotId])

  const [text, setText] = useState('')

  useEffect(() => {
    void loadBots()
  }, [loadBots])

  const canSend = conversationId !== null && text.trim().length > 0 && !sending

  async function submit(event: FormEvent<HTMLFormElement>): Promise<void> {
    event.preventDefault()
    const sent = await send(text)
    // 只有真的发出去才清空输入框：失败时用户的字要留着，方便改完重试。
    if (sent) setText('')
  }

  return (
    <section className="tryrun" aria-label="试跑">
      {error === null ? null : (
        <p className="inline-error tryrun__error" role="alert">
          {error}
        </p>
      )}

      {bots.length === 0 ? (
        <p className="panel-empty tryrun__empty" role="status">
          {botsLoaded
            ? '还没有可试跑的 bot：先打开一个带 bot 的项目。'
            : '正在读取 bot 列表…'}
        </p>
      ) : (
        <>
          <div className="tryrun__controls">
            <select
              className="tryrun__select"
              aria-label="选择 bot"
              value={selectedBotId ?? ''}
              onChange={(event) => selectBot(event.target.value)}
            >
              {bots.map((bot) => (
                <option key={bot.botId} value={bot.botId}>
                  {bot.name} · {bot.platform}
                </option>
              ))}
            </select>
            <button
              type="button"
              className="button button--primary"
              onClick={() => void start()}
              disabled={starting || selectedBotId === null}
            >
              开始试跑
            </button>
            <button
              type="button"
              className="button"
              onClick={() => void stop()}
              disabled={stopping || selectedBotId === null}
            >
              停止试跑
            </button>
            {conversationId === null ? null : (
              <span className="tryrun__session">会话：{conversationId}</span>
            )}
          </div>

          {notice === null ? null : (
            <p className="panel-hint tryrun__notice" role="status">
              {notice}
            </p>
          )}

          <form className="tryrun__form" onSubmit={(event) => void submit(event)}>
            <label className="tryrun__label" htmlFor={MESSAGE_INPUT_ID}>
              模拟消息
            </label>
            <input
              id={MESSAGE_INPUT_ID}
              className="tryrun__input"
              value={text}
              onChange={(event) => setText(event.target.value)}
              placeholder="输入一条模拟消息，回车发送"
              aria-label="要注入的消息"
            />
            <button type="submit" className="button button--primary" disabled={!canSend}>
              发送
            </button>
          </form>

          <div className="tryrun__replies">
            <h3 className="tryrun__replies-title">回复</h3>
            {replies.length === 0 ? (
              <p className="panel-empty tryrun__empty" role="status">
                还没有回复
              </p>
            ) : (
              <ol className="tryrun__reply-list" aria-label="试跑回复" aria-live="polite">
                {replies.map((reply, index) => (
                  <li className="tryrun__reply" key={index}>
                    {reply}
                  </li>
                ))}
              </ol>
            )}
          </div>
        </>
      )}
    </section>
  )
}