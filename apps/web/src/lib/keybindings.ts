/**
 * IDE 快捷键的唯一真源（docs/06-ide.md §10）。
 *
 * 命令面板从同一张表生成，组件不得自己绑快捷键。
 */

export type KeybindingId =
  | 'command-palette'
  | 'quick-open'
  | 'global-search'
  | 'save'
  | 'save-all'
  | 'run-bot'
  | 'stop-bot'
  | 'toggle-agent'
  | 'toggle-bottom-panel'
  | 'problems-panel'
  | 'debug-continue'
  | 'debug-step-over'
  | 'debug-step-into'
  | 'debug-step-out'

export type KeybindingCategory = '导航' | '文件' | '运行' | '查看' | '调试'

export interface KeyChord {
  /** 物理按键，比较时忽略大小写；`Enter`、`.`、`F8` 等特殊键原样比较。 */
  key: string
  /** Cmd（macOS）/ Ctrl（Windows、Linux）。 */
  primary?: boolean
  ctrl?: boolean
  /** 显式 Cmd（macOS），与 `primary` 互补。 */
  meta?: boolean
  shift?: boolean
  alt?: boolean
}

export interface Keybinding {
  id: KeybindingId
  title: string
  category: KeybindingCategory
  chord: KeyChord
}

export const KEYBINDINGS: readonly Keybinding[] = [
  { id: 'command-palette', title: '命令面板', category: '导航', chord: { key: 'p', primary: true, shift: true } },
  { id: 'quick-open', title: '快速打开文件', category: '导航', chord: { key: 'p', primary: true } },
  { id: 'global-search', title: '全局搜索', category: '导航', chord: { key: 'f', primary: true, shift: true } },
  { id: 'save', title: '保存', category: '文件', chord: { key: 's', primary: true } },
  { id: 'save-all', title: '全部保存', category: '文件', chord: { key: 's', primary: true, alt: true } },
  { id: 'run-bot', title: '运行 bot', category: '运行', chord: { key: 'Enter', primary: true } },
  { id: 'stop-bot', title: '停止 bot', category: '运行', chord: { key: '.', primary: true } },
  { id: 'toggle-agent', title: '切换 Agent 栏', category: '查看', chord: { key: 'a', primary: true, shift: true } },
  { id: 'toggle-bottom-panel', title: '切换底部面板', category: '查看', chord: { key: 'j', primary: true } },
  { id: 'problems-panel', title: '问题面板', category: '查看', chord: { key: 'm', primary: true, shift: true } },
  // 调试（docs/06-ide.md §6.2）：F5/F10/F11/Shift+F11 与 VS Code 一致，不加 primary 修饰键。
  { id: 'debug-continue', title: '继续', category: '调试', chord: { key: 'F5' } },
  { id: 'debug-step-over', title: '单步跳过', category: '调试', chord: { key: 'F10' } },
  { id: 'debug-step-into', title: '单步进入', category: '调试', chord: { key: 'F11' } },
  { id: 'debug-step-out', title: '单步跳出', category: '调试', chord: { key: 'F11', shift: true } },
]

/** 只取键盘事件里判断快捷键需要的字段，便于单测与复用。 */
export type KeyboardEventLike = Pick<
  KeyboardEvent,
  'key' | 'ctrlKey' | 'metaKey' | 'shiftKey' | 'altKey'
>

/** macOS 判定：快捷键在 mac 上用 Cmd，其他平台用 Ctrl。 */
export function isApplePlatform(userAgent: string = navigator.userAgent): boolean {
  return /Mac|iPhone|iPad|iPod/.test(userAgent)
}

function expectedModifiers(chord: KeyChord, apple: boolean): {
  ctrl: boolean
  meta: boolean
  shift: boolean
  alt: boolean
} {
  return {
    ctrl: chord.primary ? !apple : Boolean(chord.ctrl),
    meta: chord.primary ? apple : Boolean(chord.meta),
    shift: Boolean(chord.shift),
    alt: Boolean(chord.alt),
  }
}

/** 事件是否精确匹配某个按键组合（修饰键必须完全一致）。 */
export function matchesChord(event: KeyboardEventLike, chord: KeyChord, apple: boolean): boolean {
  const mods = expectedModifiers(chord, apple)
  return (
    event.key.toLowerCase() === chord.key.toLowerCase() &&
    event.ctrlKey === mods.ctrl &&
    event.metaKey === mods.meta &&
    event.shiftKey === mods.shift &&
    event.altKey === mods.alt
  )
}

/** 从键盘事件反查快捷键表里的命令；没有匹配返回 undefined。 */
export function keybindingFromEvent(
  event: KeyboardEventLike,
  apple: boolean = isApplePlatform(),
): Keybinding | undefined {
  return KEYBINDINGS.find((binding) => matchesChord(event, binding.chord, apple))
}

function formatKey(key: string): string {
  return key.length === 1 ? key.toUpperCase() : key
}

/** 人类可读的按键串：mac 用符号（⌘⇧P），其他平台用 Ctrl+Shift+P。 */
export function formatChord(chord: KeyChord, apple: boolean): string {
  const parts: string[] = []
  if (apple) {
    if (chord.ctrl) parts.push('⌃')
    if (chord.alt) parts.push('⌥')
    if (chord.shift) parts.push('⇧')
    if (chord.primary) parts.push('⌘')
  } else {
    if (chord.primary || chord.ctrl) parts.push('Ctrl')
    if (chord.alt) parts.push('Alt')
    if (chord.shift) parts.push('Shift')
  }
  parts.push(formatKey(chord.key))
  return apple ? parts.join('') : parts.join('+')
}

export function formatKeybinding(
  binding: Keybinding,
  apple: boolean = isApplePlatform(),
): string {
  return formatChord(binding.chord, apple)
}

export function keybindingById(id: KeybindingId): Keybinding {
  const binding = KEYBINDINGS.find((candidate) => candidate.id === id)
  if (!binding) throw new Error(`未知快捷键：${id}`)
  return binding
}