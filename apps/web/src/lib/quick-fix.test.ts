/**
 * 机械可修的判据与修法（`lib/quick-fix.ts`）。
 *
 * 两条不许退让的：
 * 1. **默认不可修**。`canFixMechanically` 是穷举白名单，未知 code（核心将来新增的、
 *    前端自产的 `web.edit-apply-conflict`、`null`）一律 false —— 这条路径不经模型，
 *    判错就等于替用户乱改代码。
 * 2. **只动报错的那一行**。修法是把消息里报的裸名加上 `./`，其它行一个字都不碰。
 */

import { describe, expect, it } from 'vitest'
import { splitDocumentLines } from './edit-apply'
import type { Diagnostic } from './ipc'
import {
  BARE_IMPORT_CODE,
  applyMechanicalFix,
  bareSpecifierOf,
  canFixMechanically,
  fixedContent,
  planMechanicalFix,
  planToLineEdit,
} from './quick-fix'

const BARE_IMPORT_MESSAGE =
  "bare module specifier 'lib/util' is not allowed; use a relative import ('./x') or the built-in 'hbot' module"

function diagnostic(patch: Partial<Diagnostic> = {}): Diagnostic {
  return {
    path: 'src/plugins/ping.ts',
    line: 2,
    column: 1,
    severity: 'error',
    message: BARE_IMPORT_MESSAGE,
    code: BARE_IMPORT_CODE,
    ...patch,
  }
}

const SOURCE = ["import { log } from 'hbot'", "import { util } from 'lib/util'", '', 'log(util)'].join(
  '\n',
)

describe('canFixMechanically：穷举白名单，默认不可修', () => {
  it('HBOT_BARE_IMPORT 可修（唯一登记的可修 code）', () => {
    expect(canFixMechanically('HBOT_BARE_IMPORT')).toBe(true)
  })

  it('核心会产出的其它 code 都不可修（没有唯一正确修法）', () => {
    for (const code of [
      'HBOT_COMPILE',
      'HBOT_IMPORT_ESCAPE',
      'HBOT_MODULE_NOT_FOUND',
      'HBOT_FORBIDDEN_SYNTAX',
      'HBOT_NO_ENTRY',
      'HBOT_JS_EXCEPTION',
      'HBOT_TIMEOUT',
      'HBOT_MANIFEST_INVALID',
    ]) {
      expect(canFixMechanically(code)).toBe(false)
    }
  })

  it('未知 code 与 null 一律不可修（默认分支就是 false）', () => {
    expect(canFixMechanically('SOMETHING_NEW_FROM_CORE')).toBe(false)
    expect(canFixMechanically('web.edit-apply-conflict')).toBe(false)
    expect(canFixMechanically(null)).toBe(false)
  })
})

describe('bareSpecifierOf', () => {
  it('从核心的消息模板里取出裸名', () => {
    expect(bareSpecifierOf(BARE_IMPORT_MESSAGE)).toBe('lib/util')
  })

  it('没有模板片段时返回 null（不猜）', () => {
    expect(bareSpecifierOf('类型不匹配')).toBeNull()
  })

  it('内置模块 hbot 与相对/绝对路径不算裸名', () => {
    expect(bareSpecifierOf("bare module specifier 'hbot' is not allowed")).toBeNull()
    expect(bareSpecifierOf("bare module specifier './x' is not allowed")).toBeNull()
    expect(bareSpecifierOf("bare module specifier '../y' is not allowed")).toBeNull()
    expect(bareSpecifierOf("bare module specifier '/abs/z' is not allowed")).toBeNull()
  })
})

describe('planMechanicalFix', () => {
  it('把报错那一行的裸名改成相对导入，其余行不动', () => {
    const plan = planMechanicalFix(diagnostic(), splitDocumentLines(SOURCE))
    expect(plan).not.toBeNull()
    expect(plan?.line).toBe(2)
    expect(plan?.before).toBe("import { util } from 'lib/util'")
    expect(plan?.after).toBe("import { util } from './lib/util'")
    expect(plan?.summary).toContain('./lib/util')
  })

  it('双引号写法也能改（引号风格保持不变）', () => {
    const source = 'import { util } from "lib/util"'
    const plan = planMechanicalFix(diagnostic({ line: 1 }), splitDocumentLines(source))
    expect(plan?.after).toBe('import { util } from "./lib/util"')
  })

  it('不可修 code 返回 null（不抛错、不静默改）', () => {
    expect(planMechanicalFix(diagnostic({ code: 'HBOT_COMPILE' }), splitDocumentLines(SOURCE))).toBeNull()
  })

  it('行号越界 / 行号为 0 返回 null', () => {
    expect(planMechanicalFix(diagnostic({ line: 0 }), splitDocumentLines(SOURCE))).toBeNull()
    expect(planMechanicalFix(diagnostic({ line: 99 }), splitDocumentLines(SOURCE))).toBeNull()
  })

  it('报错行里找不到那个裸名时返回 null（宁可没修法，也不改错地方）', () => {
    const source = "const x = 1\nimport { util } from 'lib/other'"
    expect(planMechanicalFix(diagnostic({ line: 1 }), splitDocumentLines(source))).toBeNull()
  })
})

describe('应用：只改缓冲区文本', () => {
  it('planToLineEdit 是"替换那一行"的单行编辑（走 edit-apply 的同一套坐标）', () => {
    const plan = planMechanicalFix(diagnostic(), splitDocumentLines(SOURCE))
    expect(plan).not.toBeNull()
    if (plan === null) return
    expect(planToLineEdit(plan)).toEqual({
      startLine: 2,
      endLine: 2,
      lines: ["import { util } from './lib/util'"],
    })
  })

  it('applyMechanicalFix 改出正确的文本，行数与其它行保持不变', () => {
    const plan = planMechanicalFix(diagnostic(), splitDocumentLines(SOURCE))
    if (plan === null) throw new Error('应当可修')
    const fixed = applyMechanicalFix(SOURCE, plan)
    expect(fixed).toBe(
      ["import { log } from 'hbot'", "import { util } from './lib/util'", '', 'log(util)'].join('\n'),
    )
    expect(splitDocumentLines(fixed)).toHaveLength(splitDocumentLines(SOURCE).length)
  })

  it('fixedContent 是便捷包装：不可修时返回 null', () => {
    expect(fixedContent(diagnostic(), SOURCE)).toContain("'./lib/util'")
    expect(fixedContent(diagnostic({ code: 'HBOT_IO' }), SOURCE)).toBeNull()
  })
})