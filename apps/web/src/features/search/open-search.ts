import { useSearchStore } from '../../stores/search-store'
import { useWorkspaceStore } from '../../stores/workspace-store'

/**
 * 打开全局搜索：切到底部面板的"搜索"页签，并让输入框拿焦点。
 *
 * 三处入口共用这一份（快捷键 `Ctrl+Shift+F`、活动栏、命令面板），
 * 免得每个入口各写一遍"切页签 + 抢焦点"。布局数值仍只来自 `lib/layout.ts`。
 */
export function openSearch(): void {
  useWorkspaceStore.getState().setBottomPanelTab('search')
  useSearchStore.getState().focusInput()
}