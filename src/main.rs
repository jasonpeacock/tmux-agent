//! tmux-agent: show Claude Code agent status on tmux window tabs.
//!
//! Only the pure state logic exists so far. The commands are stubs; see
//! docs/design.md, "Binary commands".

#[allow(dead_code)] // Used once the commands are implemented.
mod state;

use std::process::ExitCode;

const USAGE: &str = "\
usage: tmux-agent <command> [options]

commands:
  init        register tmux hooks and key bindings (run from tmux.conf)
  report      apply an event from the Claude plugin
  focus       mark a pane seen (tmux pane-focus-in hook)
  reconcile   clear dead state, recompute rollups and the summary
  jump        go to the most urgent agent pane
  pick        menu of agent panes
  list        print agent panes (--json for scripts)
  doctor      check the installation
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--version" | "-V") => {
            println!("tmux-agent {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("init" | "report" | "focus" | "reconcile" | "jump" | "pick") => {
            // Not yet implemented. These run from hooks and Claude, so they
            // must exit 0.
            ExitCode::SUCCESS
        }
        Some(cmd @ ("list" | "doctor")) => {
            eprintln!("tmux-agent: {cmd}: not yet implemented");
            ExitCode::FAILURE
        }
        Some("--help" | "-h") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
