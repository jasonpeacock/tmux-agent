//! `init`: set up the tmux server. Run from tmux.conf through tmux-agent.tmux.
//!
//! One call reads the config. One call writes the options and hooks. Each key
//! is then queried and bound in calls of its own, so a bad key option fails
//! alone. Last comes `reconcile`, when a session exists.

use crate::engine;
use crate::tmux::{Cmds, Tmux};
use std::io;
use std::path::{Path, PathBuf};

/// The array index every tmux-agent hook is registered at, so re-running
/// init replaces our hooks and leaves other indexes alone.
pub const HOOK_INDEX: u32 = 4242;

/// Prefix of the `bind-key -N` note on every binding tmux-agent owns.
pub const KEY_NOTE_PREFIX: &str = "tmux-agent: ";

/// Hooks that mark the focused pane seen.
pub const FOCUS_HOOKS: [&str; 1] = ["pane-focus-in"];

/// Hooks after which pane-to-window membership or liveness may have changed.
pub const RECONCILE_HOOKS: [&str; 8] = [
    "pane-exited",
    "pane-died",
    "after-kill-pane",
    "window-layout-changed",
    "window-linked",
    "window-unlinked",
    "session-closed",
    "client-attached",
];

/// `display-message -p` needs no session, so this works while tmux.conf
/// loads, before the first session exists.
const CONFIG_FMT: &str = "D\t#{@tmux-agent-bin}\t#{@tmux-agent-jump-key}\t#{@tmux-agent-pick-key}\t#{@tmux-agent-choose-tree}\t#{version}";

/// Tags `list-keys -N` lines. `list-keys -F` only exists from tmux 3.7, so
/// notes come from `list-keys -N`: `<key> <note>` per binding with a note.
const KEY_TAG: &str = "K\t";

/// tmux 3.3's default choose-tree format (window-tree.c), with the tab icon
/// added on window rows.
const CHOOSE_TREE_FMT_3_3: &str = concat!(
    "#{?pane_format,",
    "#{?pane_marked,#[reverse],}",
    "#{pane_current_command}#{?pane_active,*,}#{?pane_marked,M,}",
    "#{?#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}},: \"#{pane_title}\",}",
    ",",
    "#{?window_format,",
    "#{?window_marked_flag,#[reverse],}",
    "#{@agent_tab_icon}#{window_name}#{window_flags}",
    "#{?#{&&:#{==:#{window_panes},1},#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}}},: \"#{pane_title}\",}",
    ",",
    "#{session_windows} windows",
    "#{?session_grouped, (group #{session_group}: #{session_group_list}),}",
    "#{?session_attached, (attached),}",
    "}",
    "}",
);

/// tmux 3.7's default: `#{pane_flags}` and floating panes.
const CHOOSE_TREE_FMT_3_7: &str = concat!(
    "#{?pane_format,",
    "#{?pane_marked,#[reverse],}#{?pane_floating_flag,#[italics],}",
    "#{pane_current_command}#{pane_flags}",
    "#{?#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}},: \"#{pane_title}\",}",
    ",window_format,",
    "#{?window_marked_flag,#[reverse],}",
    "#{@agent_tab_icon}#{window_name}#{window_flags}",
    "#{?#{&&:#{==:#{window_panes},1},#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}}},: \"#{pane_title}\",}",
    ",",
    "#{session_windows} windows",
    "#{?session_grouped, (group #{session_group}: #{session_group_list}),}",
    "#{?session_attached, (attached),}",
    "}",
);

/// tmux 3.8's default: 3.7's with theme colours.
const CHOOSE_TREE_FMT_3_8: &str = concat!(
    "#{?pane_format,",
    "#{?pane_marked,#[fg=thememagenta],}#{?pane_floating_flag,#[underscore],}",
    "#{pane_current_command}#[fg=themelightgrey]#{pane_flags}",
    "#{?#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}},: \"#{pane_title}\",}",
    ",window_format,",
    "#{?window_marked_flag,#[fg=thememagenta],}",
    "#{@agent_tab_icon}#{window_name}#[fg=themelightgrey]#{window_flags}",
    "#{?#{&&:#{==:#{window_panes},1},#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}}},: \"#{pane_title}\",}",
    ",",
    "#[fg=themelightgrey]#{session_windows} windows",
    "#{?session_grouped, (group #{session_group}: #{session_group_list}),}",
    "#{?session_attached, (attached),}",
    "}",
);

