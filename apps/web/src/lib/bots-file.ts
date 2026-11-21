/**
 * bots.json 的只读视图：项目声明了哪几个平台入口。
 *
 * 为什么在 Web 侧解析：`bots.json` 是项目文件，`fs.read` 能读到；核心没有一条
 * 命令返回它（bot.list 只给已登记与已声明的 id / name / platform）。
 *
 * 安全口径：这里**只**取 id / name / platform / ws。token、clientSecret 这些凭据
 * 一律不读出来、不带到界面上（它们只在核心与 keyring 之间流动）。
 */

export interface ChannelRow {
  id: string
  name: string
  platform: string
  ws: string
}

function record(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null
}

function text(value: unknown): string {
  return typeof value === "string" ? value : ""
}

/** `platform` 既可能是字符串，也可能是 `{ kind }` 对象；都不是就给空串。 */
function platformOf(value: unknown): string {
  if (typeof value === "string") return value
  const spec = record(value)
  return spec === null ? "" : text(spec.kind)
}

/**
 * 把 bots.json 解析成渠道行。
 *
 * 解析失败、结构不认识、没有 bots 数组都返回空数组 —— 面板据此显示「读不到」，
 * 不抛异常（这份文件是用户手写的，写坏是最常见的输入）。
 */
export function parseChannels(raw: string): ChannelRow[] {
  let value: unknown
  try {
    value = JSON.parse(raw)
  } catch {
    return []
  }
  const root = record(value)
  if (root === null || !Array.isArray(root.bots)) return []
  const rows: ChannelRow[] = []
  for (const item of root.bots) {
    const entry = record(item)
    if (entry === null) continue
    rows.push({
      id: text(entry.id),
      name: text(entry.name),
      platform: platformOf(entry.platform),
      ws: text(entry.ws),
    })
  }
  return rows
}
