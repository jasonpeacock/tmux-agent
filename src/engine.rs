//! report, focus and reconcile: read the server, change pane records, write
//! back only what changed.

use crate::attach::{self, Procs};
use crate::lock::{self, Lock};
use crate::owner;
use crate::snapshot::{self, Bg, Rec, Snapshot};
use crate::state::{self, Display, Event, Summary};
use crate::tmux::{Cmds, Tmux};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A report from the Claude plugin.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub event: String,
    pub pane: String,
    /// A background session, viewed through `claude attach`: it has no pane
    /// of its own. Ignored when `pane` is set.
    pub attached: bool,
    /// The reporting Claude process. `None` skips the ownership check.
    pub pid: Option<u32>,
    pub session: Option<String>,
    pub bg: u32,
    pub proto: Option<String>,
    pub plugin: Option<String>,
}

impl Report {
    fn is_attached(&self) -> bool {
        self.attached && self.pane.is_empty()
    }
}

/// How a command learns which panes show a background session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Find {
    /// From the process table: one `ps` call, made only when a background
    /// record exists.
    Ps,
    /// From the panes' `@agent_pane_attached` markers, with no fork.
    Markers,
}

/// Runs `f` on a fresh snapshot under the server lock, then writes the
/// result. Retries once if the write fails, e.g. because a pane closed
/// between the read and the write.
fn with_snapshot(
    tmux: &Tmux,
    find: Find,
    mut f: impl FnMut(&mut Snapshot, &mut Procs),
) -> io::Result<()> {
    let _lock = Lock::acquire(&tmux.socket)?;
    let saved = lock::beside(&tmux.socket, BG_FILE);
    let mut procs = Procs::new();
    let mut last = Ok(());
    for _ in 0..2 {
        let Some(mut snap) = Snapshot::read(tmux)? else {
            return Ok(());
        };
        restore(&mut snap, &saved);
        f(&mut snap, &mut procs);
        sync(&mut snap, find, &mut procs, now());
        let cmds = plan(&snap, now());
        if cmds.is_empty() {
            return Ok(());
        }
        last = tmux.run(cmds.args()).map(drop);
        if last.is_ok() {
            if snap.bg != snap.bg_orig || snap.bg_restored {
                save(&saved, &snap.bg)?;
            }
            break;
        }
    }
    last
}

/// A copy of `@agent_bg` on disk, beside the lock. Background engines
/// outlive the tmux server and send no event when it returns, so a new
/// server takes their records from here. See docs/design.md, "Background
/// sessions".
const BG_FILE: &str = ".tmux-agent.bg";

/// After a server restart `@agent_bg` is empty: takes the records from the
/// copy on disk. Owners that died meanwhile are dropped by `sync`. With no
/// copy this is one failed `open`, no fork.
fn restore(snap: &mut Snapshot, path: &Path) {
    if !snap.bg_orig.is_empty() {
        return;
    }
    let Ok(v) = std::fs::read_to_string(path) else {
        return;
    };
    snap.bg = snapshot::parse_bg(v.trim_end());
    snap.bg_orig = snap.bg.clone();
    snap.bg_restored = true;
}

/// Writes the copy, or removes it when there are no records. Written to a
/// temporary file and renamed, so a reader never sees half of it.
fn save(path: &Path, bg: &[Bg]) -> io::Result<()> {
    let v = snapshot::format_bg(bg);
    if v.is_empty() {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, v + "\n")?;
    std::fs::rename(&tmp, path)
}

pub fn report(tmux: &Tmux, r: &Report) -> io::Result<()> {
    let find = if r.is_attached() {
        Find::Ps
    } else {
        Find::Markers
    };
    with_snapshot(tmux, find, |snap, procs| apply_report(snap, r, procs))
}

pub fn focus(tmux: &Tmux, pane: &str) -> io::Result<()> {
    with_snapshot(tmux, Find::Markers, |snap, _| apply_focus(snap, pane))
}

