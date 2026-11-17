/**
 * 浏览器 dev / 无 Tauri 时的内存后端（M0 占位）。
 *
 * 行为与真实核心对齐的部分：路径守卫（项目内、不写 `hbot.d.ts`、不写 `.hbot/`）、
 * 版本号单调递增、搜索（`search_start` 回 `data: null`、命中走 `file.search` 事件、
 * `column` 恒 1、`text` 是整行）、`bot.list` / `agent.send` 的返回形状（含会话 ID）。
 * 真实实现见 `crates/hbot-workspace` 与 `crates/hbot-rpc/src/app/commands/`。
 *
 * 这里没有 `hbot://event` 通道，所以自带一条**内存事件总线**（`subscribeMockEvents` /
 * `emitMockEvent`）：`ipc.ts` 的 `onEvent` 在非 Tauri 分支订阅它，否则搜索面板在唯一
 * 能跑的模式下永远是空的。目前走这条总线的事件有两条：`file.search`（搜索）与
 * `run.log`（本地平台试跑的回复，`bot_inject` 之后按 `[reply] ` 前缀逐行推）。
 * `bot.status` 仍靠 `bot.list` 的返回值播种（与真机路径一致：UI 不自己编运行态）。
 *
 * 取消语义也照核心来：`search_cancel` 之后那条还没送出的事件**不再发**，也不补一条
 * "假结果"（`crates/hbot-rpc/src/app/commands/files.rs` 的 `search` 在 cancelled 时直接返回）。
 */

import type {
  DebugBreakpoint,
  DeviceSettings,
  DeviceStatus,
  Event as CoreEvent,
  FixtureRunReport,
  ReplayEntry,
  ReplayReport,
  RunLog,
  SavedFixture,
} from '../generated'
import type {
  BotStatus,
  Command,
  CommandOutputs,
  CommandType,
  ConversationSummary,
  FileNode,
} from './ipc'
import { searchMatches } from './mock-search'

const BOT_ID = 'demo-bot'
const BOT_NAME = '演示 bot'
/**
 * mock 的 bot 平台是 `local`：试跑链路（`bot_inject` → `[reply] ` 日志）只有本地平台
 * 才有，换成真实平台名会让"注入之后不该联网"这条语义在浏览器里显得没有依据。
 */
const BOT_PLATFORM = 'local'
/** mock 的会话 ID：真机上由核心的 `state.ids()` 分配。 */
const MOCK_SESSION_ID = 'mock-session'

/**
 * mock 里记下的断点（`file -> { line, condition }`，与核心的 `Breakpoints` 同语义：
 * 全量覆盖、空集移除、`condition: null` 是无条件）。
 *
 * 只用来让"点了 gutter 之后命令确实发出去了"这件事在浏览器里可复现；mock **不伪造
 * `debug.paused`**（docs/06-ide.md §6.2 的暂停语义要真跑起来才有），所以调试面板在 mock
 * 下始终停在"未在调试"。想看暂停面板长什么样，用下面的 `simulateMockPause` 手动造一条
 * 事件（只给测试与手动调试，不该有产品代码调用它）。
 */
const mockBreakpoints = new Map<string, DebugBreakpoint[]>()

/**
 * mock 的设备会话（与核心的 `device_sessions` 同字段语义，见
 * `docs/09-device-capability.md` §3）。`null` = 没有会话。
 *
 * 两条 mock 纪律（与 `debug.paused` 同口径）：
 * 1. **空 `allowedApps` 必须真的拒绝**：空数组在核心那边是"全拒"，mock 收下它会让前端
 *    以为"无限制"这条路走得通，而这正是界面必须显式写清的语义。
 * 2. **不伪造 `device.status` 事件**：真实核心在授权/撤销/到期后推事件，mock 只在命令
 *    返回值里回状态。少造一条假事件，前端就不会靠"mock 恰好推了"来通过测试 ——
 *    浏览器里撤销之后面板停在旧状态是**诚实的**，别补一条假的把它圆过去。
 */
interface MockDeviceSession {
  botId: string
  sessionId: string
  state: string
  sub: string[]
  allowedApps: string[]
  grantedAt: number
  deadline: number
}

let mockDevice: MockDeviceSession | null = null

/** 真机上会话 ID 由核心的 `state.ids()` 分配；mock 用固定串（一台机只有一个设备）。 */
const MOCK_DEVICE_SESSION_ID = 'mock-device-session'
const MOCK_DEVICE_DEFAULT_MINUTES = 5
/** 契约上限：`device_grant` 的 `minutes` 超过 30 → `-32002`（docs/09 §8.1）。 */
const MOCK_DEVICE_MAX_MINUTES = 30

/** 当前 bot 的设备状态快照；没有会话时 `state: 'none'`（与核心一致，不是 `null`）。 */
function mockDeviceStatus(botId: string): DeviceStatus {
  const session = mockDevice
  if (session === null || session.botId !== botId) {
    return {
      botId,
      sessionId: null,
      state: 'none',
      sub: [],
      allowedApps: [],
      grantedAt: null,
      deadline: null,
      remainingMs: null,
    }
  }
  return {
    botId,
    sessionId: session.sessionId,
    state: session.state,
    sub: [...session.sub],
    allowedApps: [...session.allowedApps],
    grantedAt: session.grantedAt,
    deadline: session.deadline,
    // 与核心一样按"现在到 deadline"算；UI 拿它当倒计时起点。
    remainingMs: Math.max(session.deadline - Date.now(), 0),
  }
}

