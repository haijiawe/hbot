/**
 * 行槽位右键菜单的**菜单项表**（纯函数，不 import monaco / React）。
 *
 * 为什么单独一层：菜单项随"这一行有什么"变化（有断点 / 有日志点 / 有条件），而
 * `editor-surface.tsx` 里带着 Monaco 实例，jsdom 里跑不起来 —— 抽出来之后
 * "有日志点就出现编辑日志点"这条能被测试钉住，而不是只剩肉眼。
 *
 * 一行可以同时有断点与日志点（store 里同一行两条），所以两组菜单项会**同时出现**：
 * 先断点组（停下 vs 不停下是更重的决定），后日志点组。
 */

/** 菜单项的动作。`edit-*` 会把同一个面板切成输入框，其余动作直接生效并关闭菜单。 */
export type GutterMenuAction =
  | 'edit-condition'
  | 'clear-condition'
  | 'remove-breakpoint'
  | 'add-logpoint'
  | 'edit-logpoint'
  | 'remove-logpoint'

/** 一个菜单项：动作 + 文案 + 是否禁用（禁用而不隐藏，让用户知道有这个能力）。 */
export interface GutterMenuItem {
  action: GutterMenuAction
  label: string
  disabled: boolean
}

/** 这一行的当前状态（从 store 的条目里读出来的三个值）。 */
export interface GutterMenuState {
  /** 这一行有没有**断点**（`log: null` 的那条）。 */
  hasBreakpoint: boolean
  /** 断点条件原文（没有条件 / 没有断点都是 `null`）。 */
  condition: string | null
  /** 日志点模板（没有日志点就是 `null`）。 */
  logpoint: string | null
}

/**
 * 按行状态算菜单项。
 *
 * 规则：
 * - 有断点 → 编辑条件… / 清除条件（没条件时禁用）/ 删除断点；
 * - 有日志点 → 编辑日志点… / 删除日志点；没有 → 添加日志点…；
 * - 两样都没有时这个函数返回空数组（调用方根本不该打开菜单：没有断点时的右键
 *   意图不明确，而"加断点"左键一下就够了）。
 */
export function gutterMenuItems(state: GutterMenuState): GutterMenuItem[] {
  const items: GutterMenuItem[] = []
  if (state.hasBreakpoint) {
    items.push({ action: 'edit-condition', label: '编辑条件…', disabled: false })
    items.push({ action: 'clear-condition', label: '清除条件', disabled: state.condition === null })
    items.push({ action: 'remove-breakpoint', label: '删除断点', disabled: false })
  }
  if (state.logpoint === null) {
    items.push({ action: 'add-logpoint', label: '添加日志点…', disabled: false })
  } else {
    items.push({ action: 'edit-logpoint', label: '编辑日志点…', disabled: false })
    items.push({ action: 'remove-logpoint', label: '删除日志点', disabled: false })
  }
  return items
}