/**
 * 布局 / 侧栏 / 底部页签的持久化（localStorage）。
 *
 * 为什么这里用 localStorage 而缓冲区用 IndexedDB：这些是几十字节的小数据，同步读出来
 * 首帧就能用（不会像 IndexedDB 那样先闪一下默认布局再跳），配额也完全够。1 MB 的源文件
 * 才必须进 IndexedDB（见 `lib/session-journal.ts`）。
 */

import {
  DEFAULT_LAYOUT,
  clampAgentWidth,
  clampBottomPanelHeight,
  clampSidebarWidth,
  type BottomPanelTab,
  type LayoutState,
  type SidebarView,
} from './layout'
import { DEFAULT_THEME, isTheme, type Theme } from './theme'

/** 存储键：布局 + 侧栏视图 + 底部页签。 */
export const LAYOUT_STORAGE_KEY = 'hbot.ide.workspace-prefs'

/**
 * 偏好存档的版本。
 *
 * 侧栏在 v2 从「两层（资源管理器 / 设置）」变成「八个面（六个 bot 视角 + 两个旧视图）」，
 * 默认面也从资源管理器改成概览。老存档里的 `explorer` 是**旧界面下唯一的选择**，不是用户挑过的面，
 * 照搬会让升级后第一眼还是旧界面；因此 v1 存档只保留布局尺寸与底部页签，侧栏回落新默认值。
 * 之后的存档带 `version: 2`，用户再选什么就一直是那个。
 */
export const PREFS_VERSION = 2

/** 可持久化的工作区偏好（标签页与未保存缓冲区不在这里，见 journal）。 */
export interface WorkspacePrefs {
  version: number
  /** 浅色是默认（Mino 的形态）；深色是同一套变量的另一组取值。 */
  theme: Theme
  layout: LayoutState
  sidebarView: SidebarView
  bottomPanelTab: BottomPanelTab
}

export const DEFAULT_WORKSPACE_PREFS: WorkspacePrefs = {
  version: PREFS_VERSION,
  theme: DEFAULT_THEME,
  layout: DEFAULT_LAYOUT,
  sidebarView: 'overview',
  bottomPanelTab: 'logs',
}

const SIDEBAR_VIEWS: readonly SidebarView[] = [
  'overview',
  'bots',
  'capabilities',
  'conversations',
  'runtime',
  'channels',
  'board',
  'docs',
  'logs',
  'explorer',
  'settings',
]
const BOTTOM_PANEL_TABS: readonly BottomPanelTab[] = [
  'logs',
  'problems',
  'search',
  'debug',
  'tryrun',
  'device',
  'replay',
  'jobs',
]

/** 只要求 localStorage 的两个方法，测试可以注入假的（node 环境没有 localStorage）。 */
export interface PrefsStorage {
  getItem: (key: string) => string | null
  setItem: (key: string, value: string) => void
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null
}

function readWidth(value: unknown, fallback: number, clampFn: (width: number) => number): number {
  return typeof value === 'number' && Number.isFinite(value) ? clampFn(value) : fallback
}

/**
 * 把存储里读到的东西校验成偏好。
 *
 * 逐字段兜底：只有一条坏字段时其余偏好仍然生效，整份坏掉时才回落默认值 ——
 * 布局是纯表现层，绝不因为存档坏了让 IDE 起不来。
 */
export function sanitizePrefs(raw: unknown): WorkspacePrefs {
  if (!isRecord(raw)) return DEFAULT_WORKSPACE_PREFS
  const layout = isRecord(raw.layout) ? raw.layout : {}
  // v1 存档的侧栏值不采信（那时只有资源管理器 / 设置两项，见 PREFS_VERSION 的注释）。
  const current = raw.version === PREFS_VERSION
  const sidebarView = current ? SIDEBAR_VIEWS.find((view) => view === raw.sidebarView) : undefined
  const bottomPanelTab = BOTTOM_PANEL_TABS.find((tab) => tab === raw.bottomPanelTab)
  return {
    version: PREFS_VERSION,
    // 主题不跟版本门：老存档没有这个字段就是浅色，有就照办（深色是新选项，不存在"旧界面的默认值"）。
    theme: isTheme(raw.theme) ? raw.theme : DEFAULT_THEME,
    layout: {
      sidebarWidth: readWidth(layout.sidebarWidth, DEFAULT_LAYOUT.sidebarWidth, clampSidebarWidth),
      agentWidth: readWidth(layout.agentWidth, DEFAULT_LAYOUT.agentWidth, clampAgentWidth),
      bottomPanelHeight: readWidth(
        layout.bottomPanelHeight,
        DEFAULT_LAYOUT.bottomPanelHeight,
        clampBottomPanelHeight,
      ),
      sidebarCollapsed: layout.sidebarCollapsed === true,
      agentCollapsed: layout.agentCollapsed === true,
      bottomPanelCollapsed: layout.bottomPanelCollapsed === true,
    },
    sidebarView: sidebarView ?? DEFAULT_WORKSPACE_PREFS.sidebarView,
    bottomPanelTab: bottomPanelTab ?? DEFAULT_WORKSPACE_PREFS.bottomPanelTab,
  }
}

/** 浏览器里的 localStorage；没有（node / 受限 iframe）时返回 null。 */
function defaultStorage(): PrefsStorage | null {
  try {
    return typeof localStorage === 'undefined' ? null : localStorage
  } catch {
    // 某些沙箱 iframe 里访问 localStorage 本身就抛 SecurityError：
    // 这里吞掉是"没有存储可用"，不是"写失败被忽略"——写失败会照常抛给调用方。
    return null
  }
}

/** 读偏好。读失败（JSON 坏了 / 没有存储）回落默认值。 */
export function readPrefs(storage: PrefsStorage | null = defaultStorage()): WorkspacePrefs {
  if (storage === null) return DEFAULT_WORKSPACE_PREFS
  try {
    const raw = storage.getItem(LAYOUT_STORAGE_KEY)
    if (raw === null) return DEFAULT_WORKSPACE_PREFS
    return sanitizePrefs(JSON.parse(raw))
  } catch {
    // 存档坏了就回默认布局：不阻塞启动，也不静默改用户数据（只读，不重写）。
    return DEFAULT_WORKSPACE_PREFS
  }
}

/**
 * 写偏好。**失败直接抛**（配额满 / 隐私模式）：调用方在 workspace-store 里把它转成
 * `error` 显示出来，不做"写不进去也当成功"的静默降级。
 */
export function writePrefs(prefs: WorkspacePrefs, storage: PrefsStorage | null = defaultStorage()): void {
  if (storage === null) return
  storage.setItem(LAYOUT_STORAGE_KEY, JSON.stringify(prefs))
}