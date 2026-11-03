//! report, focus and reconcile against the real binary on private tmux
//! servers. See docs/design.md, "Transitions" and "Testing".

mod common;

use common::{COLORS, ICONS, Proc, Server, agent_cmd, dead_pid, tmux_bin};
use std::path::Path;
use std::time::{Duration, Instant};

fn me() -> u32 {
    std::process::id()
}

/// The icon and colour slot of a displayed state.
fn slot(state: &str) -> Option<usize> {
    ["needs-input", "done", "working"]
        .iter()
        .position(|s| *s == state)
}

/// The window options a window should carry for `state` ("" for none).
fn want_win(state: &str) -> (String, String, String) {
    match slot(state) {
        Some(i) => (state.into(), COLORS[i].into(), format!("{} ", ICONS[i])),
        None => Default::default(),
    }
}

/// The window options as stored: state, colour, icon.
fn win(s: &Server, window: &str) -> (String, String, String) {
    (
        s.win_opt(window, "@agent_window_state"),
        s.win_opt(window, "@agent_tab_color"),
        s.win_opt(window, "@agent_tab_icon"),
    )
}

fn state(s: &Server, pane: &str) -> String {
    s.pane_opt(pane, "@agent_pane_state")
}

/// The summary for counts of needs-input, done and working panes.
fn summary(counts: [u32; 3]) -> String {
    (0..3)
        .filter(|&i| counts[i] > 0)
        .map(|i| format!("{} {}", ICONS[i], counts[i]))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every window's options match the highest-ranked state of its panes, and
/// the summary counts every pane once.
fn assert_consistent(s: &Server) {
    for w in s.windows() {
        let rows = s.tmux(&["list-panes", "-t", &w, "-F", "#{@agent_pane_state}"]);
        let best = rows.lines().filter_map(slot).min();
        let want = best.map_or("", |i| ["needs-input", "done", "working"][i]);
        assert_eq!(win(s, &w), want_win(want), "window {w}");
    }
    let mut counts = [0; 3];
    for (_, st) in s.pane_states() {
        if let Some(i) = slot(&st) {
            counts[i] += 1;
        }
    }
    assert_eq!(s.summary(), summary(counts));
}

#[test]
fn rollup_prefers_needs_input_then_done_then_working() {
    let Some(s) = Server::new() else { return };
    let a = s.first_pane();
    let b = s.split(&a);
    let c = s.split(&a);
    let w = s.window_of(&a);

    s.report("working", &a, me(), &[]);
    assert_eq!(win(&s, &w), want_win("working"));
    assert_eq!(s.summary(), summary([0, 0, 1]));

    s.report("done", &b, me(), &[]);
    assert_eq!(win(&s, &w), want_win("done"));
    assert_eq!(s.summary(), summary([0, 1, 1]));

    s.report("ask", &c, me(), &[]);
    assert_eq!(win(&s, &w), want_win("needs-input"));
    assert_eq!(s.summary(), summary([1, 1, 1]));
    assert_eq!(
        [&a, &b, &c].map(|p| state(&s, p)),
        ["working", "done", "needs-input"]
    );

    s.report("resume", &c, me(), &[]);
    assert_eq!(win(&s, &w), want_win("done"));
    assert_eq!(s.summary(), summary([0, 1, 2]));

    for p in [&a, &b, &c] {
        s.report("release", p, me(), &[]);
    }
    assert_eq!(win(&s, &w), want_win(""));
    assert_eq!(s.summary(), "");
    assert_consistent(&s);
}

#[test]
fn zoomed_window_counts_hidden_panes() {
    let Some(s) = Server::new() else { return };
    let a = s.first_pane();
    let b = s.split(&a);
    let w = s.window_of(&a);
    s.tmux(&["resize-pane", "-Z", "-t", &a]);
    assert_eq!(s.opt(&a, "window_zoomed_flag"), "1");

    s.report("working", &a, me(), &[]);
    s.report("ask", &b, me(), &[]);
    assert_eq!(win(&s, &w), want_win("needs-input"));
    s.reconcile();
    assert_eq!(win(&s, &w), want_win("needs-input"));
    assert_eq!(s.summary(), summary([1, 0, 1]));
}

#[test]
fn pane_borders_follow_state_and_fall_back_on_release() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    s.tmux(&["set", "-g", "pane-border-style", "fg=blue"]);
    let border = |s: &Server| {
        (
            s.pane_opt(&p, "pane-border-style"),
            s.pane_opt(&p, "pane-active-border-style"),
        )
    };
    let fg = |i: usize| (format!("fg={}", COLORS[i]), format!("fg={}", COLORS[i]));

    s.report("claim", &p, me(), &[]);
    assert_eq!(border(&s), (String::new(), String::new()), "idle");
    s.report("working", &p, me(), &[]);
    assert_eq!(border(&s), fg(2));
    s.report("ask", &p, me(), &[]);
    assert_eq!(border(&s), fg(0));
    s.report("resume", &p, me(), &[]);
    s.report("done", &p, me(), &[]);
    assert_eq!(border(&s), fg(1));
    s.report("release", &p, me(), &[]);
    assert_eq!(border(&s), (String::new(), String::new()));
    assert_eq!(s.opt(&p, "pane-border-style"), "fg=blue", "global again");
    assert_eq!(state(&s, &p), "");
}

