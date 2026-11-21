/**
 * 主题：浅色（默认，Mino 的暖色纸面）/ 深色。
 *
 * 只做一件事：把 `data-theme` 写到 `<html>` 上。两套调色板都在 `styles/theme.css` 里，
 * 组件一律用变量，所以切换主题不需要任何组件重渲染之外的动作。
 *
 * 为什么放 localStorage 的偏好里而不是单独的键：主题和布局一样是「这台机器上这个人怎么看」
 * 的偏好，一起读写就不会出现「布局换了、主题没跟着」的半套状态。
 */

export type Theme = 'light' | 'dark'

export const DEFAULT_THEME: Theme = 'light'

/** 把主题写进文档根。没有 document（node 测试环境）时什么也不做。 */
export function applyTheme(theme: Theme): void {
  if (typeof document === 'undefined') return
  document.documentElement.dataset.theme = theme
}

export function isTheme(value: unknown): value is Theme {
  return value === 'light' || value === 'dark'
}