/**
 * mock 里那份**内存**宿主设置（真机在 `<data_dir>/device.json`）。
 *
 * 为什么要在 mock 里真的存一份而不是每次回同一个常量：`device_settings_set` 的语义是
 * "写进去，之后所有窗口读到的是新的那份"。回常量会让浏览器里的保存按钮看起来点了没反应，
 * 前端测试也就永远测不出"保存之后 get 能读回新值"。
 *
 * 只在这一处判断"什么算合法"，判据照抄 docs/09-device-capability.md §4.2：不认识的能力名、
 * `maxMinutes` 超 30、`defaultMinutes > maxMinutes`、动作上限 < 1 —— 都当场拒绝，
 * 与核心一样不"就近取一个合法值"。
 */
const MOCK_DEVICE_CAPABILITIES = ['screen', 'ocr', 'input', 'window', 'clipboard'] as const

/** 复制一份设置：调用方拿到的数组不该与 mock 的"磁盘"共享引用。 */
function cloneMockDeviceSettings(settings: DeviceSettings): DeviceSettings {
  return {
    ...settings,
    capabilities: [...settings.capabilities],
    allowedApps: [...settings.allowedApps],
  }
}

/** 校验用的硬上限：与 `MOCK_DEVICE_MAX_MINUTES` 同一个数（契约 30 分钟）。 */
function checkedMockDeviceSettings(next: DeviceSettings): DeviceSettings {
  const unknown = next.capabilities.filter(
    (name) => !MOCK_DEVICE_CAPABILITIES.some((known) => known === name),
  )
  if (unknown.length > 0) {
    throw new Error(`device_settings_set: 不认识的能力（${unknown.join('、')}，-32002）`)
  }
  if (
    !Number.isInteger(next.maxMinutes) ||
    next.maxMinutes < 1 ||
    next.maxMinutes > MOCK_DEVICE_MAX_MINUTES
  ) {
    throw new Error(
      `device_settings_set: maxMinutes 必须在 1..30 之间（收到 ${next.maxMinutes}，-32002）`,
    )
  }
  if (!Number.isInteger(next.defaultMinutes) || next.defaultMinutes < 1) {
    throw new Error(
      `device_settings_set: defaultMinutes 必须是正整数（收到 ${next.defaultMinutes}，-32002）`,
    )
  }
  if (next.defaultMinutes > next.maxMinutes) {
    throw new Error(
      `device_settings_set: defaultMinutes 不能大于 maxMinutes（${next.defaultMinutes} > ${next.maxMinutes}，-32002）`,
    )
  }
  if (!Number.isInteger(next.maxActionsPerSession) || next.maxActionsPerSession < 1) {
    throw new Error(
      `device_settings_set: maxActionsPerSession 至少为 1（收到 ${next.maxActionsPerSession}，-32002）`,
    )
  }
  if (next.allowedApps.some((app) => app.trim().length === 0)) {
    throw new Error('device_settings_set: allowedApps 里不能有空字符串（-32002）')
  }
  // 存副本：调用方之后改自己那份数组不该影响 mock 的"磁盘"。
  return cloneMockDeviceSettings(next)
}

/** mock 的初始设置：与核心默认值一致（总开关关、能力全不勾、应用不设上限）。 */
let mockDeviceSettings: DeviceSettings = {
  enabled: false,
  capabilities: [],
  allowedApps: [],
  defaultMinutes: MOCK_DEVICE_DEFAULT_MINUTES,
  maxMinutes: MOCK_DEVICE_MAX_MINUTES,
  maxActionsPerSession: 10_000,
}

/**
 * mock 里登记过的项目（内存表，重启即清空）。
 *
 * 内存后端没有真实文件系统：**打开任何路径都落到同一条演示项目上**（`TREE` / `FILES`
 * 就是它的内容）。所以这张表只用来守住两件事 ——
 * ① `project_open` 之后文件树立刻有东西可点（否则"打开成功但树是空的"看起来像没接通）；
 * ② `project_create` 对"已登记同路径"回 `-32003`，与核心一致。
 */
interface MockProject {
  path: string
  name: string
  entry: string
}

const mockProjects = new Map<string, MockProject>()

/** mock 的默认演示项目路径（任务要求给一条假路径，便于在浏览器里走完整流程）。 */
export const MOCK_PROJECT_PATH = 'C:/work/demo'

/** 归一化 mock 的项目路径：正斜杠、去尾部斜杠、去首尾空白。 */
function normalizeMockProjectPath(path: string): string {
  const trimmed = path.trim().replace(/\\/g, '/')
  if (trimmed.length === 0) throw new Error('project_open: path 不能为空')
  return trimmed.replace(/\/+$/, '')
}

/** 从项目路径取末段当项目名（核心用 `hbot.json` 的 name，mock 没有清单文件）。 */
function projectNameOf(path: string): string {
  const segments = path.split('/').filter((segment) => segment.length > 0)
  return segments[segments.length - 1] ?? 'project'
}

const HBOT_JSON = `{
  "name": "hbot-demo",
  "entry": "src/main.ts",
  "platform": "onebot11",
  "permissions": {
    "http": ["api.example.com"],
    "kv": true
  },
  "limits": {
    "steps": 12,
    "memoryMb": 32
  }
}
`

const TSCONFIG_JSON = `{
  "compilerOptions": {
    "target": "ES2022",
    "module": "ESNext",
    "moduleResolution": "bundler",
    "strict": true,
    "noEmit": true,
    "types": ["hbot"]
  },
  "include": ["src", "tests", "hbot.d.ts"]
}
`