#[test]
fn nested_claim_and_foreign_release_are_refused() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    let other = Proc::spawn();
    s.report("working", &p, me(), &[]);

    s.report("claim", &p, other.pid(), &[]);
    assert_eq!(state(&s, &p), "working");
    assert_eq!(s.pane_opt(&p, "@agent_pane_pid"), me().to_string());

    s.report("release", &p, other.pid(), &[]);
    s.report("done", &p, other.pid(), &[]);
    assert_eq!(state(&s, &p), "working");
    assert_eq!(s.pane_opt(&p, "@agent_pane_pid"), me().to_string());
    assert_eq!(win(&s, &s.window_of(&p)), want_win("working"));
}

#[test]
fn owner_reclaim_keeps_state() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    s.report("working", &p, me(), &[]);
    s.report("ask", &p, me(), &[]);
    s.report("claim", &p, me(), &["--session", "new"]);
    assert_eq!(state(&s, &p), "needs-input");
    assert_eq!(s.pane_opt(&p, "@agent_pane_loop"), "working");
    assert_eq!(s.pane_opt(&p, "@agent_pane_session"), "new");
}

#[test]
fn dead_owner_is_replaced_by_new_reporter() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    // A record left by a process that has exited, as a crash leaves it.
    let dead = dead_pid().to_string();
    for (o, v) in [
        ("@agent_pane_state", "needs-input"),
        ("@agent_pane_loop", "working"),
        ("@agent_pane_asks", "1"),
        ("@agent_pane_pid", &dead),
        // A start time too, so a reused PID still reads as another process.
        ("@agent_pane_pid_start", "1.000000"),
    ] {
        s.tmux(&["set", "-p", "-t", &p, o, v]);
    }
    s.report("done", &p, me(), &[]);
    assert_eq!(state(&s, &p), "done", "fresh state: the old ask is gone");
    assert_eq!(s.pane_opt(&p, "@agent_pane_pid"), me().to_string());
    assert_eq!(s.pane_opt(&p, "@agent_pane_asks"), "0");
    assert_eq!(win(&s, &s.window_of(&p)), want_win("done"));
}

#[test]
fn owner_that_dies_is_replaced() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    let mut owner = Proc::spawn();
    s.report("ask", &p, owner.pid(), &[]);
    let other = Proc::spawn();
    s.report("working", &p, other.pid(), &[]);
    assert_eq!(state(&s, &p), "needs-input", "owner alive");

    owner.kill();
    s.report("working", &p, other.pid(), &[]);
    assert_eq!(state(&s, &p), "working");
    assert_eq!(s.pane_opt(&p, "@agent_pane_pid"), other.pid().to_string());
}