/// The `-F` format for `choose-tree -Zw` on the server's tmux `version`
/// (`#{version}`), shared with the picker's overflow item.
pub fn choose_tree_format(version: &str) -> &'static str {
    match parse_version(version) {
        Some(v) if v >= (3, 8) => CHOOSE_TREE_FMT_3_8,
        Some(v) if v >= (3, 7) => CHOOSE_TREE_FMT_3_7,
        _ => CHOOSE_TREE_FMT_3_3,
    }
}

/// `3.8`, `3.3a` or `next-3.9` as (major, minor). OpenBSD's `openbsd-7.6`
/// numbers the OS, not tmux, so it reads as unknown.
fn parse_version(v: &str) -> Option<(u32, u32)> {
    let v = v.strip_prefix("next-").unwrap_or(v);
    let (major, rest) = v.split_once('.')?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    Some((major.parse().ok()?, rest[..end].parse().ok()?))
}

/// tmux's own `prefix w` note, restored when choose-tree is turned off.
const DEFAULT_W_NOTE: &str = "Choose a window from a list";

/// The options init reads, with defaults filled in.
#[derive(Debug, PartialEq, Eq)]
struct Config {
    /// `@tmux-agent-bin`, empty when unset.
    bin: String,
    jump_key: String,
    pick_key: String,
    choose_tree: bool,
    version: String,
    /// False while tmux.conf first loads: there is nothing to reconcile, and
    /// list-clients fails with no session.
    sessions: bool,
}

/// A `list-keys -N` line: the key as tmux prints it, and its note.
#[derive(Debug, PartialEq, Eq)]
struct Key {
    key: String,
    note: String,
}

impl Key {
    /// The action of a binding tmux-agent owns.
    fn ours(&self) -> Option<&str> {
        self.note.strip_prefix(KEY_NOTE_PREFIX)
    }
}

/// A wanted key, as the prefix table holds it.
#[derive(Debug, PartialEq, Eq)]
enum Slot {
    Free,
    /// Bound by tmux-agent, with the key as tmux prints it.
    Ours(Key),
    /// Bound by someone else: left alone.
    Taken,
    /// tmux rejects the spelling, or the query failed: left alone.
    Bad,
}

pub fn run(tmux: &Tmux) -> io::Result<()> {
    let read = [
        "list-sessions",
        "-F",
        "S",
        ";",
        "display-message",
        "-p",
        CONFIG_FMT,
    ];
    let cfg = parse(&tmux.run(&read)?);
    let bin = resolve_bin(&cfg.bin);
    let socket = socket_arg(&tmux.socket);
    // Nothing here comes from a key option, so a bad key cannot stop it.
    let setup = tmux.run(setup(&bin, &socket).args()).map(drop);
    let keys = bind_keys(tmux, &cfg, &socket);
    let rec = if cfg.sessions {
        engine::reconcile(tmux)
    } else {
        Ok(())
    };
    setup.and(keys).and(rec)
}

fn parse(out: &str) -> Config {
    let mut cfg = Config {
        bin: String::new(),
        jump_key: "a".into(),
        pick_key: "A".into(),
        choose_tree: true,
        version: String::new(),
        sessions: false,
    };
    for line in out.lines() {
        if line == "S" {
            cfg.sessions = true;
        } else if let Some(rest) = line.strip_prefix("D\t") {
            let f: Vec<&str> = rest.split('\t').collect();
            let get = |i: usize| f.get(i).copied().unwrap_or("");
            cfg.bin = get(0).to_owned();
            if !get(1).is_empty() {
                cfg.jump_key = get(1).to_owned();
            }
            if !get(2).is_empty() {
                cfg.pick_key = get(2).to_owned();
            }
            cfg.choose_tree = get(3) != "off";
            cfg.version = get(4).to_owned();
        }
    }
    cfg
}

