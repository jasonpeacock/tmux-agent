//! `jump`, `pick` and `list`. See docs/design.md, "Navigation".

use crate::engine;
use crate::init;
use crate::snapshot::{Client, Snapshot};
use crate::state::Display;
use crate::tmux::{Cmds, Tmux};
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

const NO_TARGET: &str = "tmux-agent: no agent needs you";
const NO_AGENTS: &str = "tmux-agent: no agents";

/// The targets, in order, as of the last jump: space-separated pane ids.
const JUMP_ORDER: &str = "@agent_jump_order";
const CHOOSE_TREE: &str = "@tmux-agent-choose-tree";

/// An agent pane, as navigation shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Agent {
    id: String,
    /// `session:window.pane`.
    target: String,
    window: String,
    path: String,
    state: Display,
    since: u64,
    bg: u32,
    /// The short id of the background session the pane shows through
    /// `claude attach`, or empty.
    attached: String,
}

/// Moves `client` to the most urgent agent pane, cycling on repeat.
pub fn jump(tmux: &Tmux, client: &str) -> io::Result<()> {
    let Some(snap) = engine::reconciled(tmux)? else {
        return Ok(());
    };
    let agents = agents(&snap);
    let targets: Vec<&str> = agents
        .iter()
        .filter(|a| is_target(a.state))
        .map(|a| a.id.as_str())
        .collect();
    let current = find_client(&snap, client).map(|c| c.pane.as_str());
    let saved = tmux.run(&["show-options", "-gqv", JUMP_ORDER])?;
    let order: Vec<&str> = saved.split_whitespace().collect();
    let Some(pane) = next(&order, &targets, current) else {
        return tmux.run(&message(client, NO_TARGET)).map(drop);
    };
    // Visiting a done pane makes it idle, so it leaves the targets: the saved
    // order is what lets the next press find the pane after it.
    let mut cmds = Cmds::default();
    cmds.push(["set-option", "-g", JUMP_ORDER, &targets.join(" ")]);
    cmds.push(go(client, pane));
    tmux.run(cmds.args()).map(drop)
}

/// Shows `client` a menu of agent panes.
pub fn pick(tmux: &Tmux, client: &str) -> io::Result<()> {
    let Some(snap) = engine::reconciled(tmux)? else {
        return Ok(());
    };
    let agents = agents(&snap);
    if agents.is_empty() {
        return tmux.run(&message(client, NO_AGENTS)).map(drop);
    }
    let c = find_client(&snap, client);
    let fit = c.map_or(Fit::default(), Fit::of);
    let more = if fit.rows.is_some_and(|r| agents.len() > r) {
        let on = tmux.run(&["show-options", "-gqv", CHOOSE_TREE])?.trim() != "off";
        choose_tree(on, &snap.global.version)
    } else {
        Vec::new()
    };
    if let (Some(0), Some(c)) = (fit.rows, c) {
        // Not even one item fits: go straight to the tree.
        let tree = [more, vec!["-t".into(), c.pane.clone()]].concat();
        return tmux.run(&tree).map(drop);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let icons = &snap.config.icons;
    start(
        tmux,
        &menu(&agents, client, icons, &home, now(), fit, &more),
    )
}

/// Runs a tmux command without waiting for it: before tmux 3.4,
/// display-menu returns only once the menu closes.
fn start(tmux: &Tmux, cmd: &[String]) -> io::Result<()> {
    use std::process::{Command, Stdio};
    Command::new(&tmux.bin)
        .arg("-u")
        .arg("-S")
        .arg(&tmux.socket)
        .args(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}

/// Prints agent panes to stdout, most urgent first.
pub fn list(tmux: &Tmux, json: bool) -> io::Result<()> {
    let agents = engine::reconciled(tmux)?
        .map(|s| agents(&s))
        .unwrap_or_default();
    let out = if json {
        to_json(&agents, now())
    } else {
        to_lines(&agents, now())
    };
    io::stdout().write_all(out.as_bytes())
}

fn find_client<'a>(snap: &'a Snapshot, name: &str) -> Option<&'a Client> {
    snap.clients.iter().find(|c| c.name == name)
}

