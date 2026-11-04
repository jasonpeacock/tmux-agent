//! Private tmux servers for integration tests. See docs/design.md, "Testing".

// Each test binary uses a different subset of the helpers.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The default icons and colours: needs-input, done, working.
pub const ICONS: [&str; 3] = ["\u{f009e}", "\u{f058}", "\u{f110}"];
pub const COLORS: [&str; 3] = ["#f38ba8", "#a6e3a1", "#f9e2af"];

/// `$TMUX_AGENT_TMUX`, else the tmux on PATH. `None` only when
/// `TMUX_AGENT_SKIP_TMUX_TESTS` is set; a missing tmux fails the test, so a
/// CI job that lost tmux cannot pass with nothing tested.
pub fn tmux_bin() -> Option<PathBuf> {
    if std::env::var_os("TMUX_AGENT_SKIP_TMUX_TESTS").is_some() {
        return None;
    }
    if let Some(p) = std::env::var_os("TMUX_AGENT_TMUX").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let found = std::env::split_paths(&path)
        .map(|d| d.join("tmux"))
        .find(|p| p.is_file());
    assert!(
        found.is_some(),
        "tmux not found on PATH; set TMUX_AGENT_SKIP_TMUX_TESTS=1 to skip"
    );
    found
}

/// A command with a cleared environment plus PATH, HOME and `extra`. No
/// locale and no `$TMUX`, as a script outside tmux runs: the agent must pass
/// `-u` itself, and the harness's own tmux calls do too.
fn clean(prog: impl AsRef<std::ffi::OsStr>, extra: &[(&str, &str)]) -> Command {
    let mut c = Command::new(prog);
    c.env_clear();
    for k in ["PATH", "HOME"] {
        if let Some(v) = std::env::var_os(k) {
            c.env(k, v);
        }
    }
    // A build sandbox user's login shell may be /usr/bin/false.
    c.env("SHELL", "/bin/sh");
    c.envs(extra.iter().copied());
    c
}

/// The tmux-agent binary, set up as tmux would run it.
pub fn agent_cmd(tmux: &std::path::Path) -> Command {
    let mut c = clean(env!("CARGO_BIN_EXE_tmux-agent"), &[]);
    c.env("TMUX_AGENT_TMUX", tmux);
    c
}

