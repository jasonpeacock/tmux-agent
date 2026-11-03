# tmux-agent

[![ci](https://github.com/jasonpeacock/tmux-agent/actions/workflows/ci.yml/badge.svg)](https://github.com/jasonpeacock/tmux-agent/actions/workflows/ci.yml)

Shows the status of Claude Code agents on tmux window tabs, so you can see which windows need you. Each tab is recoloured, and gets an icon, by the most urgent agent in any of its panes:

**needs input > done (not yet seen) > working > idle**

Idle shows nothing. A pane counts as seen once you focus it.

> Status: early development. Everything in the design is implemented and tested on macOS with tmux 3.8 and 3.3a, and against a live Claude Code 2.1.316 session; Linux is untested so far. Known limits are in [docs/design.md](docs/design.md).

## How it works

- A Claude Code plugin (`plugin/`) listens to Claude's in-process plugin API: turn start and end (including Esc cancels), permission prompts, questions, plan approval, and background work. It reports each change to the `tmux-agent` binary.
- The `tmux-agent` binary (Rust) keeps the state in tmux options. It rolls the panes of each window up into `@agent_tab_color` and `@agent_tab_icon`, which your theme reads when it renders. It also colours pane borders, publishes a status-right summary, and provides a jump key, an agent menu and `prefix w` badges.

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
  programs.tmux-agent.enable = true;
  programs.tmux-agent.claudePlugin.enable = true; # needs programs.claude-code.enable
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

For catppuccin, replace your window lines in catppuccin's config with:

```tmux
set -g @catppuccin_window_number_color         "#{?#{@agent_tab_color},#{@agent_tab_color},#{@thm_overlay_2}}"
set -g @catppuccin_window_current_number_color "#{?#{@agent_tab_color},#{@agent_tab_color},#{@thm_mauve}}"
set -g @catppuccin_window_text         " #{@agent_tab_icon}#W"
set -g @catppuccin_window_current_text " #{@agent_tab_icon}#W"
```

For other themes, set `@tmux-agent-tab-mode style`. Add the summary to `status-right` after you set it:

```tmux
set -ag status-right "#{@agent_summary}#(#{@agent_bin} reconcile)"
```

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