/// Parses `list-keys -N -P KEY_TAG` output. Without `-a` every listed key
/// has a note, so tmux pads it with at least one space; 3.7+ also puts a
/// space before it.
fn parse_keys(out: &str) -> Vec<Key> {
    out.lines()
        .filter_map(|l| l.strip_prefix(KEY_TAG))
        .filter_map(|rest| {
            let (key, note) = rest.trim_start().split_once(' ')?;
            Some(Key {
                key: key.to_owned(),
                note: note.trim_start().to_owned(),
            })
        })
        .collect()
}

/// The binary hooks run: `@tmux-agent-bin`, looked up in `PATH` when it is a
/// bare name (hooks run in a pane's directory), else this executable.
fn resolve_bin(opt: &str) -> String {
    let found = if opt.is_empty() {
        None
    } else if opt.contains('/') {
        Some(PathBuf::from(opt))
    } else {
        std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join(opt))
                .find(|p| is_executable(p))
        })
    };
    found
        .or_else(|| std::env::current_exe().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The socket as a `run-shell` argument: absolute, since hooks run in a
/// pane's directory and `#{socket_path}` keeps a relative `-S`.
/// Shell-quoted, with `#` doubled for run-shell's format expansion.
fn socket_arg(socket: &str) -> String {
    let abs = std::fs::canonicalize(socket)
        .or_else(|_| std::path::absolute(socket))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| socket.to_owned());
    sh_quote(&abs).replace('#', "##")
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A string as one tmux-syntax word, for a hook command that tmux parses.
fn tmux_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// A guarded call for `run-shell -b`: a missing or failing binary prints
/// nothing and exits 0, so tmux never shows it in view mode.
fn guarded(args: &str) -> String {
    format!("b=#{{q:@agent_bin}}; [ -x \"$b\" ] && \"$b\" {args}; true")
}

/// The options and hooks, as one call.
fn setup(bin: &str, socket: &str) -> Cmds {
    let mut cmds = Cmds::default();
    cmds.push(["set-option", "-g", "focus-events", "on"]);
    cmds.push(["set-option", "-g", "@agent_bin", bin]);
    cmds.push([
        "set-option",
        "-g",
        "@agent_version",
        env!("CARGO_PKG_VERSION"),
    ]);

    let focus = guarded(&format!("focus --pane #{{hook_pane}} --socket {socket}"));
    let reconcile = guarded(&format!("reconcile --socket {socket}"));
    let hooks = FOCUS_HOOKS
        .iter()
        .map(|h| (h, &focus))
        .chain(RECONCILE_HOOKS.iter().map(|h| (h, &reconcile)));
    for (hook, cmd) in hooks {
        cmds.push([
            "set-hook".into(),
            "-g".into(),
            format!("{hook}[{HOOK_INDEX}]"),
            format!("run-shell -b {}", tmux_quote(cmd)),
        ]);
    }
    cmds
}

/// A key as a single tmux argument. tmux reads an argument ending in `;` as
/// a separator, and `\;` as a literal `;`.
fn key_arg(key: &str) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    Some(match key.strip_suffix(';') {
        Some(k) => format!("{k}\\;"),
        None => key.to_owned(),
    })
}

/// How the prefix table holds `key`, letting tmux parse the spelling.
fn slot(tmux: &Tmux, key: &str) -> Slot {
    let Some(k) = key_arg(key) else {
        return Slot::Bad;
    };
    let noted = ["list-keys", "-N", "-P", KEY_TAG, "-T", "prefix", "--", &k];
    // Fails with "unknown key" also for a binding with no note.
    if let Ok(out) = tmux.run(&noted) {
        return match parse_keys(&out).pop() {
            Some(b) if b.ours().is_some() => Slot::Ours(b),
            _ => Slot::Taken,
        };
    }
    match tmux.run(&["list-keys", "-T", "prefix", "--", &k]) {
        Ok(_) => Slot::Taken,
        Err(e) if unbound(&e) => Slot::Free,
        Err(_) => Slot::Bad,
    }
}

