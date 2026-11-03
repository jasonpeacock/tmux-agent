//! Everything a command needs from the server, read in one tmux call.

use crate::state::{Loop, PaneState};
use crate::tmux::Tmux;
use std::io;

/// The `@tmux-agent-*` options, with defaults filled in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The binary path hooks run (`@tmux-agent-bin`), empty when unset.
    pub bin: String,
    pub style_mode: bool,
    pub pane_border: bool,
    /// Icons and colours: needs-input, done, working.
    pub icons: [String; 3],
    pub colors: [String; 3],
    /// `@tmux-agent-summary-style dots`: a dot in each state's colour in the
    /// summary, in place of the icons.
    pub summary_dots: bool,
}

/// Server-wide values.
#[derive(Clone, Debug, Default)]
pub struct Global {
    pub summary: String,
    pub version: String,
    /// The global window-status-style and window-status-current-style.
    pub status_style: String,
    pub status_current_style: String,
}

#[derive(Clone, Debug)]
pub struct Client {
    pub name: String,
    pub pane: String,
    pub focused: bool,
    /// 0 for a control client, which has no size.
    pub height: u32,
    pub width: u32,
}

/// The agent state stored on a pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rec {
    pub st: PaneState,
    pub pid: u32,
    pub start: String,
    pub session: String,
    pub since: u64,
    pub proto: String,
    pub plugin: String,
}

#[derive(Clone, Debug)]
pub struct Pane {
    pub id: String,
    pub window: String,
    pub dead: bool,
    pub session_name: String,
    pub window_index: String,
    pub pane_index: String,
    pub window_name: String,
    pub path: String,
    /// What the pane stores now, and what it will store after `sync`.
    pub orig: Option<Rec>,
    pub rec: Option<Rec>,
    /// The window options as read: state, colour, icon.
    pub win_state: String,
    pub win_color: String,
    pub win_icon: String,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub config: Config,
    pub global: Global,
    pub clients: Vec<Client>,
    /// One entry per pane, in server order, linked windows deduplicated.
    pub panes: Vec<Pane>,
}

const G_FMT: &str = concat!(
    "\t#{@tmux-agent-bin}\t#{@tmux-agent-tab-mode}\t#{@tmux-agent-pane-border}",
    "\t#{@tmux-agent-icon-needs-input}\t#{@tmux-agent-icon-done}\t#{@tmux-agent-icon-working}",
    "\t#{E:@tmux-agent-color-needs-input}\t#{E:@tmux-agent-color-done}\t#{E:@tmux-agent-color-working}",
    "\t#{version}\t#{@agent_summary}\t#{@tmux-agent-summary-style}",
);
const C_FMT: &str =
    "\t#{client_name}\t#{pane_id}\t#{client_flags}\t#{client_height}\t#{client_width}";
// Free-text fields go last: the final field may hold tabs.
const P_FMT: &str = concat!(
    "\t#{pane_id}\t#{window_id}\t#{pane_dead}\t#{window_index}\t#{pane_index}",
    "\t#{@agent_pane_loop}\t#{@agent_pane_asks}\t#{@agent_pane_bg}",
    "\t#{@agent_pane_pid}\t#{@agent_pane_pid_start}\t#{@agent_pane_since}\t#{@agent_pane_session}",
    "\t#{@agent_pane_proto}\t#{@agent_pane_plugin}",
    "\t#{@agent_window_state}\t#{@agent_tab_color}\t#{@agent_tab_icon}",
    "\t#{session_name}\t#{window_name}\t#{pane_current_path}",
);

pub const DEFAULT_ICONS: [&str; 3] = ["\u{f009e}", "\u{f058}", "\u{f110}"];
pub const DEFAULT_COLORS: [&str; 3] = ["#f38ba8", "#a6e3a1", "#f9e2af"];

impl Snapshot {
    /// Reads the server. `Ok(None)` when it has no sessions.
    pub fn read(tmux: &Tmux) -> io::Result<Option<Snapshot>> {
        let tag = Tag::new();
        let (g, c, p) = (
            tag.fmt('G', G_FMT),
            tag.fmt('C', C_FMT),
            tag.fmt('P', P_FMT),
        );
        let out = tmux.run(&[
            "list-sessions",
            "-F",
            &g,
            ";",
            "list-clients",
            "-F",
            &c,
            ";",
            "list-panes",
            "-a",
            "-F",
            &p,
            ";",
            "show-options",
            "-gwv",
            "window-status-style",
            ";",
            "show-options",
            "-gwv",
            "window-status-current-style",
        ])?;
        Ok(parse(&out, &tag))
    }

