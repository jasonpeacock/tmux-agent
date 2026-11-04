//! Background sessions viewed through `claude attach`: which panes show
//! which session, from one `ps` call. See docs/design.md, "Background
//! sessions (claude attach)".

use crate::fleet;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// An attach token must be at least this long to name a session.
pub const MIN_TOKEN: usize = 8;

/// One row of `ps -A -o pid= -o ppid= -o args=`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PsRow {
    pub pid: u32,
    pub ppid: u32,
    pub args: String,
}

/// The process table, read at most once per command and only when needed,
/// and Claude's config dir, where fleet viewers leave their beacons.
pub struct Procs {
    rows: Option<Option<Vec<PsRow>>>,
    claude: Option<PathBuf>,
}

impl Procs {
    pub fn new() -> Procs {
        Procs {
            rows: None,
            claude: fleet::claude_dir(),
        }
    }

    /// A table that never runs `ps`, and no config dir.
    #[cfg(test)]
    pub fn fixed(rows: Vec<PsRow>) -> Procs {
        Procs {
            rows: Some(Some(rows)),
            claude: None,
        }
    }

    /// A table whose `ps` failed.
    #[cfg(test)]
    pub fn failed() -> Procs {
        Procs {
            rows: Some(None),
            claude: None,
        }
    }

    #[cfg(test)]
    pub fn with_claude(mut self, dir: &Path) -> Procs {
        self.claude = Some(dir.to_owned());
        self
    }

    /// The rows, or `None` when `ps` failed.
    pub fn get(&mut self) -> Option<&[PsRow]> {
        self.rows.get_or_insert_with(scan).as_deref()
    }

    /// This command's own Claude config dir, for records whose engine did
    /// not report one.
    pub fn claude_dir(&self) -> Option<&Path> {
        self.claude.as_deref()
    }
}

/// `-ww`: procps cuts `args` to 80 columns when stdout is not a terminal. A
/// `ps` that rejects `-ww` (BusyBox) gets a second try without it.
fn scan() -> Option<Vec<PsRow>> {
    const COLS: [&str; 7] = ["-A", "-o", "pid=", "-o", "ppid=", "-o", "args="];
    [true, false].into_iter().find_map(|wide| {
        let out = Command::new("ps")
            .args(wide.then_some("-ww"))
            .args(COLS)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        Some(parse_ps(&String::from_utf8_lossy(&out.stdout)))
    })
}

/// Runs the scan now: `None` when `ps` fails. For doctor.
pub fn scan_works() -> bool {
    scan().is_some()
}

pub fn parse_ps(out: &str) -> Vec<PsRow> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            Some(PsRow {
                pid,
                ppid,
                args: it.collect::<Vec<_>>().join(" "),
            })
        })
        .collect()
}

/// The first eight characters of a session id: the key `claude attach`
/// shows, and the pane marker.
pub fn short(id: &str) -> &str {
    id.get(..MIN_TOKEN).unwrap_or(id)
}

/// A session id is kept only if it is safe in an option value.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The value of `--session-id` in an engine's argv, before any `--`.
pub fn session_id(args: &str) -> Option<String> {
    let mut it = args.split_whitespace().take_while(|a| *a != "--");
    while let Some(a) = it.next() {
        let v = match a.strip_prefix("--session-id") {
            Some("") => it.next()?,
            Some(v) => match v.strip_prefix('=') {
                Some(v) => v,
                None => continue,
            },
            None => continue,
        };
        return valid_id(v).then(|| v.to_owned());
    }
    None
}

/// What may run a script named `claude`: `/bin/sh ./claude attach x`, or
/// `node /usr/local/bin/claude attach x` for an npm install.
const LAUNCHERS: [&str; 7] = ["sh", "bash", "dash", "zsh", "env", "node", "bun"];

/// The token in `claude [options] attach <token>`, so an engine whose
/// arguments merely mention "attach" never matches. The command is a word
/// whose file name is `claude`: argv0, the script a launcher runs, or the end
/// of an absolute argv0 path that holds spaces. `attach` is the first word
/// after it that is not an option.
pub fn attach_token(args: &str) -> Option<&str> {
    let words: Vec<&str> = args.split_whitespace().collect();
    let name = |w: &str| w.rsplit('/').next() == Some("claude");
    let k = words.iter().take(4).position(|w| name(w))?;
    let ok = k == 0
        || (k == 1
            && words[0]
                .rsplit('/')
                .next()
                .is_some_and(|w| LAUNCHERS.contains(&w)))
        || (words[0].starts_with('/') && !words[1..k].iter().any(|w| w.starts_with('-')));
    if !ok {
        return None;
    }
    let mut rest = words[k + 1..]
        .iter()
        .take_while(|a| **a != "--")
        .filter(|a| !a.starts_with('-'));
    if *rest.next()? != "attach" {
        return None;
    }
    rest.next().copied()
}

