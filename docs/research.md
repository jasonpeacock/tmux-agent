# Prior-art research

Synthesized from reviews of the plugins in `references.md` (2026-11-03).

## Unique features

### Detection

- Hook-driven state: agent lifecycle hooks call a small CLI (e.g. `agent-state <state>`) that resolves the pane from $TMUX_PANE
- Generic agent-agnostic CLI or protocol (set one pane option / call one script) so any tool or wrapper can report state
- Pane resolution fallback chain: --pane flag, then an env override, then $TMUX_PANE, then a walk up the process ancestry (ppid chain) to a pane_pid
- Process-ancestry ownership check: accept a hook only if it descends from that pane's agent process, so an inherited TMUX_PANE cannot hijack the pane
- Record pane_pid or owner PID plus process start time to reject events after pane respawn, pane-id reuse or PID reuse
- Session scoping: accept only the session_id recorded at SessionStart and ignore other sessions' events (forks, sidebar sessions, nested claude -p)
- Turn-ID tracking (session_id + prompt/turn id) to drop late Stop events from superseded or interrupted turns
- Process detection by walking pane PID subtrees or `ps -t <tty>` with ancestry, because Claude rewrites its process title (pane_current_command can show a version string like '2.1.x')
- Configurable agent process-name list/regex for presence detection (claude, codex, opencode, aider, cursor, etc.)
- Detect agent by pane title pattern (e.g. Claude's '✳ ' prefix, braille spinner for busy)
- Prefer the foreground process group when several agents share a tty
- Screen-scraping fallback (capture-pane plus regex) for agents without hooks or to recover a stuck waiting state
- Screen-hash-diff idle detection (unchanged for N seconds) with a debounce for prompt detection
- Pluggable per-agent external state detector with fallback to a generic heuristic
- Stale-state guard: honor a busy marker only while it still equals the pane's current foreground command
- Ignore SessionStart source=compact and filter the idle_prompt Notification to avoid false transitions
- Ctrl-C root binding (send-keys C-c, then clear state) to approximate the missing interrupt hook, preserving any existing binding
- Grace delay before needs-input (e.g. 3s) so quickly answered prompts never flash or sound
- Explicit 'unknown' state for a live agent process with no hook record, so missing hooks are visible

### States

- working / busy / thinking
- needs-input / waiting / blocked: permission prompt, AskUserQuestion, ExitPlanMode plan approval, elicitation dialogs
- done: finished but not yet seen
- idle: finished and acknowledged, or session started with no prompt yet
- error: StopFailure (API error, context overflow), shown distinctly
- cancelled: explicit interrupt or a declined question
- compacting: working sub-state during PreCompact
- paused or stale: idle for longer than N minutes (e.g. 30) via a single timed watcher
- off/none: no agent, renders nothing
- Layered per-pane model (root activity, attention, pending completion, background-work ledger) projected to one public state
- Track pending permissions and questions as sets; show waiting until all are resolved
- Distinction between done (unseen) and idle (seen)

### Display

- Glyph or icon appended to the existing window-status-format and window-status-current-format without replacing the theme
- Publish state as tmux user options (pane @…-state, window @…-state/-icon) so users embed #{…} tokens in their own formats
- Placeholder interpolation tokens (#{agent_dot}, #{agent_indicator}, #{agent_summary}, etc.) replaced in status-left/right and window formats at load time
- Recolor the window tab bg/fg per state via window-status-style, saving and restoring the original styles (with an __UNSET__ sentinel)
- Tab colouring modes: marker only, attention (whole tab coloured only when blocked), or colour for every state
- Per-pane border colour by state (set-option -p pane-border-style), plus active-pane-border colour
- Optional per-pane background tint via set-option -p window-style (does not steal focus)
- pane-border-format token for per-pane status icons
- Uncolored glyph variant for the active tab so it inherits the highlight style
- Animated spinner for working (braille or custom frames, driven by date +%s % N) and blinking waiting
- Knight Rider scanner animation while running
- Fixed-width icon column to avoid tab reflow
- Use the Claude session/pane title (spinner glyph stripped) as the tab label without renaming the window
- Optionally prefix or rename the window name with the status, saving and restoring automatic-rename
- Custom user-set tab name preserved (stored in @tab_name) while the icon stays live
- Status-right summary of all agents across sessions, e.g. '● 1 waiting ● 2 working ○ 3 idle'
- Other-session corner indicator: busy or bell in sessions other than the viewing client's
- Per-session indicator for choose-tree, plus session dots with attention highlight and seen-tracking
- Native tmux bell flag (BEL to pane tty, monitor-bell, window-status-bell-style) as the attention highlight, auto-cleared on view
- Powerline/inline-colour theme tinting (rewrite literal tab colours into conditional accents)
- Nerd-font, emoji and ASCII icon sets
- Theme colour references (e.g. #{@thm_blue}) kept live at render time
- Optional Claude usage/rate-limit segment (5h/7d %, cost, context %, sparkline, reset countdown, stale marker)
- Optional macOS menu-bar app (Hammerspoon) showing dots per agent window
- Zellij and Ghostty display backends

### Aggregation

- Per-window rollup across all panes, including hidden/zoomed panes, with a configurable priority (needs-input > working > done > idle)
- Rollup computed inside tmux with #{P:…} loops and #{m:*state*,…} conditionals (no script at render time)
- Rollup computed once at write time and stored in a window option, so status refresh costs zero forks
- Numeric rank per state so priority is a single max()
- Separate pane-level and window-level fields so the active pane cannot hide a waiting agent elsewhere
- Recompute the window rollup on pane-exited, pane-died, after-kill-pane and window-layout-changed
- Session-level and server-level aggregation (#{W:}, #{S:}, list-windows -a) for detached or other sessions
- Completion alert only when every agent in the window is finished (one alert per window)

### Navigation

- Triage jump key: go to the agent that most needs you (blocked before done, longest-waiting first) across sessions; repeated presses cycle
- Popup picker (display-popup or fzf) listing agent windows/panes with icon, name, time since last change and cwd; Enter jumps
- Picker sorted by urgency, with inline rename (custom @agent_name label) and refresh
- Curses sidebar pane per window listing agents, with vim/arrow/mouse navigation
- Rebound choose-tree (prefix+w) with status badges
- Multi-client aware: pass #{client_name} so the popup and switch-client target the right client
- `inventory` / `list` subcommand printing a machine-readable agent list for scripting
- Click-to-jump from a desktop notification (terminal-notifier with switch-client)

### Notifications

- tmux display-message toasts on transitions, with a configurable format (#{agent_state}, #{window_name}, etc.) and duration
- User-defined notification command receiving AGENT_STATE/AGENT_STATE_PREV/pane/session/window env vars, run backgrounded (run-shell -b)
- Built-in OS notification backend chain: terminal-notifier, then osascript (macOS), then notify-send (Linux), then display-message, with a forced override
- Sounds: afplay (macOS), paplay/canberra-gtk-play/aplay (Linux), or a terminal bell to the pane tty (works over ssh); per-state sound overrides
- Configurable set of states that notify (default needs-input and done)
- Edge-triggered only: notify on real transitions (working to done/needs-input), never on cold start, once per working episode
- Suppress notifications when the pane is already visible (pane_active && window_active && session_attached)
- Idle reminder: re-ring the bell on idle_prompt if the agent is still blocked
- Notification content: question text, 'Plan ready for review', turn duration, custom tab name
- Debounce (e.g. 5s) and a Do Not Disturb mode
- Notification diagnostic/test script

### Config

- Per-state icon/glyph, colour, style, border and tab-colour options
- Per-state enable toggles and global toggles (border, background, indicator, bell, notify)
- Empty option value means 'do not apply' (as opposed to 'default', which resets)
- Configurable priority, idle-to-paused timeout, needs-input grace delay and poll interval
- Validate options at load (regex compile, forbidden chars like , # { }, multi-line) before mutating anything
- Option changes take effect on the next event without restarting tmux
- Presets (compact / manual) and colour presets; interactive colour setup wizard
- XDG config file for icons, never overwritten on reinstall
- Kill switch env var and debug log env var

### Multi-agent support

- Claude Code hooks (settings.json or shipped as a Claude Code plugin with hooks.json)
- Codex (hooks.json with PermissionRequest/Interrupt, or notify)
- OpenCode JS plugin, with child-session filtering
- Gemini, Qwen, Copilot CLI, goose, Amp, pi, Cursor, Devin, Droid, Grok, Kiro, Mistral Vibe adapters
- Per-agent icon map (claude=…, codex=…, default=…)
- Normalized canonical state vocabulary behind one dispatcher with thin per-agent adapters

### Background agents

- Background subagent work ledger (SubagentStart adds, SubagentStop removes, a matched TaskStop removes) keeps the pane working until all finish; no done/bell while work remains
- 'settling' phase between the last subagent stop and the wake turn's Stop, so the bell rings once
- Ignore events carrying agent_id so subagents roll up into the parent pane
- Skip headless/non-interactive sessions (claude -p) so they cannot clobber pane state
- Unroutable events (no pane ancestry) logged to a bounded audit log rather than shown
- Cross-session visibility for agents in detached sessions (summary, corner indicator, session dots, jump key)
- Explicit --pane / env override to attach an out-of-pane agent to a pane
- Env opt-outs for child agents (e.g. PI_SUBAGENT_CHILD=1)

### Install/Ops

- TPM install plus manual run-shell; entry point resolves its own directory (supports paths with spaces)
- Ship hooks as a Claude Code plugin (marketplace) so settings.json is never edited
- Installer that idempotently merges hooks into ~/.claude/settings.json: strips its own entries, preserves foreign entries, previews by default, makes atomic writes through symlinks, keeps timestamped backups and an install manifest, warns on disableAllHooks
- Uninstall that removes only unchanged entries it owns, plus runtime cleanup of formats, hooks, bindings and options from the live server
- Idempotent tmux hook registration and format injection (marker tokens, saved originals) that survives re-sourcing
- Re-apply on client-attached to survive themes that set status-right later
- `doctor` command: tmux version, hooks installed, markers present, per-state counts, last hook event
- reset_all and dump_settings utilities
- Publish the script path and version in a global tmux option so multiple copies converge on one
- Hooks always exit 0, drain stdin, never block (async or backgrounded), and no-op outside tmux
- Fallback tmux lookup (/opt/homebrew/bin, /usr/local/bin) for minimal PATH in hooks
- Tests on an isolated tmux socket (tmux -L, -f /dev/null), fake agent binaries, replay of real captured hook payloads, a pinned fork count, CI on macOS and Linux with shellcheck
- Recovery integration (tmux-resurrect plus claude --resume)

## Detection approaches

- Hook push into tmux pane user options (@agent_state, set -p) with rendering done by tmux formats. Cheap (2-4 forks per event, 0 per refresh), and state dies with the pane. Needs correct hook mapping, and gets stuck when Claude fires no event (Esc interrupt, crash).
- Hook push with server-side atomic compare-and-set: chain `set-option -F` formats in one tmux invocation (gerbenoostra). There are no read-then-write races and no lock files. The tmux format logic is more complex to write and test.
- Hook push into files under /tmp or a cache dir, read by #() scripts. Simple to write, but has staleness/cleanup and atomicity problems, collisions across tmux servers unless keyed by socket, and fork cost on every refresh.
- Hook push plus a Python daemon/ticker that publishes options (agent-pulse). Allows rich logic: ancestry routing, PID-reuse protection, GC. Costs heavy per-hook Python startup, polling every 0.5s, and an unsupervised daemon.
- Process detection only (ps on the pane tty or a PID-subtree walk). Shows whether an agent is present but not its state. Name matching is fragile because Claude's title becomes a version string, and there is node/vim false-positive risk.
- Screen scraping (capture-pane with regex, or a hash-diff). Needs no hooks and works for any agent, but it is fragile across TUI versions, gives false positives from output text, and costs forks every poll. Claude's permission menu is often misclassified.
- Native tmux bell flag as the 'done' signal (BEL plus monitor-bell). Zero code, and acknowledgment on view is automatic, but any program's bell triggers it, it is window-level only, and any client viewing clears it.
- Title-based (pane_title spinner prefix). Free for agents that set titles, but gives busy/not busy only, with no needs-input.
- Claude statusLine stdin tap: gets usage/cost/rate-limit data only, not work state.

## Background agent approaches

- Ignore entirely: hooks no-op when $TMUX/$TMUX_PANE is unset. This is the most common approach; the agents are invisible.
- Roll subagents into the parent pane: Task-tool subagents share the parent's TMUX_PANE, so their tool and permission hooks land on the parent pane. Ignore events with agent_id for lifecycle, but keep their permission asks as needs-input.
- Background work ledger in a pane option, using hex(session_id NUL agent_id) tokens: SubagentStart adds, and SubagentStop or a matched TaskStop PostToolUse removes. The pane stays working and no done bell fires until the ledger is empty, with a 'settling' phase until the automatic wake turn's Stop. Gaps: background Bash and Monitor tasks have no events, and a missing SubagentStop sticks until SessionEnd.
- Session scoping: accept only the session recorded at the latest SessionStart for the pane, so forks, sidebar sessions and nested `claude -p` cannot overwrite it.
- Skip non-interactive or headless sessions: check the payload, or the mods API surface/isInteractive flag, so `claude -p` scripts never write pane state.
- Process-ancestry routing: a hook must descend from the pane's agent process, otherwise it is logged as 'unroutable' to a bounded audit log. Nested `claude -p` inside the agent still passes, so combine this with session scoping.
- Explicit attachment: a --pane flag or env var (TMUX_AGENT_STATUS_PANE) lets a wrapper attach an out-of-pane agent to a chosen pane.
- Global/off-pane registry (not implemented by any plugin; a possible design for us): headless or unattached agents write records keyed by session_id to a state dir with PID and start time. A status-right segment or summary counts them, e.g. 'bg: 1 waiting, 2 working', and a picker lists them. Dead PIDs are GC'd.
- Cross-session visibility for agents in detached sessions: a status-right summary over list-windows -a (dedupe linked windows), an other-session corner indicator via #{S:}, session dots with seen-tracking, and a cross-session triage jump key.
- Remote agents: a menu-bar poller over ssh, or a terminal bell forwarded over ssh.

## Cross-platform pitfalls

- macOS /bin/bash is 3.2. Bash 4 features fail: `declare -A`, mapfile, fractional `read -t 0.2`. tmux #() and run-shell may not pick up Homebrew bash. Write for bash 3.2 or POSIX sh.
- bash 5.2 enables patsub_replacement, so `&` in a ${var//pat/rep} replacement behaves differently than on bash 3.2.
- Hooks and tmux run-shell run with a minimal PATH. tmux lives at /opt/homebrew/bin (Apple Silicon), /usr/local/bin (Intel) or /usr/bin (Linux), so add fallbacks or absolute paths. Do not hard-code /opt/homebrew only.
- stat: BSD `stat -f %m` vs GNU `stat -c %Y`. On Linux `stat -f` is the filesystem-stat flag and prints garbage before failing, so `stat -f … || stat -c …` breaks. Detect via uname, or avoid stat entirely.
- date: BSD `date -j -f` vs GNU `date -d`. Only `date +%s` is safely portable.
- `timeout` is absent on macOS (it is gtimeout with coreutils).
- ps differences: on Linux procps `-o state=` is a single char without '+' (use `stat`). `ps -t` accepts different tty forms (pts/3 vs basename '3'). Linux comm is truncated to 15 chars. `lstart` output is locale-dependent.
- Claude's process name or comm is often a version string (e.g. '2.1.185') or 'node', not 'claude', on both OSes. Do not rely on pane_current_command or argv basename for presence.
- TMPDIR is a per-user random path on macOS but usually unset on Linux. XDG_RUNTIME_DIR is unset on macOS, so a fallback to world-writable /tmp brings collision and symlink risk. Key state dirs by uid and tmux socket, and create them 0700.
- Notifications: osascript/terminal-notifier and afplay with /System/Library/Sounds exist only on macOS. Linux needs notify-send plus paplay/canberra/aplay, and freedesktop .oga sounds may be missing. Sound names like 'Glass' mean nothing on Linux.
- sed -i: BSD needs an argument (use `-i.bak` then rm). Avoid GNU-only extensions. grep -E (GNU) accepts extensions like \w that tmux's regcomp on macOS may not.
- `sort -V` is missing on very old macOS and busybox.
- Undeclared deps such as rg, python3 or jq, combined with `|| true`, fail silently on one machine. Minimize deps and check them in a doctor command.
- tmux version quirks: 3.4 octal-escapes control characters like \037 in -F output (use tab or a printable separator). Option-value escaping differs between versions. per-pane options need 3.0+, display-popup 3.2+, hex colours 3.2+. Check the server #{version}, not the client binary's.
- show-hooks output quoting differs between tmux versions, so parsing it is fragile.
- ${#str} width math is locale-dependent with emoji/multibyte. Emoji such as ⏳ have ambiguous width, and Nerd Font glyphs depend on the terminal font.
- BEL to /dev/tty may fail in hooks that have no controlling tty. Writing to #{pane_tty} is more reliable.

## Quality lessons

- Aggregate across ALL panes in a window by priority. Several plugins keep one state per window with last-write-wins, or render only the active pane, so one pane's SessionEnd or working event wipes another pane's needs-input.
- Do not key state by window_index. swap-window, move-window and renumbering misattribute it. Use pane_id (%N) and window_id (@N), or pane-scoped options.
- Avoid read-modify-write races between concurrent or parallel hooks. Use server-side `set-option -F` compare-and-set in one tmux invocation, or a lock. Never register two order-dependent hooks for the same event.
- Write files atomically (tmp + mv), and validate records strictly on read.
- Narrow the Notification matcher. Matcher "" or message text matching turns idle_prompt into needs-input after about 60s.
- Do not decay needs-input on a timer (e.g. 30s to idle). An unanswered prompt must keep showing.
- Handle the return from needs-input to working after approval (PostToolUse / PermissionDenied). Without it, needs-input persists through long tool runs.
- Plan for missing events: Esc interrupt and crashes leave a stale working; kill -9 leaves stale state. Mitigate with an agent-alive check (pane PID tree), focus-based cleanup, or reset on the next prompt.
- Recompute the window rollup on pane-exited, pane-died, after-kill-pane and window-layout-changed, and clean up state for closed panes. Global-env and file state leaked forever in several plugins.
- Do not store state in the tmux global environment (set-environment -g). It is copied into every new pane's shell env.
- Guard against headless `claude -p` inheriting TMUX_PANE and hijacking the pane (session_id scoping, non-interactive skip).
- Do not clobber the user's theme. Append or insert tokens idempotently, or publish options. Never overwrite status-right, window-status-format, status-format or automatic-rename-format wholesale, and never use `set-hook -g` without -a or an index.
- Make re-sourcing tmux.conf idempotent (marker tokens, saved originals, hooks matched by plugin path), or segments get duplicated.
- Keep fork counts low. Avoid per-refresh #() scripts that run ps on every pane, Python per hook, or a separate show-option per setting. Batch option reads into one `display -p` and writes into one `;`-chained command.
- Avoid unanchored process regexes (`grep -w claude`, `pgrep -f claude`). They match vim claude.md, the Claude desktop app, and the script itself.
- Pass #{session_id}, #{pane_id} and #{client_name} explicitly. Bare `tmux display-message -p` from a hook resolves to an arbitrary client or the focused pane, which misattributes state.
- Never fall back to 'the currently focused pane' when TMUX_PANE is missing. Drop the event instead.
- Do not shell-interpolate #W or pane paths inside #() or run-shell without quoting. Window names with quotes break or inject commands.
- Do not let focus-based self-heal clear needs-input. Only acknowledge done; otherwise viewing a window erases a pending prompt.
- Clean up properly: kill watcher child sleeps (one plugin leaked orphaned `sleep 1800`s), rotate logs, remove lock files.
- Hooks must always exit 0, drain stdin, be fast or backgrounded, and no-op outside tmux.
- Keep docs consistent with code (one plugin had a contradictory priority order in its README). Remove dead code paths.
- Avoid feature sprawl (VPN guards, Slack badges) entangled with the core status logic.
- Test on isolated tmux sockets with fake agent binaries and replayed real hook payloads. Run CI on both macOS and Linux.

## Plugins ranked by quality

- 1. gerbenoostra/tmux-agent-status (Rust): the best hook mapping. Every state decision is an atomic server-side tmux compare-and-set. The rollup is computed at write time with zero refresh cost. It tracks background subagents with a ledger, scopes by session, has about 590 tests, and runs CI on macOS and Linux. Weak points: no recompute on pane close, and a heavy installer.
- 2. cburmeister/tmux-agent-state (bash 3.2): purely hook-driven. Its #{P:} rollup is evaluated by tmux, it appends to the theme idempotently, it runs about 4 forks per event, and it has a triage jump key, doctor and uninstall. About 200 tests plus CI on both OSes. Weak points: no subagent handling, and the process-name check can wipe state.
- 3. BenRemer/tmux-agent-status (POSIX sh + tmux formats): zero forks at render time, careful validation, a CI matrix, and it skips headless sessions. However it has no needs-input state, and 'done' is just the bell flag.
- 4. jerriclynsjohn/tmux-agent-pulse (Python): robust ancestry routing, PID-reuse protection, pending permission/question sets, a doctor command, and a careful installer. Weak points: heavy Python per hook plus a polling daemon, and macOS-only notifications.
- 5. balvisio/tmux-agent-indicator: correct window priority rollup, turn-ID stale rejection, a FIFO lock and an input grace delay, with a large test suite. Weak points: very heavy, invasive (overwrites formats, rebinds C-c), bash 4 only, and feature sprawl.
- 6. partner0/tmux-agent-status: excellent hook filtering (idle_prompt, compact, AskUserQuestion), a good popup picker, and tests. However it has no per-pane aggregation (window name, last write wins) and leaks orphaned sleep processes.
- 7. JanSuthacheeva/tmux-agent-status: small and clean. It uses option-based rendering, offers a cross-session summary and has e2e tests. However it keeps per-window last-write-wins state, has a broad Notification matcher, and duplicates segments on re-source.
- 8. accessd/tmux-agent-indicator: rich visual and notification options with save/restore styles. Weak points: a race between two UserPromptSubmit hooks, no aggregation, global env leakage, and an undeclared rg dependency.
- 9. Gentleman-Programming/gentle-agent-state: a clean multi-backend dispatcher (tmux/Zellij/Ghostty). Its installer omits PostToolUse and SessionEnd, it has no visible done state, and it clobbers formats and hooks.
- 10. RatulMaharaj/tmux-agent-status: thoughtful done-vs-idle-on-seen and ps-based presence detection. It relies on fragile screen scraping, its notifications are macOS-only, and ps state breaks on Linux.
- 11. ball6847/tmux-pane-agent-status: generic scraping with no aggregation (active pane only). It fires false 'finished' notifications and clobbers formats.
- 12. dcryan/tmux-agentbar: a nice pane-title label and PID-tree detection, but state is keyed by window index with no aggregation, waiting decays after 30s, and stat breaks on Linux.
- 13. jimmyliao/agent-status-tmux: not a status tracker, only a usage meter. It has a global cache, a destructive installer and no tests.

## Hook event mapping

The best-practice mapping (converged on by gerbenoostra, cburmeister, partner0 and agent-pulse), event by event:

- SessionStart, matcher startup|resume|clear (exclude `compact`, which fires mid-turn): reset the pane to idle and record session_id as the accepted session.
- UserPromptSubmit: working. Also clears the previous turn's attention and unseen done. This counts as acknowledgment.
- PreToolUse, matcher AskUserQuestion|ExitPlanMode: needs-input. These tools do NOT fire PermissionRequest.
- PermissionRequest: needs-input.
- Notification, matcher permission_prompt|elicitation_dialog|elicitation_url_dialog|agent_needs_input: needs-input.
  - EXCLUDE idle_prompt. It fires about 60s after Stop (and sometimes mid-turn) and would turn done into needs-input. It can optionally be used as a 'remind' event that re-rings the bell only if the pane is still needs-input.
  - Do not use matcher "" or text matching on the message.
- PostToolUse and PostToolUseFailure: back to working. This is the only 'permission approved' signal.
  - PermissionDenied and ElicitationResult also go back to working.
  - Guard this transition: do not let a straggler PostToolUse override an unseen done when no tracked background work exists.
  - Choose deliberately whether a working event may clear needs-input. `--if-state needs-input` semantics work, or use a layered model.
- PreCompact: working (or a compacting sub-state).
- Stop: done (unseen). Ring the bell / notify only on the transition, and only if the background ledger is empty.
- StopFailure: error, or needs-input.
- SubagentStart / SubagentStop: maintain a ledger keyed by session_id + agent_id. Otherwise ignore events carrying agent_id for state changes, but still let subagent permission prompts mark needs-input.
- PostToolUse with matcher TaskStop and a matching task_id: removes a cancelled background agent from the ledger, since cancelled agents emit no SubagentStop.
- SessionEnd: clear. If the pane is currently visible, leave no unseen done behind.

Gotchas:
1. Esc/Ctrl-C interrupts fire no Stop and no hook, so the pane stays working until the next prompt. Options:
   - a root C-c binding hack (does not catch Esc)
   - a stale timeout
   - a process-alive check on render
   - reset on the next UserPromptSubmit
2. A denied permission that ends the turn may fire no PostToolUse, so the pane can stay needs-input.
3. A delayed Notification permission_prompt can arrive after approval and PostToolUse, causing a stuck needs-input.
4. Hooks for the same event run in parallel. Never register two ordered hooks (e.g. 'off' then 'running') for one event. `async: true` hooks can land out of order (e.g. PostToolUse after Stop). Use atomic server-side compare-and-set, or transition guards.
5. A headless `claude -p` launched inside a pane, including from an agent's Bash tool, inherits TMUX_PANE and clobbers or hijacks that pane's state. Mitigations:
   - session_id scoping
   - skip non-interactive sessions
   - ancestry checks
6. Background agents and background Bash keep running after Stop. Without a ledger you get done/working flapping and false done bells. Background Bash and Monitor tasks fire no lifecycle events at all.
7. tmux run-shell hooks do not set TMUX_PANE. Pass #{pane_id} explicitly.
8. Hooks installed mid-session track nothing until the next SessionStart.
9. Focus-based acknowledgment needs pane-focus-in, session-window-changed, window-pane-changed and after-select-window, with focus-events on. The first client attach fires pane-focus-in.

## Critic: additions

### Detection / event sources

- Claude Code mods API as an in-process alternative to shell hooks (BenRemer adapters/claude/hooks/register.js, hooks.json {"modules":["./register.js"]}). It exposes session.start, turn.start, turn.complete with isAborted, and session.end, each carrying e.turnId, e.agentId, e.surface and e.isInteractive. turn.complete fires on cancellation too, so an Esc/Ctrl-C interrupt can be detected without the C-c binding hack or a stale timeout. It needs a recent Claude (validated on 2.1.314). Testable with `claude plugin validate --strict` and `claude plugin test` (local event harness, no model calls).
- Use the agent's own bell instead of, or alongside, hooks: Claude `preferredNotifChannel: "terminal_bell"` plus `inputNeededNotifEnabled` / `agentPushNotifEnabled`, and Codex `[tui] notification_method="bel"`, `notification_condition="always"`. Claude's default `auto` channel may pick a desktop protocol that tmux never sees as a BEL.
- Target the right tmux server: parse the socket path out of $TMUX (`${TMUX%%,*}`) and call `tmux -S <socket>`, not the default socket. This matters with `tmux -L`, several servers or nested tmux (BenRemer, agent-pulse AGENT_PULSE_SOCKET).
- Record the pane's foreground command as the busy marker so launcher wrappers (e.g. toolbox-exec) still count as the agent (BenRemer).
- Keep SubagentStart hooks synchronous on purpose: Claude awaits them before the child's first model call, so a stop can never arrive before its start. A SubagentStop for a never-started ID is a no-op, because internal helpers emit those routinely (gerbenoostra docs/agents/claude-code.md).
- Claude sends about 12 Notification types. agent_completed, auth_success, elicitation_complete and quota_auto_resume_* do not mean blocked, so the matcher must allowlist types, not just exclude idle_prompt.
- A no-hook periodic reconciliation sweep (balvisio window-state.sh) repairs stale colours, dead PIDs, respawned panes and reused pane IDs without acknowledging anything. Rationale: tmux has no respawn hook.
- Ancestry check for tty matching: also require ancestry from the pane process, not just a matching tty, so container processes with the same pts/N name do not label host panes (balvisio pane-processes.sh).

### Acknowledgment semantics (design choices not called out)

- Scope of acknowledgment differs between plugins. gerbenoostra acknowledges only the focused pane, so a visible split sibling or a hidden zoomed sibling keeps its state. balvisio acknowledges all done panes in the window when any pane is focused. cburmeister acknowledges on window visit. We need to pick one.
- Acknowledgment is per session, not per client: two clients on one session acknowledge each other's states. The first client attach fires pane-focus-in and silently clears the pane it lands on.
- A session-ending event (SessionEnd) on the pane a client is currently displaying counts as already seen: no done glyph, no bell. On any other pane it leaves a pending done (gerbenoostra).

### Display / tmux integration

- tmux option inheritance pitfall: pane options fall back to window options of the same name, so pane-level and window-level options must use different names (e.g. @agent_pane_status vs @agent_status).
- Per-window format overrides set by themes take precedence over global window-status-format. There are two opposite strategies. cburmeister weaves the marker into that window-local format on its next event, and `doctor` flags it. gerbenoostra never writes formats, because a spliced copy in a window-local option freezes that window's format forever.
- Hand-wired detection: if the user's format already contains the token (@agent_state), leave all formats alone (cburmeister).
- Probe tmux capabilities at setup (e.g. whether per-pane border styles are supported) instead of checking only the version (cburmeister).
- Colour window titles only for non-current windows by default, with an opt-in to style the active window too (balvisio @agent-indicator-style-active-window).
- Generic non-agent tab-label icons by foreground command (editor, ssh, build tool, pager, docker, sudo, etc.), with trailing `.exe` stripped from process names (ball6847).
- Bell configuration guidance: `bell-action any` so a bell from the current tmux window still reaches the terminal behind another tab. For nested tmux, `visual-bell both` (`on` swallows the audible bell). The terminal then turns BEL into a tab badge, dock bounce or sound. Live forwarding needs an attached client.
- Remote/SSH rollup: a local window running `ssh HOST` takes the colour of the most urgent agent on that host by polling the remote tmux over ssh (balvisio ssh-hosts.sh). Also an ssh-reconnect wrapper and keyboard passthrough to a nested remote tmux (F12 toggles an `off` key table).

### Install / Ops

- Lazy self-setup: if a hook fires before the tmux plugin is loaded, the script performs setup itself. A newer script version replaces an older one's setup on its next event, with no tmux restart (cburmeister).
- Validate the final tmux config by loading it in a throwaway tmux server, without leaking probe sockets (gerbenoostra register). Also a `--dry-run` / preview mode.
- Installer honours CLAUDE_CONFIG_DIR and CODEX_HOME, with --claude-home / --codex-home overrides (agent-pulse).
- Warn against the duplicate top-level `hooks` key in settings.json: JSON last-key-wins silently drops the user's existing hooks.
- State-schema version marker on panes (gerbenoostra @agent_pane_model=1) so a plugin upgrade can migrate live pane state instead of misreading it.
- Key-binding conflict handling for any bound key, not just C-c: keep a key another plugin already owns and report the conflict (agent-pulse).
- `unload` command that stops the background process and removes owned tmux integration, separate from uninstalling hooks (agent-pulse).
- Never block on stdin when it is a terminal (hand-typed CLI use): check isatty before draining.
- Claude `--bare` skips plugins, so bare sessions never report. Running sessions need `/reload-plugins` after a plugin install. Codex needs a `/hooks` trust review.
- Lock contention strategy: queue-ordered file lock next to the tmux socket, released by the OS on kill. After 2s of contention the hook returns 0 and a detached retry applies the event in order, and the retry is discarded if the pane was replaced (balvisio).

### Privacy

- Store and log only lifecycle metadata, never prompts, tool arguments or responses (agent-pulse). This is in tension with the synthesis's 'notification content: question text', so it should be a conscious opt-in.

## Critic: corrections

- Priority is not a consensus 'needs-input > working > done'. gerbenoostra ranks waiting > error > done > working: an unseen done outranks working, on the reasoning that done is actionable and working is not. agent-pulse ranks waiting > working > cancelled > idle > unknown. cburmeister and balvisio use blocked/needs-input > working > done. The user asked for needs-input > working > done, and should be told the alternative exists.
- Hook mapping consensus: gerbenoostra's SessionStart matcher is startup|resume|clear|fork (it includes fork), not startup|resume|clear.
- Gotcha 1 ('Esc/Ctrl-C fire no hook') applies to settings.json hooks only. The mods API turn.complete reports isAborted.
- 'Hooks always async/backgrounded' is not universal: gerbenoostra keeps SubagentStart synchronous for ordering. BenRemer's Claude adapter is an in-process mod rather than a shell hook.
- gerbenoostra 'no recompute on pane close' is confirmed: share/tmux/tmux-agent-status.conf registers only pane-focus-in, session-window-changed and window-pane-changed.
- BenRemer does have a Claude adapter that reports working via @agent-status-busy-command. Its 'done' is Claude's own notification (BEL) via $.ui.notify, not hook-written state. Requires tmux 3.4+ (stricter than the synthesis implies).
