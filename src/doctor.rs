//! `doctor`: check the installation and print a report.
//!
//! Server-wide settings come from one batched tmux call, key bindings from
//! per-key `list-keys` calls (a failing call fails that check alone), and
//! agent panes from a snapshot read before `reconcile`, so stale state shows.

use crate::engine;
use crate::init::{FOCUS_HOOKS, HOOK_INDEX, KEY_NOTE_PREFIX, RECONCILE_HOOKS};
use crate::owner;
use crate::snapshot::{Rec, Snapshot};
use crate::tmux::{Cmds, Tmux};
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MIN_TMUX: (u32, u32) = (3, 3);
/// The report protocol this binary speaks.
const PROTO: &str = "1";
/// Jump and pick, used when `@tmux-agent-jump-key` and `-pick-key` are unset.
const DEFAULT_KEYS: [&str; 2] = ["a", "A"];
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Tags `list-keys -N` lines; tmux 3.7+ prints the prefix key there otherwise.
const NOTE_TAG: &str = "K\t";
const STATUS_SNIPPET: &str = "set -ag status-right \"#{@agent_summary}#(#{@agent_bin} reconcile)\"";
/// How many `#{@option}` references deep the theme checks follow.
const MAX_DEPTH: usize = 8;
/// Characters that break a hook that expands `"#{@agent_bin}"` unquoted.
const RAW_UNSAFE: [char; 4] = ['"', '$', '`', '\\'];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug)]
struct Check {
    level: Level,
    msg: String,
    hint: Option<String>,
}

fn check(level: Level, msg: impl Into<String>) -> Check {
    Check {
        level,
        msg: msg.into(),
        hint: None,
    }
}

impl Check {
    fn hint(mut self, h: impl Into<String>) -> Check {
        self.hint = Some(h.into());
        self
    }
}

/// Prints the checks; returns false when any check failed.
pub fn run(tmux: &Tmux) -> io::Result<bool> {
    let server = parse_server(&tmux.run(server_cmds().args())?);
    let mut checks = vec![
        version_check(&server.version),
        focus_check(server.focus_events),
    ];
    checks.extend(hook_checks(&server));
    checks.extend(key_checks(tmux, &server));
    checks.push(bin_check(
        &server.agent_bin,
        server.agent_bin_len,
        raw_hooks(&server),
    ));
    checks.push(agent_version_check(&server.agent_version));
    checks.push(theme_check(&server));
    checks.push(status_check(&server));
    checks.extend(agent_checks(tmux));
    print!("{}", render(&checks));
    Ok(checks.iter().all(|c| c.level != Level::Fail))
}