/// "unknown key" for an unbound key; "doesn't exist" for a missing prefix
/// table, e.g. after `unbind -a`. "invalid key" is a bad spelling.
fn unbound(e: &io::Error) -> bool {
    let m = e.to_string();
    m.contains("unknown key") || m.contains("doesn't exist")
}

/// Every noted prefix binding, or `None` if the table cannot be read.
fn noted(tmux: &Tmux) -> Option<Vec<Key>> {
    match tmux.run(&["list-keys", "-N", "-P", KEY_TAG, "-T", "prefix"]) {
        Ok(out) => Some(parse_keys(&out)),
        Err(e) if unbound(&e) => Some(Vec::new()),
        Err(_) => None,
    }
}

/// Runs each key command in a call of its own. Returns the first error.
fn bind_keys(tmux: &Tmux, cfg: &Config, socket: &str) -> io::Result<()> {
    let jump = slot(tmux, &cfg.jump_key);
    let pick = slot(tmux, &cfg.pick_key);
    let w = if cfg.choose_tree {
        Slot::Free
    } else {
        slot(tmux, "w")
    };
    let noted = noted(tmux);
    let mut first = Ok(());
    for cmd in key_plan(cfg, socket, noted.as_deref(), [&jump, &pick], &w) {
        first = first.and(tmux.run(&cmd).map(drop));
    }
    // A key option tmux cannot parse is skipped; say so in the debug log.
    for (action, s, key) in [
        ("jump", &jump, &cfg.jump_key),
        ("pick", &pick, &cfg.pick_key),
    ] {
        if matches!(s, Slot::Bad) {
            first = first.and(Err(io::Error::other(format!(
                "{action} key {key:?}: tmux does not accept it, left unbound"
            ))));
        }
    }
    first
}

