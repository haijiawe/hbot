/**
 * 设备能力会话的状态（docs/09-device-capability.md §7 状态机 / §8 契约）。
 *
 * 三条纪律，和 `debug-store.ts` 同一个口径：
 * 1. **不 import 其它 store**（AGENTS.md §0.11）：`botId` 由 `app/App.tsx` 从 runStore 注入，
 *    核心事件由 `app/use-core-events.ts` 扇出到 `ingestEvent`。
 * 2. **事件是唯一真相**（AGENTS.md §0.9 / docs/06-ide.md §1）：`revoke` / `grant` 只发命令，
 *    状态一律等 `device.status` 事件（`grant` 的返回值是"这次授权的结果"，用它覆盖 status 是
 *    契约允许的；`revoke` **没有**返回值，所以撤销后绝不自造一个 `revoked`）。
 * 3. `state` / `sub` / `allowedApps` 都是**开放字符串**：不认识的取值原样保留、不抛错
 *    （后端将来可能加 `audio` 之类，前端不能因为多一个值就白屏）。
 */

import { create } from 'zustand'
import type { DeviceSettings, DeviceStatus } from '../generated'
import { errorMessage } from '../lib/format'
import type { CoreEvent } from '../lib/ipc'
import { dispatch } from '../lib/ipc'

/**
 * Agent 发来的设备授权请求（`device.request` 事件）。
 *
 * `reason` 是 Agent 自己写的**不可信数据**：这里只原样存下来，界面只当纯文本渲染，
 * 绝不当 HTML / 指令（docs/09 §7 的注入提醒）。
 */
export interface DeviceRequestDraft {
  requestId: string
  sub: string[]
  reason: string
}

/** `grant` 的入参子集：`botId` 在 store 里（由外壳注入），界面上不重复要。 */
export interface DeviceGrantInput {
  sub: string[]
  allowedApps: string[]
  /** `null` = 用核心的默认时长。上限 30 分钟（契约 `-32002` 之外由界面只给合法档位）。 */
  minutes: number | null
}

export interface DeviceState {
  /** 当前设备会话属于哪个 bot；空串 = 还没有选中的 bot（界面显示"先启动一个 bot"）。 */
  botId: string
  /** 最近一次读到的会话状态；`null` = 还没查过（不是"没有会话"，那是 `state: 'none'`）。 */
  status: DeviceStatus | null
  /** 待处理的 Agent 请求；与 `status` 无关，来一条就覆盖一条。 */
  pending: DeviceRequestDraft | null
  busy: boolean
  error: string | null
  /**
   * 宿主级设备设置（这台机器的，不属于某个 bot）；`null` = 还没读到。
   *
   * 与 `status` 分开存：`status` 是"这次会话"，设置是"这台机器允许什么"，
   * 换 bot / 撤销会话都不该把它清掉。
   */
  settings: DeviceSettings | null
  settingsBusy: boolean
  settingsError: string | null

  setBotId: (botId: string) => void
  /** 发 `device_status`（wire 同名）。没有 bot 时只是把状态清空，不算错误。 */
  refresh: (botId: string) => Promise<void>
  /** 发 `device_grant`；返回是否成功（对话框据此决定关不关）。 */
  grant: (input: DeviceGrantInput) => Promise<boolean>
  /** 发 `device_revoke`（幂等）；**不改 `status`**，等事件。 */
  revoke: () => Promise<void>
  /** 用户点了「拒绝」：只清本地待处理项，不发任何命令（docs/09 §7 的两个入口之一）。 */
  clearPending: () => void
  /** 读宿主设置（wire `device_settings_get`）。失败只写 `settingsError`，不冲掉已有的那份。 */
  loadSettings: () => Promise<void>
  /** 写宿主设置；返回是否成功（设置卡片据此决定要不要清掉本地错误）。 */
  saveSettings: (next: DeviceSettings) => Promise<boolean>
  ingestEvent: (event: CoreEvent) => void
  dismissError: () => void
}

/** 重置用的初始值（测试与 `setBotId` 共用，避免两处各写一份）。 */
export const INITIAL_DEVICE_STATE = {
  botId: '',
  status: null,
  pending: null,
  busy: false,
  error: null,
  settings: null,
  settingsBusy: false,
  settingsError: null,
} satisfies Pick<
  DeviceState,
  'botId' | 'status' | 'pending' | 'busy' | 'error' | 'settings' | 'settingsBusy' | 'settingsError'
>