/// The panes with agent state, most urgent first.
fn agents(snap: &Snapshot) -> Vec<Agent> {
    let mut v: Vec<Agent> = snap
        .panes
        .iter()
        .filter_map(|p| {
            let r = p.rec.as_ref()?;
            Some(Agent {
                id: p.id.clone(),
                target: format!("{}:{}.{}", p.session_name, p.window_index, p.pane_index),
                window: p.window_name.clone(),
                path: p.path.clone(),
                state: r.st.display(),
                since: r.since,
                bg: r.st.bg,
                attached: r.attached.clone(),
            })
        })
        .collect();
    sort(&mut v);
    v
}

/// Needs-input, done, working, idle; oldest first within a state. The sort
/// is stable, so ties keep pane order.
fn sort(v: &mut [Agent]) {
    v.sort_by_key(|a| (std::cmp::Reverse(a.state), a.since));
}

/// Jump goes only to panes that need the user.
fn is_target(d: Display) -> bool {
    matches!(d, Display::NeedsInput | Display::Done)
}

/// Where a jump goes. If `current` is in the last jump's `order`, the first
/// later entry (wrapping) that is still a target; else the target after
/// `current` when it is one (wrapping); else the first target.
fn next<'a>(order: &[&str], targets: &[&'a str], current: Option<&str>) -> Option<&'a str> {
    let target = |id: &&str| targets.iter().find(|t| **t == *id).copied();
    if let Some(i) = current.and_then(|c| order.iter().position(|o| *o == c))
        && let Some(t) = order[i + 1..].iter().chain(&order[..=i]).find_map(target)
    {
        return Some(t);
    }
    let i = current
        .and_then(|c| targets.iter().position(|t| *t == c))
        .map_or(0, |i| i + 1);
    targets.get(i % targets.len().max(1)).copied()
}

/// `-c client`, or nothing so tmux picks the current client.
fn client_flag(client: &str) -> Vec<String> {
    if client.is_empty() {
        Vec::new()
    } else {
        vec!["-c".into(), client.into()]
    }
}

/// Switches `client` to `pane`: its session, window and pane. tmux 3.3+
/// accepts a pane id here, and unzooms the window if needed.
fn go(client: &str, pane: &str) -> Vec<String> {
    let mut v = vec!["switch-client".to_owned()];
    v.extend(client_flag(client));
    v.extend(["-t".into(), pane.into()]);
    v
}

fn message(client: &str, text: &str) -> Vec<String> {
    let mut v = vec!["display-message".to_owned()];
    v.extend(client_flag(client));
    v.push(text.into());
    v
}

/// `prefix w`'s tree when `@tmux-agent-choose-tree` is on, showing only
/// windows with an agent, idle ones included; else tmux's own.
fn choose_tree(on: bool, version: &str) -> Vec<String> {
    let mut v = vec!["choose-tree".to_owned(), "-Zw".into()];
    if on {
        v.extend(
            [
                "-f",
                "#{@agent_pane_state}",
                "-F",
                init::choose_tree_format(version),
            ]
            .map(String::from),
        );
    }
    v
}

/// The configured icon; idle has none, so a blank keeps labels aligned.
fn icon(icons: &[String; 3], d: Display) -> &str {
    match d {
        Display::NeedsInput => &icons[0],
        Display::Done => &icons[1],
        Display::Working => &icons[2],
        Display::Idle => " ",
    }
}

/// How much menu the client shows: `None` when it has no size (a control
/// client).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Fit {
    /// Items: the height less the border.
    rows: Option<usize>,
    /// Label columns: the width less the border (4) and the key, " (1)" (4).
    cols: Option<usize>,
}

impl Fit {
    fn of(c: &Client) -> Fit {
        let less = |n: u32, by: u32| (n > 0).then(|| n.saturating_sub(by) as usize);
        Fit {
            rows: less(c.height, 2),
            cols: less(c.width, 8),
        }
    }
}