#[test]
fn reconcile_clears_dead_owner() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    let q = s.split(&p);
    let mut owner = Proc::spawn();
    s.report("ask", &p, owner.pid(), &[]);
    s.report("working", &q, me(), &[]);
    let w = s.window_of(&p);
    assert_eq!(win(&s, &w), want_win("needs-input"));

    owner.kill();
    s.reconcile();
    assert_eq!(state(&s, &p), "");
    assert_eq!(s.pane_opt(&p, "@agent_pane_pid"), "");
    assert_eq!(s.pane_opt(&p, "pane-border-style"), "");
    assert_eq!(win(&s, &w), want_win("working"));
    assert_eq!(s.summary(), summary([0, 0, 1]));
}

/// Window 1 holds a (needs-input) and b (working); window 2 holds c (done).
fn three_panes(s: &Server) -> [String; 3] {
    let a = s.first_pane();
    let b = s.split(&a);
    let c = s.new_window("main");
    s.report("ask", &a, me(), &[]);
    s.report("working", &b, me(), &[]);
    s.report("done", &c, me(), &[]);
    [a, b, c]
}

#[test]
fn break_pane_recomputes_both_windows() {
    let Some(s) = Server::new() else { return };
    let [a, b, _] = three_panes(&s);
    let w1 = s.window_of(&a);
    s.tmux(&["break-pane", "-d", "-s", &a]);
    let w3 = s.window_of(&a);
    assert_ne!(w1, w3);
    s.reconcile();
    assert_eq!(win(&s, &w1), want_win("working"));
    assert_eq!(win(&s, &w3), want_win("needs-input"));
    assert_eq!(state(&s, &a), "needs-input", "pane options travel");
    assert_eq!(s.window_of(&b), w1);
    assert_consistent(&s);
}

#[test]
fn join_pane_recomputes_both_windows() {
    let Some(s) = Server::new() else { return };
    let [a, b, c] = three_panes(&s);
    let (w1, w2) = (s.window_of(&a), s.window_of(&c));
    s.tmux(&["join-pane", "-d", "-s", &b, "-t", &c]);
    s.reconcile();
    assert_eq!(win(&s, &w1), want_win("needs-input"));
    assert_eq!(win(&s, &w2), want_win("done"));

    s.tmux(&["join-pane", "-d", "-s", &a, "-t", &c]);
    s.reconcile();
    assert_eq!(win(&s, &w1), want_win(""));
    assert_eq!(win(&s, &w2), want_win("needs-input"));
    assert_consistent(&s);
}

#[test]
fn swap_pane_recomputes_both_windows() {
    let Some(s) = Server::new() else { return };
    let [a, _, c] = three_panes(&s);
    let (w1, w2) = (s.window_of(&a), s.window_of(&c));
    s.tmux(&["swap-pane", "-d", "-s", &a, "-t", &c]);
    assert_eq!(s.window_of(&a), w2);
    s.reconcile();
    assert_eq!(win(&s, &w1), want_win("done"));
    assert_eq!(win(&s, &w2), want_win("needs-input"));
    assert_consistent(&s);
}

#[test]
fn move_pane_recomputes_both_windows() {
    let Some(s) = Server::new() else { return };
    let [a, b, c] = three_panes(&s);
    let (w1, w2) = (s.window_of(&a), s.window_of(&c));
    // A pane moved to another session's window.
    let d = s.tmux(&["new-session", "-d", "-P", "-F", "#{pane_id}", "-s", "other"]);
    let w4 = s.window_of(&d);
    s.tmux(&["move-pane", "-d", "-s", &a, "-t", &d]);
    s.reconcile();
    assert_eq!(win(&s, &w1), want_win("working"));
    assert_eq!(win(&s, &w4), want_win("needs-input"));
    assert_eq!(win(&s, &w2), want_win("done"));
    s.tmux(&["move-pane", "-d", "-s", &b, "-t", &c]);
    s.reconcile();
    assert_eq!(win(&s, &w1), want_win(""));
    assert_eq!(win(&s, &w2), want_win("done"));
    assert_consistent(&s);
}