const HBOT_DTS = `// @generated by \`cargo xtask gen\`; 不要手改。
export interface MessageContext {
  readonly messageId: string
  readonly text: string
  reply(text: string): Promise<void>
  log(message: string): void
}

export interface BotDefinition {
  onMessage?(ctx: MessageContext): Promise<void>
  onStart?(): Promise<void>
}

export function defineBot(definition: BotDefinition): BotDefinition
`

const README_MD = `# hbot-demo

一个用 HBot 写的示例 bot：收到 \`hello\` 回复 \`world\`。

- 入口：\`src/main.ts\`
- 清单：\`hbot.json\`
- 只允许相对路径 import，运行时不装 npm 依赖。
`

const MAIN_TS = [
  "import { defineBot } from 'hbot'",
  "import { onGroupMessage } from './handlers/message'",
  "import { buildReply } from './lib/reply'",
  '',
  'export default defineBot({',
  '  async onStart() {',
  "    console.log('bot started')",
  '  },',
  '',
  '  async onMessage(ctx) {',
  '    const reply = buildReply(ctx.text)',
  '    await onGroupMessage(ctx, reply)',
  '  },',
  '})',
  '',
].join('\n')

const REPLY_TS = [
  '/** 纯函数：把入站文本变成一句回复；不依赖宿主 API。 */',
  'export function buildReply(text: string): string {',
  '  const trimmed = text.trim()',
  "  if (trimmed.length === 0) return '你什么都没说。'",
  "  if (trimmed === 'hello') return 'world'",
  '  return `收到：${trimmed}`',
  '}',
  '',
].join('\n')

const HANDLER_TS = [
  "import type { MessageContext } from 'hbot'",
  '',
  '/** 群消息处理：这里只做回复，真正的平台差异由适配器负责。 */',
  'export async function onGroupMessage(ctx: MessageContext, reply: string): Promise<void> {',
  '  ctx.log(`inbound: ${ctx.text}`)',
  '  await ctx.reply(reply)',
  '}',
  '',
].join('\n')

const TEST_TS = [
  "import { describe, expect, it } from 'hbot/test'",
  "import { buildReply } from '../src/lib/reply'",
  '',
  "describe('buildReply', () => {",
  "  it('replies world to hello', () => {",
  "    expect(buildReply('hello')).toBe('world')",
  '  })',
  '})',
  '',
].join('\n')

const FILES = new Map<string, string>([
  ['hbot.json', HBOT_JSON],
  ['tsconfig.json', TSCONFIG_JSON],
  ['hbot.d.ts', HBOT_DTS],
  ['README.md', README_MD],
  ['src/main.ts', MAIN_TS],
  ['src/lib/reply.ts', REPLY_TS],
  ['src/handlers/message.ts', HANDLER_TS],
  ['tests/message.test.ts', TEST_TS],
])

const VERSIONS = new Map<string, number>(
  [...FILES.keys()].map((path, index) => [path, index + 1]),
)

const TREE: FileNode[] = [
  { name: 'hbot.json', path: 'hbot.json', kind: 'file' },
  { name: 'hbot.d.ts', path: 'hbot.d.ts', kind: 'file' },
  { name: 'tsconfig.json', path: 'tsconfig.json', kind: 'file' },
  { name: 'README.md', path: 'README.md', kind: 'file' },
  {
    name: 'src',
    path: 'src',
    kind: 'dir',
    children: [
      { name: 'main.ts', path: 'src/main.ts', kind: 'file' },
      {
        name: 'handlers',
        path: 'src/handlers',
        kind: 'dir',
        children: [{ name: 'message.ts', path: 'src/handlers/message.ts', kind: 'file' }],
      },
      {
        name: 'lib',
        path: 'src/lib',
        kind: 'dir',
        children: [{ name: 'reply.ts', path: 'src/lib/reply.ts', kind: 'file' }],
      },
    ],
  },
  {
    name: 'tests',
    path: 'tests',
    kind: 'dir',
    children: [{ name: 'message.test.ts', path: 'tests/message.test.ts', kind: 'file' }],
  },
]

function versionOf(path: string): number {
  const version = VERSIONS.get(path)
  if (version === undefined) throw new Error(`文件不存在：${path}`)
  return version
}

/**
 * 写盘守卫：与 `hbot-workspace` 的规则对齐（docs/06-ide.md §4.2）。
 * 契约是"拒绝"而不是"静默失败"。
 */
/** 归一化项目内路径；越界（绝对路径、盘符、`..`）直接拒绝，读与写共用。 */
function assertInsideProject(path: string): string {
  const normalized = path.replace(/\\/g, '/')
  const segments = normalized.split('/')
  const absolute = normalized.startsWith('/') || /^[A-Za-z]:/.test(normalized)
  if (normalized.length === 0 || absolute || segments.includes('..')) {
    throw new Error(`拒绝：路径必须位于项目内（${path}）`)
  }
  return normalized
}

function assertWritable(path: string): void {
  const normalized = assertInsideProject(path)
  const segments = normalized.split('/')
  if (segments[0] === '.hbot') {
    throw new Error(`拒绝写入：${path} 属于运行数据（.hbot/）`)
  }
  if (normalized === 'hbot.d.ts') {
    throw new Error('拒绝写入：hbot.d.ts 由核心生成，请改 Rust 侧 dts.rs')
  }
}

/** mock 的 bot 运行态：只有 `bot.start` / `bot.stop` 能改它，UI 不参与。 */
let mockStatus: BotStatus = 'stopped'
let mockUpdatedAt = Date.now()

function mockBotSummary() {
  return {
    botId: BOT_ID,
    name: BOT_NAME,
    platform: BOT_PLATFORM,
    status: mockStatus,
    updatedAt: mockUpdatedAt,
  }
}