    #[cfg(test)]
    pub fn pane(&self, id: &str) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    pub fn pane_mut(&mut self, id: &str) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    /// True when some client has `pane` focused.
    pub fn focused(&self, pane: &str) -> bool {
        self.clients.iter().any(|c| c.focused && c.pane == pane)
    }
}

/// A per-call marker that starts each record line. Pane paths are printed raw
/// and may hold newlines; the marker keeps such a path from forging records.
pub struct Tag(String);

impl Tag {
    fn new() -> Tag {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        Tag(format!("{:x}{:08x}", std::process::id(), nanos))
    }

    fn fmt(&self, kind: char, fmt: &str) -> String {
        format!("{kind}{}{fmt}", self.0)
    }

    /// The rest of `line` if it is a record of `kind`.
    fn strip<'a>(&self, kind: char, line: &'a str) -> Option<&'a str> {
        line.strip_prefix(kind)?
            .strip_prefix(self.0.as_str())?
            .strip_prefix('\t')
    }
}

fn parse(out: &str, tag: &Tag) -> Option<Snapshot> {
    let mut config = None;
    let mut global = Global::default();
    let mut clients = Vec::new();
    let mut panes: Vec<Pane> = Vec::new();
    for line in out.lines() {
        if let Some(rest) = tag.strip('G', line) {
            if config.is_none() {
                let (c, g) = parse_global(rest);
                config = Some(c);
                global = g;
            }
        } else if let Some(rest) = tag.strip('C', line) {
            clients.extend(parse_client(rest));
        } else if let Some(rest) = tag.strip('P', line)
            && let Some(p) = parse_pane(rest)
            && !panes.iter().any(|q| q.id == p.id)
        {
            panes.push(p);
        }
    }
    // The two show-options values come last. Neither can hold a newline.
    let mut tail = out.lines().rev();
    global.status_current_style = tail.next().unwrap_or_default().to_owned();
    global.status_style = tail.next().unwrap_or_default().to_owned();
    Some(Snapshot {
        config: config?,
        global,
        clients,
        panes,
    })
}

fn or_default(v: &str, d: &str) -> String {
    if v.is_empty() {
        d.to_owned()
    } else {
        v.to_owned()
    }
}

fn parse_global(rest: &str) -> (Config, Global) {
    let f: Vec<&str> = rest.splitn(12, '\t').collect();
    let get = |i: usize| f.get(i).copied().unwrap_or("");
    let config = Config {
        bin: get(0).to_owned(),
        style_mode: get(1) == "style",
        pane_border: get(2) != "off",
        icons: [0, 1, 2].map(|i| or_default(get(3 + i), DEFAULT_ICONS[i])),
        colors: [0, 1, 2].map(|i| or_default(get(6 + i), DEFAULT_COLORS[i])),
        summary_dots: get(11) == "dots",
    };
    let global = Global {
        version: get(9).to_owned(),
        summary: get(10).to_owned(),
        ..Global::default()
    };
    (config, global)
}

fn parse_client(rest: &str) -> Option<Client> {
    let f: Vec<&str> = rest.split('\t').collect();
    let [name, pane, flags, height, width] = f[..] else {
        return None;
    };
    Some(Client {
        name: name.to_owned(),
        pane: pane.to_owned(),
        focused: flags.split(',').any(|f| f == "focused"),
        height: height.parse().unwrap_or(0),
        width: width.parse().unwrap_or(0),
    })
}

fn parse_pane(rest: &str) -> Option<Pane> {
    let f: Vec<&str> = rest.splitn(20, '\t').collect();
    if f.len() < 20 {
        return None;
    }
    let rec = parse_rec(&f[5..14]);
    Some(Pane {
        id: f[0].to_owned(),
        window: f[1].to_owned(),
        dead: f[2] == "1",
        window_index: f[3].to_owned(),
        pane_index: f[4].to_owned(),
        orig: rec.clone(),
        rec,
        win_state: f[14].to_owned(),
        win_color: f[15].to_owned(),
        win_icon: f[16].to_owned(),
        session_name: f[17].to_owned(),
        window_name: f[18].to_owned(),
        path: f[19].to_owned(),
    })
}

