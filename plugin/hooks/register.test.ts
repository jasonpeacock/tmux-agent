import type { On } from 'claude-code'
import { expect, mock, test } from 'claude-code/testing'

const TMUX_ENV = { TMUX: '/tmp/tmux-1/default,99,0', TMUX_PANE: '%3' }

// Stands for the engine beneath the plugin: the tmux environment, the
// session id, and process.run, recording each tmux-agent report's argv.
function world(on: On, env: Record<string, string> = TMUX_ENV) {
  mock.env(on, env)
  const clock = mock.clock(on)
  const reports: string[][] = []
  on('process.run', (_, e) => {
    if (e.argv[0] === 'sh') {
      return { value: { exitCode: 0, stdout: '4242\n', stderr: '', isStdoutTruncated: false } }
    }
    if (e.argv[1] === 'report') reports.push([...e.argv])
    return { value: { exitCode: 0, stdout: '', stderr: '', isStdoutTruncated: false } }
  })
  on('session.id', () => ({ value: 'sid-1' }))
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  on('session.end', (_, e) => ({ sessionId: e.sessionId }))
  on('turn.start', (_, e) => ({ turnId: e.turnId }))
  on('turn.complete', () => ({ text: '' }))
  on('tool.call', () => ({ result: {} }))
  // No agent is listed as running: the background fallback would report done.
  on('agent.list', () => ({ value: [] }))
  // Classic hook events: no settings hook beneath decides anything.
  on('classic.PermissionRequest', () => ({}))
  on('classic.PostToolUse', () => ({}))
  on('classic.Notification', () => ({}))
  on('classic.Stop', () => ({}))
  const events = () => reports.map(r => r[2])
  return { clock, reports, events }
}

function done(reason: 'answer' | 'aborted' | 'error', agentId?: string) {
  return { reason, answer: '', durationMs: 1, isAborted: reason === 'aborted', turnId: 't1', agentId }
}

const START = { cwd: '/', surface: 'terminal', isInteractive: true } as const

test('a turn reports claim, working, then done with its pane and pid', async ($, on) => {
  const w = world(on)
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.turn.complete(done('answer'))
  await w.clock.advance(0)

  expect(w.events()).toEqual(['claim', 'working', 'done'])
  expect(w.reports[0]).toEqual(expect.arrayContaining(['--pane', '%3', '--socket', '/tmp/tmux-1/default', '--pid', '4242', '--session', 'sid-1', '--proto', '1']))
  expect(w.reports[0]).not.toContain('--attached')
  expect(w.reports[2]).toEqual(expect.arrayContaining(['--bg', '0']))
})

async function claudeDirOf($: Parameters<Parameters<typeof test>[1]>[0], on: On, env: Record<string, string>) {
  const w = world(on, env)
  await $.session.start(START)
  await w.clock.advance(0)
  const r = w.reports.at(-1)!
  const i = r.indexOf('--claude-dir')
  return i < 0 ? undefined : r[i + 1]
}

test('an attached session reports its CLAUDE_CONFIG_DIR', async ($, on) => {
  expect(await claudeDirOf($, on, { CLAUDE_CONFIG_DIR: '/cfg dir', HOME: '/home/u' })).toBe('/cfg dir')
})

test('an attached session reports ~/.claude by default', async ($, on) => {
  expect(await claudeDirOf($, on, { HOME: '/home/u/' })).toBe('/home/u/.claude')
})

test('a relative config dir is not reported', async ($, on) => {
  expect(await claudeDirOf($, on, { CLAUDE_CONFIG_DIR: 'rel' })).toBeUndefined()
})

test('a session in a pane passes no config dir', async ($, on) => {
  const w = world(on, { ...TMUX_ENV, CLAUDE_CONFIG_DIR: '/cfg' })
  await $.session.start(START)
  await w.clock.advance(0)
  expect(w.reports[0]).not.toContain('--claude-dir')
})