function setMockStatus(status: BotStatus): void {
  mockStatus = status
  mockUpdatedAt = Date.now()
}

// ---------------------------------------------------------------------------
// 本地平台试跑：会话 + `[reply] ` 日志（`bot_inject` 的唯一可观察产出）
// ---------------------------------------------------------------------------

/**
 * 本地平台的固定会话（与后端契约逐字段对齐，别按"顺手"改）。
 *
 * 两个 id 不是一回事：
 * - `conversationId` 是**内部 UUID**（核心的 conversations 表主键，`bot_inject` 要的是它）；
 * - `externalId` 才是平台侧的 `local:default` 字面量（`local::DEFAULT_CONVERSATION_EXTERNAL_ID`）。
 *
 * 真机上 `local` 平台的 `bot_start` 内部会幂等地把这行建好（重复 start 不会堆孤儿，
 * `conversationId` 也不变），所以这里固定一个 UUID 形状的值，UI 拿它去注入才和真机同路。
 * `kind` 是开放字符串（`private` / `group` / `channel`），不要当枚举用。
 */
const MOCK_CONVERSATION: ConversationSummary = {
  conversationId: '11111111-1111-4111-8111-111111111111',
  externalId: 'local:default',
  kind: 'private',
}

/**
 * 回复前缀的真源是 `crates/hbot-adapters/src/platforms/local/mod.rs` 的 `REPLY_PREFIX`。
 * UI 侧只按它过滤（`features/tryrun/replies.ts`），mock 也用它拼假回复 —— 两处字符串
 * 必须一致，否则浏览器里能跑通的链路到真机就一条都读不出来。
 */
const REPLY_PREFIX = '[reply] '

/** mock 的运行日志（`logs.tail` 的数据源）；真机是 SQLite 的 run_log 表。 */
const mockRunLogs: Array<RunLog & { botId: string }> = []

/** mock 的运行日志缓冲上界：与真机的 `logs_tail` 语义对齐（只留最近的一段）。 */
const MOCK_RUN_LOG_LIMIT = 5_000

/**
 * 把注入的文本变成 bot 的回复行：**每行一条**，前缀 `[reply] `。
 *
 * 末尾换行不算"又多一条空回复"（`a\n` 是一行）；中间的空行保留，因为 `ctx.reply` 真的
 * 会逐行写出来 —— `[reply] ` 后面是空正文这件事 UI 也要能显示（见 `extractReplies` 的测试）。
 */
function injectedReplyLines(text: string): string[] {
  const lines = text.split(/\r?\n/)
  while (lines.length > 0 && (lines[lines.length - 1] ?? '').trim().length === 0) lines.pop()
  return lines.map((line) => `${REPLY_PREFIX}收到：${line}`)
}

// ---------------------------------------------------------------------------
// 事件总线（mock 没有 `hbot://event`，用内存订阅模拟）
// ---------------------------------------------------------------------------

type MockEventListener = (event: CoreEvent) => void

const eventListeners = new Set<MockEventListener>()

/** 订阅 mock 事件，返回取消订阅函数（`ipc.ts` 的 `onEvent` 用它兜住 mock 模式）。 */
export function subscribeMockEvents(listener: MockEventListener): () => void {
  eventListeners.add(listener)
  return () => {
    eventListeners.delete(listener)
  }
}

function emitMockEvent(event: CoreEvent): void {
  // 先复制再遍历：监听者在回调里退订不该影响本次派发。
  for (const listener of [...eventListeners]) listener(event)
}

/** mock 记下的某个文件的断点（测试用；顺序与写入时一致）。 */
export function mockBreakpointsOf(file: string): DebugBreakpoint[] {
  return (mockBreakpoints.get(file) ?? []).map((entry) => ({ ...entry }))
}

/**
 * 手动造一条 `debug.paused`（**只给测试与手动调试用**，产品代码不该调用）。
 *
 * 为什么不是 mock 自己推：`docs/06-ide.md` §6.2 的暂停语义是"worker 真的卡在断点上"，
 * mock 没有插桩执行器，自动伪造只会让浏览器里看起来能调试、真机上却不会。所以暂停
 * 事件留一个显式入口：要验证调试面板（含"命中的条件"）时，由测试/人明确造一条。
 */
export function simulateMockPause(pause: {
  botId?: string
  file: string
  line: number
  /** 命中的条件原文；`null`/省略 = 无条件断点或单步暂停。 */
  condition?: string | null
}): void {
  emitMockEvent({
    type: 'debug.paused',
    botId: pause.botId ?? BOT_ID,
    file: pause.file,
    line: pause.line,
    condition: pause.condition ?? null,
    scopes: [],
    callStack: [{ name: 'onMessage', file: pause.file, line: pause.line, column: 1 }],
  })
}

// ---------------------------------------------------------------------------
// 搜索（`search_start` 回 `data: null`，命中合成一条 `file.search`）
// ---------------------------------------------------------------------------

let searchSeq = 0

function nextSearchId(): string {
  searchSeq += 1
  return `mock-search-${searchSeq}`
}

/** 已取消、但事件还没送出的搜索 ID：送出前丢掉，模拟核心"取消后不再推"。 */
const cancelledSearches = new Set<string>()

/**
 * mock 分发的回放基线时间（固定值，不用 `Date.now()`）。
 *
 * 为什么固定：面板把 `receivedAt` 格式化成时钟，卡在"现在"上会让断言随运行时刻漂移；
 * 三条事件彼此相隔一分钟，也顺手覆盖了"按时间顺序读"这条读法。
 */
