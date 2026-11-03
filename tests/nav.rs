//! jump, pick and list against private tmux servers.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_tmux-agent");

/// `$TMUX_AGENT_TMUX`, else tmux on PATH; `None` to skip the tests.
fn tmux_bin() -> Option<PathBuf> {
    if std::env::var_os("TMUX_AGENT_SKIP_TMUX_TESTS").is_some() {
        return None;
    }
    if let Some(p) = std::env::var_os("TMUX_AGENT_TMUX").filter(|p| !p.is_empty()) {
        return Some(p.into());
    }
    let found = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("tmux"))
        .find(|p| p.is_file());
    assert!(
        found.is_some(),
        "tmux not found on PATH: install it, or set TMUX_AGENT_SKIP_TMUX_TESTS=1 to skip"
    );
    found
}

/// A private tmux server in a fresh short directory.
struct Server {
    tmux: PathBuf,
    dir: PathBuf,
    socket: String,
}

impl Server {
    /// Starts a server with session `a` (window 0, sized 100x30), or `None`
    /// to skip the test.
    fn new() -> Option<Server> {
        static N: AtomicU32 = AtomicU32::new(0);
        let tmux = tmux_bin()?;
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/ta-{}-nav{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s").to_str().unwrap().to_owned();
        let s = Server { tmux, dir, socket };
        s.tmux(&[
            "new-session",
            "-d",
            "-s",
            "a",
            "-n",
            "zero",
            "-x",
            "100",
            "-y",
            "30",
        ]);
        s.tmux(&["set-option", "-g", "focus-events", "on"]);
        Some(s)
    }

    fn env(&self, c: &mut Command) {
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", std::env::var_os("HOME").unwrap_or_default())
            // A build sandbox user's login shell may be /usr/bin/false.
            .env("SHELL", "/bin/sh")
            .env("TMUX_TMPDIR", &self.dir)
            .env("TMUX_AGENT_TMUX", &self.tmux);
    }

    fn tmux_cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.tmux);
        self.env(&mut c);
        // -u: with no UTF-8 locale, tmux turns tabs in -F output into '_'.
        c.args(["-u", "-S", &self.socket, "-f", "/dev/null"])
            .args(args);
        c
    }

    fn tmux(&self, args: &[&str]) -> String {
        let out = self.tmux_cmd(args).output().unwrap();
        assert!(out.status.success(), "tmux {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn agent(&self, args: &[&str]) -> Output {
        let mut c = Command::new(BIN);
        self.env(&mut c);
        c.args(args)
            .args(["--socket", &self.socket])
            .output()
            .unwrap()
    }

    /// Reports `event` for `pane` as this test process.
    fn report(&self, pane: &str, event: &str) {
        let pid = std::process::id().to_string();
        let out = self.agent(&["report", event, "--pane", pane, "--pid", &pid]);
        assert!(out.status.success());
    }

    fn pane(&self, target: &str) -> String {
        self.tmux(&["display-message", "-p", "-t", target, "#{pane_id}"])
            .trim()
            .to_owned()
    }

    fn set_since(&self, pane: &str, since: u64) {
        self.tmux(&[
            "set-option",
            "-p",
            "-t",
            pane,
            "@agent_pane_since",
            &since.to_string(),
        ]);
    }

    /// The pane `client` is looking at.
    fn client_pane(&self, client: &str) -> String {
        self.tmux(&["display-message", "-p", "-c", client, "#{pane_id}"])
            .trim()
            .to_owned()
    }

    /// Waits for a new client whose flags match `pred`; returns its name.
    fn wait_client(&self, pred: impl Fn(&str) -> bool) -> String {
        let mut found = None;
        wait(|| {
            found = self
                .tmux(&["list-clients", "-F", "#{client_name}\t#{client_flags}"])
                .lines()
                .filter_map(|l| l.split_once('\t'))
                .find(|(_, f)| pred(f))
                .map(|(n, _)| n.to_owned());
            found.is_some()
        });
        found.unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.tmux_cmd(&["kill-server"]).output();
        let _ = std::fs::remove_file(format!("{}.tmux-agent.lock", self.socket));
        let _ = std::fs::remove_file(&self.socket);
        // Empty directories a test made for pane cwds.
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let _ = std::fs::remove_dir(e.path());
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

fn wait(mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < end, "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Output collected from a client in the background.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Sink {
    fn drain(&self, mut r: impl Read + Send + 'static) {
        let s = self.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = r.read(&mut buf) {
                if n == 0 {
                    break;
                }
                s.0.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
    }

    fn text(&self) -> String {
        self.text_from(0)
    }

    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    /// The output after the first `from` bytes.
    fn text_from(&self, from: usize) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()[from..]).into_owned()
    }
}

/// `tmux -C attach`, with stdin held open.
struct ControlClient {
    child: Child,
    _stdin: ChildStdin,
    out: Sink,
}

impl ControlClient {
    fn attach(s: &Server, session: &str) -> ControlClient {
        let mut child = s
            .tmux_cmd(&["-C", "attach", "-t", session])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = Sink::default();
        out.drain(child.stdout.take().unwrap());
        let stdin = child.stdin.take().unwrap();
        ControlClient {
            child,
            _stdin: stdin,
            out,
        }
    }
}

impl ControlClient {
    /// Waits for `client` (this one) to be shown `text`: tmux 3.4+ sends
    /// control clients a `%message`; 3.3 only logs it.
    fn wait_message(&self, s: &Server, client: &str, text: &str) {
        let logged = format!("{client} message: {text}");
        wait(|| {
            self.out.text().contains(&format!("%message {text}"))
                || s.tmux(&["show-messages"]).contains(&logged)
        });
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A normal client on a pseudo-terminal of `COLS` columns, as a user would have.
const COLS: u16 = 100;

struct TtyClient {
    child: Child,
    master: File,
    out: Sink,
}

impl TtyClient {
    fn attach(s: &Server, session: &str, rows: u16) -> TtyClient {
        static PTSNAME: Mutex<()> = Mutex::new(());
        // SAFETY: plain libc pty calls on a descriptor we own; ptsname's
        // static buffer is guarded by PTSNAME.
        let (master, slave) = unsafe {
            let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(m >= 0);
            assert_eq!(libc::grantpt(m), 0);
            assert_eq!(libc::unlockpt(m), 0);
            let name = {
                let _g = PTSNAME.lock().unwrap();
                std::ffi::CStr::from_ptr(libc::ptsname(m))
                    .to_str()
                    .unwrap()
                    .to_owned()
            };
            (File::from_raw_fd(m), name)
        };
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&slave)
            .unwrap();
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: COLS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads a winsize from a valid pointer.
        assert_eq!(
            unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCSWINSZ, &ws) },
            0
        );
        let child = s
            .tmux_cmd(&["attach", "-t", session])
            .env("TERM", "xterm-256color")
            .stdin(tty.try_clone().unwrap())
            .stdout(tty.try_clone().unwrap())
            .stderr(tty)
            .spawn()
            .unwrap();
        let out = Sink::default();
        out.drain(master.try_clone().unwrap());
        TtyClient { child, master, out }
    }

    /// The output from `from` on, without escape sequences.
    fn text_from(&self, from: usize) -> String {
        let raw = self.out.text_from(from);
        let mut out = String::new();
        let mut it = raw.chars().peekable();
        while let Some(c) = it.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            // CSI: ESC [ params final; anything else: ESC and one char.
            if it.next_if_eq(&'[').is_some() {
                while it.next().is_some_and(|c| !('@'..='~').contains(&c)) {}
            } else {
                it.next();
            }
        }
        out
    }

    fn press(&mut self, keys: &str) {
        self.master.write_all(keys.as_bytes()).unwrap();
    }
}

