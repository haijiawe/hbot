/**
 * 核心事件的扇出点：一次订阅，分给四个 store。
 *
 * 为什么在这里而不是 store 内部：AGENTS.md §0.11 禁止 store 互相 import，
 * 所以"谁收哪些事件"由外壳决定 —— run-store 收 `bot.status` / `run.log`，
 * agent-store 收 `agent.*` / `approval.request`，search-store 收 `file.search`，
 * debug-store 收 `debug.paused` / `debug.resumed`，各自忽略不认识的事件。
 *
 * `debug.paused` 还顺带把底部面板展开并切到"调试"页签：暂停是**阻塞**状态
 * （核心在等继续 / 单步），不让用户自己去翻页签。
 *
 * 挂载时顺带用 `bot.list` 播种一次运行态（`hydrate` 自带去重，重复调用无害）。
 */

import { useEffect } from 'react'
import { onEvent } from '../lib/ipc'
import { useAgentStore } from '../stores/agent-store'
import { useDebugStore } from '../stores/debug-store'
import { useRunStore } from '../stores/run-store'
import { useSearchStore } from '../stores/search-store'
import { useWorkspaceStore } from '../stores/workspace-store'

export function useCoreEvents(): void {
  useEffect(() => {
    void useRunStore.getState().hydrate()
  }, [])

  useEffect(() => {
    // onEvent 在 mock 模式下订阅内存事件总线、旧桥模式下是空订阅，卸载时统一调它即可。
    return onEvent((event) => {
      useRunStore.getState().ingestEvent(event)
      useAgentStore.getState().ingestEvent(event)
      useSearchStore.getState().ingestEvent(event)
      useDebugStore.getState().ingestEvent(event)
      if (event.type === 'debug.paused') {
        const debug = useDebugStore.getState()
        // 只在这条暂停真的落到本 store 上时才抢页签（别的 bot 的事件被过滤掉了）。
        if (debug.paused !== null && debug.paused.file === event.file) {
          useWorkspaceStore.getState().setBottomPanelTab('debug')
        }
      }
    })
  }, [])
}