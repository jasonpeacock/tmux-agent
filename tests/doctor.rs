//! `doctor` against private tmux servers.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const HOOKS: [&str; 9] = [
    "pane-focus-in",
    "pane-exited",
    "pane-died",
    "after-kill-pane",
    "window-layout-changed",
    "window-linked",
    "window-unlinked",
    "session-closed",
    "client-attached",
];

/// A hook command in the design's form, which quotes `@agent_bin` raw.
const RAW_HOOK: &str = r##"run-shell -b 'b="#{@agent_bin}"; [ -x "$b" ] && "$b" reconcile; true'"##;

const BIN: &str = env!("CARGO_BIN_EXE_tmux-agent");

/// A private server in its own directory; on drop the server is killed and
/// the directory removed.
struct Server {
    tmux: PathBuf,
    dir: PathBuf,
    socket: String,
}

impl Server {
    /// `None` when TMUX_AGENT_SKIP_TMUX_TESTS is set.
    fn start(name: &str) -> Option<Server> {
        if std::env::var_os("TMUX_AGENT_SKIP_TMUX_TESTS").is_some() {
            return None;
        }
        let tmux = find_tmux().expect(
            "tmux not found in TMUX_AGENT_TMUX or PATH; install tmux 3.3+ or set \
             TMUX_AGENT_SKIP_TMUX_TESTS=1 to skip these tests",
        );
        // Short: socket paths are limited to about 104 bytes.
        let dir = PathBuf::from(format!("/tmp/ta-dt-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = Server {
            tmux,
            socket: format!("{}/s", dir.display()),
            dir,
        };
        s.tmux(&["-f", "/dev/null", "new-session", "-d", "-s", "main"]);
        Some(s)
    }

    fn cmd(&self, bin: &Path) -> Command {
        let mut c = Command::new(bin);
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", std::env::var_os("HOME").unwrap_or_default())
            // A build sandbox user's login shell may be /usr/bin/false.
            .env("SHELL", "/bin/sh")
            .env("TMUX_AGENT_TMUX", &self.tmux);
        c
    }

    fn tmux(&self, args: &[&str]) -> String {
        let out = self
            .cmd(&self.tmux)
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "tmux {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn doctor(&self) -> Output {
        self.cmd(BIN.as_ref())
            .args(["doctor", "--socket", &self.socket])
            .output()
            .unwrap()
    }

    fn doctor_stdout(&self) -> String {
        String::from_utf8_lossy(&self.doctor().stdout).into_owned()
    }

    /// Registers every hook at our index with `cmd`.
    fn hooks(&self, cmd: &str) {
        for h in HOOKS {
            self.tmux(&["set-hook", "-g", &format!("{h}[4242]"), cmd]);
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self
            .cmd(&self.tmux)
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn find_tmux() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("TMUX_AGENT_TMUX").filter(|p| !p.is_empty()) {
        return Some(p.into());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("tmux"))
        .find(|p| p.is_file())
}

/// The report line about `what`, e.g. "FAIL focus-events off: ...".
fn line<'a>(stdout: &'a str, what: &str) -> &'a str {
    stdout
        .lines()
        .find(|l| !l.starts_with(' ') && l.contains(what))
        .unwrap_or_else(|| panic!("no line about {what:?} in:\n{stdout}"))
}

/// A process ID that no longer runs.
fn dead_pid() -> u32 {
    let mut c = Command::new("true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    pid
}

#[test]
fn bare_server_fails_focus_events_and_hooks() {
    let Some(s) = Server::start("bare") else {
        return;
    };
    let out = s.doctor();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{stdout}");
    assert!(line(&stdout, "tmux 3.").starts_with("ok "), "{stdout}");
    assert!(
        line(&stdout, "focus-events").starts_with("FAIL "),
        "{stdout}"
    );
    let hooks = line(&stdout, "hooks not registered at [4242]");
    assert!(hooks.starts_with("FAIL "), "{stdout}");
    for h in HOOKS {
        assert!(hooks.contains(h), "{h}: {stdout}");
    }
    assert!(
        line(&stdout, "no Claude session has reported yet").starts_with("warn "),
        "{stdout}"
    );
    assert!(!stdout.contains("reconciled"), "{stdout}");
}

#[test]
fn registered_hooks_and_focus_events_pass() {
    let Some(s) = Server::start("ok") else {
        return;
    };
    s.tmux(&["set-option", "-g", "focus-events", "on"]);
    s.hooks(RAW_HOOK);
    s.tmux(&[
        "bind-key",
        "-N",
        "tmux-agent: jump",
        "a",
        "display-message",
        "x",
    ]);
    s.tmux(&["bind-key", "A", "display-message", "theirs"]);

    let out = s.doctor();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(line(&stdout, "focus-events").starts_with("ok "), "{stdout}");
    assert!(
        line(&stdout, "hooks: 9 registered at [4242]").starts_with("ok "),
        "{stdout}"
    );
    assert!(!stdout.contains("hooks not registered"), "{stdout}");
    assert!(line(&stdout, "key prefix a").starts_with("ok "), "{stdout}");
    let pick = line(&stdout, "key prefix A");
    assert!(
        pick.starts_with("warn ")
            && pick.contains("bound to something else (display-message theirs)"),
        "{stdout}"
    );
    // @agent_bin was never published, so doctor still fails.
    assert!(line(&stdout, "@agent_bin").starts_with("FAIL "), "{stdout}");
    assert!(!out.status.success(), "{stdout}");
}

#[test]
fn session_hook_shadowing_ours_warns() {
    let Some(s) = Server::start("shadow") else {
        return;
    };
    s.hooks("run-shell -b '#{@agent_bin} reconcile'");
    s.tmux(&["new-session", "-d", "-s", "other"]);
    s.tmux(&[
        "set-hook",
        "-t",
        "other",
        "client-attached",
        "display-message hi",
    ]);
    s.tmux(&[
        "set-hook",
        "-w",
        "-t",
        "main:",
        "pane-exited",
        "display-message hi",
    ]);

    let stdout = s.doctor_stdout();
    let attached = line(&stdout, "hook client-attached is replaced");
    assert!(
        attached.starts_with("warn ") && attached.contains("(session other)"),
        "{stdout}"
    );
    let exited = line(&stdout, "hook pane-exited is replaced");
    assert!(
        exited.starts_with("warn ") && exited.contains("(session main)"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("hook pane-focus-in is replaced"),
        "{stdout}"
    );
}

#[test]
fn keys_resolve_spelling_and_notes() {
    let Some(s) = Server::start("keys") else {
        return;
    };
    s.tmux(&["set-option", "-g", "@tmux-agent-jump-key", "\""]);
    // A lone ";" argument would end the tmux command.
    s.tmux(&["set-option", "-g", "@tmux-agent-pick-key", r"\;"]);
    for (k, action) in [("\"", "jump"), ("\\;", "pick")] {
        s.tmux(&[
            "bind-key",
            "-N",
            &format!("tmux-agent: {action}"),
            k,
            "display-message",
            "x",
        ]);
    }
    let stdout = s.doctor_stdout();
    assert!(
        line(&stdout, "key prefix \": jump").starts_with("ok "),
        "{stdout}"
    );
    assert!(
        line(&stdout, "key prefix ;: pick").starts_with("ok "),
        "{stdout}"
    );

    s.tmux(&["set-option", "-g", "@tmux-agent-jump-key", "Meta-x"]);
    s.tmux(&["set-option", "-g", "@tmux-agent-pick-key", "F12"]);
    let stdout = s.doctor_stdout();
    assert!(
        line(&stdout, "\"Meta-x\" is not a key tmux knows").starts_with("warn "),
        "{stdout}"
    );
    assert!(
        line(&stdout, "key prefix F12 is not bound").starts_with("warn "),
        "{stdout}"
    );
}

#[test]
fn theme_and_status_through_option_references_pass() {
    let Some(s) = Server::start("theme") else {
        return;
    };
    // The catppuccin forms from design.md: the formats read user options
    // that read ours.
    s.tmux(&[
        "set-option",
        "-g",
        "@cat_agents",
        " #{@agent_summary}#(#{@agent_bin} reconcile)",
    ]);
    s.tmux(&["set-option", "-g", "status-right", "#{E:@cat_agents}"]);
    s.tmux(&[
        "set-option",
        "-g",
        "@thm_num",
        "#{?#{@agent_tab_color},#{@agent_tab_color},blue}",
    ]);
    s.tmux(&[
        "set-option",
        "-gw",
        "@thm_win",
        "#[bg=#{E:@thm_num}]#{@agent_tab_icon}#W",
    ]);
    for f in ["window-status-format", "window-status-current-format"] {
        s.tmux(&["set-option", "-gw", f, "#{E:@thm_win}"]);
    }
    s.tmux(&["new-window", "-d", "-t", "main:"]);
    let stdout = s.doctor_stdout();
    assert!(
        line(&stdout, "status-right shows @agent_summary").starts_with("ok "),
        "{stdout}"
    );
    assert!(
        line(
            &stdout,
            "window formats read @agent_tab_color and @agent_tab_icon"
        )
        .starts_with("ok "),
        "{stdout}"
    );
}

#[test]
fn current_format_only_warns() {
    let Some(s) = Server::start("cur") else {
        return;
    };
    s.tmux(&[
        "set-option",
        "-gw",
        "window-status-current-format",
        "#[bg=#{@agent_tab_color}]#{@agent_tab_icon}#W",
    ]);
    s.tmux(&["new-window", "-d", "-t", "main:"]);
    let stdout = s.doctor_stdout();
    let theme = line(&stdout, "window-status-format lacks @agent_tab_color");
    assert!(
        theme.starts_with("warn ") && theme.contains("in 2 of 2 windows"),
        "{stdout}"
    );
    assert!(!theme.contains("current-format"), "{stdout}");
}

#[test]
fn stale_state_is_reported_then_reconciled() {
    let Some(s) = Server::start("stale") else {
        return;
    };
    let pane = s.tmux(&["display-message", "-p", "-t", "main:", "#{pane_id}"]);
    let pane = pane.trim();
    let pid = dead_pid().to_string();
    for (o, v) in [
        ("@agent_pane_loop", "working"),
        ("@agent_pane_state", "working"),
        ("@agent_pane_pid", &pid),
        ("@agent_pane_proto", "1"),
    ] {
        s.tmux(&["set-option", "-p", "-t", pane, o, v]);
    }
    let win = s.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{window_id}",
        "-t",
        "main:",
    ]);
    let win = win.trim();
    s.tmux(&["set-option", "-w", "-t", win, "@agent_tab_color", "#f00"]);

    let stdout = s.doctor_stdout();
    let p = line(&stdout, &format!("pane {pane} "));
    assert!(
        p.starts_with("warn ") && p.contains(&format!("owner {pid} dead (stale)")),
        "{stdout}"
    );
    let w = line(&stdout, &format!("window {win} "));
    assert!(
        w.starts_with("warn ") && w.contains("no agent pane (stale)"),
        "{stdout}"
    );
    assert!(line(&stdout, "reconciled").starts_with("ok "), "{stdout}");

    let stdout = s.doctor_stdout();
    assert!(!stdout.contains("stale"), "{stdout}");
    assert!(!stdout.contains("reconciled"), "{stdout}");
    assert!(
        line(&stdout, "no Claude session has reported yet").starts_with("warn "),
        "{stdout}"
    );
}

#[test]
fn agent_bin_copies_and_unsafe_paths() {
    let Some(s) = Server::start("bin") else {
        return;
    };
    s.hooks(RAW_HOOK);
    s.tmux(&["set-option", "-g", "@agent_bin", BIN]);
    let stdout = s.doctor_stdout();
    assert_eq!(
        line(&stdout, "@agent_bin "),
        format!("ok   @agent_bin {BIN}"),
        "{stdout}"
    );

    let copy = s.dir.join("copy");
    std::fs::copy(BIN, &copy).unwrap();
    let copy = copy.to_str().unwrap();
    s.tmux(&["set-option", "-g", "@agent_bin", copy]);
    let stdout = s.doctor_stdout();
    let l = line(&stdout, "@agent_bin ");
    assert!(
        l.starts_with("ok ") && l.contains("another copy of tmux-agent"),
        "{stdout}"
    );

    // The raw hook form breaks on a `"` in the path.
    let dir = s.dir.join("a\"b");
    std::fs::create_dir_all(&dir).unwrap();
    let quoted = dir.join("tmux-agent");
    std::fs::copy(BIN, &quoted).unwrap();
    s.tmux(&["set-option", "-g", "@agent_bin", quoted.to_str().unwrap()]);
    let stdout = s.doctor_stdout();
    let l = line(&stdout, "@agent_bin ");
    assert!(
        l.starts_with("FAIL ") && l.contains("breaks the hook command"),
        "{stdout}"
    );
}