test('outside tmux an interactive session reports as attached, with no pane', async ($, on) => {
  const w = world(on, {})
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.turn.complete(done('answer'))
  await w.clock.advance(0)
  expect(w.events()).toEqual(['claim', 'working', 'done'])
  for (const r of w.reports) {
    expect(r[2]).not.toBe('--attached')
    expect(r).toEqual(expect.arrayContaining(['--attached', '--pid', '4242', '--session', 'sid-1', '--proto', '1']))
    expect(r).not.toContain('--pane')
    expect(r).not.toContain('--socket')
    expect(r).not.toContain('--claude-dir')
  }
  expect(w.reports[2]).toEqual(expect.arrayContaining(['--bg', '0']))

  await $.session.end({ reason: 'prompt_input_exit', sessionId: 'sid-1', resume: undefined as never })
  expect(w.events().at(-1)).toBe('release')
})

test('outside tmux a non-interactive session (claude -p) reports nothing', async ($, on) => {
  const w = world(on, {})
  await $.session.start({ ...START, isInteractive: false })
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await w.clock.advance(0)
  expect(w.reports).toEqual([])
})

test('outside tmux a non-terminal surface reports nothing', async ($, on) => {
  const w = world(on, {})
  await $.session.start({ ...START, surface: null, isInteractive: false })
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await w.clock.advance(0)
  expect(w.reports).toEqual([])
})

test('a non-interactive session (claude -p) reports nothing', async ($, on) => {
  const w = world(on)
  await $.session.start({ ...START, isInteractive: false })
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await w.clock.advance(0)
  expect(w.reports).toEqual([])
})

for (const env of [{ TMUX: '/tmp/tmux-1/default,1,0' }, { TMUX_PANE: '%3' }]) {
  test(`half a tmux environment (${Object.keys(env)[0]} alone) reports nothing`, async ($, on) => {
    const w = world(on, env)
    await $.session.start(START)
    await $.turn.start({ text: 'hi', turnId: 't1' })
    await w.clock.advance(0)
    expect(w.reports).toEqual([])
  })
}

test('a malformed TMUX reports nothing', async ($, on) => {
  const w = world(on, { TMUX: 'garbage', TMUX_PANE: '%3' })
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await w.clock.advance(0)
  expect(w.reports).toEqual([])
})

test('a permission prompt shows after the grace delay and clears when the tool runs', async ($, on) => {
  const w = world(on)
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.classic.PermissionRequest({ tool_name: 'Bash', tool_input: { command: 'ls', description: 'x' } })
  await w.clock.advance(500)
  expect(w.events()).not.toContain('ask')
  await w.clock.advance(600)
  expect(w.events().at(-1)).toBe('ask')

  // Same input, other key order.
  await $.classic.PostToolUse({ tool_name: 'Bash', tool_input: { description: 'x', command: 'ls' }, tool_response: {}, tool_use_id: 'u1' })
  await w.clock.advance(0)
  expect(w.events().at(-1)).toBe('resume')
})

test('a prompt answered within the grace delay never shows', async ($, on) => {
  const w = world(on)
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.classic.PermissionRequest({ tool_name: 'Bash', tool_input: { command: 'ls' } })
  await w.clock.advance(300)
  await $.classic.PostToolUse({ tool_name: 'Bash', tool_input: { command: 'ls' }, tool_response: {}, tool_use_id: 'u1' })
  await w.clock.advance(2000)
  expect(w.events()).toEqual(['claim', 'working'])
})

test('two parallel prompts stay red until both are answered', async ($, on) => {
  const w = world(on)
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.classic.PermissionRequest({ tool_name: 'Bash', tool_input: { command: 'a' }, agent_id: 'sub1' })
  await $.classic.PermissionRequest({ tool_name: 'Bash', tool_input: { command: 'b' }, agent_id: 'sub2' })
  await w.clock.advance(1100)
  await $.classic.PostToolUse({ tool_name: 'Bash', tool_input: { command: 'a' }, tool_response: {}, tool_use_id: 'u1', agent_id: 'sub1' })
  await w.clock.advance(0)
  expect(w.events().at(-1)).toBe('ask')
  await $.classic.PostToolUse({ tool_name: 'Bash', tool_input: { command: 'b' }, tool_response: {}, tool_use_id: 'u2', agent_id: 'sub2' })
  await w.clock.advance(0)
  expect(w.events().at(-1)).toBe('resume')
})

