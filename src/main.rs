//! tmux-agent: show Claude Code agent status on tmux window tabs.
//!
//! See docs/design.md, "Binary commands".

mod attach;
mod doctor;
mod engine;
mod init;
mod lock;
mod nav;
mod owner;
mod snapshot;
mod state;
mod tmux;

use engine::Report;
use lexopt::prelude::*;
use std::process::ExitCode;
use tmux::Tmux;

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

All commands take --socket PATH; the default is the server in $TMUX.
";

fn main() -> ExitCode {
    let mut parser = lexopt::Parser::from_env();
    let cmd = match parser.next() {
        Ok(Some(Value(v))) => v.string().unwrap_or_default(),
        Ok(Some(Long("version") | Short('V'))) => {
            println!("tmux-agent {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(Some(Long("help") | Short('h'))) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match cmd.as_str() {
        // These run from Claude and from tmux hooks: they never fail loudly.
        "report" | "focus" | "reconcile" | "init" | "jump" | "pick" => {
            if let Err(e) = quiet(&cmd, parser) {
                debug(&format!("{cmd}: {e}"));
            }
            ExitCode::SUCCESS
        }
        "list" | "doctor" => match loud(&cmd, parser) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(e) => {
                eprintln!("tmux-agent: {cmd}: {e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Arguments shared by the commands. Unknown flags are skipped, so an older
/// binary accepts a newer plugin's reports.
#[derive(Default)]
struct Args {
    socket: Option<String>,
    /// The client a key binding runs for (`--client #{client_name}`).
    client: String,
    json: bool,
    report: Report,
}

fn parse(mut parser: lexopt::Parser, first_value_is_event: bool) -> Result<Args, lexopt::Error> {
    let mut a = Args::default();
    let mut want_event = first_value_is_event;
    while let Some(arg) = parser.next()? {
        match arg {
            Long("socket") => a.socket = Some(parser.value()?.string()?),
            Long("pane") => a.report.pane = parser.value()?.string()?,
            Long("attached") => a.report.attached = true,
            Long("pid") => a.report.pid = parser.value()?.string()?.parse().ok(),
            Long("session") => a.report.session = Some(parser.value()?.string()?),
            Long("bg") => a.report.bg = parser.value()?.string()?.parse().unwrap_or(0),
            Long("proto") => a.report.proto = Some(parser.value()?.string()?),
            Long("plugin-version") => a.report.plugin = Some(parser.value()?.string()?),
            Long("client") => a.client = parser.value()?.string()?,
            Long("json") => a.json = true,
            Value(v) if want_event => {
                a.report.event = v.string()?;
                want_event = false;
            }
            Long(_) => {
                // Skip an unknown flag's value, if it has one attached.
                let _ = parser.optional_value();
            }
            _ => {}
        }
    }
    Ok(a)
}

fn quiet(cmd: &str, parser: lexopt::Parser) -> Result<(), Box<dyn std::error::Error>> {
    let a = parse(parser, cmd == "report")?;
    // A background session's engine runs outside tmux: with no --socket and
    // no $TMUX, it reports to the default server.
    let socket = match a.socket {
        None if cmd == "report" && a.report.attached && a.report.pane.is_empty() => {
            tmux::socket_or_default()
        }
        s => s,
    };
    let Some(tmux) = Tmux::new(socket) else {
        return Ok(());
    };
    match cmd {
        "report" => engine::report(&tmux, &a.report)?,
        "focus" => engine::focus(&tmux, &a.report.pane)?,
        "reconcile" => engine::reconcile(&tmux)?,
        "init" => init::run(&tmux)?,
        "jump" => nav::jump(&tmux, &a.client)?,
        "pick" => nav::pick(&tmux, &a.client)?,
        _ => {}
    }
    Ok(())
}

/// list and doctor: errors go to the user. Returns false on a failed check.
fn loud(cmd: &str, parser: lexopt::Parser) -> Result<bool, Box<dyn std::error::Error>> {
    let a = parse(parser, false)?;
    let tmux =
        Tmux::new(a.socket).ok_or("no tmux server: not inside tmux and no --socket given")?;
    Ok(match cmd {
        "list" => nav::list(&tmux, a.json).map(|()| true)?,
        _ => doctor::run(&tmux)?,
    })
}

/// Appends to `$TMUX_AGENT_DEBUG` when it names a file.
fn debug(msg: &str) {
    use std::io::Write;
    let Some(path) = std::env::var_os("TMUX_AGENT_DEBUG") else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{msg}");
    }
}