/// A process ID that no longer runs.
pub fn dead_pid() -> u32 {
    let mut c = Command::new("true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    pid
}

/// A live process other than the test, killed on drop.
pub struct Proc(Child);

impl Proc {
    pub fn spawn() -> Proc {
        Proc(
            Command::new("sleep")
                .arg("300")
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    pub fn pid(&self) -> u32 {
        self.0.id()
    }

    /// Kills the process and waits for it, so its PID is dead.
    pub fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A control client (`tmux -C attach`), detached on drop.
pub struct Client {
    child: Child,
    pub name: String,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A private tmux server with one session, `main`, killed on drop.
pub struct Server {
    pub dir: PathBuf,
    pub socket: String,
    pub tmux: PathBuf,
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

impl Server {
    /// Starts a server, or prints a note and returns `None` to skip the test.
    pub fn new() -> Option<Server> {
        let Some(tmux) = tmux_bin() else {
            eprintln!("note: skipping: tmux not on PATH or TMUX_AGENT_SKIP_TMUX_TESTS set");
            return None;
        };
        // Short: a socket path holds at most 104 bytes.
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("{}/ta-{}-{n}", scratch(), std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s").to_str().unwrap().to_owned();
        let s = Server { dir, socket, tmux };
        s.tmux(&["new-session", "-d", "-s", "main", "-x", "200", "-y", "50"]);
        s.tmux(&["set", "-g", "focus-events", "on"]);
        Some(s)
    }

    fn cmd(&self) -> Command {
        let dir = self.dir.to_str().unwrap();
        let mut c = clean(&self.tmux, &[("SHELL", "/bin/sh"), ("TMUX_TMPDIR", dir)]);
        c.args(["-u", "-S", &self.socket, "-f", "/dev/null"]);
        c
    }

    /// Kills the server and starts a fresh one on the same socket.
    /// `kill-server` returns before the old server has exited, and a client
    /// that reaches it then fails with "server exited unexpectedly": wait
    /// until it no longer answers.
    pub fn restart(&self) {
        self.tmux(&["kill-server"]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self
            .cmd()
            .arg("list-sessions")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "old tmux server did not exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // The socket may linger briefly after the server stops answering.
        for _ in 0..50 {
            let out = self
                .cmd()
                .args(["new-session", "-d", "-s", "main", "-x", "200", "-y", "50"])
                .output()
                .unwrap();
            if out.status.success() {
                self.tmux(&["set", "-g", "focus-events", "on"]);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("could not start a new tmux server on {}", self.socket);
    }

    /// Runs one tmux command and returns its stdout, trimmed of the final
    /// newline. Panics when tmux fails.
    pub fn tmux(&self, args: &[&str]) -> String {
        let out = self.cmd().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "tmux {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let s = String::from_utf8(out.stdout).unwrap();
        s.strip_suffix('\n').unwrap_or(&s).to_owned()
    }

    /// The Claude config dir tmux-agent sees: a scratch one, so a test
    /// never reads the user's.
    pub fn claude_dir(&self) -> PathBuf {
        self.dir.join("claude")
    }

    /// Runs tmux-agent with `--socket` appended.
    pub fn agent(&self, args: &[&str]) -> Output {
        agent_cmd(&self.tmux)
            .env("CLAUDE_CONFIG_DIR", self.claude_dir())
            .args(args)
            .args(["--socket", &self.socket])
            .output()
            .unwrap()
    }

    /// Runs tmux-agent and asserts it exits 0 with no output.
    pub fn agent_ok(&self, args: &[&str]) {
        let out = self.agent(args);
        assert!(out.status.success(), "tmux-agent {args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "tmux-agent {args:?}: {out:?}");
    }

    /// `report <event>` for `pane` from `pid`, as the plugin sends it.
    pub fn report(&self, event: &str, pane: &str, pid: u32, extra: &[&str]) {
        let pid = pid.to_string();
        let mut args = vec![
            "report", event, "--proto", "1", "--pane", pane, "--pid", &pid,
        ];
        args.extend_from_slice(extra);
        self.agent_ok(&args);
    }

    pub fn reconcile(&self) {
        self.agent_ok(&["reconcile"]);
    }

    pub fn focus(&self, pane: &str) {
        self.agent_ok(&["focus", "--pane", pane]);
    }

    /// The format `#{name}` expanded for `target`. A pane target reads pane,
    /// then window, then global options.
    pub fn opt(&self, target: &str, name: &str) -> String {
        self.tmux(&["display", "-p", "-t", target, &format!("#{{{name}}}")])
    }

    /// A pane-level option alone, empty when unset.
    pub fn pane_opt(&self, pane: &str, name: &str) -> String {
        self.tmux(&["show-options", "-pqv", "-t", pane, name])
    }

    /// A window-level option alone, empty when unset.
    pub fn win_opt(&self, window: &str, name: &str) -> String {
        self.tmux(&["show-options", "-wqv", "-t", window, name])
    }

    pub fn summary(&self) -> String {
        self.tmux(&["show-options", "-gqv", "@agent_summary"])
    }

    /// Every pane id on the server.
    pub fn panes(&self) -> Vec<String> {
        self.tmux(&["list-panes", "-a", "-F", "#{pane_id}"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Every window id on the server, once each: grouped sessions and
    /// linked windows list a window more than once.
    pub fn windows(&self) -> Vec<String> {
        let mut w: Vec<String> = self
            .tmux(&["list-windows", "-a", "-F", "#{window_id}"])
            .lines()
            .map(str::to_owned)
            .collect();
        w.sort();
        w.dedup();
        w
    }

    /// Every pane's id and `@agent_pane_state`, once per pane id.
    pub fn pane_states(&self) -> Vec<(String, String)> {
        let rows = self.tmux(&["list-panes", "-a", "-F", "#{pane_id}\t#{@agent_pane_state}"]);
        let mut v: Vec<(String, String)> = rows
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(p, st)| (p.to_owned(), st.to_owned()))
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// The first pane of session `main`.
    pub fn first_pane(&self) -> String {
        self.tmux(&["display", "-p", "-t", "main:", "#{pane_id}"])
    }

    /// The window holding `pane`.
    pub fn window_of(&self, pane: &str) -> String {
        self.opt(pane, "window_id")
    }

    /// Splits `target` without selecting the new pane; returns its id.
    pub fn split(&self, target: &str) -> String {
        self.tmux(&["split-window", "-d", "-P", "-F", "#{pane_id}", "-t", target])
    }

    /// A new window in `session`, not selected; returns its pane id.
    pub fn new_window(&self, session: &str) -> String {
        let t = format!("{session}:");
        self.tmux(&["new-window", "-d", "-P", "-F", "#{pane_id}", "-t", &t])
    }

    /// Attaches a control client to `target` and waits until tmux lists it.
    pub fn attach(&self, target: &str) -> Client {
        let child = self
            .cmd()
            .args(["-C", "attach", "-t", target])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id().to_string();
        let mut client = Client {
            child,
            name: String::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while client.name.is_empty() {
            assert!(Instant::now() < deadline, "control client never attached");
            let rows = self.tmux(&["list-clients", "-F", "#{client_pid}\t#{client_name}"]);
            if let Some((_, name)) = rows
                .lines()
                .filter_map(|l| l.split_once('\t'))
                .find(|(p, _)| *p == pid)
            {
                client.name = name.to_owned();
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        client
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.cmd().arg("kill-server").output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Where tests put scratch dirs: /var/tmp, which is on disk, else /tmp (the
/// Nix build sandbox has no /var/tmp). Short: a socket path holds at most
/// 104 bytes.
pub fn scratch() -> &'static str {
    if std::path::Path::new("/var/tmp").is_dir() {
        "/var/tmp"
    } else {
        "/tmp"
    }
}