const MOCK_REPLAY_BASE_AT = 1_735_689_600_000

/**
 * mock 的回放条目：三种状态（`match` / `diff` / `error`）**各一条**。
 *
 * 为什么必须造出差异：回放面板的全部价值在于「历史 vs 重跑」的对照。若 mock 回一份全一致的
 * 报告，浏览器里永远看不到「多出 / 缺少 / 顺序不同」的标记 —— 这块 UI 在唯一能跑的模式下
 * 就无法验证（真机要跑起来才有数据）。
 */
const MOCK_REPLAY_ENTRIES: readonly ReplayEntry[] = [
  {
    inboundId: 'mock-inbound-1',
    externalId: 'local:default',
    conversationExternalId: 'local:default',
    receivedAt: MOCK_REPLAY_BASE_AT,
    status: 'match',
    historical: ['在的，有什么可以帮你'],
    replayed: ['在的，有什么可以帮你'],
    same: ['在的，有什么可以帮你'],
    extra: [],
    missing: [],
    reordered: [],
    error: null,
  },
  {
    inboundId: 'mock-inbound-2',
    externalId: 'local:default',
    conversationExternalId: 'local:default',
    receivedAt: MOCK_REPLAY_BASE_AT + 60_000,
    status: 'diff',
    historical: ['收到，我看一下', '查到了：今天有 3 条待办', '稍后给你详细列表'],
    replayed: ['查到了：今天有 3 条待办', '收到，我看一下', '另外提醒你喝水'],
    same: [],
    extra: ['另外提醒你喝水'],
    missing: ['稍后给你详细列表'],
    reordered: [
      { text: '收到，我看一下', historicalIndex: 0, replayedIndex: 1 },
      { text: '查到了：今天有 3 条待办', historicalIndex: 1, replayedIndex: 0 },
    ],
    error: null,
  },
  {
    inboundId: 'mock-inbound-3',
    externalId: 'local:default',
    conversationExternalId: 'local:default',
    receivedAt: MOCK_REPLAY_BASE_AT + 120_000,
    status: 'error',
    historical: [],
    replayed: [],
    same: [],
    extra: [],
    missing: [],
    reordered: [],
    error: 'handler 抛错：TypeError: reply 不是函数（模拟改了 reply.ts 的签名）',
  },
]

/**
 * 造一份回放报告：`from` 按 `receivedAt` 下限过滤，`limit` 截断条数。
 *
 * 计数与 `entries` 取自**同一批**数据（截断之后的），所以面板上的统计永远和下面列出的条目对得上；
 * 真机核心是否对"整批"统计属于契约细节，UI 不依赖这一点。
 */
function mockReplayReport(
  botId: string,
  from: number | null,
  limit: number | null,
): ReplayReport {
  // 0 / 负数不是"全量"而是非法参数（契约：`limit: null` 才是全量，`0` 是 -32002）。
  // 前端会先拦一道，但 mock 也照核心的形状拒一次 —— 两层都在，UI 的拦截才不是唯一防线。
  if (limit !== null && (!Number.isInteger(limit) || limit <= 0)) {
    throw new Error(`replay_run: limit 必须是正整数或 null（收到 ${limit}，-32002）`)
  }
  const inRange = MOCK_REPLAY_ENTRIES.filter(
    (entry) => from === null || entry.receivedAt >= from,
  )
  const entries = (limit === null ? inRange : inRange.slice(0, limit)).map((entry) => ({
    ...entry,
    historical: [...entry.historical],
    replayed: [...entry.replayed],
    same: [...entry.same],
    extra: [...entry.extra],
    missing: [...entry.missing],
    reordered: entry.reordered.map((change) => ({ ...change })),
  }))
  const count = (pick: (entry: ReplayEntry) => number): number =>
    entries.reduce((total, entry) => total + pick(entry), 0)
  return {
    project: MOCK_PROJECT_PATH,
    bot: botId,
    from,
    limit,
    events: entries.length,
    matchedEvents: entries.filter((entry) => entry.status === 'match').length,
    differingEvents: entries.filter((entry) => entry.status !== 'match' && entry.status !== 'error')
      .length,
    failedEvents: entries.filter((entry) => entry.status === 'error').length,
    same: count((entry) => entry.same.length),
    extra: count((entry) => entry.extra.length),
    missing: count((entry) => entry.missing.length),
    reordered: count((entry) => entry.reordered.length),
    entries,
  }
}

// ---------------------------------------------------------------------------
// 夹具：一份**内存文件系统** + 确定性重跑
// ---------------------------------------------------------------------------

/**
 * 夹具里一条事件的形状（与核心 `hbot_bot::event_json` 的十个键一致）。
 *
 * 注意它不是 `generated/` 里的 DTO —— 事件是 isolate 的入参，从来不过 wire，
 * 所以本地补一个最小接口；多出来的键核心会忽略，mock 也不造。
 */
interface MockFixtureEvent {
  botId: string
  conversationId: string
  messageId: string
  externalId: string
  senderId: string
  /** 事件里 `text` 缺失时核心会补空串（`event_json` 的语义），所以这里也恒为字符串。 */
  text: string
  kind: string
  conversationExternalId: string
  receivedAt: number
  payload: { text: string }
}

/**
 * mock 的夹具"磁盘"：项目内相对路径 → 文件内容（事件数组）。
 *
 * 为什么真的存一份而不是每次回同一个常量：面板的「保存为夹具…」之后紧跟「运行夹具…」是
 * 一条连续操作，回常量会让第二次点击看起来像"没读到我刚存的那份"。同理，`fixture_run`
 * 读不到路径就**报错**（真机是 `-32002`），不静默造一份假夹具。
 */
