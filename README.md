# tmux-agent

[![ci](https://github.com/jasonpeacock/tmux-agent/actions/workflows/ci.yml/badge.svg)](https://github.com/jasonpeacock/tmux-agent/actions/workflows/ci.yml)

Shows the status of Claude Code agents on tmux window tabs, so you can see which windows need you. Each tab gets a coloured marker for the most urgent agent in any of its panes:

**needs input** (red) **> done, not yet seen** (green) **> working** (yellow) **> idle** (nothing)

A pane counts as seen once you focus it. A status-right summary counts the agents in each state across all sessions.

> Status: early development. Tested on macOS and Linux with tmux 3.3a to 3.8, and against live Claude Code 2.1.316 sessions, both run in a pane and viewed through `claude attach`. Design and limits: [docs/design.md](docs/design.md).

## How it works

- A Claude Code plugin (`plugin/`) listens to Claude's in-process plugin API: turn start and end (including Esc cancels), permission prompts, questions, plan approval, and background work. It reports each change to the `tmux-agent` binary.
- The `tmux-agent` binary (Rust) keeps the state in tmux options. It rolls the panes of each window up into `@agent_tab_color` and `@agent_tab_icon`, which your theme reads when it renders. It also colours pane borders, publishes a status-right summary, and provides a jump key, an agent menu and `prefix w` badges.
- A session that Claude runs in a background daemon shows on every pane that runs `claude attach <id>` for it; tmux-agent finds those panes from the process table on each status refresh, and keeps its state across a tmux restart.

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

For catppuccin, a coloured dot after the window name leaves the theme's own tab colours alone, so the active tab stays recognisable. Set these in catppuccin's config, before it loads:

```tmux
set -g @catppuccin_window_text         "  #W#{?#{@agent_tab_color}, #[fg=#{@agent_tab_color}]●#[fg=#{@thm_fg}],}"
set -g @catppuccin_window_current_text "  #W#{?#{@agent_tab_color}, #[fg=#{@agent_tab_color}]●#[fg=#{@thm_fg}],}"
```

To follow your catppuccin flavour, set the state colours to its palette, e.g. `programs.tmux-agent.states.needs-input.color = "#{@thm_red}";` (likewise `done` with `@thm_green`, `working` with `@thm_yellow`). To show the icon instead of the dot, put `#{@agent_tab_icon}` before `#W`. For a theme without inline colours, `@tmux-agent-tab-mode style` makes tmux-agent colour the tab itself.

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
