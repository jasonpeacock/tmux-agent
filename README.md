# tmux-agent

[![ci](https://github.com/jasonpeacock/tmux-agent/actions/workflows/ci.yml/badge.svg)](https://github.com/jasonpeacock/tmux-agent/actions/workflows/ci.yml)

Shows the status of Claude Code agents on tmux window tabs, so you can see which windows need you. Each tab gets a coloured marker for the most urgent agent in any of its panes:

**needs input** (red) **> done, not yet seen** (green) **> working** (yellow) **> idle** (nothing)

A pane counts as seen once you focus it. A status-right summary counts the agents in each state across all sessions.

> Status: early development. Tested on macOS and Linux with tmux 3.3a to 3.8, and against live Claude Code 2.1.316 sessions, both run in a pane and viewed through `claude attach`. Design and limits: [docs/design.md](docs/design.md).

## Why tmux-agent

- **Built on Claude Code's plugin API, for accuracy.** Other tmux status plugins either read the pane's screen or run shell hooks from `settings.json`. tmux-agent runs inside Claude Code as a plugin and listens to the plugin API's events. Those carry what hooks and screen-scraping cannot:
  - whether a turn ended normally, was cancelled with Esc, or failed;
  - which agent raised a prompt;
  - which background shells, subagents and workflows are still running.
- **Reliable.** No guessing from screen text. Every state change comes from an event Claude sends, and tmux draws the tabs from options. The only process per status refresh is a cleanup check (`reconcile`), which exits at once when nothing changed.
- **Tightly integrated.** It skips `claude -p` scripts and non-terminal sessions, follows `/clear` and resume, and keeps its state when the plugin reloads mid-turn. It also shows sessions that Claude runs in the background and you view through `claude attach`.
- **Easy to install.** The Claude side is a Claude Code plugin: no `settings.json` edits, and nothing to keep in sync by hand. The tmux side is one binary and one `run-shell` line.
- **Nix flake and Home Manager module.** One flake input and `programs.tmux-agent.enable = true` installs the binary, the tmux plugin, its options and the Claude plugin.
- **macOS and Linux.** One Rust binary, which calls no shell tools from the BSD/GNU split. Works with tmux 3.3 and later.

## Features

**State**
- Four states: needs input, done (not yet seen), working, idle. A window shows its most urgent pane, including panes hidden by a zoom.
- Needs input covers:
  - permission prompts, from the main agent and from subagents;
  - `AskUserQuestion` and plan approval;
  - MCP elicitation dialogs;
  - API errors that end a turn.
- Done stays until you focus the pane, then turns idle. A turn that ends while you are already looking at the pane goes straight to idle.
- Background shells, subagents and workflows keep the tab working after the main turn ends, until they finish.

**Display**
- Publishes `@agent_tab_color` and `@agent_tab_icon` per window for the theme to read, and never rewrites the theme's formats. A `style` mode instead sets the tab's style itself, for themes without inline colours.
- Optional pane border colour per state.
- A status-right summary (`@agent_summary`) with icons or coloured dots, counting each agent once.
- Configurable icons and colours. Colours may be theme formats such as `#{@thm_red}`.

**Navigation**
- `prefix a` jumps to the most urgent agent on the server: needs input first, then done, oldest first. Pressing it again cycles to the next one.
- `prefix A` opens a menu of all agents, sorted by urgency, with window, directory and time in state.
- `prefix w` shows agent icons in tmux's window tree.
- `tmux-agent list [--json]` prints the agent panes for scripts.

**Operations**
- `tmux-agent doctor` checks:
  - the tmux version and `focus-events`;
  - hooks, keys and the binary path;
  - theme integration;
  - every agent pane and background session.
- Every tmux hook and key binding is registered idempotently, so re-sourcing `tmux.conf` changes nothing.
- A missing binary or an absent server never interrupts Claude or tmux. Commands called from hooks always exit 0.

## Edge cases handled

**Claude sessions**
- `claude -p` scripts, including ones an agent runs inside its own pane, cannot take over the pane's state.
- A second interactive `claude` started inside an occupied pane is refused while the first is alive.
- Ownership is the Claude process plus its start time, so a reused PID is not mistaken for the owner.
- `/clear` and in-process resume reset the pane, though no new session event fires.
- A plugin reload mid-turn keeps the current state.
- Esc during a turn, or during a prompt, goes to idle.
- Claude's idle reminder, about 60 seconds after a turn ends, does not turn done into needs input.
- A prompt answered within the 1-second grace delay never flashes red.
- Several prompts open at once stay red until every one is answered.
- A late notification after a prompt was answered does not bring red back.
- A workflow's own agents finishing does not end the workflow's working state early.
- Long-lived monitors and scheduled tasks are not counted as background work.
- After the left-arrow handoff or `/fork` moves a session to the background, the pane's fleet view shows the job it is attached to, once that job has a name. An unnamed job, or one whose name another job shares, is not shown, rather than risk showing the wrong job.

**Process lifecycle**
- A Claude crash, `kill -9`, or `respawn-pane -k` is caught on the next refresh: the state of a dead owner is cleared.
- Panes moved with break, join, swap or move-pane carry their state, and both windows are recomputed.
- Killed panes, windows and sessions leave no stale tab colour or summary count.
- Linked windows and grouped sessions are counted once.
- tmux-resurrect restores no stale state, and a resumed `claude` claims its pane again.
- Background sessions viewed through `claude attach` survive a tmux restart.

**tmux and portability**
- Concurrent reports from several panes are serialised by a lock beside the server socket.
- One tmux call reads all state, and one writes back only what changed.
- No UTF-8 locale needed: without one, tmux would otherwise mangle tab-separated output.
- tmux 3.4's `$` escaping in command output is undone.
- A pane path containing a newline cannot forge records: each read carries a random tag.
- Window names, paths and binary paths with spaces, quotes, `#` or `$` are quoted safely.
- Keys you already bound are never taken. tmux resolves key spellings such as `space` and `Space` to the same key.
- A relative socket path in hooks is made absolute.
- Under Nix, hooks call a profile path that survives upgrades and garbage collection.
- Hooks run with a minimal `PATH`, so the binary looks for the running server's own tmux executable before searching `PATH` and the usual install directories. `TMUX_AGENT_TMUX` overrides the search.
- Multiple clients: the jump key moves the client that pressed it.

## Install

### Home Manager

```nix
# flake.nix inputs
tmux-agent = {
  url = "github:jasonpeacock/tmux-agent";
  inputs.nixpkgs.follows = "nixpkgs";
};

# modules
tmux-agent.homeModules.default
{
  programs.tmux-agent = {
    enable = true;
    claudePlugin.enable = true; # links it into ~/.claude/skills
    summaryStyle = "dots"; # coloured dots in the summary, not icons
  };
}
```

### Manual

```sh
cargo install --git https://github.com/jasonpeacock/tmux-agent
```

Then add this to `tmux.conf`, after any `set-hook` lines of your own:

```tmux
run-shell "tmux-agent init"
```

Install the Claude plugin from a Claude Code session:

```
/plugin install tmux-agent --marketplace jasonpeacock/tmux-agent
```

Or from a shell:

```sh
claude plugin install tmux-agent --marketplace jasonpeacock/tmux-agent
```

### Theme integration

tmux-agent publishes each window's state colour as `@agent_tab_color`, and an icon as `@agent_tab_icon`, for your theme to read. It never rewrites your theme's formats.

For catppuccin, choose one of two forms. Set its options in catppuccin's config, before it loads.

A coloured dot after the window name leaves the theme's own tab colours alone:

```tmux
set -g @catppuccin_window_text         "  #W#{?#{@agent_tab_color}, #[fg=#{@agent_tab_color}]●#[fg=#{@thm_fg}],}"
set -g @catppuccin_window_current_text "  #W#{?#{@agent_tab_color}, #[fg=#{@agent_tab_color}]●#[fg=#{@thm_fg}],}"
```

A coloured background behind the window name is easier to see. The name shows in dark text on the state colour. The number segment keeps the theme's active and inactive colours, so the active tab stays recognisable:

```tmux
set -g @catppuccin_window_text_color         "#{?#{@agent_tab_color},#{@agent_tab_color},#{@thm_surface_0}}"
set -g @catppuccin_window_current_text_color "#{?#{@agent_tab_color},#{@agent_tab_color},#{@thm_surface_1}}"
set -g @catppuccin_window_text               "#{?#{@agent_tab_color},#[fg=#{@thm_crust}],}  #W"
set -g @catppuccin_window_current_text       "#{?#{@agent_tab_color},#[fg=#{@thm_crust}],}  #W"
```

To follow your catppuccin flavour, set the state colours to its palette, e.g. `programs.tmux-agent.states.needs-input.color = "#{@thm_red}";` (likewise `done` with `@thm_green`, `working` with `@thm_yellow`). To show the icon in either form, put `#{@agent_tab_icon}` before `#W`, in place of the dot or beside the name. For a theme without inline colours, `@tmux-agent-tab-mode style` makes tmux-agent colour the tab itself.

Add the summary to `status-right` after you set it. Plain:

```tmux
set -ag status-right "#{@agent_summary}#(#{@agent_bin} reconcile)"
```

As a catppuccin module, hidden when no agent is active:

```tmux
set-environment -gh MODULE_NAME agents
set -g @catppuccin_agents_color "#{@thm_peach}"
set -g @catppuccin_agents_text " #{@agent_summary}"
source-file <catppuccin>/utils/status_module.conf
set -ag status-right "#{?#{@agent_summary},#{E:@catppuccin_status_agents},}#(#{@agent_bin} reconcile)"
```

The `reconcile` call prints nothing. Once per status refresh, it clears the state of Claude sessions that exited without saying so.

When you change these options in a running server, reloading the config does not remove options you deleted from it. Unset them with `tmux set -gu <option>` before you reload. `tmux-agent doctor` checks the setup.

## Known issue: red while an approved command runs

After you approve a permission prompt, the tab stays red until the approved command finishes, and only then turns yellow. For a long build or test run, that can take minutes.

There is no plan to fix this. Claude Code tells plugins when it asks for permission, and when a tool finishes, but sends nothing at the moment you approve. This was checked against every plugin hook, including the tool and permission-check chains. Every other plugin built on Claude's hooks has the same gap. The only workaround is to read the pane's screen and guess from Claude's dialog text when the prompt is gone. That breaks whenever the wording or layout changes, so tmux-agent does not do it.

## Alternatives considered

These tmux plugins were reviewed before tmux-agent was built. The features worth having, and the pitfalls to avoid, are collected in [docs/research.md](docs/research.md).

- [partner0/tmux-agent-status](https://github.com/partner0/tmux-agent-status)
- [ball6847/tmux-pane-agent-status](https://github.com/ball6847/tmux-pane-agent-status)
- [balvisio/tmux-agent-indicator](https://github.com/balvisio/tmux-agent-indicator)
- [Gentleman-Programming/gentle-agent-state](https://github.com/Gentleman-Programming/gentle-agent-state)
- [JanSuthacheeva/tmux-agent-status](https://github.com/JanSuthacheeva/tmux-agent-status)
- [jimmyliao/agent-status-tmux](https://github.com/jimmyliao/agent-status-tmux)
- [RatulMaharaj/tmux-agent-status](https://github.com/RatulMaharaj/tmux-agent-status)
- [BenRemer/tmux-agent-status](https://github.com/BenRemer/tmux-agent-status)
- [cburmeister/tmux-agent-state](https://github.com/cburmeister/tmux-agent-state)
- [jerriclynsjohn/tmux-agent-pulse](https://github.com/jerriclynsjohn/tmux-agent-pulse)
- [gerbenoostra/tmux-agent-status](https://github.com/gerbenoostra/tmux-agent-status)
- [dcryan/tmux-agentbar](https://github.com/dcryan/tmux-agentbar)
- [accessd/tmux-agent-indicator](https://github.com/accessd/tmux-agent-indicator)

## Development

```sh
git clone https://github.com/jasonpeacock/tmux-agent
cd tmux-agent
nix develop   # or use your own Rust toolchain and tmux >= 3.3
cargo test
claude plugin validate plugin
```

Flakes see only files tracked by git: `git add` new files before `nix build`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