#[test]
fn kill_pane_clears_window() {
    let Some(s) = Server::new() else { return };
    let [a, b, c] = three_panes(&s);
    let w1 = s.window_of(&a);
    s.tmux(&["kill-pane", "-t", &a]);
    s.reconcile();
    assert_eq!(win(&s, &w1), want_win("working"));
    s.tmux(&["kill-pane", "-t", &c]);
    s.reconcile();
    assert_eq!(s.summary(), summary([0, 0, 1]));
    s.report("release", &b, me(), &[]);
    assert_eq!(win(&s, &w1), want_win(""));
    assert_eq!(s.summary(), "");
    assert_consistent(&s);
}

#[test]
fn kill_window_and_session_leave_correct_summary() {
    let Some(s) = Server::new() else { return };
    let [a, _, c] = three_panes(&s);
    let d = s.tmux(&["new-session", "-d", "-P", "-F", "#{pane_id}", "-s", "other"]);
    s.report("ask", &d, me(), &[]);
    assert_eq!(s.summary(), summary([2, 1, 1]));

    s.tmux(&["kill-window", "-t", &s.window_of(&a)]);
    s.reconcile();
    assert_eq!(s.summary(), summary([1, 1, 0]));
    assert_consistent(&s);

    s.tmux(&["kill-session", "-t", "other"]);
    s.reconcile();
    assert_eq!(s.summary(), summary([0, 1, 0]));
    assert_eq!(win(&s, &s.window_of(&c)), want_win("done"));
    assert_consistent(&s);
}

#[test]
fn respawn_pane_clears_dead_owner() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    let shell: u32 = s.opt(&p, "pane_pid").parse().unwrap();
    s.report("working", &p, shell, &[]);
    s.tmux(&["respawn-pane", "-k", "-t", &p]);
    assert_eq!(state(&s, &p), "working", "options survive respawn");
    // The old shell may take a moment to exit.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !state(&s, &p).is_empty() {
        assert!(
            Instant::now() < deadline,
            "reconcile never cleared the pane"
        );
        std::thread::sleep(Duration::from_millis(50));
        s.reconcile();
    }
    assert_eq!(win(&s, &s.window_of(&p)), want_win(""));
    assert_eq!(s.summary(), "");
}

#[test]
fn done_on_focused_pane_is_seen_on_arrival() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    let q = s.split(&p);
    let client = s.attach("main");
    let rows = s.tmux(&[
        "list-clients",
        "-F",
        "#{client_name}\t#{pane_id}\t#{client_flags}",
    ]);
    assert!(
        rows.lines()
            .any(|l| l.starts_with(&format!("{}\t{p}\t", client.name)) && l.contains("focused")),
        "{rows}"
    );

    s.report("working", &p, me(), &[]);
    s.report("working", &q, me(), &[]);
    s.report("done", &p, me(), &[]);
    s.report("done", &q, me(), &[]);
    assert_eq!(state(&s, &p), "idle");
    assert_eq!(state(&s, &q), "done");
    assert_eq!(win(&s, &s.window_of(&p)), want_win("done"));
    assert_eq!(s.summary(), summary([0, 1, 0]));
}

#[test]
fn focus_marks_done_seen_but_keeps_open_prompt() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    let q = s.split(&p);
    let r = s.split(&p);
    s.report("done", &p, me(), &[]);
    s.report("ask", &q, me(), &[]);
    s.report("error", &r, me(), &[]);

    s.focus(&p);
    assert_eq!(state(&s, &p), "idle");
    s.focus(&q);
    assert_eq!(
        state(&s, &q),
        "needs-input",
        "the prompt still needs an answer"
    );
    assert_eq!(s.pane_opt(&q, "@agent_pane_asks"), "1");
    s.focus(&r);
    assert_eq!(state(&s, &r), "idle", "a seen error goes idle");
    assert_eq!(win(&s, &s.window_of(&p)), want_win("needs-input"));
    assert_eq!(s.summary(), summary([1, 0, 0]));
}

