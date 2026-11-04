// Reports Claude Code agent state to the tmux-agent binary, which shows it on
// the tmux window tab. The event mapping is in docs/design.md, "The Claude
// plugin". Every hook passes its event on at once: reports run on one chain
// that no hook waits for, so tmux can never hold up a turn.
import type { EngineInterface, Register } from 'claude-code'

const PLUGIN_VERSION = '0.1.0'
const PROTO = '1'

// Background task types that keep the pane working after the main turn.
// Monitors and crons are long-lived and would pin it there.
const BG_TYPES = new Set(['shell', 'subagent', 'workflow'])
const BG_LIVE = new Set(['pending', 'running'])
const AGENT_LIVE = new Set(['pending', 'running', 'waiting'])

// Notification types that mean a dialog is waiting for the person.
const DIALOGS = ['elicitation_dialog', 'elicitation_url_dialog', 'agent_needs_input', 'worker_permission_prompt']
const DIALOGS_DONE = ['elicitation_complete', 'elicitation_response']

// Where reports go: the pane this session runs in, or, with no pane, a
// session a background daemon runs and `claude attach` shows in some pane;
// the binary finds those panes.
type Target = { pid: string; pane?: { id: string; socket: string }; claudeDir?: string }
type Ask = { agent: string; tool: string; input: string; timer: { cancel: () => void } }

// The session's state. A reload starts it over; the binary keeps what the
// tab shows, and the claim a reload sends leaves that alone.
const s = {
  bin: 'tmux-agent',
  graceMs: 1000,
  // Settles to where to report, or null when this session reports nothing.
  target: Promise.resolve(null) as Promise<Target | null>,
  chain: Promise.resolve() as Promise<unknown>,
  asks: new Map<string, Ask>(),
  askShown: false,
  bg: 0,
  // True when all of bg is background subagents. Shells and workflows wake
  // the main thread when they finish, and that turn reports done; only
  // subagents may need the agent-turn fallback.
  bgOnlyAgents: false,
  mainRunning: false,
}

export const register: Register = (on, options) => {
  s.bin = typeof options.binary === 'string' && options.binary ? options.binary : 'tmux-agent'
  s.graceMs = typeof options.graceMs === 'number' ? options.graceMs : 1000

  on('session.start', ($, e, next) => {
    s.target = e.surface === 'terminal' && e.isInteractive ? findTarget($) : Promise.resolve(null)
    report($, 'claim')
    return next(e)
  })

  on('turn.start', ($, e, next) => {
    if (e.agentId) {
      closeAgent($, e.agentId)
    } else {
      s.mainRunning = true
      s.bg = 0
      closeMain($)
      report($, 'working')
    }
    return next(e)
  })

  on('turn.complete', ($, e, next) => {
    if (e.agentId) {
      closeAgent($, e.agentId)
      if (!s.mainRunning && s.bg > 0 && s.bgOnlyAgents) checkBackground($)
    } else {
      s.mainRunning = false
      turnEnded($, e.reason)
    }
    return next(e)
  })

  on('session.end', async ($, e, next) => {
    if (e.reason === 'clear' || e.reason === 'resume') {
      closeAll($)
      s.bg = 0
      s.mainRunning = false
      report($, 'reset')
    } else {
      await report($, 'release')
    }
    return next(e)
  })

  on('classic.PermissionRequest', ($, e, next) => {
    openAsk($, e.agent_id ?? 'main', e.tool_name, stable(e.tool_input))
    return next(e)
  }).catch(($, e, next) => next(e))

  on('classic.PreToolUse', { tool: ['AskUserQuestion', 'ExitPlanMode'] }, ($, e, next) => {
    // The envelope carries no agent_id: '*' matches any agent's PostToolUse.
    const { tool, tool_use_id: _id, consent: _consent, ...input } = e as Record<string, unknown>
    openAsk($, '*', String(tool), stable(input))
    return next(e)
  }).catch(($, e, next) => next(e))

  on('classic.Notification', { notification_type: DIALOGS }, ($, e, next) => {
    openAsk($, e.agent_id ?? 'main', 'dialog', e.notification_type)
    return next(e)
  }).catch(($, e, next) => next(e))

  on('classic.Notification', { notification_type: DIALOGS_DONE }, ($, e, next) => {
    closeDialogs($, e.agent_id ?? 'main')
    return next(e)
  }).catch(($, e, next) => next(e))

  on('classic.ElicitationResult', ($, e, next) => {
    closeDialogs($, e.agent_id ?? 'main')
    return next(e)
  }).catch(($, e, next) => next(e))

  on('classic.PostToolUse', ($, e, next) => {
    toolFinished($, e.agent_id ?? 'main', e.tool_name, stable(e.tool_input))
    return next(e)
  }).catch(($, e, next) => next(e))

  on('classic.PostToolUseFailure', ($, e, next) => {
    toolFinished($, e.agent_id ?? 'main', e.tool_name, stable(e.tool_input))
    return next(e)
  }).catch(($, e, next) => next(e))

  on('classic.PermissionDenied', ($, e, next) => {
    toolFinished($, e.agent_id ?? 'main', e.tool_name, stable(e.tool_input))
    return next(e)
  }).catch(($, e, next) => next(e))

  on('classic.UserPromptSubmit', ($, e, next) => {
    closeAll($)
    return next(e)
  }).catch(($, e, next) => next(e))

  on('classic.Stop', ($, e, next) => {
    if (e.agent_id === undefined) {
      const live = (e.background_tasks ?? []).filter(t => BG_TYPES.has(t.type) && BG_LIVE.has(t.status))
      s.bg = live.length
      s.bgOnlyAgents = live.every(t => t.type === 'subagent')
    }
    return next(e)
  }).catch(($, e, next) => next(e))
}

