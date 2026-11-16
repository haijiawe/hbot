/**
 * 试跑的"回复"从哪来：本地平台的 bot 不联网，`ctx.reply(text)` 的**每一行**都会被写成
 * 一条 run log，前缀 `[reply] `（真源 `crates/hbot-adapters/src/platforms/local/mod.rs`
 * 的 `REPLY_PREFIX`，改动那里就要同步这里的常量）。
 *
 * 所以"回复区"不是第二个日志缓冲，而是一次纯函数投影：日志仍然只归 `run-store` 收
 * （`Event::RunLog` 的唯一订阅点在 `app/use-core-events.ts`），本文件只把已经归约好的
 * 行过滤成回复文本。
 */

import type { LogLine } from '../../lib/ipc'

/** 本地平台的回复前缀；必须与 Rust 侧 `REPLY_PREFIX` 逐字一致。 */
export const REPLY_PREFIX = '[reply] '

/**
 * 一行日志里能被当成回复的部分：`source` 是 wire 语义（`bot` / `agent`），
 * `line` 是正文。这样 `extractReplies` 与 `run-store` 的 `LogLine` 解耦，
 * 单测不需要造整个 store 的形状。
 */
export interface ReplySourceLine {
  source: string
  line: string
}

/**
 * 从日志行里按时间顺序取回复：只认 `source === 'bot'` 且带 `[reply] ` 前缀的行，
 * 去掉前缀后原样返回（`[reply] ` 后面是空字符串时保留一个空回复，不要静默丢掉）。
 */
export function extractReplies(lines: readonly ReplySourceLine[]): string[] {
  const replies: string[] = []
  for (const item of lines) {
    if (item.source !== 'bot') continue
    if (!item.line.startsWith(REPLY_PREFIX)) continue
    replies.push(item.line.slice(REPLY_PREFIX.length))
  }
  return replies
}

/**
 * 面板用的适配层：从 `run-store` 已归约的日志里取**选中 bot** 的回复。
 *
 * 为什么需要这一步：`run-store` 的 `LogLine.source` 是 **botId**（见
 * `stores/run-store.ts` 的 `ingestEvent`），不是 wire 的 `bot` / `agent`。面板按选中的
 * bot 过滤（多 bot 时不把别人的回复显示成本 bot 的），再折回 wire 形状交给
 * `extractReplies` —— 过滤规则只有 `extractReplies` 一份，这里只做形状转换。
 */
export function repliesOfBot(logs: readonly LogLine[], botId: string | null): string[] {
  if (botId === null || botId.length === 0) return []
  const lines: ReplySourceLine[] = []
  for (const log of logs) {
    if (log.source !== botId) continue
    lines.push({ source: 'bot', line: log.message })
  }
  return extractReplies(lines)
}