#[test]
fn concurrent_reports_leave_consistent_rollups() {
    let Some(s) = Server::new() else { return };
    let a = s.first_pane();
    let b = s.split(&a);
    let c = s.new_window("main");
    let d = s.split(&c);
    // One writer per pane, as the plugin's promise chain gives; the panes
    // race each other.
    let plan: [(&str, [&str; 5]); 4] = [
        (&a, ["claim", "working", "ask", "resume", "done"]),
        (&b, ["claim", "working", "done", "working", "ask"]),
        (&c, ["claim", "working", "ask", "resume", "working"]),
        (&d, ["claim", "working", "error", "working", "done"]),
    ];
    std::thread::scope(|t| {
        for (pane, events) in &plan {
            let s = &s;
            t.spawn(move || {
                for ev in events {
                    s.report(ev, pane, me(), &[]);
                }
            });
        }
    });
    let want = ["done", "needs-input", "working", "done"];
    for ((pane, _), w) in plan.iter().zip(want) {
        assert_eq!(state(&s, pane), w, "pane {pane}");
    }
    assert_eq!(win(&s, &s.window_of(&a)), want_win("needs-input"));
    assert_eq!(win(&s, &s.window_of(&c)), want_win("done"));
    assert_eq!(s.summary(), summary([1, 2, 1]));
    assert_consistent(&s);
}

#[test]
fn unknown_event_and_flags_are_ignored() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    let q = s.split(&p);

    s.report("frobnicate", &q, me(), &[]);
    assert_eq!(state(&s, &q), "");
    assert_eq!(s.pane_opt(&q, "@agent_pane_pid"), "");

    s.report("working", &p, me(), &[]);
    let before = s.tmux(&["show-options", "-p", "-t", &p]);
    s.report("frobnicate", &p, me(), &["--bg", "3"]);
    assert_eq!(s.tmux(&["show-options", "-p", "-t", &p]), before);

    // A newer plugin's flags, with and without values, change nothing else.
    s.report(
        "done",
        &p,
        me(),
        &["--new-flag=x", "--other", "--proto", "2"],
    );
    assert_eq!(state(&s, &p), "done");
    assert_eq!(s.pane_opt(&p, "@agent_pane_proto"), "2");
    assert_eq!(win(&s, &s.window_of(&p)), want_win("done"));
    assert_eq!(s.summary(), summary([0, 1, 0]));
}

#[test]
fn no_server_exits_zero_quickly() {
    let Some(tmux) = tmux_bin() else { return };
    let dir = format!("/tmp/ta-{}-none", std::process::id());
    let _ = std::fs::create_dir_all(&dir);
    let socket = format!("{dir}/s");
    let started = Instant::now();
    let out = agent_cmd(&tmux)
        .args(["report", "working", "--pane", "%0", "--socket", &socket])
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
    // It takes about 20 ms; the slack is for a loaded machine running
    // many tests at once (2.3 s seen with 64 test threads).
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

/// The harness gives the binary no locale and no `$TMUX`, so every test runs
/// this path; this one names it. Tabs and non-ASCII must survive.
#[test]
fn report_works_without_utf8_locale() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    s.report("working", &p, me(), &[]);
    assert_eq!(state(&s, &p), "working");
    assert_eq!(win(&s, &s.window_of(&p)), want_win("working"));
    assert_eq!(s.summary(), summary([0, 0, 1]));
}

/// Sets pane options by hand, as a crash or an older process leaves them.
fn set_pane(s: &Server, pane: &str, opts: &[(&str, &str)]) {
    for (o, v) in opts {
        s.tmux(&["set", "-p", "-t", pane, o, v]);
    }
}

/// A new window in `main` whose pane starts in a directory named `name`
/// under the server's directory; returns its pane id.
fn window_in(s: &Server, name: &str) -> String {
    let dir = s.dir.join(name);
    std::fs::create_dir(&dir).unwrap();
    let dir = dir.to_str().unwrap();
    let p = s.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-c",
        dir,
        "-t",
        "main:",
    ]);
    let path = s.opt(&p, "pane_current_path");
    assert!(
        Path::new(&path).ends_with(name),
        "{path:?} does not end in {name:?}"
    );
    p
}