impl Drop for TtyClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Session a: window 0 (no agent), window 1 with two panes. Session b: a
/// two-pane window and a second window. Returns the agent panes in the
/// order nav should rank them, and the non-agent pane.
fn agents_setup(s: &Server) -> ([String; 5], String) {
    s.tmux(&["new-window", "-d", "-t", "a:1", "-n", "one"]);
    s.tmux(&["split-window", "-d", "-t", "a:1"]);
    s.tmux(&["new-session", "-d", "-s", "b", "-n", "q\"b\\s"]);
    s.tmux(&["split-window", "-d", "-t", "b:0"]);
    s.tmux(&["new-window", "-d", "-t", "b:1", "-n", "idle"]);
    let [p0, p1, p2, p3, p4, p5] =
        ["a:0.0", "a:1.0", "a:1.1", "b:0.0", "b:0.1", "b:1.0"].map(|t| s.pane(t));
    s.report(&p1, "done");
    s.report(&p2, "ask");
    s.report(&p3, "ask");
    s.report(&p4, "working");
    s.report(&p5, "claim");
    s.set_since(&p1, 50);
    s.set_since(&p2, 200);
    s.set_since(&p3, 100);
    ([p3, p2, p1, p4, p5], p0)
}

#[test]
fn list_ranks_agent_panes() {
    let Some(s) = Server::new() else { return };
    let (want, _) = agents_setup(&s);

    let out = s.agent(&["list"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let rows: Vec<Vec<&str>> = text.lines().map(|l| l.split('\t').collect()).collect();
    let ids: Vec<&str> = rows.iter().map(|r| r[0]).collect();
    assert_eq!(ids, want.iter().map(String::as_str).collect::<Vec<_>>());
    let states: Vec<&str> = rows.iter().map(|r| r[2]).collect();
    assert_eq!(
        states,
        ["needs-input", "needs-input", "done", "working", "idle"]
    );
    assert_eq!(rows[0][1], "b:0.0");
    // tmux 3.8 stores the backslash escaped, as `q"b\\s`.
    let name = s.tmux(&["display-message", "-p", "-t", "b:0", "#{window_name}"]);
    let name = name.trim_end_matches('\n');
    assert!(name.starts_with("q\"b\\"));
    assert_eq!(rows[0][5], name);
    assert_eq!(rows[2][1..2], ["a:1.0"]);
    assert_eq!(rows[2][5], "one");
    assert!(rows[2][3].parse::<u64>().unwrap() > 1_000_000, "since 50");
    assert!(rows[3][3].parse::<u64>().unwrap() < 60);
    assert_eq!(rows[3][4], "0");
    assert!(rows.iter().all(|r| r.len() == 7 && !r[6].is_empty()));

    let out = s.agent(&["list", "--json"]);
    assert!(out.status.success());
    let json = String::from_utf8(out.stdout).unwrap();
    assert!(json.starts_with(&format!(
        r#"[{{"pane_id":"{}","target":"b:0.0","state":"needs-input","seconds":"#,
        want[0]
    )));
    let name = name.replace('\\', r"\\").replace('"', r#"\""#);
    assert!(json.contains(&format!(r#""bg":0,"window":"{name}","cwd":""#)));
    assert_eq!(json.matches("\"pane_id\"").count(), 5);
    assert!(json.ends_with("}]\n"));
}

#[test]
fn list_with_no_agents_succeeds() {
    let Some(s) = Server::new() else { return };
    let out = s.agent(&["list"]);
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    let out = s.agent(&["list", "--json"]);
    assert!(out.status.success());
    assert_eq!(out.stdout, b"[]\n");
}

#[test]
fn no_server_fails_list_but_not_jump_or_pick() {
    let Some(s) = Server::new() else { return };
    s.tmux(&["kill-server"]);
    assert!(!s.agent(&["list"]).status.success());
    for cmd in ["jump", "pick"] {
        let out = s.agent(&[cmd, "--client", "nobody"]);
        assert!(out.status.success(), "{cmd}");
        assert!(out.stderr.is_empty(), "{cmd}");
    }
}

#[test]
fn jump_cycles_through_panes_that_need_you() {
    let Some(s) = Server::new() else { return };
    let ([p3, p2, p1, _, _], p0) = agents_setup(&s);
    let cc = ControlClient::attach(&s, "a");
    let c = s.wait_client(|f| f.contains("control-mode"));
    assert_eq!(s.client_pane(&c), p0);

    for want in [&p3, &p2, &p1, &p3, &p2] {
        let out = s.agent(&["jump", "--client", &c]);
        assert!(out.status.success());
        assert_eq!(&s.client_pane(&c), want);
    }

    // With nothing left to jump to, the client gets a message and stays.
    for p in [&p1, &p2, &p3] {
        s.report(p, "cancel");
    }
    assert!(s.agent(&["jump", "--client", &c]).status.success());
    assert_eq!(s.client_pane(&c), p2);
    cc.wait_message(&s, &c, "tmux-agent: no agent needs you");
}

#[test]
fn jump_cycles_on_after_visited_done_panes_turn_idle() {
    let Some(s) = Server::new() else { return };
    for w in 1..=3 {
        s.tmux(&["new-window", "-d", "-t", &format!("a:{w}")]);
    }
    let [a, b, c] = ["a:1", "a:2", "a:3"].map(|t| s.pane(t));
    s.report(&a, "ask");
    s.report(&b, "done");
    s.report(&c, "done");
    s.set_since(&b, 100);
    s.set_since(&c, 200);
    let _cc = ControlClient::attach(&s, "a");
    let cl = s.wait_client(|f| f.contains("control-mode"));

    let mut seen = Vec::new();
    for _ in 0..5 {
        assert!(s.agent(&["jump", "--client", &cl]).status.success());
        let cur = s.client_pane(&cl);
        // What the pane-focus-in hook runs.
        assert!(s.agent(&["focus", "--pane", &cur]).status.success());
        seen.push(cur);
    }
    assert_eq!(seen, [&a, &b, &c, &a, &a].map(String::as_str));
    let list = String::from_utf8(s.agent(&["list"]).stdout).unwrap();
    for p in [&b, &c] {
        assert!(list.contains(&format!("{p}\t")), "{list}");
        let row = list
            .lines()
            .find(|l| l.starts_with(&format!("{p}\t")))
            .unwrap();
        assert_eq!(row.split('\t').nth(2), Some("idle"), "focus made {p} idle");
    }
}

#[test]
fn jump_unzooms_to_reach_a_hidden_pane() {
    let Some(s) = Server::new() else { return };
    s.tmux(&["split-window", "-t", "a:0"]);
    let (p0, p1) = (s.pane("a:0.0"), s.pane("a:0.1"));
    s.tmux(&["select-pane", "-t", &p1]);
    s.tmux(&["resize-pane", "-Z", "-t", &p1]);
    s.report(&p0, "ask");
    let _cc = ControlClient::attach(&s, "a");
    let c = s.wait_client(|f| f.contains("control-mode"));
    assert!(s.agent(&["jump", "--client", &c]).status.success());
    assert_eq!(s.client_pane(&c), p0);
    let zoomed = s.tmux(&[
        "display-message",
        "-p",
        "-t",
        "a:0",
        "#{window_zoomed_flag}",
    ]);
    assert_eq!(zoomed.trim(), "0");
}

#[test]
fn pick_shows_a_menu_that_moves_the_client() {
    let Some(s) = Server::new() else { return };
    let ([p3, p2, ..], p0) = agents_setup(&s);
    // A '#' in a window name must render as a single '#'.
    s.tmux(&["rename-window", "-t", "a:1", "w#1"]);
    // Runs of '#', and '#' before '[', in a cwd render as they are.
    let odd = "d##x#[reverse]y";
    let dir = s.dir.join(odd);
    std::fs::create_dir(&dir).unwrap();
    // tmux format-expands -c; a '#' run before '[' passes as it is.
    let c_arg = dir.to_str().unwrap().replace("##x", "####x");
    s.tmux(&["new-window", "-d", "-t", "a:2", "-c", &c_arg]);
    s.report(&s.pane("a:2.0"), "working");
    // The new shell's cwd shows once it has started.
    let path = || s.tmux(&["display-message", "-p", "-t", "a:2", "#{pane_current_path}"]);
    wait(|| path().trim_end().ends_with(odd));
    let mut tc = TtyClient::attach(&s, "a", 30);
    let c = s.wait_client(|f| !f.contains("control-mode"));
    assert_eq!(s.client_pane(&c), p0);

    let out = s.agent(&["pick", "--client", &c]);
    assert!(out.status.success());
    let leaf = format!("/{}/{odd}  ", s.dir.file_name().unwrap().to_str().unwrap());
    wait(|| {
        let t = tc.out.text();
        t.contains("agents") && t.contains("a:1.1  w#1") && t.contains(&leaf)
    });
    tc.press("2");
    wait(|| s.client_pane(&c) == p2);

    let mark = tc.out.len();
    assert!(s.agent(&["pick", "--client", &c]).status.success());
    wait(|| tc.out.text_from(mark).contains("agents"));
    tc.press("1");
    wait(|| s.client_pane(&c) == p3);
}

#[test]
fn pick_caps_the_menu_to_the_client_height() {
    let Some(s) = Server::new() else { return };
    // 12 agent panes on a 12-row client: 9 items plus "… 3 more".
    for i in 0..12 {
        s.tmux(&["new-window", "-d", "-t", &format!("a:{}", i + 1)]);
        let p = s.pane(&format!("a:{}.0", i + 1));
        s.report(&p, "working");
        s.set_since(&p, 1000 + i);
    }
    s.tmux(&["set-option", "-g", "status", "off"]);
    let mut tc = TtyClient::attach(&s, "a", 12);
    let c = s.wait_client(|f| !f.contains("control-mode"));
    assert!(s.agent(&["pick", "--client", &c]).status.success());
    wait(|| tc.out.text().contains("3 more"));
    let text = tc.out.text();
    assert!(text.contains("a:9.0"));
    assert!(!text.contains("a:10.0"));

    // The last item opens choose-tree with the tab icons, agent windows only.
    let mark = tc.out.len();
    tc.press("\x1b[A\r");
    let mode = || s.tmux(&["display-message", "-p", "-c", &c, "#{pane_mode}"]);
    wait(|| mode().trim() == "tree-mode");
    let icon = "\u{f110} ";
    wait(|| tc.text_from(mark).matches(icon).count() >= 12);
    let tree = tc.text_from(mark);
    assert!(!tree.contains("zero"), "window 0 has no agent: {tree}");
}

#[test]
fn pick_cuts_long_names_to_the_client_width() {
    let Some(s) = Server::new() else { return };
    let long = "W".repeat(150);
    s.tmux(&["new-window", "-d", "-t", "a:1", "-n", &long, "-c", "/"]);
    s.report(&s.pane("a:1.0"), "ask");
    let tc = TtyClient::attach(&s, "a", 30);
    let c = s.wait_client(|f| !f.contains("control-mode"));
    assert!(s.agent(&["pick", "--client", &c]).status.success());
    // 100 columns: label + key (4) + border (4); the cwd and age survive.
    let label = format!("a:1.0  {}…  /  ", "W".repeat(75));
    wait(|| tc.out.text().contains(&label));
    assert!(tc.out.text().contains("(1)"));
}

#[test]
fn pick_with_no_agents_says_so() {
    let Some(s) = Server::new() else { return };
    let cc = ControlClient::attach(&s, "a");
    let c = s.wait_client(|f| f.contains("control-mode"));
    assert!(s.agent(&["pick", "--client", &c]).status.success());
    cc.wait_message(&s, &c, "tmux-agent: no agents");
}