fn render(checks: &[Check]) -> String {
    let mut s = String::new();
    for c in checks {
        let tag = match c.level {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        s += &format!("{tag:<4} {}\n", c.msg);
        if let Some(h) = &c.hint {
            s += &format!("     hint: {h}\n");
        }
    }
    s
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn all_hooks() -> impl Iterator<Item = &'static str> {
    FOCUS_HOOKS.into_iter().chain(RECONCILE_HOOKS)
}

/// Server-wide state, as read by `server_cmds`.
#[derive(Debug, Default)]
struct Server {
    version: String,
    focus_events: bool,
    style_mode: bool,
    jump_key: String,
    pick_key: String,
    agent_version: String,
    agent_bin: String,
    /// The byte length tmux reports for `@agent_bin`; differs from
    /// `agent_bin.len()` when a newline cut the value short.
    agent_bin_len: Option<usize>,
    /// Global hooks, from `show-hooks -g` and `show-hooks -gw`.
    hooks: Vec<Hook>,
    /// Global user options (`@name`), with values as `show-options` quotes
    /// them; only scanned for `@name` references.
    opts: BTreeMap<String, String>,
    /// Per window, deduplicated: id and the two formats in effect there.
    windows: Vec<Win>,
    /// Per session: name and status-right.
    sessions: Vec<(String, String)>,
    /// Per pane row: id, session, and per hook in `all_hooks` order whether
    /// the hook in effect there at our index runs `@agent_bin`.
    panes: Vec<(String, String, Vec<bool>)>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Win {
    id: String,
    /// window-status-format and window-status-current-format.
    formats: [String; 2],
}

#[derive(Debug, PartialEq, Eq)]
struct Hook {
    name: String,
    index: Option<u32>,
    cmd: String,
}

/// A format that prints 1 when the expansion of `value` contains `needle`.
fn mentions(needle: &str, value: &str) -> String {
    format!("#{{m:*{needle}*,{value}}}")
}

/// One call: globals, the global hooks (`-gw` lists the pane and window
/// hooks, `-g` the session and client ones), the global user options, and
/// per window, session and pane the values that may override globals.
/// An option read from a pane row resolves through pane, window, session and
/// global options, as when tmux fires a hook, so a 0 there means shadowed.
/// Two free-text fields share a line by prefixing the first one's length.
fn server_cmds() -> Cmds {
    let mut c = Cmds::default();
    c.push([
        "display-message",
        "-p",
        concat!(
            "D\t#{version}\t#{focus-events}\t#{@tmux-agent-tab-mode}",
            "\t#{@tmux-agent-jump-key}\t#{@tmux-agent-pick-key}\t#{@agent_version}",
            "\t#{n:@agent_bin}\t#{@agent_bin}"
        ),
    ]);
    c.push(["show-hooks", "-g"]);
    c.push(["show-hooks", "-gw"]);
    c.push(["show-options", "-g"]);
    c.push(["show-options", "-gw"]);
    c.push([
        "list-windows",
        "-a",
        "-F",
        "W\t#{window_id}\t#{n:window-status-format}\t#{window-status-format}#{window-status-current-format}",
    ]);
    c.push([
        "list-sessions",
        "-F",
        "S\t#{n:session_name}\t#{session_name}#{status-right}",
    ]);
    let flags: String = all_hooks()
        .map(|h| mentions("@agent_bin", &format!("#{{{h}[{HOOK_INDEX}]}}")))
        .collect();
    let panes = format!("H\t#{{pane_id}}\t{flags}\t#{{session_name}}");
    c.push(["list-panes", "-a", "-F", &panes]);
    c
}

/// Splits `"<n>\t<rest>"` into the first `n` bytes of rest and the remainder.
fn split_len(s: &str) -> (String, String) {
    let parsed = s.split_once('\t').and_then(|(n, rest)| {
        let n: usize = n.parse().ok()?;
        rest.is_char_boundary(n).then(|| rest.split_at(n))
    });
    let (a, b) = parsed.unwrap_or((s, ""));
    (a.to_owned(), b.to_owned())
}

fn parse_server(out: &str) -> Server {
    let mut s = Server::default();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("D\t") {
            let f: Vec<&str> = rest.splitn(8, '\t').collect();
            let get = |i: usize| f.get(i).copied().unwrap_or("").to_owned();
            s.version = get(0);
            s.focus_events = matches!(get(1).as_str(), "1" | "on");
            s.style_mode = get(2) == "style";
            s.jump_key = get(3);
            s.pick_key = get(4);
            s.agent_version = get(5);
            s.agent_bin_len = get(6).parse().ok();
            s.agent_bin = get(7);
        } else if let Some(rest) = line.strip_prefix("W\t") {
            let (id, rest) = rest.split_once('\t').unwrap_or((rest, ""));
            if !s.windows.iter().any(|w| w.id == id) {
                let (normal, current) = split_len(rest);
                s.windows.push(Win {
                    id: id.to_owned(),
                    formats: [normal, current],
                });
            }
        } else if let Some(rest) = line.strip_prefix("S\t") {
            s.sessions.push(split_len(rest));
        } else if let Some(rest) = line.strip_prefix("H\t") {
            let f: Vec<&str> = rest.splitn(3, '\t').collect();
            if let [id, flags, session] = f[..] {
                let flags = flags.bytes().map(|b| b == b'1').collect();
                s.panes.push((id.to_owned(), session.to_owned(), flags));
            }
        } else if line.starts_with('@') {
            let (name, value) = line.split_once(' ').unwrap_or((line, ""));
            s.opts
                .entry(name.to_owned())
                .or_insert_with(|| value.to_owned());
        } else if !line.is_empty() {
            s.hooks.push(parse_hook(line));
        }
    }
    s
}

/// A `show-hooks` line: `name`, or `name[index] command`.
fn parse_hook(line: &str) -> Hook {
    let (key, cmd) = line.split_once(' ').unwrap_or((line, ""));
    let (name, index) = match key.split_once('[') {
        Some((n, i)) => (n, i.strip_suffix(']').and_then(|i| i.parse().ok())),
        None => (key, None),
    };
    Hook {
        name: name.to_owned(),
        index,
        cmd: cmd.to_owned(),
    }
}

/// The `@name` user options that `text` mentions.
fn refs(text: &str) -> impl Iterator<Item = &str> {
    text.match_indices('@').map(move |(i, _)| {
        let rest = &text[i + 1..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(rest.len());
        &text[i..i + 1 + end]
    })
}

/// Whether `text` reads option `name`, directly or through the global user
/// options it references, such as catppuccin's `#{E:@catppuccin_status_x}`.
/// Expanding the format in tmux instead would start the user's `#()` jobs.
fn reads(text: &str, name: &str, opts: &BTreeMap<String, String>) -> bool {
    let mut seen: Vec<&str> = Vec::new();
    let mut todo = vec![(text, 0)];
    while let Some((t, depth)) = todo.pop() {
        for r in refs(t) {
            if r == name {
                return true;
            }
            if depth < MAX_DEPTH
                && !seen.contains(&r)
                && let Some(v) = opts.get(r)
            {
                seen.push(r);
                todo.push((v, depth + 1));
            }
        }
    }
    false
}

/// `(major, minor)` from `#{version}`: "3.3a", "3.8", "next-3.6". OpenBSD's
/// base tmux reports the OS version instead, so it is unknown.
fn parse_version(v: &str) -> Option<(u32, u32)> {
    if v.starts_with("openbsd") {
        return None;
    }
    let v = v.trim_start_matches(|c: char| !c.is_ascii_digit());
    let (major, rest) = v.split_once('.')?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    Some((major.parse().ok()?, rest[..end].parse().ok()?))
}

fn version_check(v: &str) -> Check {
    match parse_version(v) {
        Some(n) if n >= MIN_TMUX => check(Level::Ok, format!("tmux {v}")),
        Some(_) => check(Level::Fail, format!("tmux {v} is older than 3.3"))
            .hint("install tmux 3.3 or later, e.g. from Nix or Homebrew"),
        None => check(
            Level::Warn,
            format!("tmux version {v:?} not recognised; 3.3 or later is required"),
        ),
    }
}

fn focus_check(on: bool) -> Check {
    if on {
        check(Level::Ok, "focus-events on")
    } else {
        check(
            Level::Fail,
            "focus-events off: focusing a pane never marks it seen",
        )
        .hint("tmux-agent init turns it on; or add `set -g focus-events on` to tmux.conf")
    }
}

fn our_hooks(s: &Server) -> impl Iterator<Item = &Hook> {
    s.hooks.iter().filter(|h| h.index == Some(HOOK_INDEX))
}

fn hook_checks(s: &Server) -> Vec<Check> {
    let mut missing = Vec::new();
    let mut foreign = Vec::new();
    let mut out = Vec::new();
    for (i, name) in all_hooks().enumerate() {
        match our_hooks(s).find(|h| h.name == name) {
            None => missing.push(name),
            Some(h) if !h.cmd.contains("@agent_bin") => foreign.push(name),
            Some(_) => out.extend(shadow_check(s, i, name)),
        }
    }
    let at = format!("[{HOOK_INDEX}]");
    if !missing.is_empty() {
        out.push(
            check(
                Level::Fail,
                format!("hooks not registered at {at}: {}", missing.join(", ")),
            )
            .hint("put `run-shell \"tmux-agent init\"` after your own set-hook lines in tmux.conf"),
        );
    }
    if !foreign.is_empty() {
        out.push(
            check(
                Level::Fail,
                format!(
                    "hooks at {at} do not run @agent_bin: {}",
                    foreign.join(", ")
                ),
            )
            .hint(format!(
                "something else uses hook index {HOOK_INDEX}; re-run tmux-agent init"
            )),
        );
    }
    if missing.is_empty() && foreign.is_empty() {
        let n = all_hooks().count();
        out.insert(
            0,
            check(Level::Ok, format!("hooks: {n} registered at {at}")),
        );
    }
    out
}

/// A warning when a session, window or pane hook of the same name replaces
/// our global hook for some pane.
fn shadow_check(s: &Server, i: usize, name: &str) -> Option<Check> {
    let shadowed: Vec<_> = s
        .panes
        .iter()
        .filter(|p| p.2.get(i) == Some(&false))
        .collect();
    let (pane, session, _) = shadowed.first()?;
    let more = match shadowed.len() {
        1 => String::new(),
        n => format!(" and {} more", n - 1),
    };
    Some(
        check(
            Level::Warn,
            format!("hook {name} is replaced by a local hook at pane {pane} (session {session}){more}"),
        )
        .hint("set-hook -t, -w or -p replaces the whole global hook there; set yours globally instead"),
    )
}

/// A prefix-table key as `list-keys -T prefix <key>` finds it.
#[derive(Debug, PartialEq, Eq)]
enum Binding {
    Unbound,
    /// tmux cannot parse the key option.
    Invalid,
    /// Bound: tmux's spelling of the key, and its command.
    Bound {
        key: String,
        cmd: String,
    },
}

/// A key as one tmux argument: an argument ending in `;` separates commands
/// unless the `;` is escaped.
fn key_arg(key: &str) -> String {
    match key.strip_suffix(';') {
        Some(k) => format!("{k}\\;"),
        None => key.to_owned(),
    }
}

/// A key as `list-keys` prints it (`\"`, `"M-;"`) in `list-keys -N` spelling.
fn unescape_key(k: &str) -> &str {
    // tmux quotes a key holding `"` with single quotes, others with double.
    if let Some(inner) = k
        .strip_prefix('"')
        .and_then(|k| k.strip_suffix('"'))
        .or_else(|| k.strip_prefix('\'').and_then(|k| k.strip_suffix('\'')))
    {
        inner
    } else if k.len() > 1 {
        k.strip_prefix('\\').unwrap_or(k)
    } else {
        k
    }
}

/// The result of `list-keys -T prefix <key>`. tmux resolves the spelling
/// (`M-x`, `Space`), and the call works on tmux 3.3, unlike `list-keys -F`.
fn lookup_key(tmux: &Tmux, key: &str) -> Result<Binding, String> {
    match tmux.run(&["list-keys", "-T", "prefix", "--", &key_arg(key)]) {
        Ok(out) => Ok(parse_binding(&out)),
        Err(e) if e.to_string().starts_with("invalid key") => Ok(Binding::Invalid),
        Err(e) if e.to_string().starts_with("unknown key") => Ok(Binding::Unbound),
        Err(e) if e.to_string().starts_with("table prefix") => Ok(Binding::Unbound),
        Err(e) => Err(e.to_string()),
    }
}

/// `bind-key [-r] -T prefix <key> <command...>`.
fn parse_binding(out: &str) -> Binding {
    let line = out.lines().next().unwrap_or("");
    let t: Vec<&str> = line.split_whitespace().collect();
    match t.windows(2).position(|w| w == ["-T", "prefix"]) {
        Some(i) if i + 2 < t.len() => Binding::Bound {
            key: unescape_key(t[i + 2]).to_owned(),
            cmd: t[i + 3..].join(" "),
        },
        _ => Binding::Unbound,
    }
}

/// `list-keys -N -P "K\t" -T prefix` rows: key and note. tmux 3.3 prints
/// `K\t<key>   <note>`; 3.7+ adds a space before the key.
fn parse_notes(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| l.strip_prefix(NOTE_TAG))
        .map(|l| {
            let l = l.trim_start();
            let (k, n) = l.split_once(' ').unwrap_or((l, ""));
            (k.to_owned(), n.trim_start().to_owned())
        })
        .collect()
}

fn key_checks(tmux: &Tmux, s: &Server) -> Vec<Check> {
    // Without -a, only bindings with a note are listed.
    let notes = parse_notes(
        &tmux
            .run(&["list-keys", "-N", "-P", NOTE_TAG, "-T", "prefix"])
            .unwrap_or_default(),
    );
    let want = [
        ("jump", &s.jump_key, "@tmux-agent-jump-key"),
        ("pick", &s.pick_key, "@tmux-agent-pick-key"),
    ];
    want.into_iter()
        .zip(DEFAULT_KEYS)
        .map(|((action, key, opt), default)| {
            let key = if key.is_empty() { default } else { key };
            match lookup_key(tmux, key) {
                Ok(b) => key_check(&b, &notes, key, action, opt),
                Err(e) => check(Level::Warn, format!("cannot look up key prefix {key}: {e}")),
            }
        })
        .collect()
}

fn key_check(b: &Binding, notes: &[(String, String)], key: &str, action: &str, opt: &str) -> Check {
    let (canon, cmd) = match b {
        Binding::Unbound => {
            return check(
                Level::Warn,
                format!("key prefix {key} is not bound; {action} is unavailable"),
            )
            .hint("run tmux-agent init");
        }
        Binding::Invalid => {
            return check(
                Level::Warn,
                format!("{opt} {key:?} is not a key tmux knows; {action} is unavailable"),
            )
            .hint(format!("set {opt} to a key such as {:?}", DEFAULT_KEYS[0]));
        }
        Binding::Bound { key, cmd } => (key, cmd),
    };
    let note = notes
        .iter()
        .find(|(k, _)| k == canon)
        .map_or("", |(_, n)| n.as_str());
    if note == format!("{KEY_NOTE_PREFIX}{action}") {
        return check(Level::Ok, format!("key prefix {key}: {action}"));
    }
    let what = if note.is_empty() { cmd } else { note };
    check(
        Level::Warn,
        format!("key prefix {key} is bound to something else ({what}); {action} is not bound"),
    )
    .hint(format!(
        "set {opt} to a free key, or unbind prefix {key} before init"
    ))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Whether some hook at our index expands `#{@agent_bin}` unquoted, as
/// `b="#{@agent_bin}"`, rather than as `#{q:@agent_bin}`.
fn raw_hooks(s: &Server) -> bool {
    our_hooks(s).any(|h| h.cmd.contains("#{@agent_bin}"))
}

/// True when `a` and `b` are one binary: the same file, or a Nix
/// `makeWrapper` pair `X` and `.X-wrapped` in one directory.
fn same_binary(a: &Path, b: &Path) -> bool {
    let wraps = |x: &Path, y: &Path| {
        x.parent() == y.parent()
            && x.file_name().zip(y.file_name()).is_some_and(|(x, y)| {
                y.to_string_lossy() == format!(".{}-wrapped", x.to_string_lossy())
            })
    };
    a == b || wraps(a, b) || wraps(b, a)
}

/// What `bin --version` prints, or `None` if it fails or takes over 2s.
fn version_of(bin: &str) -> Option<String> {
    let mut child = spawn_version(bin)?;
    // Read on a thread: a child that leaves stdout open (e.g. to a process
    // it started) must not hold doctor past the deadline.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        let _ = tx.send(out);
    });
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(out) => {
            let ok = child.wait().is_ok_and(|st| st.success());
            ok.then(|| out.trim().to_owned())
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// Starts `bin --version`. Linux refuses to run a file another process still
/// has open for writing (ETXTBSY), as just after a copy: retry briefly.
fn spawn_version(bin: &str) -> Option<std::process::Child> {
    for _ in 0..20 {
        let r = Command::new(bin)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        match r {
            Ok(child) => return Some(child),
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }
    None
}

/// `len` is the byte length tmux reports for `@agent_bin`. `raw` means the
/// hooks put `bin` inside double quotes without escaping it.
fn bin_check(bin: &str, len: Option<usize>, raw: bool) -> Check {
    if bin.is_empty() {
        return check(Level::Fail, "@agent_bin is not set: the hooks do nothing")
            .hint("run tmux-agent init");
    }
    let bad = if len.is_some_and(|n| n != bin.len()) {
        Some("a newline")
    } else if raw && bin.contains(RAW_UNSAFE) {
        Some("one of \" $ ` \\")
    } else {
        None
    };
    if let Some(bad) = bad {
        return check(
            Level::Fail,
            format!("@agent_bin {bin:?} contains {bad}, which breaks the hook command"),
        )
        .hint("install tmux-agent at a plainer path, or set @tmux-agent-bin to one");
    }
    if !is_executable(Path::new(bin)) {
        return check(
            Level::Fail,
            format!("@agent_bin {bin} is not an executable file"),
        )
        .hint("re-run tmux-agent init, or fix @tmux-agent-bin");
    }
    let this = std::env::current_exe().and_then(std::fs::canonicalize);
    let same = match (std::fs::canonicalize(bin), &this) {
        (Ok(a), Ok(b)) => same_binary(&a, b),
        _ => true,
    };
    if same {
        return check(Level::Ok, format!("@agent_bin {bin}"));
    }
    if version_of(bin).is_some_and(|v| v == format!("tmux-agent {VERSION}")) {
        return check(
            Level::Ok,
            format!("@agent_bin {bin} (another copy of tmux-agent {VERSION})"),
        );
    }
    let this = this.map_or_else(|_| "?".into(), |b| b.display().to_string());
    check(
        Level::Warn,
        format!("@agent_bin {bin} is not this binary ({this})"),
    )
    .hint("hooks run @agent_bin; re-run init with this binary, or set @tmux-agent-bin")
}

fn agent_version_check(v: &str) -> Check {
    if v == VERSION {
        check(Level::Ok, format!("@agent_version {v} matches this binary"))
    } else {
        let v = if v.is_empty() { "unset" } else { v };
        check(
            Level::Warn,
            format!("@agent_version is {v}; this binary is {VERSION}"),
        )
        .hint("re-run tmux-agent init")
    }
}

fn theme_check(s: &Server) -> Check {
    const FORMATS: [&str; 2] = ["window-status-format", "window-status-current-format"];
    // In style mode the binary sets the colour itself.
    let opts: &[&str] = if s.style_mode {
        &["@agent_tab_icon"]
    } else {
        &["@agent_tab_color", "@agent_tab_icon"]
    };
    let m = s.windows.len();
    let mut gaps = Vec::new();
    let mut none = true;
    for (i, format) in FORMATS.iter().enumerate() {
        let lacking: Vec<(&str, usize)> = opts
            .iter()
            .map(|&o| {
                let n = s
                    .windows
                    .iter()
                    .filter(|w| !reads(&w.formats[i], o, &s.opts));
                (o, n.count())
            })
            .filter(|&(_, n)| n > 0)
            .collect();
        none &= lacking.len() == opts.len() && lacking.iter().all(|&(_, n)| n == m);
        match lacking[..] {
            [] => {}
            [(_, n), ..] if lacking.iter().all(|&(_, k)| k == n) => {
                let o: Vec<&str> = lacking.iter().map(|&(o, _)| o).collect();
                gaps.push(format!(
                    "{format} lacks {} in {n} of {m} windows",
                    o.join(" and ")
                ));
            }
            _ => gaps.extend(
                lacking
                    .iter()
                    .map(|(o, n)| format!("{format} lacks {o} in {n} of {m} windows")),
            ),
        }
    }
    if gaps.is_empty() {
        return check(
            Level::Ok,
            format!("window formats read {}", opts.join(" and ")),
        );
    }
    if none {
        gaps = vec![format!("no window format reads {}", opts.join(" or "))];
    }
    check(Level::Warn, gaps.join("; ")).hint(
        "make both formats read #{@agent_tab_color} and #{@agent_tab_icon} \
         (README, \"Tab colour and icon\"), or set @tmux-agent-tab-mode style",
    )
}

fn status_check(s: &Server) -> Check {
    let without: Vec<&str> = s
        .sessions
        .iter()
        .filter(|(_, status)| !reads(status, "@agent_summary", &s.opts))
        .map(|(name, _)| name.as_str())
        .collect();
    if without.is_empty() {
        check(Level::Ok, "status-right shows @agent_summary")
    } else {
        check(
            Level::Warn,
            format!(
                "status-right lacks @agent_summary in session(s): {}",
                without.join(", ")
            ),
        )
        .hint(format!("add to tmux.conf: {STATUS_SNIPPET}"))
    }
}

/// Agent panes and stale state as the server holds them now, then
/// `reconcile`, reported only when it changed something.
fn agent_checks(tmux: &Tmux) -> Vec<Check> {
    let before = match Snapshot::read(tmux) {
        Ok(s) => s,
        Err(e) => return vec![check(Level::Fail, format!("cannot read agent panes: {e}"))],
    };
    let mut out = pane_checks(before.as_ref(), now());
    out.extend(orphan_checks(before.as_ref()));
    match engine::reconcile(tmux).and_then(|()| Snapshot::read(tmux)) {
        Ok(after) if fingerprint(before.as_ref()) != fingerprint(after.as_ref()) => {
            out.push(check(Level::Ok, "reconciled: stale agent state cleared"));
        }
        Ok(_) => {}
        Err(e) => out.push(check(Level::Warn, format!("cannot reconcile: {e}"))),
    }
    out
}

type Print<'a> = (Vec<(&'a str, &'a Option<Rec>, [&'a str; 3])>, &'a str);

/// The agent state `reconcile` may change: pane records, window options and
/// the summary.
fn fingerprint(snap: Option<&Snapshot>) -> Option<Print<'_>> {
    let s = snap?;
    let panes = s
        .panes
        .iter()
        .map(|p| {
            (
                p.id.as_str(),
                &p.rec,
                [
                    p.win_state.as_str(),
                    p.win_color.as_str(),
                    p.win_icon.as_str(),
                ],
            )
        })
        .collect();
    Some((panes, s.global.summary.as_str()))
}