/// The key calls, one each: unbind our stale keys, bind the jump and pick
/// keys, then `prefix w`. `noted` is `None` when the table was unreadable.
fn key_plan(
    cfg: &Config,
    socket: &str,
    noted: Option<&[Key]>,
    slots: [&Slot; 2],
    w: &Slot,
) -> Vec<Vec<String>> {
    let want = [
        (&cfg.jump_key, "jump", slots[0]),
        (&cfg.pick_key, "pick", slots[1]),
    ];
    let call = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut calls = Vec::new();

    // Drop our bindings that are no longer wanted where they are, e.g.
    // after the user changed a key option. choose-tree is handled below.
    for b in noted.unwrap_or_default() {
        let Some(action) = b.ours() else { continue };
        let kept = want
            .iter()
            .any(|(_, a, s)| *a == action && matches!(s, Slot::Ours(o) if o.key == b.key));
        if action != "choose-tree"
            && !kept
            && let Some(k) = key_arg(&b.key)
        {
            calls.push(call(&["unbind-key", "-T", "prefix", "--", &k]));
        }
    }

    for (key, action, s) in want {
        if let (Slot::Free | Slot::Ours(_), Some(k)) = (s, key_arg(key)) {
            let note = format!("{KEY_NOTE_PREFIX}{action}");
            let cmd = guarded(&format!(
                "{action} --client #{{q:client_name}} --socket {socket}"
            ));
            calls.push(call(&[
                "bind-key",
                "-N",
                &note,
                "-T",
                "prefix",
                "--",
                &k,
                "run-shell",
                "-b",
                &cmd,
            ]));
        }
    }

    if cfg.choose_tree {
        let note = format!("{KEY_NOTE_PREFIX}choose-tree");
        calls.push(call(&[
            "bind-key",
            "-N",
            &note,
            "-T",
            "prefix",
            "w",
            "choose-tree",
            "-Zw",
            "-F",
            choose_tree_format(&cfg.version),
        ]));
    } else if matches!(w, Slot::Ours(b) if b.ours() == Some("choose-tree"))
        && cfg.jump_key != "w"
        && cfg.pick_key != "w"
    {
        calls.push(call(&[
            "bind-key",
            "-N",
            DEFAULT_W_NOTE,
            "-T",
            "prefix",
            "w",
            "choose-tree",
            "-Zw",
        ]));
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: &str, note: &str) -> Key {
        Key {
            key: k.into(),
            note: note.into(),
        }
    }

    fn ours(k: &str, action: &str) -> Slot {
        Slot::Ours(key(k, &format!("{KEY_NOTE_PREFIX}{action}")))
    }

    fn cfg() -> Config {
        parse("")
    }

    const SOCK: &str = "'/t/s'";

    /// The key calls, one per line.
    fn plan(cfg: &Config, noted: &[Key], slots: [&Slot; 2], w: &Slot) -> String {
        key_plan(cfg, SOCK, Some(noted), slots, w)
            .iter()
            .map(|c| c.join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn parses_config() {
        let c = parse("S\nS\nD\t/x/ta\tb\t\toff\t3.3a\n");
        assert_eq!(
            c,
            Config {
                bin: "/x/ta".into(),
                jump_key: "b".into(),
                pick_key: "A".into(),
                choose_tree: false,
                version: "3.3a".into(),
                sessions: true,
            }
        );
        let c = parse("D\t\t\t\t\t\n");
        assert_eq!((c.jump_key.as_str(), c.pick_key.as_str()), ("a", "A"));
        assert!(c.choose_tree);
        assert_eq!(c, cfg());
    }

    #[test]
    fn parses_both_list_keys_layouts() {
        let out = "K\tw    Choose a window from a list\n\
                   K\t M-;  tmux-agent: jump\n\
                   other\n\
                   K\tnospace\n";
        let keys = parse_keys(out);
        assert_eq!(
            keys,
            [
                key("w", "Choose a window from a list"),
                key("M-;", "tmux-agent: jump"),
            ]
        );
        assert_eq!(keys[1].ours(), Some("jump"));
        assert_eq!(keys[0].ours(), None);
    }

    #[test]
    fn hooks_are_indexed_guarded_and_quoted() {
        let a = setup("/bin/ta", &socket_arg("/t/it's #1")).args().join(" ");
        assert!(a.starts_with("set-option -g focus-events on ; set-option -g @agent_bin /bin/ta"));
        // The tmux word closes its quote around each shell quote.
        let sh = r#"'b=#{q:@agent_bin}; [ -x "$b" ] && "$b" focus --pane #{hook_pane} --socket '"'"'/t/it'"'"'\'"'"''"'"'s ##1'"'"'; true'"#;
        assert!(
            a.contains(&format!(
                "set-hook -g pane-focus-in[{HOOK_INDEX}] run-shell -b {sh}"
            )),
            "{a}"
        );
        for h in RECONCILE_HOOKS {
            assert!(
                a.contains(&format!("set-hook -g {h}[{HOOK_INDEX}] run-shell -b 'b=")),
                "{h}: {a}"
            );
        }
    }

    #[test]
    fn socket_is_absolute_and_escaped() {
        assert_eq!(
            socket_arg("/nonexistent/a#b'c"),
            r"'/nonexistent/a##b'\''c'"
        );
        let rel = socket_arg("rel/s");
        assert!(rel.starts_with("'/") && rel.ends_with("/rel/s'"), "{rel}");
    }

    #[test]
    fn binds_free_and_owned_keys_only() {
        let a = plan(&cfg(), &[], [&Slot::Free, &Slot::Taken], &Slot::Free);
        assert!(a.contains("bind-key -N tmux-agent: jump -T prefix -- a run-shell -b b="));
        assert!(a.contains("jump --client #{q:client_name} --socket '/t/s'; true"));
        assert!(!a.contains("tmux-agent: pick"), "{a}");

        let a = plan(&cfg(), &[], [&Slot::Bad, &ours("A", "pick")], &Slot::Free);
        assert!(
            a.contains("tmux-agent: pick -T prefix -- A run-shell"),
            "{a}"
        );
        assert!(!a.contains("jump"), "{a}");
        assert!(!a.contains("unbind-key"), "{a}");
    }

    #[test]
    fn unbinds_our_old_keys() {
        let mut c = cfg();
        c.jump_key = "j".into();
        let noted = [
            key("a", "tmux-agent: jump"),
            key("M-;", "tmux-agent: gone"),
            key("A", "tmux-agent: pick"),
            key("w", "tmux-agent: choose-tree"),
            key("z", "user"),
        ];
        let a = plan(&c, &noted, [&Slot::Free, &ours("A", "pick")], &Slot::Free);
        let lines: Vec<&str> = a.lines().collect();
        assert_eq!(lines[0], "unbind-key -T prefix -- a");
        assert_eq!(lines[1], "unbind-key -T prefix -- M-\\;");
        assert!(lines[2].contains("jump -T prefix -- j run-shell"), "{a}");
        assert!(lines[3].contains("pick -T prefix -- A run-shell"), "{a}");
        assert_eq!(lines.len(), 5, "{a}");

        // Unreadable: no unbinding.
        let a = key_plan(&c, SOCK, None, [&Slot::Free, &Slot::Free], &Slot::Free);
        assert!(a.iter().all(|c| c[0] != "unbind-key"));
    }

    #[test]
    fn choose_tree_on_off() {
        let mut c = cfg();
        c.version = "3.3a".into();
        let a = plan(&c, &[], [&Slot::Free, &Slot::Free], &Slot::Free);
        let w = a.lines().last().unwrap();
        assert!(
            w.starts_with("bind-key -N tmux-agent: choose-tree -T prefix w choose-tree -Zw -F")
        );
        assert!(w.contains("#{@agent_tab_icon}#{window_name}"), "{w}");

        c.choose_tree = false;
        let free = [&Slot::Free, &Slot::Free];
        assert!(!plan(&c, &[], free, &Slot::Taken).contains(" w "));
        let a = plan(&c, &[], free, &ours("w", "choose-tree"));
        assert!(a.ends_with("bind-key -N Choose a window from a list -T prefix w choose-tree -Zw"));
        c.jump_key = "w".into();
        let w = ours("w", "choose-tree");
        assert!(!plan(&c, &[], [&w, &Slot::Free], &w).contains("Choose a window"));
    }

    #[test]
    fn choose_tree_format_by_version() {
        for (v, f) in [
            ("3.3a", CHOOSE_TREE_FMT_3_3),
            ("3.6b", CHOOSE_TREE_FMT_3_3),
            ("3.7", CHOOSE_TREE_FMT_3_7),
            ("3.7c", CHOOSE_TREE_FMT_3_7),
            ("3.8", CHOOSE_TREE_FMT_3_8),
            ("next-3.9", CHOOSE_TREE_FMT_3_8),
            ("4.0", CHOOSE_TREE_FMT_3_8),
            ("openbsd-7.6", CHOOSE_TREE_FMT_3_3),
            ("", CHOOSE_TREE_FMT_3_3),
        ] {
            assert_eq!(choose_tree_format(v), f, "{v}");
        }
        for f in [
            CHOOSE_TREE_FMT_3_3,
            CHOOSE_TREE_FMT_3_7,
            CHOOSE_TREE_FMT_3_8,
        ] {
            assert_eq!(f.matches("#{@agent_tab_icon}").count(), 1);
            assert_eq!(f.matches('{').count(), f.matches('}').count());
        }
    }

    #[test]
    fn semicolon_keys() {
        assert_eq!(key_arg(";").as_deref(), Some("\\;"));
        assert_eq!(key_arg("M-;").as_deref(), Some("M-\\;"));
        assert_eq!(key_arg("a").as_deref(), Some("a"));
        assert_eq!(key_arg(""), None);
    }

    #[test]
    fn bare_bin_is_found_in_path_or_falls_back() {
        let me = std::env::current_exe().unwrap();
        let me = me.to_string_lossy();
        assert_eq!(resolve_bin(""), me);
        assert_eq!(resolve_bin("/opt/ta"), "/opt/ta");
        assert_eq!(resolve_bin("no-such-binary-ta"), me);
        let sh = resolve_bin("sh");
        assert!(sh.starts_with('/') && sh.ends_with("/sh"), "{sh}");
    }
}
