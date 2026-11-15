/**
 * 调试变量的展示格式化；纯函数，便于单测（不碰 DOM、不碰 Monaco）。
 *
 * 契约（docs/06-ide.md §6.2）：宿主只发可 JSON 序列化的局部变量，函数 / 循环引用 /
 * 超大对象由宿主替换成字符串 `<unserializable>`。UI 侧**不解释、不求值**任何表达式，
 * 只负责把收到的值渲染成一段 JSON 文本。
 *
 * 这里兜住的是"事件里的值仍然序列化不了"的情况（`undefined`、函数、`BigInt`、循环引用），
 * 保证面板永远显示得出一个字面量，而不是把异常冒进 React 渲染。
 */

/** 宿主约定的不可序列化占位符（与 `src/generated/DebugScope.ts` 的注释一致）。 */
export const UNSERIALIZABLE = '<unserializable>'

/**
 * 把一个调试变量值格式化成 JSON 文本。
 *
 * - 字符串 → 带引号的 JSON 字符串（`"hi"`），这样"字符串"和"值是 null"能区分开；
 * - 数字 / 布尔 → `String()`（与 JSON 字面量一致）；
 * - 其余（对象、数组）→ 紧凑 JSON；
 * - 序列化不了（`undefined`、函数、`Symbol`、`BigInt`、循环引用）→ `<unserializable>`。
 */
export function formatDebugValue(value: unknown): string {
  if (typeof value === 'number' || typeof value === 'boolean') return String(value)
  if (value === null) return 'null'
  try {
    const json = JSON.stringify(value)
    return json ?? UNSERIALIZABLE
  } catch {
    // JSON.stringify 只在 BigInt 与循环引用上抛；没有可展示的 JSON 形态。
    return UNSERIALIZABLE
  }
}