/// True when `pid` runs under a Claude daemon: its parent is a pty host
/// (`--bg-pty-host`). Only such an engine gets a background record, so a
/// plain `claude` in a terminal outside tmux leaves none.
pub fn hosted(rows: &[PsRow], pid: u32) -> bool {
    let Some(r) = rows.iter().find(|r| r.pid == pid) else {
        return false;
    };
    rows.iter().find(|p| p.pid == r.ppid).is_some_and(|p| {
        p.args
            .split_whitespace()
            .take_while(|a| *a != "--")
            .any(|a| a == "--bg-pty-host")
    })
}

/// True when `token` names session `id`.
pub fn matches(id: &str, token: &str) -> bool {
    token.len() >= MIN_TOKEN && id.starts_with(token)
}

/// The panes that show a session: for each `claude attach` process whose
/// token names one of `ids`, the pane whose `pane_pid` is the process or an
/// ancestor of it. Returns (pane id, session id) pairs, one per pane.
pub fn attachments(rows: &[PsRow], panes: &[(u32, &str)], ids: &[&str]) -> Vec<(String, String)> {
    let parent = parents(rows);
    let mut out: Vec<(String, String)> = Vec::new();
    for r in rows {
        let Some(token) = attach_token(&r.args) else {
            continue;
        };
        let Some(id) = ids.iter().find(|id| matches(id, token)) else {
            continue;
        };
        if let Some(pane) = walk(&parent, panes, r.pid)
            && !out.iter().any(|(p, _)| p == pane)
        {
            out.push((pane.to_owned(), (*id).to_owned()));
        }
    }
    out
}

/// The pane whose `pane_pid` is `pid` or an ancestor of it.
pub fn pane_of<'a>(rows: &[PsRow], panes: &[(u32, &'a str)], pid: u32) -> Option<&'a str> {
    walk(&parents(rows), panes, pid)
}

fn parents(rows: &[PsRow]) -> HashMap<u32, u32> {
    rows.iter().map(|r| (r.pid, r.ppid)).collect()
}

