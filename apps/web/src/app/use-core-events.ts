/**
 * 核心事件的扇出点：一次订阅，分给五个 store。
 *
 * 为什么在这里而不是 store 内部：AGENTS.md §0.11 禁止 store 互相 import，
 * 所以"谁收哪些事件"由外壳决定 —— run-store 收 `bot.status` / `run.log`，
 * agent-store 收 `agent.*` / `approval.request`，search-store 收 `file.search`，
 * debug-store 收 `debug.paused` / `debug.resumed`，device-store 收 `device.status` /
 * `device.request` / `device.settings`，workspace-store 收 `diagnostics`（问题面板的数据），
 * 各自忽略不认识的事件。
 *
 * 两条事件还顺带抢底部面板的页签：`debug.paused` 切到"调试"（核心在等继续 / 单步，
 * 那是**阻塞**状态），`device.request` 切到"设备"（Agent 在等用户决定要不要把屏幕和键鼠
 * 交出去 —— 错过这条就等于让请求静默超时）。两处都只在该 store 真的收下这条事件之后才切。
 *
 * `debug.log`（日志点输出）不抢页签：它是**高频**事件，边跑边抢页签等于把用户正在看的
 * 面板一直顶走。它只经 `debug-store.ingestEvent` 进环形缓冲，用户自己去"调试"页签看。
 *
 * 挂载时顺带用 `bot.list` 播种一次运行态（`hydrate` 自带去重，重复调用无害）。
 */

import { useEffect } from 'react'
import { onEvent } from '../lib/ipc'
import { useAgentStore } from '../stores/agent-store'
import { useDebugStore } from '../stores/debug-store'
import { useDeviceStore } from '../stores/device-store'
import { useRunStore } from '../stores/run-store'
import { useSearchStore } from '../stores/search-store'
import { useWorkspaceStore } from '../stores/workspace-store'

export function useCoreEvents(): void {
  // 项目切换（含从"没打开"到打开）时重新播种：`bot.list` 只对当前项目有效，
  // 没有项目时核心直接回 `-32002`，那会变成首屏的一条红字。
  const projectRoot = useWorkspaceStore((state) => state.projectRoot)

  useEffect(() => {
    if (projectRoot === null) return
    void useRunStore.getState().hydrate()
  }, [projectRoot])

  useEffect(() => {
    // onEvent 在 mock 模式下订阅内存事件总线、旧桥模式下是空订阅，卸载时统一调它即可。
    return onEvent((event) => {
      useRunStore.getState().ingestEvent(event)
      useAgentStore.getState().ingestEvent(event)
      useSearchStore.getState().ingestEvent(event)
      useDebugStore.getState().ingestEvent(event)
      useDeviceStore.getState().ingestEvent(event)
      useWorkspaceStore.getState().ingestEvent(event)
      if (event.type === 'debug.paused') {
        const debug = useDebugStore.getState()
        // 只在这条暂停真的落到本 store 上时才抢页签（别的 bot 的事件被过滤掉了）。
        if (debug.paused !== null && debug.paused.file === event.file) {
          useWorkspaceStore.getState().setBottomPanelTab('debug')
        }
      }
      if (event.type === 'device.request') {
        const device = useDeviceStore.getState()
        // 同上：请求收下了（`pending` 有值）才切页签，别为了别人的 bot 抢页面。
        if (device.pending !== null) {
          useWorkspaceStore.getState().setBottomPanelTab('device')
        }
      }
    })
  }, [])
}