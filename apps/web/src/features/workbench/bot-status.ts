/** bot 状态的展示口径（标签与色调类名）。 */

import type { BotStatus } from '../../lib/ipc'

export const BOT_STATUS_LABEL: Readonly<Record<BotStatus, string>> = {
  stopped: '已停止',
  starting: '启动中',
  running: '运行中',
  stopping: '停止中',
  degraded: '降级',
}

/** 状态点 + 文案的类名（颜色不是唯一信号：降级是空心环，见 app.css）。 */
export function statusDotClass(status: BotStatus): string {
  return 'status-dot status-dot--' + status
}