fn pane_checks(snap: Option<&Snapshot>, now: u64) -> Vec<Check> {
    let mut out = Vec::new();
    for p in snap.map(|s| s.panes.as_slice()).unwrap_or_default() {
        let Some(r) = &p.rec else { continue };
        let mut level = Level::Ok;
        let owner = if r.pid == 0 {
            "no owner pid".to_owned()
        } else if owner::alive(r.pid, &r.start) {
            format!("owner {} alive", r.pid)
        } else {
            level = Level::Warn;
            format!("owner {} dead (stale)", r.pid)
        };
        let proto = match r.proto.as_str() {
            PROTO => format!("proto {PROTO}"),
            "" => {
                level = Level::Warn;
                "no proto".to_owned()
            }
            v => {
                level = Level::Warn;
                format!("proto {v} (this binary speaks {PROTO})")
            }
        };
        let plugin = if r.plugin.is_empty() {
            "unknown"
        } else {
            &r.plugin
        };
        let age = if r.since == 0 {
            "?".to_owned()
        } else {
            age(now.saturating_sub(r.since))
        };
        let dead = if p.dead {
            level = Level::Warn;
            ", pane dead (stale)"
        } else {
            ""
        };
        out.push(check(
            level,
            format!(
                "pane {} {}:{}.{} {}, {owner}, plugin {plugin}, {proto}, {age} in state{dead}",
                p.id,
                p.session_name,
                p.window_index,
                p.pane_index,
                r.st.display()
            ),
        ));
    }
    if out.is_empty() {
        out.push(
            check(Level::Warn, "no Claude session has reported yet")
                .hint("install the tmux-agent Claude plugin, then start claude in a tmux pane"),
        );
    }
    out
}