// The pane this interactive session runs in. With neither TMUX nor
// TMUX_PANE, an attached target: a session run by a background daemon has no
// TMUX, and attaching raises no event, so the binary finds the panes that
// show it (and ignores engines that no daemon hosts).
async function findTarget($: EngineInterface): Promise<Target | null> {
  try {
    const tmux = await $.env.get('TMUX')
    const id = await $.env.get('TMUX_PANE')
    let pane: Target['pane']
    // Half a tmux environment is neither a pane nor a background session.
    if (!tmux !== !id) return null
    if (tmux && id) {
      const socket = tmux.match(/^(.*),\d+,\d+$/)?.[1]
      if (!socket || !/^%\d+$/.test(id)) return null
      pane = { id, socket }
    }
    // $.process.run is spawned by the engine, so the shell's parent is the
    // Claude process that owns this pane.
    const r = await $.process.run(['sh', '-c', 'echo $PPID'], { timeoutMs: 2000 })
    const pid = r.stdout.trim()
    if (!/^\d+$/.test(pid)) return null
    if (pane) return { pid, pane }
    // A background engine's config dir holds its fleet viewers' beacons;
    // the binary keeps it, as a reconcile that tmux runs has another
    // environment.
    const dir = (await $.env.get('CLAUDE_CONFIG_DIR')) || (await homeClaude($))
    return { pid, claudeDir: dir?.startsWith('/') ? dir : undefined }
  } catch {
    return null
  }
}

async function homeClaude($: EngineInterface): Promise<string | undefined> {
  const home = await $.env.get('HOME')
  return home ? `${home.replace(/\/+$/, '')}/.claude` : undefined
}

// Queues a report. Returns the chain, which only session.end awaits.
function report($: EngineInterface, event: string, extra: string[] = []): Promise<unknown> {
  s.chain = s.chain
    .then(() => s.target)
    .then(async t => {
      if (!t) return
      const session = await $.session.id().catch(() => '')
      const where = t.pane
        ? ['--pane', t.pane.id, '--socket', t.pane.socket]
        : ['--attached', ...(t.claudeDir ? ['--claude-dir', t.claudeDir] : [])]
      // The event comes first: an older binary skips flags it does not know.
      await $.process.run(
        [s.bin, 'report', event, ...where, '--pid', t.pid,
          '--session', session, '--proto', PROTO, '--plugin-version', PLUGIN_VERSION, ...extra],
        { timeoutMs: 2000 },
      )
    })
    .catch(() => {})
  return s.chain
}

function turnEnded($: EngineInterface, reason: string) {
  if (reason === 'aborted') {
    closeAll($)
    s.bg = 0
    report($, 'cancel')
    return
  }
  // The main loop waits on none of its own prompts once its turn ended.
  closeMain($)
  if (reason === 'error') {
    s.bg = 0
    report($, 'error')
  } else {
    report($, 'done', ['--bg', String(s.bg)])
  }
}

// After a background agent's turn: if none of the session's agents is still
// running and the main thread did not wake, the background work is done.
function checkBackground($: EngineInterface) {
  $.clock.after(250, () => {
    if (s.mainRunning) return
    $.agent
      .list()
      .then(list => {
        const live = list.some(a => AGENT_LIVE.has(a.status) && !('teammateId' in a && a.teammateId))
        if (!live && !s.mainRunning && s.bg > 0 && s.bgOnlyAgents) {
          s.bg = 0
          report($, 'done')
        }
      })
      .catch(() => {})
  })
}

function openAsk($: EngineInterface, agent: string, tool: string, input: string) {
  const key = `${agent}\u0000${tool}\u0000${input}`
  if (s.asks.has(key)) return
  const timer = $.clock.after(s.graceMs, () => {
    if (s.asks.has(key) && !s.askShown) {
      s.askShown = true
      report($, 'ask')
    }
  })
  s.asks.set(key, { agent, tool, input, timer })
}

function closeWhere($: EngineInterface, pred: (a: Ask) => boolean) {
  for (const [key, a] of [...s.asks]) {
    if (!pred(a)) continue
    a.timer.cancel()
    s.asks.delete(key)
  }
  if (s.asks.size === 0 && s.askShown) {
    s.askShown = false
    report($, 'resume')
  }
}

function closeAll($: EngineInterface) {
  closeWhere($, () => true)
}

function closeMain($: EngineInterface) {
  closeWhere($, a => a.agent === 'main' || a.agent === '*')
}

function closeAgent($: EngineInterface, agent: string) {
  closeWhere($, a => a.agent === agent)
}

function closeDialogs($: EngineInterface, agent: string) {
  closeWhere($, a => a.tool === 'dialog' && a.agent === agent)
}

// A tool that ran, failed or was denied no longer waits on the person, and
// a dialog its agent raised was answered if the agent is working again.
function toolFinished($: EngineInterface, agent: string, tool: string, input: string) {
  closeWhere($, a =>
    (a.tool === 'dialog' && a.agent === agent) ||
    ((a.agent === agent || a.agent === '*') && a.tool === tool && a.input === input))
}

// A tool input as a string that is the same for equal inputs, whatever the
// key order.
function stable(v: unknown): string {
  if (v === null || typeof v !== 'object') return JSON.stringify(v) ?? ''
  if (Array.isArray(v)) return `[${v.map(stable).join(',')}]`
  const o = v as Record<string, unknown>
  return `{${Object.keys(o)
    .sort()
    .map(k => `${JSON.stringify(k)}:${stable(o[k])}`)
    .join(',')}}`
}
