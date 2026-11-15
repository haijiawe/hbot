/** 设置：M0 只读占位，展示布局与运行时的事实，不提供写入。 */

import { runtimeMode } from '../../lib/ipc'
import {
  ACTIVITY_BAR_WIDTH,
  AGENT_DEFAULT_WIDTH,
  AGENT_MAX_WIDTH,
  AGENT_MIN_WIDTH,
  BOTTOM_PANEL_DEFAULT_HEIGHT,
  BOTTOM_PANEL_MAX_HEIGHT,
  BOTTOM_PANEL_MIN_HEIGHT,
  SIDEBAR_DEFAULT_WIDTH,
  SIDEBAR_MAX_WIDTH,
  SIDEBAR_MIN_WIDTH,
  STATUS_BAR_HEIGHT,
} from '../../lib/layout'

export function SettingsView() {
  const mode = runtimeMode()

  return (
    <section className="settings" aria-label="设置">
      <header className="panel-header">
        <h2 className="panel-header__title">设置</h2>
      </header>
      <div className="panel-scroll">
        <p className="panel-hint">M0 只读：设置持久化与主题切换在 M1 接入核心配置后提供。</p>
        <dl className="settings__list">
          <dt>主题</dt>
          <dd>深色（固定）</dd>
          <dt>运行时后端</dt>
          <dd>{mode === 'tauri' ? 'Tauri（window.__HBOT__ 已注入）' : '浏览器 mock（无后端）'}</dd>
          <dt>侧栏宽度</dt>
          <dd>
            {SIDEBAR_MIN_WIDTH}–{SIDEBAR_MAX_WIDTH}，默认 {SIDEBAR_DEFAULT_WIDTH}px
          </dd>
          <dt>Agent 栏宽度</dt>
          <dd>
            {AGENT_MIN_WIDTH}–{AGENT_MAX_WIDTH}，默认 {AGENT_DEFAULT_WIDTH}px
          </dd>
          <dt>底部面板高度</dt>
          <dd>
            {BOTTOM_PANEL_MIN_HEIGHT}–{BOTTOM_PANEL_MAX_HEIGHT}，默认 {BOTTOM_PANEL_DEFAULT_HEIGHT}px
          </dd>
          <dt>活动栏 / 状态栏</dt>
          <dd>
            {ACTIVITY_BAR_WIDTH}px / {STATUS_BAR_HEIGHT}px
          </dd>
        </dl>
      </div>
    </section>
  )
}