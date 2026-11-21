/**
 * 清单（hbot.json）的只读视图模型。
 *
 * 为什么在 Web 侧再解析一遍：`hbot.json` 是项目的声明层，核心目前没有一条命令
 * 返回它（能力面只有 `docs/07` 的设计表）。读文件用现成的 `fs.read`，解析放在这里
 * 一个纯函数里，测试可以直接喂字符串。
 *
 * 重要口径：这里显示的是**声明**，不是装配、也不是授权（ADR-0005 的三层求交）。
 * 面板必须把这件事写出来 —— 声明了 `ai: true` 但宿主没配模型时，运行时是
 * `HBOT_AI_UNAVAILABLE`，界面上不能显示成"已启用"。
 */

/** 一条能力在面板上的一行。 */
export interface CapabilityRow {
  /** 稳定的机器可读 id（测试与样式用，不参与展示）。 */
  id: string
  /** 中文名。 */
  label: string
  /** 是否在清单里声明。 */
  declared: boolean
  /** 声明里的关键参数（条数、目标、子能力）；没有就是空串。 */
  detail: string
  /** 从哪个字段读出来的，出错时能指回清单。 */
  source: string
}

/** 清单视图。 */
export interface ManifestView {
  name: string
  entry: string
  platform: string
  /** 授权过的 HTTP 源（`permissions.http`），已经去重并保留原顺序。 */
  httpHosts: string[]
  limits: { handlerMs: number | null; heapMb: number | null; maxPendingJobs: number | null; quotaMb: number | null }
  capabilities: CapabilityRow[]
  /** 声明了几项能力（`declared` 为真的行数）。 */
  declaredCount: number
}

/** 解析失败的人话原因（不再抛出，调用方决定显示在哪）。 */
export type ManifestParse = { ok: true; view: ManifestView } | { ok: false; reason: string }

function record(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null
}

function text(value: unknown): string {
  return typeof value === "string" ? value : ""
}

function num(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null
}

function bool(value: unknown): boolean {
  return value === true
}

function listLength(value: unknown): number {
  return Array.isArray(value) ? value.length : 0
}

/** 平台段是 `{ kind, ws? }`；只认 kind，识别不了就给空串。 */
function platformOf(value: unknown): string {
  const spec = record(value)
  return spec === null ? "" : text(spec.kind)
}

/** `permissions.http` 是一个源列表；非字符串条目直接丢掉（不猜）。 */
function httpHostsOf(value: unknown): string[] {
  if (!Array.isArray(value)) return []
  const seen = new Set<string>()
  const hosts: string[] = []
  for (const item of value) {
    if (typeof item !== "string" || item.length === 0 || seen.has(item)) continue
    seen.add(item)
    hosts.push(item)
  }
  return hosts
}

/** 把能力段摊平成面板要的行。顺序固定：先状态子能力，再动作与入站。 */
function rowsOf(capabilities: Record<string, unknown>): CapabilityRow[] {
  const state = record(capabilities.state) ?? {}
  const files = state.files
  const filesDeclared = files === true || record(files) !== null
  const device = record(capabilities.device)
  const deviceSubs = device === null ? [] : Array.isArray(device.sub) ? device.sub.map((item) => text(item)) : []
  const notify = record(capabilities.notify)
  const notifyDetail =
    notify === null
      ? ""
      : [text(notify.conversation), text(notify.kind)].filter((part) => part.length > 0).join(" · ")
  const timerCount = listLength(capabilities.timer)
  const jobCount = listLength(capabilities.jobs)
  const webhookCount = listLength(capabilities.webhook)
  return [
    { id: "state.kv", label: "状态 KV", declared: bool(state.kv), detail: "", source: "capabilities.state.kv" },
    { id: "state.history", label: "消息历史", declared: bool(state.history), detail: "", source: "capabilities.state.history" },
    { id: "state.sql", label: "SQL 读", declared: bool(state.sql), detail: "", source: "capabilities.state.sql" },
    {
      id: "state.files",
      label: "项目文件读写",
      declared: filesDeclared,
      detail: filesDeclared ? "已声明" : "",
      source: "capabilities.state.files",
    },
    { id: "state.session", label: "多步会话等待", declared: bool(state.session), detail: "", source: "capabilities.state.session" },
    { id: "ai", label: "模型调用", declared: bool(capabilities.ai), detail: "", source: "capabilities.ai" },
    { id: "timer", label: "定时器", declared: timerCount > 0, detail: timerCount > 0 ? timerCount + " 条计划" : "", source: "capabilities.timer" },
    { id: "jobs", label: "后台作业", declared: jobCount > 0, detail: jobCount > 0 ? jobCount + " 种类型" : "", source: "capabilities.jobs" },
    { id: "send", label: "主动发送", declared: bool(capabilities.send), detail: "", source: "capabilities.send" },
    { id: "notify", label: "通知目标", declared: notify !== null, detail: notifyDetail, source: "capabilities.notify" },
    { id: "webhook", label: "入站 Webhook", declared: webhookCount > 0, detail: webhookCount > 0 ? webhookCount + " 条路由" : "", source: "capabilities.webhook" },
    { id: "device", label: "设备能力", declared: device !== null, detail: deviceSubs.length > 0 ? deviceSubs.join(" / ") : "", source: "capabilities.device" },
  ]
}

/**
 * 解析一份清单文本。
 *
 * 失败只返回一句人话，不抛异常：这里读的是用户手写的 JSON，语法错是最常见的输入，
 * 界面要能显示"哪份文件没读成"，而不是让整块面板白屏。
 */
export function parseManifest(raw: string): ManifestParse {
  let value: unknown
  try {
    value = JSON.parse(raw)
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error)
    return { ok: false, reason: "hbot.json 不是合法 JSON：" + detail }
  }
  const root = record(value)
  if (root === null) return { ok: false, reason: "hbot.json 的顶层必须是对象" }
  const name = text(root.name)
  if (name.length === 0) return { ok: false, reason: "hbot.json 缺少 name" }
  const capabilities = record(root.capabilities) ?? {}
  const rows = rowsOf(capabilities)
  const limits = record(root.limits) ?? {}
  const permissions = record(root.permissions) ?? {}
  return {
    ok: true,
    view: {
      name,
      entry: text(root.entry),
      platform: platformOf(root.platform),
      httpHosts: httpHostsOf(permissions.http),
      limits: {
        handlerMs: num(limits.handlerMs),
        heapMb: num(limits.heapMb),
        maxPendingJobs: num(limits.maxPendingJobs),
        quotaMb: num(limits.quotaMb),
      },
      capabilities: rows,
      declaredCount: rows.filter((row) => row.declared).length,
    },
  }
}

/** 把 limits 里非 null 的项拼成一句摘要（面板副标题用）。 */
export function limitsSummary(view: ManifestView): string {
  const parts: string[] = []
  if (view.limits.handlerMs !== null) parts.push("handler " + view.limits.handlerMs + " ms")
  if (view.limits.heapMb !== null) parts.push("堆 " + view.limits.heapMb + " MB")
  if (view.limits.maxPendingJobs !== null) parts.push("待处理作业上限 " + view.limits.maxPendingJobs)
  if (view.limits.quotaMb !== null) parts.push(view.limits.quotaMb === 0 ? "文件配额不限" : "文件配额 " + view.limits.quotaMb + " MB")
  return parts.join(" · ")
}
