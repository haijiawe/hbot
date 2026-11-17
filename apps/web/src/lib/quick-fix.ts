/**
 * 问题面板的「Fix」：**机械可修**的判据与修法（docs/06-ide.md §5.3）。
 *
 * 判据是白名单，不是"能不能改"的猜测：
 * - `canFixMechanically` 是穷举 switch，**只有**登记过的 code 返回 true；
 * - 其它一切（核心将来新增的 code、前端自产的 `web.edit-apply-conflict`、`null`）
 *   走 `default: false` —— 未知 code 一律不可修。
 *
 * 为什么这么保守：Fix 是**不经模型**、直接改缓冲区的路径。判错就等于替用户乱改代码，
 * 而用户点按钮时的心理预期是"这一步是确定的"。宁可少一个按钮，让用户去点
 * 「请求 Agent 修复」（那条由模型自己判断怎么改）。
 *
 * 目前唯一实现：`HBOT_BARE_IMPORT`。
 * - 常量：`crates/hbot-code/src/error.rs:15`（`pub const BARE_IMPORT: &str = "HBOT_BARE_IMPORT";`）
 * - 消息模板：`crates/hbot-code/src/error.rs:118-120`
 *   （`bare module specifier '{specifier}' is not allowed; use a relative import ('./x')
 *   or the built-in 'hbot' module`）
 * - code 映射：`crates/hbot-code/src/error.rs:272`（`Self::BareImport { .. } => code::BARE_IMPORT`）
 * - 抛出点：`crates/hbot-code/src/modules.rs`（解析裸模块名时），所以运行时诊断里真的会出现。
 *
 * 修法就是把消息里报的那个裸名加上 `./` 前缀 —— 这正是核心消息自己给的提示
 * （"use a relative import ('./x')"）。改名之后目标文件是否存在由核心的下一轮编译
 * 再判定（`HBOT_MODULE_NOT_FOUND`），前端不在这里猜文件系统。
 */

import { applyLineEdits, splitDocumentLines, type LineEdit } from './edit-apply'
import type { Diagnostic } from './ipc'

/** `HBOT_BARE_IMPORT`：裸模块名（`import x from 'lib/x'` 里的 `lib/x`）。 */
export const BARE_IMPORT_CODE = 'HBOT_BARE_IMPORT'

export interface QuickFixPlan {
  /** 1-based 行号（诊断自己报的那一行）。 */
  line: number
  /** 改之前的整行文本。 */
  before: string
  /** 改之后的整行文本。 */
  after: string
  /** 一句话说明这次改什么（按钮 title 与可读性都靠它）。 */
  summary: string
}

/**
 * 这个 code 能不能机械修。
 *
 * 穷举 + 显式默认：新增 code 时必须在这里做一次决定，而不是被 `default: true` 蒙进去。
 * 已知的其它 code 都没有**唯一**的正确修法（改哪一行、改成什么取决于用户意图），
 * 所以一律不可修：`HBOT_COMPILE`、`HBOT_IMPORT_ESCAPE`、`HBOT_MODULE_NOT_FOUND`、
 * `HBOT_FORBIDDEN_SYNTAX`、`HBOT_NO_ENTRY`、`HBOT_IO`、`HBOT_JS_EXCEPTION`、
 * `HBOT_TIMEOUT`、`HBOT_MEMORY_LIMIT`、`HBOT_SANDBOX_DEAD`、`HBOT_MANIFEST_MISSING`、
 * `HBOT_MANIFEST_INVALID`、`HBOT_SCAFFOLD`、`web.edit-apply-conflict`。
 */
export function canFixMechanically(code: string | null): boolean {
  switch (code) {
    case BARE_IMPORT_CODE:
      return true
    default:
      return false
  }
}

/**
 * 从 `HBOT_BARE_IMPORT` 的消息里取出裸名。
 *
 * 消息模板的第一个引号串就是裸名（模板后面还跟着 `('./x')` 这种示例，所以只取第一处）。
 * 内置模块 `hbot` 是允许的（核心不会报它），相对/绝对路径本来就不是裸名 —— 这几种
 * 都返回 `null`：宁可没有修法，也不要把一条合法 import 改成错的。
 */
export function bareSpecifierOf(message: string): string | null {
  const match = /bare module specifier '([^']+)'/.exec(message)
  const specifier = match?.[1]
  if (specifier === undefined || specifier.length === 0) return null
  if (specifier === 'hbot') return null
  if (specifier.startsWith('./') || specifier.startsWith('../') || specifier.startsWith('/')) {
    return null
  }
  return specifier
}

/**
 * 算出这次机械修复要改成什么；不可修（或定位不到）时返回 `null`。
 *
 * 只改诊断自己报的那一行里的那个字符串字面量，不碰别处：一行里写两个 import（分号分隔）
 * 时也只动第一处匹配，剩下的交给下一轮编译再报。
 */
export function planMechanicalFix(
  item: Diagnostic,
  lines: readonly string[],
): QuickFixPlan | null {
  if (!canFixMechanically(item.code)) return null
  // 判据说可修，但修法只有下面这一种：新增可修 code 时必须同时补实现，
  // 否则这里返回 null（按钮点了会给出"没有机械修法"的原因，不会静默什么都不做）。
  if (item.code !== BARE_IMPORT_CODE) return null
  if (item.line <= 0) return null
  const before = lines[item.line - 1]
  if (before === undefined) return null
  const specifier = bareSpecifierOf(item.message)
  if (specifier === null) return null

  const single = `'${specifier}'`
  const double = `"${specifier}"`
  const needle = before.includes(single) ? single : before.includes(double) ? double : null
  if (needle === null) return null

  const quote = needle.slice(0, 1)
  const after = before.replace(needle, `${quote}./${specifier}${quote}`)
  if (after === before) return null
  return {
    line: item.line,
    before,
    after,
    summary: `把裸模块名 '${specifier}' 改成相对导入 './${specifier}'`,
  }
}

/** 计划 → 行级编辑（喂给 `applyLineEdits` / `applyEditsAsOneUndoGroup` 的同一套坐标）。 */
export function planToLineEdit(plan: QuickFixPlan): LineEdit {
  return { startLine: plan.line, endLine: plan.line, lines: [plan.after] }
}

/** 纯文本路径：把计划落到文本上（行尾符沿用 `edit-apply` 的同一套规则）。 */
export function applyMechanicalFix(content: string, plan: QuickFixPlan): string {
  return applyLineEdits(content, [planToLineEdit(plan)])
}

/** 便捷：给定缓冲区文本直接算出修好的文本；不可修时返回 `null`。 */
export function fixedContent(item: Diagnostic, content: string): string | null {
  const plan = planMechanicalFix(item, splitDocumentLines(content))
  return plan === null ? null : applyMechanicalFix(content, plan)
}