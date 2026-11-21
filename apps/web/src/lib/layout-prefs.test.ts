/**
 * 偏好存档的版本迁移。
 *
 * 这一份测试钉的是「升级后第一眼看到什么」：v1 存档里的侧栏值来自只有两个面的旧界面，
 * 照搬会把新的六个 bot 面藏起来；但布局尺寸与底部页签是用户真调过的，必须原样留住。
 */
import { describe, expect, it } from 'vitest'
import {
  DEFAULT_WORKSPACE_PREFS,
  LAYOUT_STORAGE_KEY,
  PREFS_VERSION,
  type PrefsStorage,
  readPrefs,
  sanitizePrefs,
} from './layout-prefs'

function storageWith(value: unknown): PrefsStorage {
  const raw = typeof value === 'string' ? value : JSON.stringify(value)
  return { getItem: () => raw, setItem: () => undefined }
}

describe('prefs version migration', () => {
  it('drops the sidebar face of a v1 save but keeps the layout', () => {
    const prefs = sanitizePrefs({
      layout: { sidebarWidth: 300, agentWidth: 420, bottomPanelHeight: 260, sidebarCollapsed: true },
      sidebarView: 'explorer',
      bottomPanelTab: 'jobs',
    })
    expect(prefs.version).toBe(PREFS_VERSION)
    expect(prefs.sidebarView).toBe('overview')
    expect(prefs.layout.sidebarWidth).toBe(300)
    expect(prefs.layout.agentWidth).toBe(420)
    expect(prefs.layout.bottomPanelHeight).toBe(260)
    expect(prefs.layout.sidebarCollapsed).toBe(true)
    expect(prefs.bottomPanelTab).toBe('jobs')
  })

  it('keeps the sidebar face once the save carries the current version', () => {
    const prefs = sanitizePrefs({ version: PREFS_VERSION, sidebarView: 'channels' })
    expect(prefs.sidebarView).toBe('channels')
  })

  it('still refuses an unknown face from a current save', () => {
    const prefs = sanitizePrefs({ version: PREFS_VERSION, sidebarView: 'kanban' })
    expect(prefs.sidebarView).toBe(DEFAULT_WORKSPACE_PREFS.sidebarView)
  })

  it('reads a v1 save out of storage as the new default face', () => {
    const prefs = readPrefs(
      storageWith({ layout: {}, sidebarView: 'settings', bottomPanelTab: 'device' }),
    )
    expect(prefs.sidebarView).toBe('overview')
    expect(prefs.bottomPanelTab).toBe('device')
  })

  it('falls back to the defaults for a broken save', () => {
    const prefs = readPrefs(storageWith('{not json'))
    expect(prefs).toEqual(DEFAULT_WORKSPACE_PREFS)
    expect(storageWith(null).getItem(LAYOUT_STORAGE_KEY)).toBe('null')
  })
})