const mockFixtureFiles = new Map<string, MockFixtureEvent[]>()

/**
 * 夹具路径守卫：项目内 + `.json` 结尾。
 *
 * 与 `assertWritable` 的区别是这里**允许** `.hbot/` —— 夹具的默认位置就是
 * `.hbot/fixtures/`（它是运行数据，不是源码），真机也只对它做"项目内"这一条要求。
 */
function assertFixturePath(path: string): string {
  const normalized = assertInsideProject(path)
  if (!normalized.toLowerCase().endsWith('.json')) {
    throw new Error(`夹具路径必须以 .json 结尾（${path}，-32002）`)
  }
  return normalized
}

/**
 * mock 的"handler"：把事件文本变成回复，**确定性**、没有随机量。
 *
 * 空文本会让它抛错 —— 这不是为了凑 `errors > 0`，而是照 `MOCK_REPLAY_ENTRIES` 里那条
 * error 条目（当时 handler 抛错，历史回复为空）保持同一条线索：夹具从回放条目导出，
 * 重跑时那条仍然炸。界面因此能在浏览器里看到"出错高亮"这条路径。
 */
function mockFixtureHandler(text: string): string[] {
  if (text.trim().length === 0) {
    throw new Error('handler 抛错：事件没有文本，mock 的 handler 拒绝空文本')
  }
  return [`pong:${text}`]
}

/**
 * 造一批事件（`replay_save_fixture` 的"读库"那一步）。
 *
 * 数据源就是 `MOCK_REPLAY_ENTRIES`：mock 没有 SQLite，回放的历史与夹具的历史必须是
 * 同一批，否则"存了再跑"和"直接回放"在浏览器里会给出两套说法。
 */
function mockFixtureEvents(
  botId: string,
  from: number | null,
  limit: number | null,
): MockFixtureEvent[] {
  if (limit !== null && (!Number.isInteger(limit) || limit <= 0)) {
    throw new Error(`replay_save_fixture: limit 必须是正整数或 null（收到 ${limit}，-32002）`)
  }
  const inRange = MOCK_REPLAY_ENTRIES.filter((entry) => from === null || entry.receivedAt >= from)
  const picked = limit === null ? inRange : inRange.slice(0, limit)
  return picked.map((entry, index) => {
    const text = entry.historical[0] ?? ''
    return {
      botId,
      conversationId: MOCK_CONVERSATION.conversationId,
      messageId: `mock-message-${index + 1}`,
      externalId: entry.externalId,
      senderId: '10086',
      text,
      kind: 'private',
      conversationExternalId: entry.conversationExternalId,
      receivedAt: entry.receivedAt,
      payload: { text },
    }
  })
}

/** 存夹具：校验路径与参数 → 写进内存 → 回 `SavedFixture`。 */
function mockSaveFixture(
  botId: string,
  from: number | null,
  limit: number | null,
  path: string,
): SavedFixture {
  if (botId.length === 0) throw new Error('replay_save_fixture: botId 不能为空（-32002）')
  if (botId !== BOT_ID) throw new Error(`replay_save_fixture: 未知 bot（${botId}）`)
  const target = assertFixturePath(path)
  const events = mockFixtureEvents(botId, from, limit)
  if (events.length === 0) {
    throw new Error('replay_save_fixture: 这个范围里没有可保存的历史入站（-32001）')
  }
  mockFixtureFiles.set(target, events)
  return { path: target, events: events.length }
}

/** 跑夹具：只读那份内存文件，逐条过 `mockFixtureHandler`，一条出错不影响其余。 */
function mockRunFixture(botId: string, path: string): FixtureRunReport {
  if (botId.length === 0) throw new Error('fixture_run: botId 不能为空（-32002）')
  // 故意**不**校验 botId 是不是演示 bot：这条命令在真机上不查库，库里没有那个 bot
  // 也照跑。mock 若在这里拒了，前端就会误以为"夹具重跑要求库里先有 bot"。
  const target = assertFixturePath(path)
  const events = mockFixtureFiles.get(target)
  if (events === undefined) {
    throw new Error(`fixture_run: 读不了夹具文件（${target}，不存在或不是 JSON 数组，-32002）`)
  }

  let replies = 0
  let errors = 0
  const entries = events.map((event, index) => {
    const text = event.text.trim().length === 0 ? null : event.text
    let entryReplies: string[] = []
    let error: string | null = null
    try {
      entryReplies = mockFixtureHandler(event.text)
      replies += entryReplies.length
    } catch (failure) {
      errors += 1
      error = failure instanceof Error ? failure.message : String(failure)
    }
    return {
      index,
      externalId: event.externalId,
      text,
      replies: entryReplies,
      error,
    }
  })

  return { path: target, events: events.length, replies, errors, entries }
}