/// Windows that still carry `@agent_window_state` or `@agent_tab_*` with no
/// agent pane left in them.
fn orphan_checks(snap: Option<&Snapshot>) -> Vec<Check> {
    let panes = snap.map(|s| s.panes.as_slice()).unwrap_or_default();
    let mut out = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for p in panes {
        let w = p.window.as_str();
        let has_opts = [&p.win_state, &p.win_color, &p.win_icon]
            .iter()
            .any(|v| !v.is_empty());
        if seen.contains(&w) || !has_opts {
            continue;
        }
        seen.push(w);
        if panes.iter().all(|q| q.window != w || q.rec.is_none()) {
            out.push(check(
                Level::Warn,
                format!(
                    "window {w} {}:{} has @agent_tab_* set but no agent pane (stale)",
                    p.session_name, p.window_index
                ),
            ));
        }
    }
    out
}

/// "42s", "5m", "3h", "2d".
fn age(secs: u64) -> String {
    match secs {
        0..120 => format!("{secs}s"),
        120..7200 => format!("{}m", secs / 60),
        7200..172_800 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Config, Global, Pane};
    use crate::state::{Loop, PaneState};

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("3.3a"), Some((3, 3)));
        assert_eq!(parse_version("3.8"), Some((3, 8)));
        assert_eq!(parse_version("next-3.6"), Some((3, 6)));
        assert_eq!(parse_version("3.10"), Some((3, 10)));
        assert_eq!(parse_version("master"), None);
        assert_eq!(parse_version("openbsd-7.4"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn version_floor() {
        assert_eq!(version_check("3.3").level, Level::Ok);
        assert_eq!(version_check("3.3a").level, Level::Ok);
        assert_eq!(version_check("next-3.6").level, Level::Ok);
        assert_eq!(version_check("3.10").level, Level::Ok);
        assert_eq!(version_check("3.2a").level, Level::Fail);
        assert_eq!(version_check("2.9").level, Level::Fail);
        assert_eq!(version_check("master").level, Level::Warn);
    }

    #[test]
    fn parses_hook_lines() {
        assert_eq!(
            parse_hook("pane-focus-in[4242] run-shell -b \"x\""),
            Hook {
                name: "pane-focus-in".into(),
                index: Some(4242),
                cmd: "run-shell -b \"x\"".into()
            }
        );
        let h = parse_hook("client-attached");
        assert_eq!(
            (h.name.as_str(), h.index, h.cmd.as_str()),
            ("client-attached", None, "")
        );
    }

    /// Server output as tmux 3.8 prints it, with `window_line` for
    /// pane-focus-in. Pane %2's session overrides client-attached, the last
    /// hook; window-unlinked[4242] belongs to someone else. Window @1 is
    /// listed twice, as a linked window is.
    fn server_out(window_line: &str) -> String {
        let ours = r##"run-shell -b "b=\"#{@agent_bin}\"; true""##;
        format!(
            "D\t3.8\t1\t\t\tz\t0.1.0\t9\t/bin/ta\tx\n\
             after-bind-key\n\
             after-kill-pane[4242] {ours}\n\
             client-attached[0] display-message hi\n\
             client-attached[4242] {ours}\n\
             session-closed[4242] {ours}\n\
             window-linked[4242] {ours}\n\
             window-unlinked[4242] display-message other\n\
             pane-died[4242] {ours}\n\
             pane-exited[4242] {ours}\n\
             {window_line}\n\
             window-layout-changed[4242] {ours}\n\
             @cat \" #{{@agent_summary}}\"\n\
             @thm_win \"#{{@agent_tab_icon}}\"\n\
             W\t@1\t3\tab\tcd\t#{{E:@thm_win}}\n\
             W\t@2\t0\t\n\
             W\t@1\t0\t\n\
             S\t4\tmain#{{E:@cat}}\n\
             S\t10\tother\tname#{{@x}}\n\
             H\t%1\t111111111\tmain\n\
             H\t%2\t111111110\tother\tname\n"
        )
    }

    #[test]
    fn parses_server_output() {
        let s = parse_server(&server_out("pane-focus-in"));
        assert_eq!(s.version, "3.8");
        assert!(s.focus_events);
        assert!(!s.style_mode);
        assert_eq!((s.jump_key.as_str(), s.pick_key.as_str()), ("", "z"));
        assert_eq!(s.agent_version, "0.1.0");
        assert_eq!(s.agent_bin, "/bin/ta\tx");
        assert_eq!(s.agent_bin_len, Some(9));
        assert_eq!(s.windows.len(), 2);
        assert_eq!(s.windows[0].formats, ["ab\t", "cd\t#{E:@thm_win}"]);
        assert_eq!(s.windows[1].formats, ["", ""]);
        assert_eq!(s.sessions[0], ("main".into(), "#{E:@cat}".into()));
        assert_eq!(s.sessions[1], ("other\tname".into(), "#{@x}".into()));
        assert_eq!(s.opts["@cat"], "\" #{@agent_summary}\"");
        assert_eq!(s.panes[1].1, "other\tname");
        assert_eq!(s.panes[1].2.len(), 9);
        assert_eq!(s.hooks.len(), 11);
    }

    #[test]
    fn splits_length_prefixed_fields() {
        assert_eq!(split_len("3\tab\tcd"), ("ab\t".into(), "cd".into()));
        assert_eq!(split_len("0\t"), (String::new(), String::new()));
        // A length that does not fit leaves the text whole.
        assert_eq!(split_len("9\tab"), ("9\tab".into(), String::new()));
    }

    #[test]
    fn reads_follows_option_references() {
        let opts: BTreeMap<String, String> = [
            ("@a", "x #{E:@b}"),
            ("@b", "\"#{?@c,y,}\""),
            ("@c", "#{@agent_tab_color}"),
            ("@loop", "#{@loop}"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .into();
        assert!(reads("#{@agent_tab_color}", "@agent_tab_color", &opts));
        assert!(reads("#{E:@a}", "@agent_tab_color", &opts));
        assert!(!reads("#{E:@a}", "@agent_tab_icon", &opts));
        assert!(!reads("#{@agent_tab_colorful}", "@agent_tab_color", &opts));
        assert!(!reads("#{@loop}#{@nope}", "@agent_tab_color", &opts));
    }

    #[test]
    fn hook_checks_report_missing_foreign_and_shadowed() {
        let text = render(&hook_checks(&parse_server(&server_out("pane-focus-in"))));
        assert!(
            text.contains("FAIL hooks not registered at [4242]: pane-focus-in\n"),
            "{text}"
        );
        assert!(
            text.contains("FAIL hooks at [4242] do not run @agent_bin: window-unlinked\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "warn hook client-attached is replaced by a local hook at pane %2 (session other\tname)\n"
            ),
            "{text}"
        );
        assert!(!text.lines().any(|l| l.starts_with("ok")), "{text}");
    }

    #[test]
    fn hook_checks_pass_when_all_registered() {
        let mut s = parse_server(&server_out("pane-focus-in[4242] #{@agent_bin}"));
        for h in &mut s.hooks {
            if h.name == "window-unlinked" {
                h.cmd = "#{@agent_bin}".into();
            }
        }
        for p in &mut s.panes {
            p.2 = vec![true; 9];
        }
        let c = hook_checks(&s);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].level, Level::Ok);
        assert_eq!(c[0].msg, "hooks: 9 registered at [4242]");
    }

    fn bound(key: &str, cmd: &str) -> Binding {
        Binding::Bound {
            key: key.into(),
            cmd: cmd.into(),
        }
    }

    #[test]
    fn parses_bindings_and_notes() {
        let p = parse_binding;
        assert_eq!(
            p("bind-key  -T prefix a display-message x\n"),
            bound("a", "display-message x")
        );
        assert_eq!(
            p("bind-key -r -T prefix DC refresh-client -c\n"),
            bound("DC", "refresh-client -c")
        );
        assert_eq!(
            p(r#"bind-key -T prefix \" display q"#),
            bound("\"", "display q")
        );
        assert_eq!(
            p(r#"bind-key -T prefix "M-;" display q"#),
            bound("M-;", "display q")
        );
        assert_eq!(
            p(r"bind-key -T prefix \\ display q"),
            bound("\\", "display q")
        );
        assert_eq!(p(""), Binding::Unbound);
        assert_eq!(key_arg(";"), r"\;");
        assert_eq!(key_arg("M-;"), r"M-\;");
        assert_eq!(key_arg("a"), "a");
        // tmux 3.3 lines, then a 3.8 one; untagged lines are not notes.
        let notes = parse_notes("K\ta       tmux-agent: jump\nK\t \"       mine\nC-b x y\n");
        assert_eq!(
            notes,
            [
                ("a".to_owned(), "tmux-agent: jump".to_owned()),
                ("\"".to_owned(), "mine".to_owned())
            ]
        );
    }

    #[test]
    fn key_ownership_and_conflicts() {
        let notes = parse_notes("K\ta tmux-agent: jump\nK\tx mine\nK\tA tmux-agent: jump\n");
        let theirs = |k: &str| bound(k, "display-message theirs");
        let c = key_check(&theirs("a"), &notes, "a", "jump", "o");
        assert_eq!((c.level, c.msg.as_str()), (Level::Ok, "key prefix a: jump"));
        // Our binding, but for the other action.
        let c = key_check(&theirs("A"), &notes, "A", "pick", "@tmux-agent-pick-key");
        assert_eq!(c.level, Level::Warn);
        assert!(c.msg.contains("(tmux-agent: jump)"), "{}", c.msg);
        let c = key_check(&theirs("b"), &notes, "b", "pick", "o");
        assert!(
            c.msg
                .contains("bound to something else (display-message theirs)"),
            "{}",
            c.msg
        );
        let c = key_check(&theirs("x"), &notes, "x", "pick", "o");
        assert!(c.msg.contains("(mine)"), "{}", c.msg);
        // The note is looked up under tmux's spelling of the key.
        let c = key_check(&theirs("a"), &notes, "0x61", "jump", "o");
        assert_eq!(c.level, Level::Ok);
        let c = key_check(&Binding::Unbound, &notes, "b", "pick", "o");
        assert!(c.msg.contains("not bound"), "{}", c.msg);
        let c = key_check(&Binding::Invalid, &notes, "Meta-x", "jump", "o");
        assert_eq!(c.level, Level::Warn);
        assert!(c.msg.contains("not a key tmux knows"), "{}", c.msg);
    }

    fn win(normal: &str, current: &str) -> Win {
        Win {
            id: "@1".into(),
            formats: [normal.into(), current.into()],
        }
    }

    const BOTH: &str = "#{@agent_tab_color}#{@agent_tab_icon}";

    #[test]
    fn theme_needs_both_formats_and_both_options() {
        let mut s = Server {
            windows: vec![win(BOTH, BOTH), win(BOTH, BOTH)],
            ..Server::default()
        };
        assert_eq!(theme_check(&s).level, Level::Ok);
        s.windows[1] = win("", BOTH);
        let c = theme_check(&s);
        assert_eq!(c.level, Level::Warn);
        assert_eq!(
            c.msg,
            "window-status-format lacks @agent_tab_color and @agent_tab_icon in 1 of 2 windows"
        );
        s.windows[0] = win("#{@agent_tab_icon}", BOTH);
        assert_eq!(
            theme_check(&s).msg,
            "window-status-format lacks @agent_tab_color in 2 of 2 windows; \
             window-status-format lacks @agent_tab_icon in 1 of 2 windows"
        );
        // Only the current window themed: the other tabs show nothing.
        s.windows = vec![win("#W", BOTH)];
        let c = theme_check(&s);
        assert_eq!(c.level, Level::Warn);
        assert!(!c.msg.contains("current"), "{}", c.msg);
        // An icon alone does not colour the tab.
        s.windows = vec![win("#{@agent_tab_icon}", "#{@agent_tab_icon}")];
        let c = theme_check(&s);
        assert_eq!(c.level, Level::Warn);
        assert!(!c.msg.contains("_icon"), "{}", c.msg);
        // In style mode the binary sets the colour; the icon still needs the format.
        s.style_mode = true;
        assert_eq!(theme_check(&s).level, Level::Ok);
        s.windows = vec![win("#W", "#W")];
        assert_eq!(
            theme_check(&s).msg,
            "no window format reads @agent_tab_icon"
        );
        s.style_mode = false;
        assert_eq!(
            theme_check(&s).msg,
            "no window format reads @agent_tab_color or @agent_tab_icon"
        );
    }

    #[test]
    fn theme_and_status_follow_option_references() {
        let mut s = Server {
            windows: vec![win("#{E:@thm}", "#{E:@thm}")],
            sessions: vec![
                ("a".into(), "#{@agent_summary}".into()),
                ("b".into(), "#{E:@cat}".into()),
            ],
            ..Server::default()
        };
        assert_eq!(theme_check(&s).level, Level::Warn);
        let c = status_check(&s);
        assert_eq!(c.level, Level::Warn);
        assert!(c.msg.ends_with(": b"), "{}", c.msg);
        s.opts.insert("@thm".into(), BOTH.into());
        s.opts
            .insert("@cat".into(), "\" #{@agent_summary}#(x)\"".into());
        assert_eq!(theme_check(&s).level, Level::Ok);
        assert_eq!(status_check(&s).level, Level::Ok);
    }

    #[test]
    fn versions_and_bin() {
        assert_eq!(agent_version_check(VERSION).level, Level::Ok);
        assert!(agent_version_check("").msg.contains("unset"));
        assert_eq!(bin_check("", None, false).level, Level::Fail);
        assert_eq!(bin_check("/nonexistent/ta", None, false).level, Level::Fail);
        let me = std::env::current_exe().unwrap();
        let me = me.to_str().unwrap();
        assert_eq!(bin_check(me, Some(me.len()), true).level, Level::Ok);
        assert_eq!(bin_check("/bin/sh", None, false).level, Level::Warn);
        // tmux counted more bytes than the line held: a newline cut it.
        let c = bin_check(me, Some(me.len() + 3), false);
        assert_eq!(c.level, Level::Fail);
        assert!(c.msg.contains("a newline"), "{}", c.msg);
    }

    #[test]
    fn unsafe_bin_paths_fail_only_for_raw_hooks() {
        for bad in ["/a\"b/ta", "/a$b/ta", "/a`b/ta", r"/a\b/ta"] {
            let c = bin_check(bad, Some(bad.len()), true);
            assert_eq!(c.level, Level::Fail, "{bad}");
            assert!(c.msg.contains("breaks the hook command"), "{}", c.msg);
            // #{q:@agent_bin} escapes these.
            let c = bin_check(bad, Some(bad.len()), false);
            assert!(c.msg.contains("not an executable"), "{}", c.msg);
        }
        let mut s = parse_server(&server_out("pane-focus-in"));
        assert!(raw_hooks(&s));
        for h in &mut s.hooks {
            h.cmd = h.cmd.replace("#{@agent_bin}", "#{q:@agent_bin}");
        }
        assert!(!raw_hooks(&s));
    }

    #[test]
    fn wrapper_pairs_are_one_binary() {
        let p = Path::new;
        assert!(same_binary(p("/n/bin/ta"), p("/n/bin/ta")));
        assert!(same_binary(
            p("/n/bin/tmux-agent"),
            p("/n/bin/.tmux-agent-wrapped")
        ));
        assert!(same_binary(
            p("/n/bin/.tmux-agent-wrapped"),
            p("/n/bin/tmux-agent")
        ));
        assert!(!same_binary(
            p("/n/bin/tmux-agent"),
            p("/m/bin/.tmux-agent-wrapped")
        ));
        assert!(!same_binary(
            p("/n/bin/tmux-agent"),
            p("/n/bin/.other-wrapped")
        ));
        assert!(!same_binary(p("/n/bin/tmux-agent"), p("/n/bin/sh")));
    }

    /// A scratch directory, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn script(dir: &Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_str().unwrap().to_owned()
    }

    #[test]
    fn version_of_and_other_copies() {
        let dir = Scratch(std::env::temp_dir().join(format!("ta-dv-{}", std::process::id())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let copy = script(&dir.0, "copy", &format!("echo 'tmux-agent {VERSION}'"));
        let old = script(&dir.0, "old", "echo 'tmux-agent 0.0.0'");
        let slow = script(&dir.0, "slow", "exec sleep 10");
        let fails = script(&dir.0, "fails", "echo 'tmux-agent x'; exit 1");
        assert_eq!(version_of(&copy), Some(format!("tmux-agent {VERSION}")));
        assert_eq!(version_of("/nonexistent/ta"), None);
        assert_eq!(version_of(&fails), None);
        let t = std::time::Instant::now();
        assert_eq!(version_of(&slow), None);
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());

        let c = bin_check(&copy, Some(copy.len()), false);
        assert_eq!(c.level, Level::Ok, "{}", c.msg);
        assert!(c.msg.contains("another copy"), "{}", c.msg);
        assert_eq!(bin_check(&old, Some(old.len()), false).level, Level::Warn);
    }

    fn pane(id: &str, window: &str, rec: Option<Rec>, win_color: &str) -> Pane {
        Pane {
            id: id.into(),
            window: window.into(),
            dead: false,
            session_name: "main".into(),
            window_index: "1".into(),
            pane_index: "0".into(),
            window_name: "w".into(),
            path: "/".into(),
            orig: rec.clone(),
            rec,
            win_state: String::new(),
            win_color: win_color.into(),
            win_icon: String::new(),
        }
    }

    fn snap(panes: Vec<Pane>) -> Snapshot {
        Snapshot {
            config: Config {
                bin: String::new(),
                style_mode: false,
                pane_border: true,
                icons: Default::default(),
                colors: Default::default(),
            },
            global: Global::default(),
            clients: Vec::new(),
            panes,
        }
    }

    fn snap_with(rec: Option<Rec>) -> Snapshot {
        snap(vec![pane("%3", "@1", rec, "")])
    }

    fn live_rec() -> Rec {
        let pid = std::process::id();
        Rec {
            st: PaneState {
                lp: Loop::Working,
                asks: true,
                bg: 0,
            },
            pid,
            start: owner::start_time(pid).unwrap(),
            session: "sid".into(),
            since: 100,
            proto: PROTO.into(),
            plugin: "0.1.0".into(),
        }
    }

    #[test]
    fn agent_pane_lines() {
        let pid = std::process::id();
        let rec = live_rec();
        let c = pane_checks(Some(&snap_with(Some(rec.clone()))), 142);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].level, Level::Ok);
        assert_eq!(
            c[0].msg,
            format!(
                "pane %3 main:1.0 needs-input, owner {pid} alive, plugin 0.1.0, proto 1, 42s in state"
            )
        );
        let old = Rec {
            proto: "9".into(),
            ..rec.clone()
        };
        let c = pane_checks(Some(&snap_with(Some(old))), 142);
        assert_eq!(c[0].level, Level::Warn);
        assert!(
            c[0].msg.contains("proto 9 (this binary speaks 1)"),
            "{}",
            c[0].msg
        );
        let mut s = snap_with(Some(rec));
        s.panes[0].dead = true;
        let c = pane_checks(Some(&s), 142);
        assert_eq!(c[0].level, Level::Warn);
        assert!(c[0].msg.ends_with(", pane dead (stale)"), "{}", c[0].msg);
    }

    #[test]
    fn dead_owner_is_stale() {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let rec = Rec {
            pid,
            start: String::new(),
            ..live_rec()
        };
        let c = pane_checks(Some(&snap_with(Some(rec))), 142);
        assert_eq!(c[0].level, Level::Warn);
        assert!(
            c[0].msg.contains(&format!("owner {pid} dead (stale)")),
            "{}",
            c[0].msg
        );
    }

    #[test]
    fn window_options_without_agent_panes_are_stale() {
        let s = snap(vec![
            pane("%1", "@1", None, "#f00"),
            pane("%2", "@1", None, "#f00"),
            pane("%3", "@2", Some(live_rec()), "#f00"),
            pane("%4", "@2", None, "#f00"),
            pane("%5", "@3", None, ""),
        ]);
        let c = orphan_checks(Some(&s));
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].level, Level::Warn);
        assert_eq!(
            c[0].msg,
            "window @1 main:1 has @agent_tab_* set but no agent pane (stale)"
        );
        assert!(orphan_checks(None).is_empty());
    }

    #[test]
    fn fingerprint_sees_reconcile_changes() {
        let a = snap_with(Some(live_rec()));
        let mut b = a.clone();
        assert!(fingerprint(Some(&a)) == fingerprint(Some(&b)));
        b.panes[0].rec = None;
        assert!(fingerprint(Some(&a)) != fingerprint(Some(&b)));
        let mut b = a.clone();
        b.global.summary = "x".into();
        assert!(fingerprint(Some(&a)) != fingerprint(Some(&b)));
        let mut b = a.clone();
        b.panes[0].win_icon = "i".into();
        assert!(fingerprint(Some(&a)) != fingerprint(Some(&b)));
        assert!(fingerprint(Some(&a)) != fingerprint(None));
    }

    #[test]
    fn no_agent_panes_warns() {
        for snap in [None, Some(snap_with(None))] {
            let c = pane_checks(snap.as_ref(), 0);
            assert_eq!(c.len(), 1);
            assert_eq!(c[0].level, Level::Warn);
            assert_eq!(c[0].msg, "no Claude session has reported yet");
        }
    }

    #[test]
    fn ages() {
        assert_eq!(age(5), "5s");
        assert_eq!(age(119), "119s");
        assert_eq!(age(600), "10m");
        assert_eq!(age(7200), "2h");
        assert_eq!(age(3 * 86_400), "3d");
    }

    #[test]
    fn render_tags_and_hints() {
        let c = [
            check(Level::Ok, "a"),
            check(Level::Fail, "b").hint("h"),
            check(Level::Warn, "c"),
        ];
        assert_eq!(render(&c), "ok   a\nFAIL b\n     hint: h\nwarn c\n");
    }
}
