//! report, focus and reconcile: read the server, change pane records, write
//! back only what changed.

use crate::lock::Lock;
use crate::owner;
use crate::snapshot::{Rec, Snapshot};
use crate::state::{self, Display, Event, Summary};
use crate::tmux::{Cmds, Tmux};
use std::collections::BTreeMap;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

/// A report from the Claude plugin.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub event: String,
    pub pane: String,
    /// The reporting Claude process. `None` skips the ownership check.
    pub pid: Option<u32>,
    pub session: Option<String>,
    pub bg: u32,
    pub proto: Option<String>,
    pub plugin: Option<String>,
}

/// Runs `f` on a fresh snapshot under the server lock, then writes the
/// result. Retries once if the write fails, e.g. because a pane closed
/// between the read and the write.
fn with_snapshot(tmux: &Tmux, mut f: impl FnMut(&mut Snapshot)) -> io::Result<()> {
    let _lock = Lock::acquire(&tmux.socket)?;
    let mut last = Ok(());
    for _ in 0..2 {
        let Some(mut snap) = Snapshot::read(tmux)? else {
            return Ok(());
        };
        f(&mut snap);
        gc(&mut snap);
        let cmds = plan(&snap, now());
        if cmds.is_empty() {
            return Ok(());
        }
        last = tmux.run(cmds.args()).map(drop);
        if last.is_ok() {
            break;
        }
    }
    last
}

pub fn report(tmux: &Tmux, r: &Report) -> io::Result<()> {
    with_snapshot(tmux, |snap| apply_report(snap, r))
}

pub fn focus(tmux: &Tmux, pane: &str) -> io::Result<()> {
    with_snapshot(tmux, |snap| {
        let focused = snap.focused(pane);
        if let Some(p) = snap.pane_mut(pane)
            && let Some(rec) = &mut p.rec
        {
            rec.st = state::apply(Some(rec.st), Event::Focus, focused).unwrap_or(rec.st);
        }
    })
}

pub fn reconcile(tmux: &Tmux) -> io::Result<()> {
    with_snapshot(tmux, |_| {})
}

/// Reconciles, then returns the server as it now stands, for commands that
/// read agent state (jump, pick, list, doctor). `None` when the server has no
/// sessions.
pub fn reconciled(tmux: &Tmux) -> io::Result<Option<Snapshot>> {
    reconcile(tmux)?;
    Snapshot::read(tmux)
}

fn parse_event(name: &str, bg: u32) -> Option<Event> {
    Some(match name {
        "claim" => Event::Claim,
        "reset" => Event::Reset,
        "working" => Event::Working,
        "ask" => Event::Ask,
        "resume" => Event::Resume,
        "done" => Event::Done { bg },
        "error" => Event::Error,
        "cancel" => Event::Cancel,
        "release" => Event::Release,
        _ => return None,
    })
}

/// Applies a report to the snapshot: the ownership check, then the
/// transition. Unknown events and panes are ignored.
pub fn apply_report(snap: &mut Snapshot, r: &Report) {
    let Some(mut ev) = parse_event(&r.event, r.bg) else {
        return;
    };
    let focused = snap.focused(&r.pane);
    let Some(pane) = snap.pane_mut(&r.pane) else {
        return;
    };

    // An owner recorded without a PID (a hand-run report) never blocks others.
    let owner_alive = pane
        .rec
        .as_ref()
        .is_some_and(|o| o.pid != 0 && owner::alive(o.pid, &o.start));
    let is_owner = match (&pane.rec, r.pid) {
        (Some(o), Some(pid)) => o.pid == pid,
        (_, None) => true,
        (None, _) => false,
    };
    if owner_alive && !is_owner {
        return;
    }
    // A claim from the live owner is a reload: keep the state.
    if ev == Event::Claim && owner_alive {
        ev = Event::Reclaim;
    }
    // A report on an unowned pane, or one whose owner died, starts fresh.
    let prev = if owner_alive || r.pid.is_none() {
        pane.rec.clone()
    } else {
        None
    };
    let Some(st) = state::apply(prev.as_ref().map(|p| p.st), ev, focused) else {
        pane.rec = None;
        return;
    };
    let mut rec = prev.unwrap_or_else(|| Rec {
        st,
        pid: r.pid.unwrap_or(0),
        start: r.pid.and_then(owner::start_time).unwrap_or_default(),
        session: String::new(),
        since: 0,
        proto: String::new(),
        plugin: String::new(),
    });
    rec.st = st;
    if let Some(s) = &r.session {
        rec.session.clone_from(s);
    }
    if let Some(p) = &r.proto {
        rec.proto.clone_from(p);
    }
    if let Some(p) = &r.plugin {
        rec.plugin.clone_from(p);
    }
    pane.rec = Some(rec);
}