/** mock 分发：与 `dispatch()` 的返回类型一致，由 ipc.ts 收窄。 */
export async function mockDispatch(command: Command): Promise<CommandOutputs[CommandType]> {
  switch (command.type) {
    case 'workspace.tree':
      // 只有一条演示项目（见 `MockProject` 的注释）：无论 `project_open` 打开的是哪个
      // 路径，树都来自 `TREE`。真机上这里由核心按"当前 project 根"现扫。
      return structuredClone(TREE)

    case 'fs.read': {
      const path = assertInsideProject(command.path)
      const content = FILES.get(path)
      if (content === undefined) throw new Error(`文件不存在：${command.path}`)
      return { path, content, version: versionOf(path) }
    }

    case 'fs.write': {
      assertWritable(command.path)
      const path = command.path.replace(/\\/g, '/')
      FILES.set(path, command.content)
      const version = versionOf(path) + 1
      VERSIONS.set(path, version)
      return { path, version }
    }

    case 'search.start': {
      // 命中先算出来（非法正则在这里抛，和核心一样走错误行），事件用 `queueMicrotask`
      // 送出：保证它在 `dispatch` 的 Promise 结算之后到达，与真机的顺序一致。
      const matches = searchMatches(FILES, command.query, command.regex, command.globs)
      const searchId = nextSearchId()
      if (cancelledSearches.size > 64) cancelledSearches.clear()
      queueMicrotask(() => {
        // `delete` 兼顾"已经取消过"的判定与集合清理。
        if (cancelledSearches.delete(searchId)) return
        emitMockEvent({ type: 'file.search', searchId, matches })
      })
      return null
    }

    case 'search.cancel':
      cancelledSearches.add(command.searchId)
      return null

    case 'diagnostics': {
      // mock 没有 TS 编译器（真机由 `hbot_code::compile_project` 出诊断），所以这里恒回
      // 空集；但**照核心的契约补推一条 `diagnostics` 事件**（`files.rs` 的 `diagnostics`
      // 无论 items 是否为空都会推），否则问题面板在浏览器里永远等不到"刷新完成"。
      const eventPath = command.path ?? ''
      queueMicrotask(() => {
        emitMockEvent({ type: 'diagnostics', path: eventPath, items: [] })
      })
      return []
    }

    // 真机上是 `bot_list`：行来自 hbot-store 的 bot 表 + 本进程的 supervisor 状态。
    case 'bot.list':
      return [mockBotSummary()]

    // 启停只回"命令已接受"（真机 `data: null`），运行态由 `bot.status` 事件推进；
    // mock 在这里改内部状态**并补推一条 `bot.status`**：真机核心启停后总会推它，
    // 少了这条，浏览器里的状态栏/试跑面板会永远停在 stopped。
    case 'bot.start':
      if (command.botId !== BOT_ID) throw new Error(`bot_start: 未知 bot（${command.botId}）`)
      setMockStatus('running')
      queueMicrotask(() => {
        emitMockEvent({ type: 'bot.status', botId: BOT_ID, status: 'running', error: null })
      })
      return null

    case 'bot.stop':
      if (command.botId !== BOT_ID) throw new Error(`bot_stop: 未知 bot（${command.botId}）`)
      setMockStatus('stopped')
      queueMicrotask(() => {
        emitMockEvent({ type: 'bot.status', botId: BOT_ID, status: 'stopped', error: null })
      })
      return null

    // 本地平台的会话是固定的（见 `MOCK_CONVERSATION`）：真机上这条命令查的是适配器
    // 登记过的会话表，空数组是合法结果（还没登记过任何会话）。
    case 'bot_conversations':
      if (command.botId !== BOT_ID) throw new Error(`bot_conversations: 未知 bot（${command.botId}）`)
      return [MOCK_CONVERSATION]

    // 试跑的核心：把注入的文本逐行回显成 `[reply] ` 日志，并照真机顺序推 `run.log` 事件。
    // 事件用 `queueMicrotask` 送出，保证它在 `dispatch` 的 Promise 结算之后到达
    // （与 `search.start` 同一个理由：run-store 先拿到"命令成功"，再收日志）。
    case 'bot_inject': {
      if (command.botId !== BOT_ID) throw new Error(`bot_inject: 未知 bot（${command.botId}）`)
      // 只认内部 UUID：把 `externalId`（`local:default`）传进来必须被拒 —— 真机是
      // `get_conversation` 查不到 → -32001，UI 若拿错字段，mock 不该替它兜住。
      if (command.conversationId !== MOCK_CONVERSATION.conversationId) {
        throw new Error(`bot_inject: 会话不存在（${command.conversationId}）`)
      }
      if (mockStatus !== 'running') {
        // 真机上注入一个没在跑的 bot 是拒绝而不是静默丢弃（试跑面板会把它显示成错误）。
        throw new Error('bot_inject: bot 未在运行（先点「开始试跑」）')
      }
      const lines = injectedReplyLines(command.text)
      if (lines.length === 0) throw new Error('bot_inject: text 不能为空')
      const at = Date.now()
      const rows = lines.map((line, index) => ({
        botId: command.botId,
        at: at + index,
        level: 'info' as const,
        line,
        source: 'bot',
      }))
      mockRunLogs.push(...rows)
      if (mockRunLogs.length > MOCK_RUN_LOG_LIMIT) {
        mockRunLogs.splice(0, mockRunLogs.length - MOCK_RUN_LOG_LIMIT)
      }
      queueMicrotask(() => {
        for (const row of rows) {
          emitMockEvent({
            type: 'run.log',
            botId: row.botId,
            level: row.level,
            line: row.line,
            at: row.at,
          })
        }
      })
      return null
    }

    // mock 没有 SQLite：只回本进程里试跑过的那批 run log（真机按 bot 过滤查数据库）。
    case 'logs.tail': {
      const rows =
        command.botId === null
          ? mockRunLogs
          : mockRunLogs.filter((row) => row.botId === command.botId)
      const tail = command.limit > 0 ? rows.slice(-command.limit) : rows
      return tail.map((row) => ({
        at: row.at,
        level: row.level,
        line: row.line,
        source: row.source,
      }))
    }

    case 'agent.send':
      return { sessionId: command.sessionId ?? MOCK_SESSION_ID }

    case 'agent.cancel':
    case 'agent.approve':
    case 'edit.apply':
    case 'edit.reject':
      return null

    case 'debug.set-breakpoints':
      // 全量覆盖：空数组 = 移除该文件（与核心 `Breakpoints::set` 一致）。
      // 条件原样记下（`null` = 无条件），mock 不求值任何表达式。
      mockBreakpoints.set(command.file, command.breakpoints.map((entry) => ({ ...entry })))
      return null

    case 'debug.resume':
    case 'debug.step':
      // mock 没有插桩执行器，收下命令即可；不会推 `debug.paused`。
      return null

    // 设备能力：状态与授权有 `data`，撤销与请求只回 null（docs/09 §8.1）。
    // 真机的这四条目前还是桩（`crates/hbot-rpc/src/app/commands/device.rs`），
    // 但**形状**已经冻结，所以 mock 按最终契约实现，前端测试才有东西可测。
    case 'deviceStatus':
      if (command.botId !== BOT_ID) throw new Error(`device_status: 未知 bot（${command.botId}）`)
      return mockDeviceStatus(command.botId)

    case 'deviceGrant': {
      if (command.botId !== BOT_ID) throw new Error(`device_grant: 未知 bot（${command.botId}）`)
      // 空列表 = 全拒而不是"无限制"：在这里拒绝，前端才不会以为空数组是放行。
      if (command.allowedApps.length === 0) {
        throw new Error('device_grant: allowedApps 不能为空（空数组等于全拒，-32002）')
      }
      const minutes = command.minutes ?? MOCK_DEVICE_DEFAULT_MINUTES
      if (!Number.isInteger(minutes) || minutes <= 0 || minutes > MOCK_DEVICE_MAX_MINUTES) {
        throw new Error(`device_grant: minutes 必须在 1..30 之间（收到 ${minutes}，-32002）`)
      }
      const grantedAt = Date.now()
      mockDevice = {
        botId: command.botId,
        sessionId: MOCK_DEVICE_SESSION_ID,
        state: 'active',
        sub: [...command.sub],
        allowedApps: [...command.allowedApps],
        grantedAt,
        deadline: grantedAt + minutes * 60_000,
      }
      return mockDeviceStatus(command.botId)
    }

    case 'deviceRevoke':
      if (command.botId !== BOT_ID) throw new Error(`device_revoke: 未知 bot（${command.botId}）`)
      // 幂等：没有会话时也回成功（核心 `device_revoke` 同语义）。
      mockDevice = null
      return null

    case 'deviceRequest':
      if (command.botId !== BOT_ID) throw new Error(`device_request: 未知 bot（${command.botId}）`)
      // mock 不自动放行、也不假造 `device.request` 事件：请求必须由人点「允许」。
      return null

    // 宿主设置：get 读回内存里那份，set 校验后**真的覆盖**它（见 `checkedMockDeviceSettings`）。
    // 不额外推 `device.settings` 事件，理由与 `device.status` 同一条（上面的 mock 纪律）：
    // 保存成功已经把生效值回给了调用方，前端不该靠"mock 恰好推了事件"才正确。
    case 'deviceSettingsGet':
      return cloneMockDeviceSettings(mockDeviceSettings)

    case 'deviceSettingsSet': {
      mockDeviceSettings = checkedMockDeviceSettings(command.settings)
      return cloneMockDeviceSettings(mockDeviceSettings)
    }

    // 打开项目：核心规范化路径并切换 workspace（`system.rs` 的 `project_open`）。
    // mock 只有一条演示项目，所以任何路径都登记到这同一张表上——`TREE` / `FILES`
    // 就是它的内容，于是"打开项目之后树不是空的"在浏览器里也成立。
    case 'project.open': {
      const path = normalizeMockProjectPath(command.path)
      if (!mockProjects.has(path)) {
        mockProjects.set(path, { path, name: projectNameOf(path), entry: 'src/main.ts' })
      }
      return { path }
    }

    // 新建项目：核心会 scaffold 目录并回 `ProjectSummary`；mock 不写盘，只登记 + 回摘要。
    // "已登记同路径"照核心回 -32003（目录非空那条也算它）。
    case 'project.create': {
      const path = normalizeMockProjectPath(command.path)
      const name = command.name.trim()
      if (name.length === 0) throw new Error('project_create: name 不能为空')
      if (mockProjects.has(path)) {
        throw new Error(`project_create: 该路径已登记为项目（${path}，-32003）`)
      }
      mockProjects.set(path, { path, name, entry: 'src/main.ts' })
      return { path, name, entry: 'src/main.ts' }
    }

    // 回放：不联网、不写库，一次回整份对账报告（没有事件可等）。
    // botId 照 device 系列同样校验：拿空串/别的 bot 去回放，等于对着不存在的 bot 跑一遍历史。
    case 'replay_run': {
      if (command.botId.length === 0) throw new Error('replay_run: botId 不能为空（-32002）')
      if (command.botId !== BOT_ID) throw new Error(`replay_run: 未知 bot（${command.botId}）`)
      return mockReplayReport(command.botId, command.from, command.limit)
    }

    // 夹具：写一份、再读一份（见上面那节）。两条命令的路径守卫都与核心同一套
    // （项目内 + `.json`），所以越界路径在 mock 里也会被拒 —— 前端测试才测得到错误分支。
    case 'replaySaveFixture':
      return mockSaveFixture(command.botId, command.from, command.limit, command.path)

    case 'fixtureRun':
      return mockRunFixture(command.botId, command.path)

    default: {
      const exhaustive: never = command
      throw new Error(`未实现的命令：${JSON.stringify(exhaustive)}`)
    }
  }
}