/// loop, asks, bg, pid, pid_start, since, session, proto, plugin.
fn parse_rec(f: &[&str]) -> Option<Rec> {
    let lp = Loop::parse(f[0])?;
    Some(Rec {
        st: PaneState {
            lp,
            asks: f[1] == "1",
            bg: f[2].parse().unwrap_or(0),
        },
        pid: f[3].parse().unwrap_or(0),
        start: f[4].to_owned(),
        since: f[5].parse().unwrap_or(0),
        session: f[6].to_owned(),
        proto: f[7].to_owned(),
        plugin: f[8].to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "\
Gt\t/bin/ta\t\toff\t\t\tW\t#ff0000\t\t\t3.8\tS\tdots
Gt\tignored
Ct\t/dev/ttys001\t%1\tattached,focused,UTF-8\t50\t120
Ct\t/dev/ttys002\t%2\tattached\t40\t80
Pt\t%1\t@1\t0\t1\t1\tdone\t0\t0\t42\t1.5\t100\tsid\t1\t0.1.0\tdone\t#a6e3a1\t \tmain\tzsh\t/home/u
Pt\t%2\t@1\t0\t1\t2\t\t\t\t\t\t\t\t\t\tdone\t#a6e3a1\t \tmain\tvim\t/a\tb
Pt\t%1\t@1\t0\t1\t1\tdone\t0\t0\t42\t1.5\t100\tsid\t1\t0.1.0\tdone\t#a6e3a1\t \tgrouped\tzsh\t/home/u
default
underscore
";

    fn tag() -> Tag {
        Tag("t".into())
    }

    #[test]
    fn parses_every_record() {
        let s = parse(OUT, &tag()).unwrap();
        assert_eq!(s.config.bin, "/bin/ta");
        assert!(!s.config.style_mode);
        assert!(!s.config.pane_border);
        assert_eq!(
            s.config.icons,
            [
                DEFAULT_ICONS[0].to_owned(),
                DEFAULT_ICONS[1].into(),
                "W".into()
            ]
        );
        assert_eq!(s.config.colors[0], "#ff0000");
        assert_eq!(s.config.colors[1], DEFAULT_COLORS[1]);
        assert_eq!(s.global.version, "3.8");
        assert_eq!(s.global.summary, "S");
        assert!(s.config.summary_dots);
        assert_eq!(s.global.status_style, "default");
        assert_eq!(s.global.status_current_style, "underscore");

        assert_eq!(s.clients.len(), 2);
        assert_eq!((s.clients[0].height, s.clients[0].width), (50, 120));
        assert!(s.focused("%1"));
        assert!(!s.focused("%2"));

        assert_eq!(s.panes.len(), 2, "grouped session row is deduplicated");
        let p = &s.panes[0];
        let r = p.rec.as_ref().unwrap();
        assert_eq!(
            r.st,
            PaneState {
                lp: Loop::Done,
                asks: false,
                bg: 0
            }
        );
        assert_eq!((r.pid, r.start.as_str(), r.since), (42, "1.5", 100));
        assert_eq!(p.win_state, "done");
        assert!(s.panes[1].rec.is_none());
        assert_eq!(s.panes[1].path, "/a\tb");
    }

    #[test]
    fn newline_in_a_path_cannot_forge_records() {
        // A pane whose path is "/x\nC\tfake..." followed by more lines: none
        // carry this call's tag, so all are dropped.
        let out = "\
Gt\t\t\t\t\t\t\t\t\t\t3.8\t
Pt\t%1\t@1\t0\t1\t1\tworking\t0\t0\t42\t1.5\t100\tsid\t1\t0.1.0\tworking\t#f\tW \tmain\tzsh\t/x
C\tfake\t%1\tfocused\t40\t80
Cu\tfake\t%1\tfocused\t40\t80
fg=blue
bold
";
        let s = parse(out, &tag()).unwrap();
        assert!(s.clients.is_empty());
        assert_eq!(s.panes.len(), 1);
        assert_eq!(s.global.status_style, "fg=blue");
        assert_eq!(s.global.status_current_style, "bold");
    }

    #[test]
    fn no_sessions_is_none() {
        assert!(parse("", &tag()).is_none());
    }
}