test('AskUserQuestion shows needs-input until it is answered', async ($, on) => {
  const w = world(on)
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  const questions = [{ question: 'Which?', header: 'Pick', options: [{ label: 'a', description: 'a' }, { label: 'b', description: 'b' }], multiSelect: false }]
  await $.tool.call({ tool: 'AskUserQuestion', questions })
  await w.clock.advance(1100)
  expect(w.events().at(-1)).toBe('ask')
  await $.classic.PostToolUse({ tool_name: 'AskUserQuestion', tool_input: { questions }, tool_response: {}, tool_use_id: 'u1' })
  await w.clock.advance(0)
  expect(w.events().at(-1)).toBe('resume')
})

test('the idle reminder notification does not raise needs-input', async ($, on) => {
  const w = world(on)
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.turn.complete(done('answer'))
  await $.classic.Notification({ message: 'waiting', notification_type: 'idle_prompt' })
  await w.clock.advance(2000)
  expect(w.events()).toEqual(['claim', 'working', 'done'])
})

test('Esc during a prompt cancels and clears it', async ($, on) => {
  const w = world(on)
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.classic.PermissionRequest({ tool_name: 'Bash', tool_input: { command: 'ls' } })
  await w.clock.advance(1100)
  await $.turn.complete(done('aborted'))
  await w.clock.advance(2000)
  expect(w.events().at(-1)).toBe('cancel')
})

test('an API error reports error', async ($, on) => {
  const w = world(on)
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.turn.complete(done('error'))
  await w.clock.advance(0)
  expect(w.events().at(-1)).toBe('error')
})

test('background work at Stop keeps the pane working', async ($, on) => {
  const w = world(on)
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.classic.Stop({
    stop_hook_active: false,
    background_tasks: [
      { id: '1', type: 'shell', status: 'running', description: 'build' },
      { id: '2', type: 'monitor', status: 'running', description: 'watch' },
      { id: '3', type: 'subagent', status: 'completed', description: 'old' },
    ],
  })
  await $.turn.complete(done('answer'))
  await w.clock.advance(0)
  expect(w.reports.at(-1)).toEqual(expect.arrayContaining(['done', '--bg', '1']))

  // The wake turn after the shell finishes starts over with no background work.
  await $.turn.start({ text: '', turnId: 't2' })
  await $.classic.Stop({ stop_hook_active: false, background_tasks: [] })
  await $.turn.complete({ ...done('answer'), turnId: 't2' })
  await w.clock.advance(0)
  expect(w.reports.at(-1)).toEqual(expect.arrayContaining(['done', '--bg', '0']))
})

test('/clear resets the pane and exit releases it', async ($, on) => {
  const w = world(on)
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.session.end({ reason: 'clear', sessionId: 'sid-1', resume: undefined as never })
  await w.clock.advance(0)
  expect(w.events().at(-1)).toBe('reset')

  await $.turn.start({ text: 'again', turnId: 't2' })
  await w.clock.advance(0)
  expect(w.events().at(-1)).toBe('working')

  await $.session.end({ reason: 'prompt_input_exit', sessionId: 'sid-1', resume: undefined as never })
  expect(w.events().at(-1)).toBe('release')
})

test('a workflow agent finishing does not end the workflow early', async ($, on) => {
  const w = world(on)
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.classic.Stop({
    stop_hook_active: false,
    background_tasks: [{ id: 'w1', type: 'workflow', status: 'running', description: 'review' }],
  })
  await $.turn.complete(done('answer'))
  await w.clock.advance(0)
  expect(w.reports.at(-1)).toEqual(expect.arrayContaining(['done', '--bg', '1']))

  // One of the workflow's agents ends its turn while the workflow runs on.
  await $.turn.complete(done('answer', 'wf-agent-1'))
  await w.clock.advance(1000)
  expect(w.reports.at(-1)).toEqual(expect.arrayContaining(['done', '--bg', '1']))
})

test('background subagents with no wake still end in done', async ($, on) => {
  const w = world(on)
  await $.session.start(START)
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.classic.Stop({
    stop_hook_active: false,
    background_tasks: [{ id: 'a1', type: 'subagent', status: 'running', description: 'research' }],
  })
  await $.turn.complete(done('answer'))
  await $.turn.complete(done('answer', 'a1'))
  await w.clock.advance(1000)
  expect(w.reports.at(-1)?.slice(2, 3)).toEqual(['done'])
  expect(w.reports.at(-1)).not.toContain('--bg')
})