/// Drops the state of dead panes and panes whose owner has exited.
fn gc(snap: &mut Snapshot) {
    for p in &mut snap.panes {
        let dead = p.dead
            || p.rec
                .as_ref()
                .is_some_and(|r| r.pid != 0 && !owner::alive(r.pid, &r.start));
        if dead {
            p.rec = None;
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn display_of(rec: &Option<Rec>) -> Display {
    rec.as_ref().map_or(Display::Idle, |r| r.st.display())
}

/// Index into the per-state icon and colour arrays.
fn slot(d: Display) -> Option<usize> {
    match d {
        Display::NeedsInput => Some(0),
        Display::Done => Some(1),
        Display::Working => Some(2),
        Display::Idle => None,
    }
}

const PANE_OPTS: [&str; 10] = [
    "@agent_pane_state",
    "@agent_pane_loop",
    "@agent_pane_asks",
    "@agent_pane_bg",
    "@agent_pane_pid",
    "@agent_pane_pid_start",
    "@agent_pane_since",
    "@agent_pane_session",
    "@agent_pane_proto",
    "@agent_pane_plugin",
];

/// The tmux commands that bring the server in line with the snapshot's
/// records: pane options, pane borders, window rollups and the summary.
pub fn plan(snap: &Snapshot, now: u64) -> Cmds {
    let cfg = &snap.config;
    let mut cmds = Cmds::default();

    for p in &snap.panes {
        if p.rec == p.orig {
            continue;
        }
        let (old, new) = (display_of(&p.orig), display_of(&p.rec));
        match &p.rec {
            None => {
                for o in PANE_OPTS {
                    cmds.push(["set-option", "-pu", "-t", &p.id, o]);
                }
            }
            Some(r) => {
                let since = if old != new || p.orig.is_none() {
                    now
                } else {
                    r.since
                };
                let vals = [
                    new.as_str().to_owned(),
                    r.st.lp.as_str().to_owned(),
                    u8::from(r.st.asks).to_string(),
                    r.st.bg.to_string(),
                    r.pid.to_string(),
                    r.start.clone(),
                    since.to_string(),
                    r.session.clone(),
                    r.proto.clone(),
                    r.plugin.clone(),
                ];
                for (o, v) in PANE_OPTS.iter().zip(vals) {
                    cmds.push([
                        "set-option".into(),
                        "-p".into(),
                        "-t".into(),
                        p.id.clone(),
                        (*o).into(),
                        v,
                    ]);
                }
            }
        }
        if cfg.pane_border && old != new {
            for o in ["pane-border-style", "pane-active-border-style"] {
                match slot(new) {
                    Some(i) => cmds.push([
                        "set-option",
                        "-p",
                        "-t",
                        &p.id,
                        o,
                        &format!("fg={}", cfg.colors[i]),
                    ]),
                    None => cmds.push(["set-option", "-pu", "-t", &p.id, o]),
                }
            }
        }
    }

    // Window rollups. A window's options read the same on each of its rows.
    let mut windows: BTreeMap<&str, (Vec<Display>, &crate::snapshot::Pane)> = BTreeMap::new();
    for p in &snap.panes {
        windows
            .entry(&p.window)
            .or_insert_with(|| (Vec::new(), p))
            .0
            .push(display_of(&p.rec));
    }
    for (win, (displays, first)) in windows {
        let want = state::rollup(displays);
        let (state, color, icon) = match want.and_then(slot) {
            Some(i) => (
                want.unwrap().as_str().to_owned(),
                cfg.colors[i].clone(),
                format!("{} ", cfg.icons[i]),
            ),
            None => (String::new(), String::new(), String::new()),
        };
        if (
            first.win_state.as_str(),
            first.win_color.as_str(),
            first.win_icon.as_str(),
        ) == (state.as_str(), color.as_str(), icon.as_str())
        {
            continue;
        }
        let opts = [
            ("@agent_window_state", &state),
            ("@agent_tab_color", &color),
            ("@agent_tab_icon", &icon),
        ];
        for (o, v) in opts {
            if v.is_empty() {
                cmds.push(["set-option", "-wu", "-t", win, o]);
            } else {
                cmds.push(["set-option", "-w", "-t", win, o, v]);
            }
        }
        if cfg.style_mode {
            let styles = [
                ("window-status-style", &snap.global.status_style),
                (
                    "window-status-current-style",
                    &snap.global.status_current_style,
                ),
            ];
            for (o, base) in styles {
                if color.is_empty() {
                    cmds.push(["set-option", "-wu", "-t", win, o]);
                } else {
                    cmds.push(["set-option", "-w", "-t", win, o, &with_bg(base, &color)]);
                }
            }
        }
    }

    let summary = summary_text(snap);
    if summary != snap.global.summary {
        cmds.push(["set-option", "-g", "@agent_summary", &summary]);
    }
    cmds
}

fn with_bg(base: &str, color: &str) -> String {
    if base.is_empty() || base == "default" {
        format!("bg={color}")
    } else {
        format!("{base},bg={color}")
    }
}

/// e.g. "󰂞 1  2": a count per shown state, most urgent first.
pub fn summary_text(snap: &Snapshot) -> String {
    let mut s = Summary::default();
    for p in &snap.panes {
        s.add(display_of(&p.rec));
    }
    let cfg = &snap.config;
    let counts = [s.needs_input, s.done, s.working];
    let parts: Vec<String> = (0..3)
        .filter(|&i| counts[i] > 0)
        .map(|i| {
            if cfg.summary_dots {
                // `default` restores the style in force where the summary is
                // drawn, which push-default saved below.
                format!("#[fg={}]\u{25cf}#[default] {}", cfg.colors[i], counts[i])
            } else {
                format!("{} {}", cfg.icons[i], counts[i])
            }
        })
        .collect();
    if cfg.summary_dots && !parts.is_empty() {
        format!("#[push-default]{}#[pop-default]", parts.join(" "))
    } else {
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Client, Config, Global, Pane};
    use crate::state::{Loop, PaneState};

    fn config() -> Config {
        Config {
            bin: String::new(),
            style_mode: false,
            pane_border: true,
            icons: ["N".into(), "D".into(), "W".into()],
            colors: ["red".into(), "green".into(), "yellow".into()],
            summary_dots: false,
        }
    }

    fn pane(id: &str, win: &str) -> Pane {
        Pane {
            id: id.into(),
            window: win.into(),
            dead: false,
            session_name: "s".into(),
            window_index: "1".into(),
            pane_index: "1".into(),
            window_name: "w".into(),
            path: "/".into(),
            orig: None,
            rec: None,
            win_state: String::new(),
            win_color: String::new(),
            win_icon: String::new(),
        }
    }

    fn snap(panes: Vec<Pane>) -> Snapshot {
        Snapshot {
            config: config(),
            global: Global::default(),
            clients: Vec::new(),
            panes,
        }
    }

    fn me() -> u32 {
        std::process::id()
    }

    fn report(event: &str, pane: &str, pid: u32) -> Report {
        Report {
            event: event.into(),
            pane: pane.into(),
            pid: Some(pid),
            ..Report::default()
        }
    }

    fn dead_pid() -> u32 {
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    }

    fn st(s: &Snapshot, id: &str) -> Option<PaneState> {
        s.pane(id).unwrap().rec.as_ref().map(|r| r.st)
    }

    #[test]
    fn claim_records_owner() {
        let mut s = snap(vec![pane("%1", "@1")]);
        apply_report(&mut s, &report("claim", "%1", me()));
        let r = s.pane("%1").unwrap().rec.clone().unwrap();
        assert_eq!(r.pid, me());
        assert_eq!(r.start, owner::start_time(me()).unwrap());
        assert_eq!(r.st, PaneState::IDLE);
    }

    #[test]
    fn foreign_report_is_dropped_while_owner_lives() {
        let mut s = snap(vec![pane("%1", "@1")]);
        apply_report(&mut s, &report("working", "%1", me()));
        for ev in ["claim", "release", "done"] {
            apply_report(&mut s, &report(ev, "%1", me() + 1));
            assert_eq!(st(&s, "%1").unwrap().lp, Loop::Working, "{ev}");
        }
    }

    #[test]
    fn reclaim_by_owner_keeps_state() {
        let mut s = snap(vec![pane("%1", "@1")]);
        apply_report(&mut s, &report("working", "%1", me()));
        apply_report(&mut s, &report("claim", "%1", me()));
        assert_eq!(st(&s, "%1").unwrap().lp, Loop::Working);
    }

    #[test]
    fn dead_owner_is_replaced() {
        let mut p = pane("%1", "@1");
        p.rec = Some(Rec {
            st: PaneState {
                lp: Loop::Working,
                asks: true,
                bg: 0,
            },
            pid: dead_pid(),
            start: String::new(),
            session: String::new(),
            since: 0,
            proto: String::new(),
            plugin: String::new(),
        });
        let mut s = snap(vec![p]);
        apply_report(&mut s, &report("done", "%1", me()));
        let r = s.pane("%1").unwrap().rec.clone().unwrap();
        assert_eq!(r.pid, me());
        assert_eq!(
            r.st,
            PaneState {
                lp: Loop::Done,
                asks: false,
                bg: 0
            }
        );
    }

    #[test]
    fn gc_clears_dead_owner_and_dead_pane() {
        let mut a = pane("%1", "@1");
        a.rec = Some(Rec {
            st: PaneState::IDLE,
            pid: dead_pid(),
            start: String::new(),
            session: String::new(),
            since: 0,
            proto: String::new(),
            plugin: String::new(),
        });
        let mut b = pane("%2", "@1");
        b.dead = true;
        b.rec = a.rec.clone().map(|r| Rec { pid: me(), ..r });
        let mut s = snap(vec![a, b]);
        gc(&mut s);
        assert!(s.panes.iter().all(|p| p.rec.is_none()));
    }

    #[test]
    fn done_on_focused_pane_goes_idle() {
        let mut s = snap(vec![pane("%1", "@1")]);
        s.clients.push(Client {
            name: "c".into(),
            pane: "%1".into(),
            focused: true,
            height: 40,
            width: 80,
        });
        apply_report(&mut s, &report("working", "%1", me()));
        apply_report(&mut s, &report("done", "%1", me()));
        assert_eq!(st(&s, "%1").unwrap().lp, Loop::Idle);
    }

    #[test]
    fn plan_writes_rollup_borders_and_summary() {
        let mut s = snap(vec![pane("%1", "@1"), pane("%2", "@1"), pane("%3", "@2")]);
        apply_report(&mut s, &report("working", "%1", me()));
        apply_report(&mut s, &report("ask", "%2", me()));
        let args = plan(&s, 7).args().join(" ");
        assert!(
            args.contains("set-option -p -t %1 @agent_pane_state working"),
            "{args}"
        );
        assert!(
            args.contains("set-option -p -t %2 @agent_pane_state needs-input"),
            "{args}"
        );
        assert!(
            args.contains("set-option -p -t %1 @agent_pane_since 7"),
            "{args}"
        );
        assert!(
            args.contains("set-option -p -t %1 pane-border-style fg=yellow"),
            "{args}"
        );
        assert!(
            args.contains("set-option -w -t @1 @agent_window_state needs-input"),
            "{args}"
        );
        assert!(
            args.contains("set-option -w -t @1 @agent_tab_color red"),
            "{args}"
        );
        assert!(
            args.contains("set-option -w -t @1 @agent_tab_icon N "),
            "{args}"
        );
        assert!(!args.contains("@2"), "untouched window: {args}");
        assert!(
            args.contains("set-option -g @agent_summary N 1 W 1"),
            "{args}"
        );
    }

    #[test]
    fn plan_is_empty_when_nothing_changed() {
        let mut p = pane("%1", "@1");
        let rec = Rec {
            st: PaneState {
                lp: Loop::Working,
                asks: false,
                bg: 0,
            },
            pid: me(),
            start: String::new(),
            session: String::new(),
            since: 1,
            proto: String::new(),
            plugin: String::new(),
        };
        p.orig = Some(rec.clone());
        p.rec = Some(rec);
        p.win_state = "working".into();
        p.win_color = "yellow".into();
        p.win_icon = "W ".into();
        let mut s = snap(vec![p]);
        s.global.summary = "W 1".into();
        assert!(plan(&s, 9).is_empty(), "{:?}", plan(&s, 9).args());
    }

    #[test]
    fn plan_unsets_cleared_pane_and_its_window() {
        let mut p = pane("%1", "@1");
        p.orig = Some(Rec {
            st: PaneState {
                lp: Loop::Done,
                asks: false,
                bg: 0,
            },
            pid: me(),
            start: String::new(),
            session: String::new(),
            since: 1,
            proto: String::new(),
            plugin: String::new(),
        });
        p.win_state = "done".into();
        p.win_color = "green".into();
        p.win_icon = "D ".into();
        let mut s = snap(vec![p]);
        s.global.summary = "D 1".into();
        let args = plan(&s, 9).args().join(" ");
        assert!(
            args.contains("set-option -pu -t %1 @agent_pane_state"),
            "{args}"
        );
        assert!(
            args.contains("set-option -pu -t %1 pane-border-style"),
            "{args}"
        );
        assert!(
            args.contains("set-option -wu -t @1 @agent_tab_color"),
            "{args}"
        );
        assert!(args.ends_with("set-option -g @agent_summary "), "{args}");
    }

    #[test]
    fn summary_dots_colour_each_count() {
        let mut s = snap(vec![pane("%1", "@1"), pane("%2", "@1"), pane("%3", "@2")]);
        s.config.summary_dots = true;
        assert_eq!(summary_text(&s), "", "nothing shows when no agent does");
        apply_report(&mut s, &report("ask", "%1", me()));
        apply_report(&mut s, &report("working", "%2", me()));
        apply_report(&mut s, &report("working", "%3", me()));
        assert_eq!(
            summary_text(&s),
            "#[push-default]#[fg=red]\u{25cf}#[default] 1 #[fg=yellow]\u{25cf}#[default] 2#[pop-default]"
        );
    }

    #[test]
    fn style_mode_keeps_base_style() {
        assert_eq!(with_bg("default", "red"), "bg=red");
        assert_eq!(with_bg("fg=blue,bold", "red"), "fg=blue,bold,bg=red");
    }
}