/// The `display-menu` argv. Past `fit.rows`, a last item runs `more`.
fn menu(
    agents: &[Agent],
    client: &str,
    icons: &[String; 3],
    home: &str,
    now: u64,
    fit: Fit,
    more: &[String],
) -> Vec<String> {
    let (shown, rest) = match fit.rows {
        Some(r) if agents.len() > r => {
            let n = r.saturating_sub(1);
            (&agents[..n], agents.len() - n)
        }
        _ => (agents, 0),
    };
    let rows: Vec<[String; 5]> = shown
        .iter()
        .map(|a| {
            [
                icon(icons, a.state).to_owned(),
                a.target.clone(),
                a.window.clone(),
                short_path(&a.path, home),
                age(now.saturating_sub(a.since)),
            ]
        })
        .collect();
    let w = widths(&rows, fit.cols);

    let mut v = vec!["display-menu".to_owned()];
    v.extend(client_flag(client));
    v.extend(["-T".into(), "#[align=centre]agents".into()]);
    for (i, (a, [ic, target, window, path, age])) in shown.iter().zip(&rows).enumerate() {
        let label = format!(
            "{:w0$} {:w1$}  {:w2$}  {:w3$}  {}",
            ic,
            clip(target, w[1]),
            clip(window, w[2]),
            clip_start(path, w[3]),
            age,
            w0 = w[0],
            w1 = w[1],
            w2 = w[2],
            w3 = w[3],
        );
        v.push(escape_label(&label));
        v.push(key(i));
        v.push(escape(&command_line(&go(client, &a.id))));
    }
    if rest > 0 {
        v.extend([
            format!("… {rest} more"),
            String::new(),
            escape(&command_line(more)),
        ]);
    }
    v
}

/// Column widths: the widest value, then narrowed to fit `cols` by cutting
/// the window name, then the target, then the path, to no less than 1.
fn widths(rows: &[[String; 5]], cols: Option<usize>) -> [usize; 5] {
    let mut w = [0; 5];
    for r in rows {
        for (w, s) in w.iter_mut().zip(r) {
            *w = (*w).max(s.chars().count());
        }
    }
    // One space after the icon, two between the other columns.
    let total = w.iter().sum::<usize>() + 7;
    let mut over = cols.map_or(0, |c| total.saturating_sub(c));
    for i in [2, 1, 3] {
        let cut = over.min(w[i].saturating_sub(1));
        w[i] -= cut;
        over -= cut;
    }
    w
}

/// `s` in at most `w` columns, its end replaced by '…' if cut.
fn clip(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `s` in at most `w` columns, its start replaced by '…' if cut.
fn clip_start(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        return s.to_owned();
    }
    let tail: String = s.chars().skip(n + 1 - w.max(1)).collect();
    format!("…{tail}")
}

/// Mnemonic keys 1-9 then 0; none after that.
fn key(i: usize) -> String {
    match i {
        0..9 => (i + 1).to_string(),
        9 => "0".into(),
        _ => String::new(),
    }
}

/// A menu command is format-expanded once. That halves a run of '#',
/// except one before '[', which it keeps as is for styles.
fn escape(s: &str) -> String {
    escape_runs(s, 1, 2)
}

/// A menu label is expanded as a format, then again as it is drawn: a run
/// of n '#' comes out whole as 4n, or as 2n before '[' (seen on 3.3a and
/// 3.8).
fn escape_label(s: &str) -> String {
    escape_runs(s, 2, 4)
}

/// Each run of n '#' as `style` * n before '[', else `other` * n.
fn escape_runs(s: &str, style: usize, other: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = 0;
    for c in s.chars() {
        if c == '#' {
            run += 1;
            continue;
        }
        out.push_str(&"#".repeat(run * if c == '[' { style } else { other }));
        run = 0;
        out.push(c);
    }
    out.push_str(&"#".repeat(run * other));
    out
}

/// One tmux command as a string, for a menu item to parse.
fn command_line(argv: &[String]) -> String {
    argv.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ")
}

/// Quotes a word for tmux's command parser when it needs it.
fn quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "%@/._-:+,".contains(c));
    if plain {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// `~` for the home directory, and at most the last two components.
fn short_path(path: &str, home: &str) -> String {
    let in_home = path
        .strip_prefix(home)
        .filter(|r| home.len() > 1 && (r.is_empty() || r.starts_with('/')));
    let (root, rest) = match in_home {
        Some(r) => ("~", r),
        None => ("", path),
    };
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match parts.len() {
        0 if root == "~" => root.into(),
        0..=2 => format!("{root}/{}", parts.join("/")),
        n => format!("…/{}", parts[n - 2..].join("/")),
    }
}

/// e.g. "45s", "3m", "2h", "5d".
fn age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// pane_id, target, state, seconds in state, bg, attached session (`-`
/// for none), window name, cwd.
fn to_lines(agents: &[Agent], now: u64) -> String {
    agents
        .iter()
        .map(|a| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                a.id,
                a.target,
                a.state,
                now.saturating_sub(a.since),
                a.bg,
                if a.attached.is_empty() {
                    "-"
                } else {
                    &a.attached
                },
                a.window,
                a.path
            )
        })
        .collect()
}

