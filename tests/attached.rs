//! Background sessions viewed through `claude attach`, against the real
//! binary on private tmux servers. See docs/design.md, "Background sessions
//! (claude attach)".
//!
//! The client is a fake `claude`: a `#!/bin/sh` script run as
//! `./claude attach <short id>`, which `ps` shows as
//! `/bin/sh ./claude attach <short id>`, as for any script install. The
//! engine is `sh -c 'sleep …' engine --session-id <id>` under a host `sh`
//! whose argv holds `--bg-pty-host`, as the daemon's pty host does: only
//! their argv and parentage matter.

mod common;

use common::{COLORS, ICONS, Proc, Server, agent_cmd};
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

static N: AtomicU32 = AtomicU32::new(0);

/// The host skips its own arguments up to `+`, then starts the engine with
/// the rest and prints its pid. No `--` word: the host's flag counts only
/// before one.
const HOST: &str = "while [ \"$1\" != + ]; do shift; done; shift; \
                    sh -c 'sleep 120; exit 0' engine \"$@\" & echo $!; wait";

/// A fake background engine under a fake pty host, in its own process
/// group, all killed on drop.
struct Engine {
    host: Child,
    pid: u32,
    id: String,
}

impl Engine {
    fn spawn() -> Engine {
        Engine::spawn_with(true)
    }

