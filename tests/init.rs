//! `init` on private tmux servers.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const HOOK_INDEX: &str = "[4242]";
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
const BIN: &str = env!("CARGO_BIN_EXE_tmux-agent");

/// A private tmux server, killed on drop.
struct Server {
    tmux: PathBuf,
    dir: PathBuf,
    sock: String,
    control: Option<Child>,
}

/// The tmux on PATH, or `None` when TMUX_AGENT_SKIP_TMUX_TESTS is set.
fn find_tmux() -> Option<PathBuf> {
    if std::env::var_os("TMUX_AGENT_SKIP_TMUX_TESTS").is_some() {
        return None;
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let found = std::env::split_paths(&path)
        .map(|d| d.join("tmux"))
        .find(|p| p.is_file());
    Some(found.expect("tmux is not on PATH; set TMUX_AGENT_SKIP_TMUX_TESTS=1 to skip"))
}

/// `s` as one shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl Server {
    /// A server with one session `t`, or `None` when tmux tests are skipped.
    fn start() -> Option<Server> {
        Server::start_in("", None)
    }

    /// A server whose directory name ends in `suffix`, loading the config
    /// that `conf` writes (given the directory) before its session exists.
    fn start_in(suffix: &str, conf: Option<&dyn Fn(&Path) -> String>) -> Option<Server> {
        static N: AtomicUsize = AtomicUsize::new(0);
        let Some(tmux) = find_tmux() else {
            eprintln!("skipping: TMUX_AGENT_SKIP_TMUX_TESTS is set");
            return None;
        };
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/ta-{}-{n}{suffix}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s").to_str().unwrap().to_owned();
        let s = Server {
            tmux,
            dir,
            sock,
            control: None,
        };
        let conf = match conf {
            Some(f) => {
                let p = s.dir.join("tmux.conf");
                std::fs::write(&p, f(&s.dir)).unwrap();
                p
            }
            None => PathBuf::from("/dev/null"),
        };
        let mut c = s.cmd();
        if conf != Path::new("/dev/null") {
            // init runs from the config.
            c.env("TMUX_AGENT_DEBUG", s.dir.join("debug"));
        }
        let out = c
            .args(["-u", "-S", &s.sock, "-f", conf.to_str().unwrap()])
            .args(["new-session", "-d", "-s", "t", "-x", "80", "-y", "24"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        Some(s)
    }

    /// A command with a clean environment: no `$TMUX` and no locale.
    fn env(&self, c: &mut Command) {
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", std::env::var_os("HOME").unwrap_or_default())
            // A build sandbox user's login shell may be /usr/bin/false.
            .env("SHELL", "/bin/sh")
            .env("TMUX_TMPDIR", &self.dir)
            .env("TMUX_AGENT_TMUX", &self.tmux);
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.tmux);
        self.env(&mut c);
        c
    }

    /// Runs tmux; `None` when it fails.
    fn try_tmux(&self, args: &[&str]) -> Option<String> {
        let out = self
            .cmd()
            .args(["-u", "-S", &self.sock, "-f", "/dev/null"])
            .args(args)
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn tmux(&self, args: &[&str]) -> String {
        self.try_tmux(args)
            .unwrap_or_else(|| panic!("tmux {args:?} failed"))
    }

    /// The binary, logging errors to `debug_log`. Hooks get no log: one
    /// still running when the test ends would write into a removed directory.
    fn agent_cmd(&self) -> Command {
        let mut c = Command::new(BIN);
        self.env(&mut c);
        c.env("TMUX_AGENT_DEBUG", self.dir.join("debug"));
        c
    }

    fn agent(&self, args: &[&str]) {
        let st = self
            .agent_cmd()
            .args(args)
            .args(["--socket", &self.sock])
            .status()
            .unwrap();
        assert!(st.success(), "tmux-agent {args:?}");
    }

    fn init(&self) {
        self.agent(&["init"]);
    }

    fn set(&self, opt: &str, value: &str) {
        self.tmux(&["set-option", "-g", opt, value]);
    }

    fn opt(&self, opt: &str) -> String {
        self.tmux(&["show-options", "-gqv", opt])
            .trim_end()
            .to_owned()
    }

    /// All global hooks: session and server hooks, then window and pane hooks.
    fn hooks(&self) -> String {
        self.tmux(&["show-hooks", "-g"]) + &self.tmux(&["show-hooks", "-gw"])
    }

    fn assert_hooks(&self) {
        let hooks = self.hooks();
        assert_eq!(hooks.matches(HOOK_INDEX).count(), HOOKS.len(), "{hooks}");
    }

    /// The binding of `key`, empty when unbound.
    fn key(&self, key: &str) -> String {
        self.try_tmux(&["list-keys", "-T", "prefix", "--", key])
            .unwrap_or_default()
    }

    fn notes(&self) -> String {
        self.try_tmux(&["list-keys", "-N", "-T", "prefix"])
            .unwrap_or_default()
    }

    /// The canonical socket path, as init writes it into hooks and keys.
    fn canonical_sock(&self) -> String {
        std::fs::canonicalize(&self.sock)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    fn attach_control(&mut self) {
        let child = self
            .cmd()
            .args(["-u", "-S", &self.sock, "-C", "attach", "-t", "t"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.control = Some(child);
        let start = Instant::now();
        while self.tmux(&["list-clients", "-F", "x"]).is_empty() {
            assert!(start.elapsed() < Duration::from_secs(5), "no client");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn pane_option(&self, pane: &str, opt: &str) -> String {
        let f = format!("#{{{opt}}}");
        self.tmux(&["display-message", "-p", "-t", pane, &f])
            .trim_end()
            .to_owned()
    }

    /// Waits for `pane`'s `opt` to read `want`: hooks run in the background.
    fn wait_for(&self, pane: &str, opt: &str, want: &str) {
        wait(
            || self.pane_option(pane, opt) == want,
            &format!("{pane} {opt}"),
        );
    }

    fn split(&self) -> String {
        let p = self.tmux(&["split-window", "-d", "-t", "t", "-P", "-F", "#{pane_id}"]);
        p.trim_end().to_owned()
    }

    /// Errors from `agent_cmd` runs, and from init run by the config.
    fn debug_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("debug")).unwrap_or_default()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut c) = self.control.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = self
            .cmd()
            .args(["-u", "-S", &self.sock, "kill-server"])
            .stderr(Stdio::null())
            .status();
        // A background hook may still write its lock file: retry.
        for _ in 0..10 {
            if std::fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn wait(mut done: impl FnMut() -> bool, what: &str) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn registers_hooks_options_and_focus_events() {
    let Some(s) = Server::start() else { return };
    s.set("focus-events", "off");
    s.init();
    let hooks = s.hooks();
    for h in HOOKS {
        let line = hooks
            .lines()
            .find(|l| l.starts_with(&format!("{h}{HOOK_INDEX} ")))
            .unwrap_or_else(|| panic!("{h} not registered: {hooks}"));
        assert!(line.contains("run-shell -b"), "{line}");
        let cmd = if h == "pane-focus-in" {
            "focus --pane #{hook_pane}"
        } else {
            "reconcile --socket"
        };
        assert!(line.contains(cmd), "{line}");
    }
    assert_eq!(s.opt("focus-events"), "on");
    assert_eq!(s.opt("@agent_bin"), BIN);
    assert_eq!(s.opt("@agent_version"), env!("CARGO_PKG_VERSION"));
    assert_eq!(s.debug_log(), "");
}

#[test]
fn reinit_is_idempotent_and_keeps_other_hooks() {
    let Some(s) = Server::start() else { return };
    s.tmux(&[
        "set-hook",
        "-g",
        "pane-exited[0]",
        "display-message -p mine",
    ]);
    s.tmux(&[
        "set-hook",
        "-g",
        "client-attached[1]",
        "display-message -p mine",
    ]);
    s.set("@tmux-agent-bin", "/opt/ta");
    s.init();
    let first = s.hooks();
    s.init();
    let hooks = s.hooks();
    assert_eq!(first, hooks);
    s.assert_hooks();
    assert!(
        hooks.contains("pane-exited[0] display-message -p mine"),
        "{hooks}"
    );
    assert!(
        hooks.contains("client-attached[1] display-message -p mine"),
        "{hooks}"
    );
    assert_eq!(s.opt("@agent_bin"), "/opt/ta");
    assert_eq!(s.notes().matches("tmux-agent: jump").count(), 1);
}

#[test]
fn binds_keys_with_notes() {
    let Some(s) = Server::start() else { return };
    s.init();
    let notes = s.notes();
    for n in ["jump", "pick", "choose-tree"] {
        assert!(notes.contains(&format!("tmux-agent: {n}")), "{notes}");
    }
    assert!(s.key("a").contains("jump --client #{q:client_name}"));
    assert!(s.key("A").contains("pick --client #{q:client_name}"));
    let w = s.key("w");
    assert!(w.contains("choose-tree -Zw -F"), "{w}");
    assert!(w.contains("#{@agent_tab_icon}#{window_name}"), "{w}");
}

#[test]
fn does_not_steal_a_user_key() {
    let Some(s) = Server::start() else { return };
    s.tmux(&["bind-key", "-T", "prefix", "A", "display-message", "mine"]);
    s.init();
    s.init();
    assert!(s.key("A").contains("display-message mine"));
    assert!(!s.notes().contains("tmux-agent: pick"));
    assert!(s.notes().contains("tmux-agent: jump"));
}

#[test]
fn leaves_a_user_key_alone_after_unbind_all() {
    let Some(s) = Server::start() else { return };
    // No noted keys at all: tmux 3.3 `list-keys -aN` would print "aselect…".
    s.tmux(&["unbind-key", "-a", "-T", "prefix"]);
    s.tmux(&["bind-key", "a", "display-message", "x"]);
    s.init();
    assert!(s.key("a").contains("display-message x"), "{}", s.key("a"));
    let notes = s.notes();
    assert!(!notes.contains("tmux-agent: jump"), "{notes}");
    assert!(notes.contains("tmux-agent: pick"), "{notes}");
    assert!(notes.contains("tmux-agent: choose-tree"), "{notes}");
}

#[test]
fn binds_into_a_missing_prefix_table() {
    let Some(s) = Server::start() else { return };
    s.tmux(&["unbind-key", "-a", "-T", "prefix"]);
    s.init();
    let notes = s.notes();
    for n in ["jump", "pick", "choose-tree"] {
        assert!(notes.contains(&format!("tmux-agent: {n}")), "{notes}");
    }
}

#[test]
fn other_spellings_do_not_steal_user_keys() {
    let Some(s) = Server::start() else { return };
    s.tmux(&[
        "bind-key",
        "-T",
        "prefix",
        "C-M-a",
        "display-message",
        "cma",
    ]);
    s.tmux(&[
        "bind-key",
        "-T",
        "prefix",
        "Enter",
        "display-message",
        "ent",
    ]);
    let space = s.key("Space");
    assert!(space.contains("next-layout"), "{space}");
    s.set("@tmux-agent-jump-key", "space");
    s.set("@tmux-agent-pick-key", "M-C-a");
    s.init();
    assert_eq!(s.key("Space"), space);
    assert!(s.key("C-M-a").contains("display-message cma"));
    s.set("@tmux-agent-pick-key", "enter");
    s.init();
    assert!(s.key("Enter").contains("display-message ent"));
    let notes = s.notes();
    assert!(!notes.contains("tmux-agent: jump"), "{notes}");
    assert!(!notes.contains("tmux-agent: pick"), "{notes}");

    // Our own key under another spelling is kept, not unbound.
    s.set("@tmux-agent-pick-key", "M-C-b");
    s.init();
    s.init();
    assert!(
        s.key("C-M-b").contains("pick --client"),
        "{}",
        s.key("C-M-b")
    );
}

#[test]
fn bad_key_options_fail_alone() {
    let Some(s) = Server::start() else { return };
    s.set("focus-events", "off");
    s.set("@agent_summary", "junk");
    for bad in ["-x", "foo"] {
        s.set("@tmux-agent-jump-key", bad);
        s.init();
        s.assert_hooks();
        assert_eq!(s.opt("focus-events"), "on");
        assert_eq!(s.opt("@agent_bin"), BIN);
        // reconcile ran: it rewrites the summary.
        assert_eq!(s.opt("@agent_summary"), "", "{bad}");
        assert!(s.key("A").contains("pick --client"), "{bad}");
        assert!(s.key("w").contains("choose-tree -Zw -F"), "{bad}");
        s.set("@agent_summary", "junk");
    }
}

#[test]
fn semicolon_keys_bind_and_move() {
    let Some(s) = Server::start() else { return };
    // As argv, a trailing ";" is a separator; "\;" is a literal ";".
    s.set("@tmux-agent-pick-key", "M-\\;");
    assert_eq!(s.opt("@tmux-agent-pick-key"), "M-;");
    s.init();
    s.init();
    assert!(s.key("M-\\;").contains("pick --client"), "{}", s.notes());
    assert_eq!(s.notes().matches("tmux-agent: pick").count(), 1);
    s.tmux(&["set-option", "-gu", "@tmux-agent-pick-key"]);
    s.init();
    assert_eq!(s.key("M-\\;"), "");
    assert!(s.key("A").contains("pick --client"));
    assert!(s.key("\\;").contains("last-pane"));
}

#[test]
fn moves_a_changed_key_and_restores_prefix_w() {
    let Some(s) = Server::start() else { return };
    s.init();
    s.set("@tmux-agent-jump-key", "j");
    s.set("@tmux-agent-choose-tree", "off");
    s.init();
    assert!(s.key("j").contains("jump --client"));
    assert_eq!(s.key("a"), "");
    let w = s.key("w");
    assert!(w.contains("choose-tree -Zw") && !w.contains("-F"), "{w}");
    assert!(s.notes().contains("Choose a window from a list"));
}

#[test]
fn leaves_a_user_prefix_w_alone_when_off() {
    let Some(s) = Server::start() else { return };
    s.set("@tmux-agent-choose-tree", "off");
    s.tmux(&["bind-key", "-T", "prefix", "w", "display-message", "mine"]);
    s.init();
    assert!(s.key("w").contains("display-message mine"));
}

#[test]
fn focus_hook_marks_pane_seen() {
    let Some(mut s) = Server::start() else { return };
    let other = s.split();
    s.init();
    s.agent(&["report", "done", "--pane", &other]);
    assert_eq!(s.pane_option(&other, "@agent_pane_state"), "done");

    // Attaching focuses the active pane, not this one.
    s.attach_control();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(s.pane_option(&other, "@agent_pane_state"), "done");

    s.tmux(&["select-pane", "-t", &other]);
    s.wait_for(&other, "@agent_pane_state", "idle");
}

#[test]
fn odd_socket_and_binary_paths_still_run() {
    let Some(mut s) = Server::start_in(" q'\"#$x", None) else {
        return;
    };
    // A wrapper at an odd path that logs its arguments, then runs the binary.
    let bin_dir = s.dir.join("b '\"#$y");
    std::fs::create_dir(&bin_dir).unwrap();
    let wrapper = bin_dir.join("tmux-agent");
    let log = s.dir.join("log");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexec {} \"$@\"\n",
        sh_quote(log.to_str().unwrap()),
        sh_quote(BIN)
    );
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let wrapper = wrapper.to_str().unwrap();
    let read_log = || std::fs::read_to_string(&log).unwrap_or_default();

    let other = s.split();
    s.set("@tmux-agent-bin", wrapper);
    s.init();
    // tmux 3.4 prints `$` as `\$` in command output; the stored value, and so
    // what the hooks expand, is unchanged.
    assert_eq!(s.opt("@agent_bin").replace("\\$", "$"), wrapper);
    s.agent(&["report", "done", "--pane", &other]);
    s.attach_control();
    s.tmux(&["select-pane", "-t", &other]);
    s.wait_for(&other, "@agent_pane_state", "idle");
    let sock = s.canonical_sock();
    let want = format!("focus --pane {other} --socket {sock}");
    assert!(read_log().lines().any(|l| l == want), "{}", read_log());

    // Replay the jump binding's command, as a key press would run it.
    let mut key = s.key("a");
    // tmux 3.4 prints `$` as `\$` in list-keys output too.
    if s.tmux(&["display-message", "-p", "#{version}"]).trim() == "3.4" {
        key = key.replace("\\$", "$");
    }
    let cmd = &key[key.find("run-shell").expect(&key)..];
    let conf = s.dir.join("key.conf");
    std::fs::write(&conf, cmd).unwrap();
    s.tmux(&["source-file", conf.to_str().unwrap()]);
    let jumped =
        |l: &str| l.starts_with("jump --client") && l.ends_with(&format!("--socket {sock}"));
    wait(|| read_log().lines().any(jumped), "jump in log");
    assert_eq!(s.debug_log(), "");
}

#[test]
fn relative_socket_is_written_absolute() {
    let Some(s) = Server::start() else { return };
    let st = s
        .agent_cmd()
        .current_dir(&s.dir)
        .args(["init", "--socket", "s"])
        .status()
        .unwrap();
    assert!(st.success());
    let want = format!("--socket '{}'", s.canonical_sock());
    let hooks = s.hooks();
    assert_eq!(hooks.matches(&want).count(), HOOKS.len(), "{hooks}");
    assert!(s.key("a").contains(&want), "{}", s.key("a"));
}

#[test]
fn bare_bin_name_is_resolved_through_path() {
    let Some(s) = Server::start() else { return };
    let path_dir = s.dir.join("pathbin");
    std::fs::create_dir(&path_dir).unwrap();
    std::os::unix::fs::symlink(BIN, path_dir.join("ta-bare")).unwrap();
    let path = std::env::join_paths(
        std::iter::once(path_dir.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let init = || {
        let st = s
            .agent_cmd()
            .env("PATH", &path)
            .args(["init", "--socket", &s.sock])
            .status()
            .unwrap();
        assert!(st.success());
    };
    s.set("@tmux-agent-bin", "ta-bare");
    init();
    assert_eq!(
        s.opt("@agent_bin"),
        path_dir.join("ta-bare").to_str().unwrap()
    );
    s.set("@tmux-agent-bin", "ta-nowhere");
    init();
    assert_eq!(s.opt("@agent_bin"), BIN);
}

#[test]
fn init_from_config_before_any_session() {
    let conf = |_: &Path| format!("run-shell \"{BIN} init\"\n");
    let Some(s) = Server::start_in("", Some(&conf)) else {
        return;
    };
    s.assert_hooks();
    assert!(s.key("a").contains("jump --client"));
    assert!(s.key("w").contains("choose-tree -Zw -F"));
    assert_eq!(s.opt("@agent_bin"), BIN);
    assert_eq!(s.debug_log(), "");
}

#[test]
fn missing_binary_stays_quiet() {
    let Some(mut s) = Server::start() else { return };
    let other = s.split();
    s.set("@tmux-agent-bin", "/nonexistent/tmux-agent");
    s.init();
    s.attach_control();
    s.tmux(&["select-pane", "-t", &other]);
    s.tmux(&["select-pane", "-t", "t:0.0"]);
    // Nothing to wait for: give the background hooks time to fail.
    std::thread::sleep(Duration::from_millis(500));
    let modes = s.tmux(&["list-panes", "-a", "-F", "#{pane_in_mode}"]);
    assert!(modes.lines().all(|m| m == "0"), "{modes}");
}