export const useDeviceStore = create<DeviceState>()((set, get) => ({
  ...INITIAL_DEVICE_STATE,

  setBotId(botId) {
    // 换 bot 就是换会话：上一个 bot 的会话状态与待处理请求一律丢弃，别让面板显示
    // "别人的会话"（设备会话在核心是按 bot 记的，见 docs/09 §3 的 device_sessions）。
    if (get().botId === botId) return
    // 只丢**会话**相关状态：宿主设置属于这台机器（不是某个 bot），换 bot 不该让它退回"还没读到"，
    // 否则设置卡片会永远停在加载态（它的读取只在挂载时做一次）。
    set({ ...INITIAL_DEVICE_STATE, botId, settings: get().settings })
  },

  async refresh(botId) {
    if (botId.length === 0) {
      // 没有 bot 就没有可查的会话：这不是错误，界面显示空状态即可。
      // 宿主设置同样保留（原因同 `setBotId`）。
      set({ ...INITIAL_DEVICE_STATE, settings: get().settings })
      return
    }
    set({ botId, busy: true, error: null })
    try {
      const status = await dispatch({ type: 'deviceStatus', botId })
      set({ status, busy: false })
    } catch (error) {
      set({ busy: false, error: `读取设备状态失败：${errorMessage(error)}` })
    }
  },

  async grant(input) {
    const botId = get().botId
    if (botId.length === 0) {
      set({ error: '授权失败：还没有选中的 bot（先启动一个 bot）' })
      return false
    }
    set({ busy: true, error: null })
    try {
      const status = await dispatch({
        type: 'deviceGrant',
        botId,
        sub: input.sub,
        allowedApps: input.allowedApps,
        minutes: input.minutes,
      })
      // `device_grant` 的返回值就是这次授权后的会话（docs/09 §8.1）：用它覆盖 status。
      // 待处理请求同时清掉 —— 同一个请求授过权之后不该再弹第二次。
      set({ status, pending: null, busy: false })
      return true
    } catch (error) {
      set({ busy: false, error: `授权失败：${errorMessage(error)}` })
      return false
    }
  },

  async revoke() {
    const botId = get().botId
    if (botId.length === 0) {
      set({ error: '撤销失败：还没有选中的 bot（先启动一个 bot）' })
      return
    }
    set({ busy: true, error: null })
    try {
      await dispatch({ type: 'deviceRevoke', botId })
      // **不伪造 `revoked`**：`device_revoke` 只回 `data: null`，唯一真相是随后的
      // `device.status` 事件（AGENTS.md：IDE 不拥有运行时真相）。
      set({ busy: false })
    } catch (error) {
      set({ busy: false, error: `撤销失败：${errorMessage(error)}` })
    }
  },

  clearPending() {
    set({ pending: null })
  },

  async loadSettings() {
    set({ settingsBusy: true, settingsError: null })
    try {
      const settings = await dispatch({ type: 'deviceSettingsGet' })
      set({ settings, settingsBusy: false })
    } catch (error) {
      // 读失败不动 `settings`：上一次读到的那份仍然是用户最可能想看的，
      // 清掉只会让界面在"设置存在但这次没读到"和"这台机器没有设置"之间失去区分。
      set({ settingsBusy: false, settingsError: `读取设备设置失败：${errorMessage(error)}` })
    }
  },

  async saveSettings(next) {
    set({ settingsBusy: true, settingsError: null })
    try {
      // 契约里 set 的 `data` 是**生效后**的设置：用它覆盖本地那份，不自己猜核心做了什么规范化
      // （未知能力被丢弃、大小写被统一都由核心决定）。
      const settings = await dispatch({ type: 'deviceSettingsSet', settings: next })
      set({ settings, settingsBusy: false })
      return true
    } catch (error) {
      // 后端 message 原样带进 `settingsError`（`errorMessage` 只取 message，不吞原文）。
      set({ settingsBusy: false, settingsError: `保存设备设置失败：${errorMessage(error)}` })
      return false
    }
  },

  ingestEvent(event) {
    switch (event.type) {
      case 'device.status': {
        const state = get()
        if (state.botId.length > 0 && state.botId !== event.status.botId) return
        set({ botId: event.status.botId, status: event.status })
        return
      }
      case 'device.request': {
        const state = get()
        // 请求是安全相关的：即使还没选 bot 也先收下（`botId` 从事件里补上），
        // 否则"用户在别的 bot 上时来的请求"会被静默丢掉。
        set({
          botId: state.botId.length > 0 ? state.botId : event.botId,
          pending: { requestId: event.requestId, sub: [...event.sub], reason: event.reason },
        })
        return
      }
      case 'device.settings': {
        // 别的窗口（或命令自身的广播）改了宿主设置：直接覆盖本地那份。设置与 bot 无关，
        // 所以这里不用像 `device.status` 那样过滤 botId。
        set({ settings: event.settings, settingsError: null })
        return
      }
      default:
        return
    }
  },

  dismissError() {
    set({ error: null })
  },
}))