    /// `with_id`: false for a spare engine, which has no `--session-id`.
    fn spawn_with(with_id: bool) -> Engine {
        // Unique per test, so parallel tests' clients never match each other.
        let n = N.fetch_add(1, Ordering::Relaxed);
        let id = format!(
            "{:08x}-cb86-42e0-a42c-a122da5cf5ec",
            (std::process::id() << 8) ^ n
        );
        let mut args = vec!["-c", HOST, "host", "--bg-pty-host", "/x/pty.sock", "+"];
        if with_id {
            args.extend(["--session-id", &id]);
        }
        let mut host = Command::new("sh")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(host.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        let pid = line.trim().parse().unwrap();
        Engine { host, pid, id }
    }

    fn pid(&self) -> String {
        self.pid.to_string()
    }

    fn short(&self) -> &str {
        &self.id[..8]
    }

    fn kill(&mut self) {
        // SAFETY: kill has no memory effects; the group is the host's own.
        unsafe { libc::kill(-(self.host.id() as libc::pid_t), libc::SIGKILL) };
        let _ = self.host.wait();
        // The engine is the host's child: gone once the group is.
        let deadline = Instant::now() + Duration::from_secs(5);
        while ps().iter().any(|(pid, _, _)| *pid == self.pid) {
            assert!(Instant::now() < deadline, "engine {} never died", self.pid);
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The directory holding the fake client.
fn client_dir(s: &Server) -> PathBuf {
    let dir = s.dir.join("bin");
    let bin = dir.join("claude");
    if !bin.exists() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&bin, "#!/bin/sh\nsleep 120\nexit 0\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// `ps` rows: pid, ppid, args.
fn ps() -> Vec<(u32, u32, String)> {
    let out = Command::new("ps")
        .args(["-A", "-ww", "-o", "pid=", "-o", "ppid=", "-o", "args="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            Some((pid, ppid, it.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

/// Waits until `pane` runs the client, as its own process or a child: not
/// the `sh -c` that tmux starts it with, whose argv ends the same way.
fn wait_for_client(s: &Server, pane: &str, token: &str) {
    let pane_pid: u32 = s.opt(pane, "pane_pid").parse().unwrap();
    let want = format!(" ./claude attach {token}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ps().iter().any(|(pid, ppid, args)| {
        (*pid == pane_pid || *ppid == pane_pid) && args.ends_with(&want) && !args.contains(" -c ")
    }) {
        assert!(Instant::now() < deadline, "client never started in {pane}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A new window whose pane runs `claude attach <token>`; returns the pane.
fn attach_window(s: &Server, token: &str) -> String {
    let dir = client_dir(s);
    let pane = s.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "main:",
        "-c",
        dir.to_str().unwrap(),
        &format!("./claude attach {token}"),
    ]);
    wait_for_client(s, &pane, token);
    pane
}

/// The client exits and the pane goes on running something else.
fn detach(s: &Server, pane: &str) {
    s.tmux(&["respawn-pane", "-k", "-t", pane, "sleep 120"]);
}

fn bg_report(s: &Server, event: &str, e: &Engine, extra: &[&str]) {
    let pid = e.pid();
    let mut args = vec![
        "report",
        event,
        "--attached",
        "--pid",
        &pid,
        "--proto",
        "1",
        "--plugin-version",
        "0.1.0",
    ];
    args.extend_from_slice(extra);
    s.agent_ok(&args);
}

fn state(s: &Server, pane: &str) -> String {
    s.pane_opt(pane, "@agent_pane_state")
}

fn tab(s: &Server, pane: &str) -> (String, String) {
    let w = s.window_of(pane);
    (
        s.win_opt(&w, "@agent_window_state"),
        s.win_opt(&w, "@agent_tab_color"),
    )
}

fn want_tab(state: &str) -> (String, String) {
    let i = ["needs-input", "done", "working"]
        .iter()
        .position(|s| *s == state);
    match i {
        Some(i) => (state.into(), COLORS[i].into()),
        None => Default::default(),
    }
}

fn bg_opt(s: &Server) -> String {
    s.tmux(&["show-options", "-gqv", "@agent_bg"])
}

#[test]
fn report_shows_on_the_attached_pane_and_its_window() {
    let Some(s) = Server::new() else { return };
    let e = Engine::spawn();
    let plain = s.first_pane();
    let a = attach_window(&s, e.short());

    bg_report(&s, "claim", &e, &[]);
    bg_report(&s, "working", &e, &["--session", "sid-1"]);
    assert_eq!(state(&s, &a), "working");
    assert_eq!(s.pane_opt(&a, "@agent_pane_attached"), e.short());
    assert_eq!(s.pane_opt(&a, "@agent_pane_pid"), e.pid());
    assert_eq!(s.pane_opt(&a, "@agent_pane_session"), "sid-1");
    assert_eq!(tab(&s, &a), want_tab("working"));
    assert_eq!(
        s.win_opt(&s.window_of(&a), "@agent_tab_icon"),
        format!("{} ", ICONS[2])
    );
    assert_eq!(state(&s, &plain), "");
    assert_eq!(tab(&s, &plain), want_tab(""));
    assert_eq!(s.summary(), format!("{} 1", ICONS[2]));
    let rec = bg_opt(&s);
    assert!(
        rec.starts_with(&format!("{}|working|0|0|{}|", e.id, e.pid())),
        "{rec}"
    );

    bg_report(&s, "ask", &e, &[]);
    assert_eq!(tab(&s, &a), want_tab("needs-input"));

    // list marks the pane.
    let out = s.agent(&["list"]);
    let text = String::from_utf8(out.stdout).unwrap();
    let row: Vec<&str> = text.lines().next().unwrap().split('\t').collect();
    assert_eq!(
        (row[0], row[2], row[5]),
        (a.as_str(), "needs-input", e.short())
    );

    // doctor lists the session.
    let out = s.agent(&["doctor"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(&format!(
            "background session {} needs-input, owner {} alive, attached in {a}",
            e.short(),
            e.pid()
        )),
        "{text}"
    );

    bg_report(&s, "release", &e, &[]);
    assert_eq!(state(&s, &a), "");
    assert_eq!(s.pane_opt(&a, "@agent_pane_attached"), "");
    assert_eq!(tab(&s, &a), want_tab(""));
    assert_eq!(bg_opt(&s), "");
    assert_eq!(s.summary(), "");
}

#[test]
fn detach_and_attach_follow_on_reconcile() {
    let Some(s) = Server::new() else { return };
    let e = Engine::spawn();
    let a = attach_window(&s, e.short());
    bg_report(&s, "working", &e, &[]);
    bg_report(&s, "done", &e, &[]);
    assert_eq!(tab(&s, &a), want_tab("done"));

    detach(&s, &a);
    s.reconcile();
    assert_eq!(state(&s, &a), "");
    assert_eq!(s.pane_opt(&a, "pane-border-style"), "");
    assert_eq!(tab(&s, &a), want_tab(""));
    assert!(bg_opt(&s).starts_with(&e.id), "the session lives on");

    // The full id names it too.
    let b = attach_window(&s, &e.id);
    assert_eq!(state(&s, &b), "", "not before reconcile");
    s.reconcile();
    assert_eq!(state(&s, &b), "done");
    assert_eq!(tab(&s, &b), want_tab("done"));
    assert_eq!(s.pane_opt(&b, "@agent_pane_attached"), e.short());
}

#[test]
fn dead_engine_is_cleared_on_reconcile() {
    let Some(s) = Server::new() else { return };
    let mut e = Engine::spawn();
    let a = attach_window(&s, e.short());
    bg_report(&s, "working", &e, &[]);
    assert_eq!(state(&s, &a), "working");
    e.kill();
    s.reconcile();
    assert_eq!(bg_opt(&s), "");
    assert_eq!(state(&s, &a), "");
    assert_eq!(tab(&s, &a), want_tab(""));
    assert_eq!(s.summary(), "");
}

#[test]
fn a_session_running_in_the_pane_is_not_overwritten() {
    let Some(s) = Server::new() else { return };
    let e = Engine::spawn();
    let a = attach_window(&s, e.short());
    let b = attach_window(&s, e.short());
    let direct = Proc::spawn();
    s.report("working", &a, direct.pid(), &[]);

    bg_report(&s, "done", &e, &[]);
    assert_eq!(state(&s, &a), "working");
    assert_eq!(s.pane_opt(&a, "@agent_pane_pid"), direct.pid().to_string());
    assert_eq!(s.pane_opt(&a, "@agent_pane_attached"), "");
    assert_eq!(state(&s, &b), "done");
    s.reconcile();
    assert_eq!(state(&s, &a), "working");
    assert_eq!(s.summary(), format!("{} 1 {} 1", ICONS[1], ICONS[2]));
}

#[test]
fn focus_on_one_attached_pane_marks_all_of_them_seen() {
    let Some(s) = Server::new() else { return };
    let e = Engine::spawn();
    let a = attach_window(&s, e.short());
    let b = attach_window(&s, e.short());
    bg_report(&s, "working", &e, &[]);
    bg_report(&s, "done", &e, &[]);
    assert_eq!([state(&s, &a), state(&s, &b)], ["done", "done"]);

    s.focus(&a);
    assert_eq!([state(&s, &a), state(&s, &b)], ["idle", "idle"]);
    assert_eq!(tab(&s, &b), want_tab(""));
    assert!(bg_opt(&s).contains("|idle|"), "{}", bg_opt(&s));
}

#[test]
fn foreign_reports_cannot_take_a_live_session() {
    let Some(s) = Server::new() else { return };
    let e = Engine::spawn();
    let a = attach_window(&s, e.short());
    bg_report(&s, "working", &e, &[]);
    // Another hosted engine, with no --session-id, naming the same session.
    let other = Engine::spawn_with(false);
    bg_report(&s, "release", &other, &["--session", &e.id]);
    assert_eq!(state(&s, &a), "working");
    assert!(bg_opt(&s).starts_with(&format!("{}|working|", e.id)));
}

#[test]
fn engines_no_daemon_hosts_leave_no_record() {
    let Some(s) = Server::new() else { return };
    let e = Engine::spawn();
    let a = attach_window(&s, e.short());
    // A plain `claude` in a terminal outside tmux reports the same way.
    let plain = Proc::spawn();
    let pid = plain.pid().to_string();
    for extra in [&["--pid", &pid][..], &[]] {
        let mut args = vec!["report", "working", "--attached", "--session", &e.id];
        args.extend_from_slice(extra);
        s.agent_ok(&args);
    }
    assert_eq!(bg_opt(&s), "");
    assert_eq!(state(&s, &a), "");
}

#[test]
fn a_server_restart_keeps_background_sessions() {
    let Some(s) = Server::new() else { return };
    let e = Engine::spawn();
    let a = attach_window(&s, e.short());
    bg_report(&s, "working", &e, &[]);
    bg_report(&s, "done", &e, &[]);
    assert_eq!(state(&s, &a), "done");
    let since = s.pane_opt(&a, "@agent_pane_since");

    s.restart();
    let b = attach_window(&s, e.short());
    assert_eq!(bg_opt(&s), "", "a new server starts empty");
    s.reconcile();
    assert_eq!(state(&s, &b), "done");
    assert_eq!(tab(&s, &b), want_tab("done"));
    assert_eq!(s.pane_opt(&b, "@agent_pane_since"), since);
    assert!(bg_opt(&s).starts_with(&format!("{}|done|", e.id)));

    // Released, the copy on disk goes too: the next server restores nothing.
    bg_report(&s, "release", &e, &[]);
    s.restart();
    let c = attach_window(&s, e.short());
    s.reconcile();
    assert_eq!(state(&s, &c), "");
    assert_eq!(bg_opt(&s), "");
}

/// A tmux stand-in that records its `-S` argument and fails.
fn recording_tmux(dir: &Path) -> (PathBuf, PathBuf) {
    std::fs::create_dir_all(dir).unwrap();
    let log = dir.join("args");
    let bin = dir.join("tmux");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\nexit 1\n",
            log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

fn socket_used(log: &Path) -> String {
    let args = std::fs::read_to_string(log).unwrap_or_default();
    let mut it = args.lines();
    it.find(|a| *a == "-S");
    it.next().unwrap_or_default().to_owned()
}

#[test]
fn attached_report_without_tmux_env_uses_the_default_socket() {
    // No real tmux runs here, so the user's default server is never touched.
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(format!("/tmp/ta-d-{}-{n}", std::process::id()));
    let (bin, log) = recording_tmux(&dir.join("fake"));
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let sock_dir = dir.join(format!("tmux-{uid}"));
    std::fs::create_dir_all(&sock_dir).unwrap();

    let run = |envs: &[(&str, &str)]| {
        let _ = std::fs::remove_file(&log);
        let out = agent_cmd(&bin)
            .args(["report", "working", "--attached", "--pid", "1"])
            .envs(envs.iter().copied())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        socket_used(&log)
    };
    let want = std::fs::canonicalize(&sock_dir)
        .unwrap()
        .join("default")
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(run(&[("TMUX_TMPDIR", dir.to_str().unwrap())]), want);
    // $TMUX wins over the default.
    let env_sock = dir.join("other").to_str().unwrap().to_owned();
    assert_eq!(
        run(&[
            ("TMUX_TMPDIR", dir.to_str().unwrap()),
            ("TMUX", &format!("{env_sock},1,0")),
        ]),
        env_sock
    );
    for f in ["fake/args", "fake/tmux", "other.tmux-agent.lock"] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    let _ = std::fs::remove_file(sock_dir.join("default.tmux-agent.lock"));
    for d in [dir.join("fake"), sock_dir, dir] {
        let _ = std::fs::remove_dir(d);
    }
}