fn to_json(agents: &[Agent], now: u64) -> String {
    let items: Vec<String> = agents
        .iter()
        .map(|a| {
            format!(
                concat!(
                    r#"{{"pane_id":{},"target":{},"state":{},"seconds":{},"bg":{},"#,
                    r#""attached":{},"window":{},"cwd":{}}}"#
                ),
                json_str(&a.id),
                json_str(&a.target),
                json_str(a.state.as_str()),
                now.saturating_sub(a.since),
                a.bg,
                if a.attached.is_empty() {
                    "null".to_owned()
                } else {
                    json_str(&a.attached)
                },
                json_str(&a.window),
                json_str(&a.path),
            )
        })
        .collect();
    format!("[{}]\n", items.join(","))
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' || c == '\u{7f}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use Display::*;

    fn agent(id: &str, state: Display, since: u64) -> Agent {
        Agent {
            id: id.into(),
            target: format!("s:1.{}", &id[1..]),
            window: "zsh".into(),
            path: "/tmp".into(),
            state,
            since,
            bg: 0,
            attached: String::new(),
        }
    }

    fn ids(v: &[Agent]) -> Vec<&str> {
        v.iter().map(|a| a.id.as_str()).collect()
    }

    fn icons() -> [String; 3] {
        ["N".into(), "D".into(), "W".into()]
    }

    #[test]
    fn sorts_by_urgency_then_age_then_pane_order() {
        let mut v = vec![
            agent("%1", Idle, 1),
            agent("%2", Working, 1),
            agent("%3", Done, 50),
            agent("%4", NeedsInput, 300),
            agent("%5", NeedsInput, 200),
            agent("%6", Done, 10),
            agent("%7", NeedsInput, 200),
            agent("%8", Working, 0),
        ];
        sort(&mut v);
        assert_eq!(ids(&v), ["%5", "%7", "%4", "%6", "%3", "%8", "%2", "%1"]);
    }

    #[test]
    fn only_needs_input_and_done_are_targets() {
        assert!(is_target(NeedsInput));
        assert!(is_target(Done));
        assert!(!is_target(Working));
        assert!(!is_target(Idle));
    }

    #[test]
    fn next_cycles_from_the_current_target() {
        let t = ["%1", "%2", "%3"];
        assert_eq!(next(&[], &t, None), Some("%1"));
        assert_eq!(next(&[], &t, Some("%9")), Some("%1"));
        assert_eq!(next(&[], &t, Some("%1")), Some("%2"));
        assert_eq!(next(&[], &t, Some("%3")), Some("%1"));
        assert_eq!(next(&[], &["%1"], Some("%1")), Some("%1"));
        assert_eq!(next(&[], &[], Some("%1")), None);
        assert_eq!(next(&["%1"], &[], Some("%1")), None);
        assert_eq!(next(&[], &[], None), None);
    }

    #[test]
    fn next_follows_the_saved_order_past_panes_no_longer_targets() {
        let order = ["%1", "%2", "%3", "%4"];
        // %2 was done and turned idle when visited.
        assert_eq!(next(&order, &["%1", "%3", "%4"], Some("%2")), Some("%3"));
        assert_eq!(next(&order, &["%1", "%4"], Some("%2")), Some("%4"));
        assert_eq!(next(&order, &["%1"], Some("%4")), Some("%1"), "wraps");
        assert_eq!(next(&order, &["%2"], Some("%2")), Some("%2"), "itself last");
        // Nothing saved is a target any more: the first target.
        assert_eq!(next(&order, &["%7", "%8"], Some("%2")), Some("%7"));
        // Not in the saved order: the target after it, else the first.
        assert_eq!(next(&order, &["%7", "%8"], Some("%7")), Some("%8"));
        assert_eq!(next(&order, &["%3", "%1"], Some("%9")), Some("%3"));
        assert_eq!(next(&order, &["%3", "%1"], None), Some("%3"));
    }

    #[test]
    fn go_and_message_target_the_client() {
        assert_eq!(
            go("/dev/pts/1", "%3"),
            ["switch-client", "-c", "/dev/pts/1", "-t", "%3"]
        );
        assert_eq!(go("", "%3"), ["switch-client", "-t", "%3"]);
        assert_eq!(message("c", "hi"), ["display-message", "-c", "c", "hi"]);
    }

    #[test]
    fn menu_items_are_aligned_keyed_and_escaped() {
        let mut a = agent("%1", NeedsInput, 100);
        a.window = "a#b".into();
        a.path = "/home/u/src/x".into();
        let mut b = agent("%12", Idle, 0);
        b.target = "long:10.12".into();
        b.path = "/home/u".into();
        let v = menu(&[a, b], "/dev/pts/1", &icons(), "/home/u", 280, NONE, &[]);
        assert_eq!(
            v,
            [
                "display-menu",
                "-c",
                "/dev/pts/1",
                "-T",
                "#[align=centre]agents",
                "N s:1.1       a####b  ~/src/x  3m",
                "1",
                "switch-client -c /dev/pts/1 -t %1",
                "  long:10.12  zsh  ~        4m",
                "2",
                "switch-client -c /dev/pts/1 -t %12",
            ]
        );
    }

    const NONE: Fit = Fit {
        rows: None,
        cols: None,
    };

    fn rows(n: usize) -> Fit {
        Fit {
            rows: Some(n),
            cols: None,
        }
    }

    #[test]
    fn menu_is_capped_with_a_more_item() {
        let v: Vec<Agent> = (0..12).map(|i| agent(&format!("%{i}"), Done, 0)).collect();
        let more = choose_tree(false, "");
        let args = menu(&v, "c", &icons(), "", 0, rows(5), &more);
        let items: Vec<&[String]> = args[5..].chunks(3).collect();
        assert_eq!(items.len(), 5);
        assert_eq!(items[3][1], "4");
        assert_eq!(items[4], ["… 8 more", "", "choose-tree -Zw"]);

        let args = menu(&v, "c", &icons(), "", 0, rows(1), &more);
        assert_eq!(args[5..], ["… 12 more", "", "choose-tree -Zw"]);

        let args = menu(&v, "c", &icons(), "", 0, rows(12), &more);
        assert_eq!(args.len(), 5 + 12 * 3, "exactly at the cap: no more item");

        let args = menu(&v, "c", &icons(), "", 0, NONE, &[]);
        let keys: Vec<&str> = args[5..].chunks(3).map(|c| c[1].as_str()).collect();
        assert_eq!(
            keys,
            ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "", ""]
        );
        assert!(!args.iter().any(|a| a == "-C" || a == "-M"));
    }

    #[test]
    fn more_item_opens_the_agent_tree() {
        assert_eq!(choose_tree(false, "3.3a"), ["choose-tree", "-Zw"]);
        let t = choose_tree(true, "3.3a");
        assert_eq!(
            t[..5],
            ["choose-tree", "-Zw", "-f", "#{@agent_pane_state}", "-F"]
        );
        assert_eq!(t[5], init::choose_tree_format("3.3a"));

        let v: Vec<Agent> = (0..3).map(|i| agent(&format!("%{i}"), Done, 0)).collect();
        let args = menu(&v, "c", &icons(), "", 0, rows(2), &t);
        // The command is format-expanded once, then parsed.
        assert_eq!(args[8], "… 2 more");
        let cmd = &args[10];
        assert!(cmd.starts_with("choose-tree -Zw -f '##{@agent_pane_state}' -F '##{?pane_format,"));
        assert_eq!(cmd.replace("##", "#"), command_line(&t));
        assert!(!init::choose_tree_format("3.3a").contains('\''));
    }

    #[test]
    fn fit_comes_from_the_client_size() {
        let c = |height, width| Client {
            name: "c".into(),
            pane: "%1".into(),
            focused: true,
            height,
            width,
        };
        assert_eq!(Fit::of(&c(0, 0)), NONE, "control client");
        assert_eq!(
            Fit::of(&c(30, 100)),
            Fit {
                rows: Some(28),
                cols: Some(92)
            }
        );
        assert_eq!(Fit::of(&c(2, 5)).rows, Some(0), "a real 0, not no cap");
        assert_eq!(Fit::of(&c(1, 5)).cols, Some(0));
    }

    #[test]
    fn labels_are_cut_to_the_client_width() {
        let mut a = agent("%1", NeedsInput, 100);
        a.target = "session:1.1".into();
        a.window = "w".repeat(80);
        a.path = "/a/b".into();
        let b = agent("%2", Done, 100);
        let label = |cols, v: &[Agent]| {
            let fit = Fit {
                rows: None,
                cols: Some(cols),
            };
            menu(v, "c", &icons(), "", 100, fit, &[])[5].clone()
        };
        let all = [a.clone(), b];
        let l = label(92, &all);
        assert_eq!(l, format!("N session:1.1  {}…  /a/b  0s", "w".repeat(66)));
        assert_eq!(l.chars().count(), 92);
        // Fits already: untouched.
        assert_eq!(label(200, &all).chars().count(), 105);
        // Then the target, then the path, down to one column each.
        assert_eq!(label(26, &all), "N session:1.1  …  /a/b  0s");
        assert_eq!(label(20, &all), "N sess…  …  /a/b  0s");
        assert_eq!(label(1, &all), "N …  …  …  0s");
        a.path = "/x/abcdef/ghijkl".into();
        assert_eq!(label(16, &[a]), "N …  …  …jkl  0s");
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(quote("%1"), "%1");
        assert_eq!(quote("client-123"), "client-123");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a;"), "'a;'");
        assert_eq!(quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn shortens_paths() {
        let h = "/home/u";
        assert_eq!(short_path("/home/u", h), "~");
        assert_eq!(short_path("/home/u/", h), "~");
        assert_eq!(short_path("/home/u/a", h), "~/a");
        assert_eq!(short_path("/home/u/a/b", h), "~/a/b");
        assert_eq!(short_path("/home/u/a/b/c", h), "…/b/c");
        assert_eq!(short_path("/home/user2/x", h), "…/user2/x");
        assert_eq!(short_path("/", h), "/");
        assert_eq!(short_path("", h), "/");
        assert_eq!(short_path("/a/b", h), "/a/b");
        assert_eq!(short_path("/a/b/c", h), "…/b/c");
        assert_eq!(short_path("/a/b", ""), "/a/b");
        assert_eq!(short_path("/a/b", "/"), "/a/b");
    }

    #[test]
    fn formats_ages() {
        assert_eq!(age(0), "0s");
        assert_eq!(age(59), "59s");
        assert_eq!(age(60), "1m");
        assert_eq!(age(3599), "59m");
        assert_eq!(age(3600), "1h");
        assert_eq!(age(86399), "23h");
        assert_eq!(age(5 * 86400), "5d");
    }

    #[test]
    fn escapes_runs_of_hash_by_how_often_tmux_expands() {
        assert_eq!(escape_label("a#b"), "a####b");
        assert_eq!(escape_label("a##b"), "a########b");
        assert_eq!(escape_label("a#"), "a####");
        assert_eq!(escape_label("a#[reverse]b"), "a##[reverse]b");
        assert_eq!(escape_label("a###[x]##"), "a######[x]########");
        assert_eq!(escape_label("d##x#[reverse]y"), "d########x##[reverse]y");
        assert_eq!(escape_label("#{pane_id}"), "####{pane_id}");
        assert_eq!(escape_label("plain é…"), "plain é…");
        // Commands: doubled, but kept as is before '['.
        assert_eq!(escape("a##b#[x]##[y]#"), "a####b#[x]##[y]##");
    }

    #[test]
    fn mnemonic_keys() {
        assert_eq!(key(0), "1");
        assert_eq!(key(8), "9");
        assert_eq!(key(9), "0");
        assert_eq!(key(10), "");
    }

    #[test]
    fn lists_tab_separated() {
        let mut a = agent("%1", Working, 90);
        a.bg = 2;
        assert_eq!(
            to_lines(&[a.clone()], 100),
            "%1\ts:1.1\tworking\t10\t2\t-\tzsh\t/tmp\n"
        );
        a.attached = "686fd2bf".into();
        assert_eq!(
            to_lines(&[a], 100),
            "%1\ts:1.1\tworking\t10\t2\t686fd2bf\tzsh\t/tmp\n"
        );
        assert_eq!(to_lines(&[], 100), "");
    }

    #[test]
    fn lists_json() {
        let mut a = agent("%1", NeedsInput, 100);
        a.window = "q\"b\\s".into();
        a.path = "/x\ny\t\u{1}\u{7f}é".into();
        let one = concat!(
            r#"{"pane_id":"%1","target":"s:1.1","state":"needs-input","seconds":3,"bg":0,"#,
            r#""attached":null,"window":"q\"b\\s","cwd":"/x\ny\t\u0001\u007fé"}"#,
        );
        assert_eq!(
            to_json(&[a.clone(), a.clone()], 103),
            format!("[{one},{one}]\n")
        );
        a.attached = "686fd2bf".into();
        assert!(to_json(&[a], 103).contains(r#""bg":0,"attached":"686fd2bf","window""#));
        assert_eq!(to_json(&[], 0), "[]\n");
    }
}