#[test]
fn newline_in_pane_path_cannot_forge_a_client() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    // tmux prints #{pane_current_path} raw, so this path's later lines read
    // as client rows that have `p` focused, with and without a width.
    let row = format!("C\tfake\t{p}\tfocused\t40");
    let q = window_in(&s, &format!("x\n{row}\n{row}\t80"));
    assert!(s.tmux(&["list-clients"]).is_empty());

    s.report("working", &p, me(), &[]);
    s.report("done", &p, me(), &[]);
    assert_eq!(state(&s, &p), "done", "no client looks at p");
    s.report("working", &q, me(), &[]);
    assert_eq!(state(&s, &q), "working");
    assert_eq!(win(&s, &s.window_of(&q)), want_win("working"));
    s.focus(&q);
    s.reconcile();
    assert_eq!(state(&s, &p), "done");
    assert_eq!(s.summary(), summary([0, 1, 1]));
    assert_consistent(&s);
}

#[test]
fn window_name_with_quotes_and_hash() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    let w = s.window_of(&p);
    let name = "a#b\"c'd ##{pane_id} ;x";
    s.tmux(&["rename-window", "-t", &w, name]);
    let name = s.opt(&w, "window_name");
    assert!(name.contains("#b\"c'd"), "{name:?}");
    let q = s.new_window("main");
    s.tmux(&[
        "rename-window",
        "-t",
        &s.window_of(&q),
        "na\u{ef}ve \u{f110}",
    ]);

    s.report("ask", &p, me(), &[]);
    s.report("working", &q, me(), &[]);
    assert_eq!(state(&s, &p), "needs-input");
    assert_eq!(win(&s, &w), want_win("needs-input"));
    assert_eq!(win(&s, &s.window_of(&q)), want_win("working"));
    assert_eq!(s.opt(&w, "window_name"), name, "name untouched");
    s.reconcile();
    assert_eq!(s.summary(), summary([1, 0, 1]));
    assert_consistent(&s);
}

#[test]
fn style_mode_keeps_global_style() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    let w = s.window_of(&p);
    s.tmux(&["set", "-g", "@tmux-agent-tab-mode", "style"]);
    s.tmux(&["set", "-g", "window-status-style", "fg=blue"]);
    s.tmux(&["set", "-g", "window-status-current-style", "fg=red,bold"]);
    // A stray line in a path must not be read as a global style.
    let q = window_in(&s, "a\nb");
    let styles = |w: &str| {
        (
            s.win_opt(w, "window-status-style"),
            s.win_opt(w, "window-status-current-style"),
        )
    };
    let bg = |i: usize| {
        (
            format!("fg=blue,bg={}", COLORS[i]),
            format!("fg=red,bold,bg={}", COLORS[i]),
        )
    };

    s.report("working", &p, me(), &[]);
    assert_eq!(styles(&w), bg(2));
    s.report("done", &p, me(), &[]);
    assert_eq!(styles(&w), bg(1));
    s.report("ask", &q, me(), &[]);
    assert_eq!(styles(&s.window_of(&q)), bg(0));
    assert_eq!(win(&s, &w), want_win("done"), "options are published too");

    s.report("release", &p, me(), &[]);
    assert_eq!(styles(&w), (String::new(), String::new()));
    assert_eq!(s.opt(&w, "window-status-style"), "fg=blue", "global again");
    assert_eq!(styles(&s.window_of(&q)), bg(0));
}

#[test]
fn config_icons_colours_and_border_off() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    let q = s.split(&p);
    let r = s.new_window("main");
    for (o, v) in [
        ("@tmux-agent-pane-border", "off"),
        ("@thm_green", "#00ff00"),
        ("@tmux-agent-color-done", "#{@thm_green}"),
        ("@tmux-agent-icon-done", "D#"),
        ("@tmux-agent-icon-working", "W"),
        ("@tmux-agent-color-needs-input", "colour1"),
    ] {
        s.tmux(&["set", "-g", o, v]);
    }

    s.report("done", &p, me(), &[]);
    s.report("working", &q, me(), &[]);
    s.report("ask", &r, me(), &[]);
    let opts = |st: &str, c: &str, i: &str| (st.to_owned(), c.to_owned(), i.to_owned());
    assert_eq!(win(&s, &s.window_of(&p)), opts("done", "#00ff00", "D# "));
    assert_eq!(
        win(&s, &s.window_of(&r)),
        opts("needs-input", "colour1", &format!("{} ", ICONS[0]))
    );
    for x in [&p, &q, &r] {
        assert_eq!(s.pane_opt(x, "pane-border-style"), "", "border off");
        assert_eq!(s.pane_opt(x, "pane-active-border-style"), "");
    }
    assert_eq!(s.summary(), format!("{} 1 D# 1 W 1", ICONS[0]));

    s.report("release", &q, me(), &[]);
    assert_eq!(win(&s, &s.window_of(&p)), opts("done", "#00ff00", "D# "));
    assert_eq!(s.summary(), format!("{} 1 D# 1", ICONS[0]));
}

