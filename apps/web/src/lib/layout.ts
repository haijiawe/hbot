import type { CSSProperties } from 'react'

/**
 * IDE 布局的唯一真源（docs/06-ide.md §2）。
 *
 * 侧栏 / Agent 栏 / 底部面板的宽度与折叠阈值只在这里定义：
 * 组件不得写死数值，只能读这里导出的常量、`clamp*` 函数或 `resolveLayout()` 结果。
 */

/** 活动栏宽度（图标列，固定）。 */
export const ACTIVITY_BAR_WIDTH = 48
/** 状态栏高度（固定）。 */
export const STATUS_BAR_HEIGHT = 24
/** 分隔条命中区厚度（拖拽手柄）。 */
export const DIVIDER_SIZE = 5
/** 文件树每层缩进。 */
export const TREE_INDENT = 12

/** 侧栏 240–420，默认 280。 */
export const SIDEBAR_MIN_WIDTH = 240
export const SIDEBAR_MAX_WIDTH = 420
export const SIDEBAR_DEFAULT_WIDTH = 280

/** Agent 栏 320–560，默认 380。 */
export const AGENT_MIN_WIDTH = 320
export const AGENT_MAX_WIDTH = 560
export const AGENT_DEFAULT_WIDTH = 380

/** 底部面板 160–400，默认 220。 */
export const BOTTOM_PANEL_MIN_HEIGHT = 160
export const BOTTOM_PANEL_MAX_HEIGHT = 400
export const BOTTOM_PANEL_DEFAULT_HEIGHT = 220

/** 拖拽时的键盘步长（px）。 */
export const RESIZE_STEP = 8
export const RESIZE_STEP_LARGE = 32

/**
 * 调试面板里"调用栈"一列的宽度（px）。
 *
 * 调试面板本体挂在底部面板的 `--bottom-panel-height` 上（尺寸与开合唯一真源仍是
 * `resolveLayout`），这里只给内部分栏一个常量，免得组件里出现字面量像素。
 */
export const DEBUG_STACK_WIDTH = 240

/**
 * 侧栏当前显示哪个视图。
 *
 * 放在 layout.ts 而不是 store 里：它和布局一起被持久化（见 `lib/layout-prefs.ts`），
 * 而且组件只从这里取类型，避免"布局真源"和"页签类型"分成两处分头维护。
 */
export type SidebarView = 'explorer' | 'settings'

/** 底部面板当前页签（同样随布局一起持久化）。 */
export type BottomPanelTab = 'logs' | 'problems' | 'search' | 'debug'

/** 可持久化的布局状态（只存"用户意图"，不存像素级派生值）。 */
export interface LayoutState {
  sidebarWidth: number
  agentWidth: number
  bottomPanelHeight: number
  sidebarCollapsed: boolean
  agentCollapsed: boolean
  bottomPanelCollapsed: boolean
}

export const DEFAULT_LAYOUT: LayoutState = {
  sidebarWidth: SIDEBAR_DEFAULT_WIDTH,
  agentWidth: AGENT_DEFAULT_WIDTH,
  bottomPanelHeight: BOTTOM_PANEL_DEFAULT_HEIGHT,
  sidebarCollapsed: false,
  agentCollapsed: false,
  bottomPanelCollapsed: false,
}

/**
 * 把数值夹到 [min, max]。
 * 非有限数（NaN / Infinity）回落到 min，避免把布局算成 NaN 后整块面板消失。
 */
export function clamp(value: number, min: number, max: number): number {
  if (!Number.isFinite(value)) return min
  const lower = Math.min(min, max)
  const upper = Math.max(min, max)
  return Math.min(Math.max(value, lower), upper)
}

export function clampSidebarWidth(width: number): number {
  return clamp(width, SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
}

export function clampAgentWidth(width: number): number {
  return clamp(width, AGENT_MIN_WIDTH, AGENT_MAX_WIDTH)
}

export function clampBottomPanelHeight(height: number): number {
  return clamp(height, BOTTOM_PANEL_MIN_HEIGHT, BOTTOM_PANEL_MAX_HEIGHT)
}

/** 解析后的布局：折叠时尺寸为 0，其余已 clamp。 */
export interface ResolvedLayout {
  activityBarWidth: number
  statusBarHeight: number
  dividerSize: number
  sidebarWidth: number
  agentWidth: number
  bottomPanelHeight: number
  sidebarVisible: boolean
  agentVisible: boolean
  bottomPanelVisible: boolean
}

/** 唯一的布局解析入口：组件只能消费它的输出。 */
export function resolveLayout(state: LayoutState): ResolvedLayout {
  return {
    activityBarWidth: ACTIVITY_BAR_WIDTH,
    statusBarHeight: STATUS_BAR_HEIGHT,
    dividerSize: DIVIDER_SIZE,
    sidebarWidth: state.sidebarCollapsed ? 0 : clampSidebarWidth(state.sidebarWidth),
    agentWidth: state.agentCollapsed ? 0 : clampAgentWidth(state.agentWidth),
    bottomPanelHeight: state.bottomPanelCollapsed ? 0 : clampBottomPanelHeight(state.bottomPanelHeight),
    sidebarVisible: !state.sidebarCollapsed,
    agentVisible: !state.agentCollapsed,
    bottomPanelVisible: !state.bottomPanelCollapsed,
  }
}

/**
 * 把解析结果转成 CSS 自定义属性，挂在 `.ide` 根节点上。
 * CSS 里用 `var(--sidebar-width)`，保证宽度只有 layout.ts 一个出处。
 */
export function layoutStyleVars(layout: ResolvedLayout): CSSProperties {
  const vars: Record<string, string> = {
    '--activity-bar-width': `${layout.activityBarWidth}px`,
    '--status-bar-height': `${layout.statusBarHeight}px`,
    '--divider-size': `${layout.dividerSize}px`,
    '--sidebar-width': `${layout.sidebarWidth}px`,
    '--agent-width': `${layout.agentWidth}px`,
    '--bottom-panel-height': `${layout.bottomPanelHeight}px`,
  }
  // CSS 自定义属性不在 React.CSSProperties 的类型里，这里是有意为之的唯一转换点。
  return vars as unknown as CSSProperties
}