pub fn reconcile(tmux: &Tmux) -> io::Result<()> {
    with_snapshot(tmux, Find::Ps, |_, _| {})
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

/// What a report does to a record once the ownership check has run.
#[derive(Debug)]
enum Outcome {
    /// From a non-owner while the owner lives.
    Drop,
    Clear,
    Set(Rec),
}

/// The ownership check, then the transition: the same rules for a pane and
/// for a background session's record.
fn transition(cur: Option<&Rec>, r: &Report, mut ev: Event, focused: bool) -> Outcome {
    // An owner recorded without a PID (a hand-run report) never blocks others.
    let owner_alive = cur.is_some_and(|o| o.pid != 0 && owner::alive(o.pid, &o.start));
    let is_owner = match (cur, r.pid) {
        (Some(o), Some(pid)) => o.pid == pid,
        (_, None) => true,
        (None, _) => false,
    };
    if owner_alive && !is_owner {
        return Outcome::Drop;
    }
    // A claim from the live owner is a reload: keep the state.
    if ev == Event::Claim && owner_alive {
        ev = Event::Reclaim;
    }
    // A report on an unowned record, or one whose owner died, starts fresh.
    let prev = if owner_alive || r.pid.is_none() {
        cur.cloned()
    } else {
        None
    };
    let Some(st) = state::apply(prev.as_ref().map(|p| p.st), ev, focused) else {
        return Outcome::Clear;
    };
    let mut rec = prev.unwrap_or_else(|| Rec {
        st,
        pid: r.pid.unwrap_or(0),
        start: r.pid.and_then(owner::start_time).unwrap_or_default(),
        session: String::new(),
        since: 0,
        proto: String::new(),
        plugin: String::new(),
        attached: String::new(),
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
    Outcome::Set(rec)
}

/// Applies a report to the snapshot: the ownership check, then the
/// transition. Unknown events and panes are ignored.
pub fn apply_report(snap: &mut Snapshot, r: &Report, procs: &mut Procs) {
    let Some(ev) = parse_event(&r.event, r.bg) else {
        return;
    };
    if r.is_attached() {
        apply_attached(snap, r, ev, procs);
        return;
    }
    let focused = snap.focused(&r.pane);
    let Some(pane) = snap.pane_mut(&r.pane) else {
        return;
    };
    // A background session shown here gives way to one running in the pane.
    let cur = pane.rec.as_ref().filter(|o| o.attached.is_empty());
    match transition(cur, r, ev, focused) {
        Outcome::Drop => {}
        Outcome::Clear => pane.rec = None,
        Outcome::Set(rec) => pane.rec = Some(rec),
    }
}

/// A report from a background session: the transition applies to its
/// server-wide record, which `sync` then projects onto its panes.
fn apply_attached(snap: &mut Snapshot, r: &Report, ev: Event, procs: &mut Procs) {
    let Some(id) = session_of(snap, r, procs) else {
        return;
    };
    let focused = panes_showing(snap, &id, procs)
        .iter()
        .any(|p| snap.focused(p));
    let i = snap.bg.iter().position(|b| b.id == id);
    match transition(i.map(|i| &snap.bg[i].rec), r, ev, focused) {
        Outcome::Drop => {}
        Outcome::Clear => {
            if let Some(i) = i {
                snap.bg.remove(i);
            }
        }
        Outcome::Set(rec) => match i {
            Some(i) => snap.bg[i].rec = rec,
            None => snap.bg.push(Bg { id, rec }),
        },
    }
}

/// The reporting engine's session id, or `None` to ignore the report. The
/// engine must be a live process that a Claude daemon hosts, so a plain
/// `claude` in a terminal outside tmux leaves no record. Of the ids the
/// session may go by (the engine's `--session-id`, its live record's id, the
/// reported `--session`), the first that some `claude attach` names wins,
/// else the first. The engine's live record under another id is renamed:
/// after `/clear` the reported id changes but the attach key does not.
fn session_of(snap: &mut Snapshot, r: &Report, procs: &mut Procs) -> Option<String> {
    let pid = r.pid?;
    let rows = procs.get()?;
    if !attach::hosted(rows, pid) {
        return None;
    }
    let argv = rows
        .iter()
        .find(|row| row.pid == pid)
        .and_then(|row| attach::session_id(&row.args));
    let own = snap
        .bg
        .iter()
        .position(|b| b.rec.pid == pid && owner::alive(pid, &b.rec.start));
    let mut ids: Vec<String> = Vec::new();
    let reported = r.session.clone().filter(|s| attach::valid_id(s));
    for id in [argv, own.map(|i| snap.bg[i].id.clone()), reported]
        .into_iter()
        .flatten()
    {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    let id = {
        let pids = pane_pids(snap);
        ids.iter()
            .find(|id| !attach::attachments(rows, &pids, &[id.as_str()]).is_empty())
            .or(ids.first())?
            .clone()
    };
    if let Some(i) = own
        && snap.bg[i].id != id
    {
        if snap.bg.iter().any(|b| b.id == id) {
            snap.bg.remove(i);
        } else {
            snap.bg[i].id.clone_from(&id);
        }
    }
    Some(id)
}

/// The panes that show session `id` now, from the process table.
fn panes_showing(snap: &Snapshot, id: &str, procs: &mut Procs) -> Vec<String> {
    procs
        .get()
        .map(|rows| {
            attach::attachments(rows, &pane_pids(snap), &[id])
                .into_iter()
                .map(|(p, _)| p)
                .collect()
        })
        .unwrap_or_default()
}

fn pane_pids(snap: &Snapshot) -> Vec<(u32, &str)> {
    snap.panes
        .iter()
        .filter(|p| !p.dead)
        .map(|p| (p.pid, p.id.as_str()))
        .collect()
}

/// Panes that carry a background session's marker: (pane, short id).
fn marked(snap: &Snapshot) -> Vec<(String, String)> {
    snap.panes
        .iter()
        .filter_map(|p| {
            let r = p.rec.as_ref()?;
            (!r.attached.is_empty()).then(|| (p.id.clone(), r.attached.clone()))
        })
        .collect()
}

/// Focus marks a pane seen. On a pane that shows a background session, it
/// marks the session seen, and so all of its panes.
pub fn apply_focus(snap: &mut Snapshot, pane: &str) {
    let focused = snap.focused(pane);
    let Some(p) = snap.pane_mut(pane) else {
        return;
    };
    let Some(rec) = &mut p.rec else {
        return;
    };
    let seen = |st: state::PaneState| state::apply(Some(st), Event::Focus, focused).unwrap_or(st);
    if rec.attached.is_empty() {
        rec.st = seen(rec.st);
        return;
    }
    let marker = rec.attached.clone();
    if let Some(b) = snap.bg.iter_mut().find(|b| attach::short(&b.id) == marker) {
        b.rec.st = seen(b.rec.st);
    }
}

/// After a command's change: drops background records whose owner is dead
/// or unknown, stamps the rest, projects them onto the panes that show
/// them, then drops dead pane state.
fn sync(snap: &mut Snapshot, find: Find, procs: &mut Procs, now: u64) {
    snap.bg
        .retain(|b| b.rec.pid != 0 && owner::alive(b.rec.pid, &b.rec.start));
    for b in &mut snap.bg {
        let old = snap.bg_orig.iter().find(|o| o.id == b.id);
        if b.rec.since == 0 || old.is_none_or(|o| o.rec.st.display() != b.rec.st.display()) {
            b.rec.since = now;
        }
    }
    let shown = shown(snap, find, procs);
    project(snap, &shown);
    gc(snap);
}

/// (pane, session id) for each pane that shows a background session.
fn shown(snap: &Snapshot, find: Find, procs: &mut Procs) -> Vec<(String, String)> {
    if snap.bg.is_empty() {
        return Vec::new();
    }
    if find == Find::Ps
        && let Some(rows) = procs.get()
    {
        let ids: Vec<&str> = snap.bg.iter().map(|b| b.id.as_str()).collect();
        return attach::attachments(rows, &pane_pids(snap), &ids);
    }
    marked(snap)
        .into_iter()
        .filter_map(|(p, m)| {
            let b = snap.bg.iter().find(|b| attach::short(&b.id) == m)?;
            Some((p, b.id.clone()))
        })
        .collect()
}

/// Copies each background record onto the panes that show it, and clears
/// panes that show a session no longer. A pane where a live session runs
/// directly keeps that session's state.
fn project(snap: &mut Snapshot, shown: &[(String, String)]) {
    for p in &mut snap.panes {
        let bg = shown
            .iter()
            .find(|(pane, _)| *pane == p.id)
            .and_then(|(_, id)| snap.bg.iter().find(|b| b.id == *id));
        match bg {
            Some(b) => {
                let direct = p.rec.as_ref().is_some_and(|r| {
                    r.attached.is_empty() && r.pid != 0 && owner::alive(r.pid, &r.start)
                });
                if !direct {
                    p.rec = Some(Rec {
                        attached: attach::short(&b.id).to_owned(),
                        ..b.rec.clone()
                    });
                }
            }
            None => {
                if p.rec.as_ref().is_some_and(|r| !r.attached.is_empty()) {
                    p.rec = None;
                }
            }
        }
    }
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

const PANE_OPTS: [&str; 11] = [
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
    "@agent_pane_attached",
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
                // A background session's panes show its own time in state.
                let since = if !r.attached.is_empty() && r.since != 0 {
                    r.since
                } else if old != new || p.orig.is_none() {
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
                    r.attached.clone(),
                ];
                let was_attached = p.orig.as_ref().is_some_and(|o| !o.attached.is_empty());
                for (o, v) in PANE_OPTS.iter().zip(vals) {
                    // Only a pane that showed a background session has a
                    // marker to unset.
                    if *o == "@agent_pane_attached" && v.is_empty() {
                        if was_attached {
                            cmds.push(["set-option", "-pu", "-t", &p.id, o]);
                        }
                        continue;
                    }
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

    if snap.bg != snap.bg_orig || snap.bg_restored {
        match snapshot::format_bg(&snap.bg) {
            v if v.is_empty() => cmds.push(["set-option", "-gu", "@agent_bg"]),
            v => cmds.push(["set-option", "-g", "@agent_bg", &v]),
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

/// e.g. "󰂞 1  2": a count per shown state, most urgent first. A
/// background session counts once however many panes show it.
pub fn summary_text(snap: &Snapshot) -> String {
    let mut s = Summary::default();
    let mut seen: Vec<&str> = Vec::new();
    for p in &snap.panes {
        if let Some(r) = &p.rec
            && !r.attached.is_empty()
        {
            if seen.contains(&r.attached.as_str()) {
                continue;
            }
            seen.push(&r.attached);
        }
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
            pid: 0,
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
            bg: Vec::new(),
            bg_orig: Vec::new(),
            bg_restored: false,
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
        apply_report(
            &mut s,
            &report("claim", "%1", me()),
            &mut Procs::fixed(Vec::new()),
        );
        let r = s.pane("%1").unwrap().rec.clone().unwrap();
        assert_eq!(r.pid, me());
        assert_eq!(r.start, owner::start_time(me()).unwrap());
        assert_eq!(r.st, PaneState::IDLE);
    }

    #[test]
    fn foreign_report_is_dropped_while_owner_lives() {
        let mut s = snap(vec![pane("%1", "@1")]);
        apply_report(
            &mut s,
            &report("working", "%1", me()),
            &mut Procs::fixed(Vec::new()),
        );
        for ev in ["claim", "release", "done"] {
            apply_report(
                &mut s,
                &report(ev, "%1", me() + 1),
                &mut Procs::fixed(Vec::new()),
            );
            assert_eq!(st(&s, "%1").unwrap().lp, Loop::Working, "{ev}");
        }
    }

    #[test]
    fn reclaim_by_owner_keeps_state() {
        let mut s = snap(vec![pane("%1", "@1")]);
        apply_report(
            &mut s,
            &report("working", "%1", me()),
            &mut Procs::fixed(Vec::new()),
        );
        apply_report(
            &mut s,
            &report("claim", "%1", me()),
            &mut Procs::fixed(Vec::new()),
        );
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
            attached: String::new(),
        });
        let mut s = snap(vec![p]);
        apply_report(
            &mut s,
            &report("done", "%1", me()),
            &mut Procs::fixed(Vec::new()),
        );
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
            attached: String::new(),
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
        apply_report(
            &mut s,
            &report("working", "%1", me()),
            &mut Procs::fixed(Vec::new()),
        );
        apply_report(
            &mut s,
            &report("done", "%1", me()),
            &mut Procs::fixed(Vec::new()),
        );
        assert_eq!(st(&s, "%1").unwrap().lp, Loop::Idle);
    }

    #[test]
    fn plan_writes_rollup_borders_and_summary() {
        let mut s = snap(vec![pane("%1", "@1"), pane("%2", "@1"), pane("%3", "@2")]);
        apply_report(
            &mut s,
            &report("working", "%1", me()),
            &mut Procs::fixed(Vec::new()),
        );
        apply_report(
            &mut s,
            &report("ask", "%2", me()),
            &mut Procs::fixed(Vec::new()),
        );
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
            attached: String::new(),
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
            attached: String::new(),
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

    const ID: &str = "686fd2bf-cb86-42e0-a42c-a122da5cf5ec";

    /// Panes %1 and %2 run `claude attach 686fd2bf` (pane pids 100 and 200,
    /// through a wrapper in %2); %3 runs a shell. The engine is `engine`,
    /// under the daemon's pty host 90, beside spare engine 95.
    fn attached_world(engine: u32) -> (Snapshot, Vec<attach::PsRow>) {
        let mut panes = vec![pane("%1", "@1"), pane("%2", "@2"), pane("%3", "@2")];
        for (p, pid) in panes.iter_mut().zip([100, 200, 300]) {
            p.pid = pid;
        }
        let rows = attach::parse_ps(&format!(
            "90 1 /opt/cc/claude --bg-pty-host /tmp/p.sock 1 1 -- /opt/cc/claude\n\
             {engine} 90 /opt/cc/claude --session-id {ID} --fork-session\n\
             95 90 claude bg-spare --bg-spare /tmp/c.sock\n\
             100 1 /bin/zsh\n101 100 claude attach 686fd2bf\n\
             200 1 /bin/sh\n201 200 sh /x/bin/claude attach 686fd2bf\n\
             202 201 /x/claude attach 686fd2bf\n\
             300 1 /bin/sh\n301 300 tmux attach -t 686fd2bf\n"
        ));
        (snap(panes), rows)
    }

    fn bg_report(event: &str, pid: u32) -> Report {
        Report {
            event: event.into(),
            attached: true,
            pid: Some(pid),
            proto: Some("1".into()),
            ..Report::default()
        }
    }

    /// One command: the report, then what `with_snapshot` runs before the
    /// write.
    fn run(s: &mut Snapshot, rows: &[attach::PsRow], r: &Report) {
        let mut procs = Procs::fixed(rows.to_vec());
        apply_report(s, r, &mut procs);
        sync(s, Find::Ps, &mut procs, 50);
    }

    /// The write, then the next command's read: orig catches up.
    fn written(s: &mut Snapshot) {
        s.bg_orig = s.bg.clone();
        for p in &mut s.panes {
            p.orig = p.rec.clone();
        }
    }

    fn marker(s: &Snapshot, id: &str) -> Option<String> {
        s.pane(id).unwrap().rec.as_ref().map(|r| r.attached.clone())
    }

    #[test]
    fn attached_report_projects_onto_the_panes_that_show_it() {
        let (mut s, rows) = attached_world(me());
        run(&mut s, &rows, &bg_report("working", me()));
        assert_eq!(s.bg.len(), 1);
        assert_eq!(s.bg[0].id, ID);
        assert_eq!(s.bg[0].rec.pid, me());
        assert_eq!(s.bg[0].rec.since, 50);
        for p in ["%1", "%2"] {
            assert_eq!(st(&s, p).unwrap().lp, Loop::Working, "{p}");
            assert_eq!(marker(&s, p).as_deref(), Some("686fd2bf"), "{p}");
        }
        assert_eq!(st(&s, "%3"), None, "tmux attach is not a client");

        let args = plan(&s, 50).args().join(" ");
        assert!(
            args.contains(&format!(
                "set-option -g @agent_bg {ID}|working|0|0|{}|",
                me()
            )),
            "{args}"
        );
        assert!(
            args.contains("set-option -p -t %2 @agent_pane_attached 686fd2bf"),
            "{args}"
        );
        assert!(
            args.contains("set-option -w -t @2 @agent_window_state working"),
            "{args}"
        );
        assert!(
            args.contains("@agent_summary W 1"),
            "one session in two panes counts once: {args}"
        );

        // The next reconcile, with nothing changed, writes nothing.
        let mut s = written_as_read(&s, &args);
        sync(&mut s, Find::Ps, &mut Procs::fixed(rows), 90);
        assert!(plan(&s, 90).is_empty(), "{:?}", plan(&s, 90).args());
    }

    /// The snapshot the next command reads after `plan`'s write.
    fn written_as_read(s: &Snapshot, args: &str) -> Snapshot {
        let mut s = s.clone();
        for p in &mut s.panes {
            if let Some(r) = &mut p.rec {
                // plan stores a background pane's own since.
                assert!(args.contains(&format!("-t {} @agent_pane_since {}", p.id, r.since)));
            }
        }
        written(&mut s);
        let w = |p: &Pane| display_of(&p.rec);
        let wins: Vec<(String, Display)> =
            s.panes.iter().map(|p| (p.window.clone(), w(p))).collect();
        for p in &mut s.panes {
            let best = state::rollup(wins.iter().filter(|(w, _)| *w == p.window).map(|(_, d)| *d));
            if let Some(i) = best.and_then(slot) {
                p.win_state = best.unwrap().as_str().into();
                p.win_color = s.config.colors[i].clone();
                p.win_icon = format!("{} ", s.config.icons[i]);
            }
        }
        s.global.summary = summary_text(&s);
        s
    }

    #[test]
    fn detached_panes_clear_and_new_ones_project() {
        let (mut s, rows) = attached_world(me());
        run(&mut s, &rows, &bg_report("done", me()));
        written(&mut s);
        // %1's client exits; %3 starts one.
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|r| r.pid != 101)
            .chain(attach::parse_ps("302 300 claude attach 686fd2bf-cb86"))
            .collect();
        let mut procs = Procs::fixed(rows);
        sync(&mut s, Find::Ps, &mut procs, 60);
        assert_eq!(st(&s, "%1"), None);
        assert_eq!(st(&s, "%3").unwrap().lp, Loop::Done);
        assert_eq!(s.pane("%3").unwrap().rec.as_ref().unwrap().since, 50);
        let args = plan(&s, 60).args().join(" ");
        assert!(
            args.contains("set-option -pu -t %1 @agent_pane_attached"),
            "{args}"
        );
        assert!(!args.contains("@agent_bg"), "record unchanged: {args}");
    }

    #[test]
    fn markers_stand_in_when_ps_fails() {
        let (mut s, rows) = attached_world(me());
        run(&mut s, &rows, &bg_report("working", me()));
        written(&mut s);
        let mut procs = Procs::failed();
        // With no process table the engine cannot be checked: the report is
        // ignored, and the panes keep what they show.
        apply_report(&mut s, &bg_report("ask", me()), &mut procs);
        sync(&mut s, Find::Ps, &mut procs, 60);
        assert_eq!(s.bg, s.bg_orig);
        for p in ["%1", "%2"] {
            assert_eq!(st(&s, p).unwrap().lp, Loop::Working, "{p}");
        }
        // With no record left, markers clear without a ps call.
        s.bg.clear();
        sync(&mut s, Find::Markers, &mut Procs::failed(), 70);
        assert!(s.panes.iter().all(|p| p.rec.is_none()));
    }

    #[test]
    fn a_session_running_in_the_pane_wins() {
        let (mut s, rows) = attached_world(me());
        let other = Proc::spawn();
        s.pane_mut("%1").unwrap().rec = Some(Rec {
            pid: other.pid(),
            ..rec(Loop::Idle)
        });
        run(&mut s, &rows, &bg_report("working", me()));
        assert_eq!(st(&s, "%1").unwrap().lp, Loop::Idle, "not overwritten");
        assert_eq!(st(&s, "%2").unwrap().lp, Loop::Working);

        // And a direct report takes a pane that shows a background session.
        let mut r = report("done", "%2", other.pid());
        r.proto = None;
        run(&mut s, &rows, &r);
        let p = s.pane("%2").unwrap().rec.clone().unwrap();
        assert_eq!((p.pid, p.attached.as_str()), (other.pid(), ""));
    }

    #[test]
    fn focus_on_an_attached_pane_marks_the_session_seen() {
        let (mut s, rows) = attached_world(me());
        run(&mut s, &rows, &bg_report("working", me()));
        run(&mut s, &rows, &bg_report("done", me()));
        written(&mut s);
        assert_eq!(st(&s, "%2").unwrap().lp, Loop::Done);
        apply_focus(&mut s, "%1");
        sync(&mut s, Find::Markers, &mut Procs::failed(), 60);
        assert_eq!(s.bg[0].rec.st.lp, Loop::Idle);
        for p in ["%1", "%2"] {
            assert_eq!(st(&s, p).unwrap().lp, Loop::Idle, "{p}");
        }
    }

    #[test]
    fn done_while_an_attached_pane_is_focused_is_seen() {
        let (mut s, rows) = attached_world(me());
        s.clients.push(Client {
            name: "c".into(),
            pane: "%2".into(),
            focused: true,
            height: 40,
            width: 80,
        });
        run(&mut s, &rows, &bg_report("working", me()));
        run(&mut s, &rows, &bg_report("done", me()));
        assert_eq!(st(&s, "%1").unwrap().lp, Loop::Idle);
    }

    #[test]
    fn release_and_dead_owners_drop_the_record_and_its_panes() {
        let (mut s, rows) = attached_world(me());
        run(&mut s, &rows, &bg_report("working", me()));
        written(&mut s);
        run(&mut s, &rows, &bg_report("release", me() + 1));
        assert_eq!(s.bg.len(), 1, "a non-owner's release does nothing");
        run(&mut s, &rows, &bg_report("release", me()));
        assert!(s.bg.is_empty());
        assert!(s.panes.iter().all(|p| p.rec.is_none()));
        assert!(
            plan(&s, 60)
                .args()
                .join(" ")
                .contains("set-option -gu @agent_bg")
        );

        let mut engine = Proc::spawn();
        let (mut s, rows) = attached_world(engine.pid());
        run(&mut s, &rows, &bg_report("working", engine.pid()));
        assert_eq!(st(&s, "%1").unwrap().lp, Loop::Working);
        engine.kill();
        sync(&mut s, Find::Ps, &mut Procs::fixed(rows), 60);
        assert!(s.bg.is_empty());
        assert!(s.panes.iter().all(|p| p.rec.is_none()));
    }

    #[test]
    fn the_engine_session_comes_from_argv_then_the_report() {
        let (mut s, rows) = attached_world(me());
        let mut procs = Procs::fixed(rows.clone());
        let r = Report {
            session: Some("abcdef12-x".into()),
            ..bg_report("claim", me())
        };
        assert_eq!(session_of(&mut s, &r, &mut procs).as_deref(), Some(ID));
        // A spare engine has no --session-id: the reported one.
        let spare = Report {
            pid: Some(95),
            ..r.clone()
        };
        assert_eq!(
            session_of(&mut s, &spare, &mut procs).as_deref(),
            Some("abcdef12-x")
        );
        let bad = Report {
            session: Some("bad id".into()),
            ..spare.clone()
        };
        assert_eq!(session_of(&mut s, &bad, &mut procs), None);
        assert!(s.bg.is_empty(), "session_of only names a record");
    }

    #[test]
    fn engines_no_daemon_hosts_get_no_record() {
        let (mut s, mut rows) = attached_world(me() + 1);
        // A plain `claude` in a terminal outside tmux, and a report with no
        // pid.
        rows.extend(attach::parse_ps(&format!(
            "7 1 /bin/zsh\n{} 7 /opt/cc/claude --session-id {ID}",
            me()
        )));
        let r = Report {
            session: Some(ID.into()),
            ..bg_report("working", me())
        };
        run(&mut s, &rows, &r);
        run(&mut s, &rows, &Report { pid: None, ..r });
        assert!(s.bg.is_empty());
        assert!(s.panes.iter().all(|p| p.rec.is_none()));
    }

    #[test]
    fn a_live_record_follows_the_id_that_a_client_names() {
        let spare = Proc::spawn();
        let (mut s, rows) = attached_world(me());
        let rows: Vec<_> = rows
            .into_iter()
            .map(|mut r| {
                if r.pid == 95 {
                    r.pid = spare.pid();
                }
                r
            })
            .collect();
        let from = |session: &str| Report {
            pid: Some(spare.pid()),
            session: Some(session.into()),
            ..bg_report("working", 0)
        };
        // The spare's first report keys its record; after /clear the
        // reported id changes, and the record keeps the first.
        run(&mut s, &rows, &from("aaaaaaaa-1"));
        run(&mut s, &rows, &from("bbbbbbbb-2"));
        assert_eq!(s.bg.len(), 1);
        assert_eq!(s.bg[0].id, "aaaaaaaa-1");
        // Unless a client attaches by the reported id: then it moves.
        let rows: Vec<_> = rows
            .into_iter()
            .chain(attach::parse_ps("302 300 claude attach bbbbbbbb"))
            .collect();
        run(&mut s, &rows, &from("bbbbbbbb-2"));
        assert_eq!(s.bg.len(), 1);
        assert_eq!(s.bg[0].id, "bbbbbbbb-2");
        assert_eq!(st(&s, "%3").unwrap().lp, Loop::Working);

        // The engine's --session-id wins over a record keyed by an old id.
        let (mut s, rows) = attached_world(me());
        s.bg.push(Bg {
            id: "cccccccc-3".into(),
            rec: Rec {
                pid: me(),
                start: owner::start_time(me()).unwrap(),
                ..rec(Loop::Done)
            },
        });
        run(&mut s, &rows, &bg_report("working", me()));
        assert_eq!(s.bg.len(), 1);
        assert_eq!(
            (s.bg[0].id.as_str(), s.bg[0].rec.st.lp),
            (ID, Loop::Working)
        );
    }

    #[test]
    fn background_records_survive_a_server_restart_on_disk() {
        let dir = std::env::temp_dir().join(format!("ta-unit-bg-{}", me()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.tmux-agent.bg");
        let (mut s, rows) = attached_world(me());
        run(&mut s, &rows, &bg_report("done", me()));
        save(&path, &s.bg).unwrap();

        // A new server: no @agent_bg, no markers.
        let (mut fresh, _) = attached_world(me());
        restore(&mut fresh, &path);
        assert!(fresh.bg_restored);
        assert_eq!(fresh.bg, s.bg, "since is kept too");
        sync(&mut fresh, Find::Ps, &mut Procs::fixed(rows), 90);
        assert_eq!(st(&fresh, "%1").unwrap().lp, Loop::Done);
        let args = plan(&fresh, 90).args().join(" ");
        assert!(args.contains("set-option -g @agent_bg "), "{args}");

        // A server that has records ignores the copy.
        let mut other = fresh.clone();
        other.bg_orig.clear();
        other.bg_orig.push(Bg {
            id: "x".into(),
            rec: rec(Loop::Idle),
        });
        other.bg_restored = false;
        let before = other.bg.clone();
        restore(&mut other, &path);
        assert!(!other.bg_restored);
        assert_eq!(other.bg, before);

        // No records left: the copy goes, and no copy restores nothing.
        save(&path, &[]).unwrap();
        assert!(!path.exists());
        save(&path, &[]).unwrap();
        let (mut fresh, _) = attached_world(me());
        restore(&mut fresh, &path);
        assert!(!fresh.bg_restored && fresh.bg.is_empty());
        let _ = std::fs::remove_dir(&dir);
    }

    fn rec(lp: Loop) -> Rec {
        Rec {
            st: PaneState {
                lp,
                asks: false,
                bg: 0,
            },
            pid: 0,
            start: String::new(),
            session: String::new(),
            since: 1,
            proto: String::new(),
            plugin: String::new(),
            attached: String::new(),
        }
    }

    /// A live process other than the test, killed on drop.
    struct Proc(std::process::Child);

    impl Proc {
        fn spawn() -> Proc {
            Proc(
                std::process::Command::new("sleep")
                    .arg("60")
                    .spawn()
                    .unwrap(),
            )
        }
        fn pid(&self) -> u32 {
            self.0.id()
        }
        fn kill(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    impl Drop for Proc {
        fn drop(&mut self) {
            self.kill();
        }
    }

    #[test]
    fn summary_dots_colour_each_count() {
        let mut s = snap(vec![pane("%1", "@1"), pane("%2", "@1"), pane("%3", "@2")]);
        s.config.summary_dots = true;
        assert_eq!(summary_text(&s), "", "nothing shows when no agent does");
        let mut procs = Procs::fixed(Vec::new());
        apply_report(&mut s, &report("ask", "%1", me()), &mut procs);
        apply_report(&mut s, &report("working", "%2", me()), &mut procs);
        apply_report(&mut s, &report("working", "%3", me()), &mut procs);
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