#[test]
fn grouped_session_and_linked_window_count_once() {
    let Some(s) = Server::new() else { return };
    let a = s.first_pane();
    let b = s.split(&a);
    let w = s.window_of(&a);
    s.tmux(&["new-session", "-d", "-s", "grp", "-t", "main"]);
    let d = s.tmux(&["new-session", "-d", "-P", "-F", "#{pane_id}", "-s", "other"]);
    s.tmux(&["link-window", "-d", "-s", &w, "-t", "other:"]);
    assert_eq!(
        s.tmux(&["list-windows", "-a", "-F", "#{window_id}"])
            .lines()
            .filter(|l| *l == w)
            .count(),
        3,
        "main, grp and other all list the window"
    );

    s.report("ask", &a, me(), &[]);
    s.report("working", &b, me(), &[]);
    s.report("done", &d, me(), &[]);
    assert_eq!(win(&s, &w), want_win("needs-input"));
    assert_eq!(s.summary(), summary([1, 1, 1]));
    assert_consistent(&s);

    let link = format!(
        "other:{}",
        s.tmux(&[
            "list-windows",
            "-t",
            "other",
            "-F",
            "#{window_id}\t#{window_index}"
        ])
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{w}\t")))
        .unwrap()
    );
    s.tmux(&["unlink-window", "-t", &link]);
    s.reconcile();
    assert_eq!(win(&s, &w), want_win("needs-input"));
    assert_eq!(win(&s, &s.window_of(&d)), want_win("done"));
    assert_eq!(s.summary(), summary([1, 1, 1]));
    assert_consistent(&s);

    s.tmux(&["kill-session", "-t", "grp"]);
    s.report("release", &a, me(), &[]);
    assert_eq!(win(&s, &w), want_win("working"));
    assert_eq!(s.summary(), summary([0, 1, 1]));
    assert_consistent(&s);
}

#[test]
fn live_pid_with_another_start_time_is_dead() {
    let Some(s) = Server::new() else { return };
    let p = s.first_pane();
    let q = s.split(&p);
    let w = s.window_of(&p);
    // A live process holds the recorded PID, but it is not the process that
    // claimed the pane: the start time differs, as after PID reuse.
    let reused = Proc::spawn();
    let pid = reused.pid().to_string();
    let rec = [
        ("@agent_pane_state", "needs-input"),
        ("@agent_pane_loop", "working"),
        ("@agent_pane_asks", "1"),
        ("@agent_pane_pid", &pid),
        ("@agent_pane_pid_start", "1.000000"),
    ];
    set_pane(&s, &p, &rec);
    set_pane(&s, &q, &rec);

    s.report("done", &p, me(), &[]);
    assert_eq!(state(&s, &p), "done", "replaced: the old ask is gone");
    assert_eq!(s.pane_opt(&p, "@agent_pane_pid"), me().to_string());
    assert_eq!(s.pane_opt(&p, "@agent_pane_asks"), "0");
    assert_ne!(s.pane_opt(&p, "@agent_pane_pid_start"), "1.000000");
    // The same report already reconciled q.
    assert_eq!(state(&s, &q), "");
    assert_eq!(s.pane_opt(&q, "@agent_pane_pid"), "");

    // Reconcile alone also clears such a record.
    set_pane(&s, &q, &rec);
    s.reconcile();
    assert_eq!(state(&s, &q), "");
    assert_eq!(win(&s, &w), want_win("done"));
    assert_eq!(s.summary(), summary([0, 1, 0]));
    drop(reused);
}
