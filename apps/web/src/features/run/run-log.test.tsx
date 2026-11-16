// @vitest-environment jsdom
/** 日志面板：Agent 行要能和普通 bot 行在视觉上区分开。 */

import { cleanup, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { useRunStore } from '../../stores/run-store'
import { RunLog } from './run-log'

beforeEach(() => {
  useRunStore.setState({
    botId: '',
    status: 'stopped',
    logs: [
      {
        level: 'info',
        source: '[agent]',
        message: '[session 3f2a…] agent 工具 fs_read 成功（1 ms）',
        at: 1_700_000_000_000,
      },
    ],
    busy: false,
    error: null,
    hydrated: false,
    live: false,
  })
})

afterEach(cleanup)

describe('RunLog', () => {
  it('渲染 [agent] 来源前缀，与 bot 日志区分', () => {
    render(<RunLog />)

    expect(screen.getByText('[agent]')).toBeTruthy()
    expect(screen.getByText('[session 3f2a…] agent 工具 fs_read 成功（1 ms）')).toBeTruthy()
  })
})