fn walk<'a>(parent: &HashMap<u32, u32>, panes: &[(u32, &'a str)], mut pid: u32) -> Option<&'a str> {
    // Bounded by the table's size, so a PID cycle cannot loop forever.
    for _ in 0..=parent.len() {
        if let Some((_, pane)) = panes.iter().find(|(p, _)| *p == pid && pid != 0) {
            return Some(pane);
        }
        match parent.get(&pid) {
            Some(&pp) if pp > 1 && pp != pid => pid = pp,
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "686fd2bf-cb86-42e0-a42c-a122da5cf5ec";
    const CC: &str = "/Users/u/.toolbox/tools/claude-code/2.1.316.1544";

    /// The process tree seen live (see docs/design.md), plus decoys.
    fn table() -> String {
        [
            "    1     0 /sbin/launchd".to_owned(),
            format!("25723     1 {CC}/claude daemon run --origin transient --spawned-by {{\"pid\":25372}}"),
            format!("25762 25723 {CC}/claude --bg-pty-host /tmp/cc-daemon-503/74b4acfd/pty/686fd2bf.sock 213 63 -- {CC}/claude --session-id {ID}"),
            format!("25769 25762 {CC}/claude --session-id {ID} --fork-session --resume /x/a.jsonl"),
            "25761 25723 claude bg-pty-host --bg-pty-host /tmp/cc-daemon-503/74b4acfd/spare/dc7dc1d6.pty.sock 200 50 -- /x/claude --bg-spare /tmp/s.claim.sock".to_owned(),
            "25772 25761 claude bg-spare --bg-spare /tmp/cc-daemon-503/74b4acfd/spare/dc7dc1d6.claim.sock".to_owned(),
            // A plain session in a terminal outside tmux.
            format!("56987  5000 {CC}/bin/claude"),
            format!("57006 56987 {CC}/claude --settings {{\"a\":1}}"),
            "49343  4000 /bin/zsh -l".to_owned(),
            "50369 49343 claude attach 686fd2bf".to_owned(),
            format!("50373 50369 {CC}/bin/claude attach 686fd2bf"),
            format!("50391 50373 {CC}/claude attach 686fd2bf"),
            // Decoys in other panes.
            "60000  4001 /bin/sh".to_owned(),
            "60001 60000 tmux attach -t 686fd2bf".to_owned(),
            "60002 60000 git attach 686fd2bf".to_owned(),
            "60003 60000 /usr/bin/claude-wrapper attach 686fd2bf".to_owned(),
            "60004 60000 claude attach 686f".to_owned(),
            "60005 60000 claude attach 77777777".to_owned(),
        ]
        .join("\n")
    }

    #[test]
    fn parses_ps_rows() {
        let rows = parse_ps(&format!("{}\nnot a row\n  12\n", table()));
        assert_eq!(rows.len(), 18);
        assert_eq!(rows[3].pid, 25769);
        assert_eq!(rows[3].ppid, 25762);
        assert!(
            rows[3]
                .args
                .starts_with(&format!("{CC}/claude --session-id"))
        );
    }

    #[test]
    fn finds_the_engine_session_id() {
        let rows = parse_ps(&table());
        assert_eq!(session_id(&rows[3].args).as_deref(), Some(ID));
        assert_eq!(
            session_id(&format!("claude --session-id={ID} -c")).as_deref(),
            Some(ID)
        );
        // The pty host names it only after `--`, for the engine it starts.
        assert_eq!(session_id(&rows[2].args), None);
        assert_eq!(session_id("claude --session-id"), None);
        assert_eq!(session_id("claude --session-ids x"), None);
        assert_eq!(session_id("claude --session-id a;b"), None);
        assert_eq!(session_id("claude --resume x"), None);
    }

    #[test]
    fn attach_tokens_need_a_claude_argv0() {
        assert_eq!(attach_token("claude attach 686fd2bf"), Some("686fd2bf"));
        assert_eq!(
            attach_token(&format!("{CC}/bin/claude attach 686fd2bf")),
            Some("686fd2bf")
        );
        assert_eq!(attach_token("tmux attach -t 686fd2bf"), None);
        assert_eq!(attach_token("git attach 686fd2bf"), None);
        assert_eq!(
            attach_token("/usr/bin/claude-wrapper attach 686fd2bf"),
            None
        );
        assert_eq!(attach_token("claude attach"), None);
        assert_eq!(
            attach_token("claude --debug attach 686fd2bf"),
            Some("686fd2bf")
        );
        assert_eq!(attach_token("claude attach --x 686fd2bf"), Some("686fd2bf"));
        // An engine whose settings mention the words.
        let engine =
            format!("{CC}/claude --session-id {ID} --settings {{\"a\":\"attach 686fd2bf\"}}");
        assert_eq!(attach_token(&engine), None);
        assert_eq!(attach_token("claude resume attach 686fd2bf"), None);
        assert_eq!(attach_token("claude attach --help"), None);
        assert_eq!(attach_token("claude -p -- attach 686fd2bf"), None);
        assert_eq!(attach_token(""), None);
    }

    #[test]
    fn attach_tokens_through_a_launcher_or_a_path_with_spaces() {
        let t = Some("686fd2bf");
        assert_eq!(attach_token("/bin/sh ./claude attach 686fd2bf"), t);
        assert_eq!(
            attach_token("node /usr/local/bin/claude attach 686fd2bf"),
            t
        );
        assert_eq!(attach_token("/usr/bin/env claude attach 686fd2bf"), t);
        assert_eq!(
            attach_token("/Users/u/Library/Application Support/Claude/claude attach 686fd2bf"),
            t
        );
        // A shell that runs a command line is not the client; its child is.
        assert_eq!(attach_token("sh -c claude attach 686fd2bf"), None);
        assert_eq!(attach_token("/bin/sh -c claude attach 686fd2bf"), None);
        assert_eq!(attach_token("vim claude attach 686fd2bf"), None);
        assert_eq!(attach_token("python claude attach 686fd2bf"), None);
        assert_eq!(attach_token("a b c d claude attach 686fd2bf"), None);
    }

    #[test]
    fn only_daemon_hosted_engines_count() {
        let rows = parse_ps(&table());
        assert!(hosted(&rows, 25769), "the engine under its pty host");
        assert!(hosted(&rows, 25772), "a spare engine");
        assert!(!hosted(&rows, 57006), "a plain session outside tmux");
        assert!(!hosted(&rows, 25762), "the pty host itself");
        assert!(!hosted(&rows, 1));
        assert!(!hosted(&rows, 4242), "not in the table");
        // The flag counts only before `--`.
        let rows = parse_ps("10 1 sh -c x -- --bg-pty-host\n11 10 claude\n");
        assert!(!hosted(&rows, 11));
    }

    #[test]
    fn tokens_match_by_prefix_of_eight_or_more() {
        assert!(matches(ID, "686fd2bf"));
        assert!(matches(ID, ID));
        assert!(!matches(ID, "686fd2b"));
        assert!(!matches(ID, "686fd2bx"));
        assert_eq!(short(ID), "686fd2bf");
        assert_eq!(short("abc"), "abc");
    }

    #[test]
    fn finds_attached_panes_through_ancestry() {
        let rows = parse_ps(&table());
        let panes = [(49343, "%3"), (60000, "%4"), (25723, "%9")];
        assert_eq!(
            attachments(&rows, &panes, &[ID]),
            [("%3".to_owned(), ID.to_owned())],
            "three client processes, one pane; decoys and the engine match nothing"
        );
        assert_eq!(
            attachments(&rows, &panes, &["77777777-x", "0000"]),
            [("%4".to_owned(), "77777777-x".to_owned())]
        );
        assert!(attachments(&rows, &panes, &[]).is_empty());
        // The pane's own process may be the client.
        let panes = [(50391, "%7")];
        assert_eq!(attachments(&rows, &panes, &[ID])[0].0, "%7");
    }

    #[test]
    fn pid_cycles_end() {
        let rows =
            parse_ps("10 11 claude attach 686fd2bf\n11 10 sh\n12 12 claude attach 686fd2bf\n");
        assert!(attachments(&rows, &[(99, "%1")], &[ID]).is_empty());
    }
}
