//! Background sessions viewed in a Claude process's own terminal. After the
//! left-arrow handoff to the background, Claude's fleet view shows daemon
//! jobs inside the same process, with no `claude attach` process to find.
//! Claude writes a beacon while it is attached to a job, and a registry file
//! for each job's engine; the pane title names the job. See docs/design.md,
//! "Fleet viewers".
//!
//! These files are Claude internals with no documented format: every read
//! here is defensive, and a file that does not parse is ignored.

use crate::owner;
use std::path::{Path, PathBuf};

/// Beacons and registry files are small; anything larger is not one.
const MAX_FILE: u64 = 64 * 1024;

/// Claude's config dir: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) if !d.is_empty() => Some(d.into()),
        _ => std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(|h| Path::new(&h).join(".claude")),
    }
}

/// The PIDs of live Claude processes attached to a daemon job in their own
/// terminal: the `fleet` beacons in `daemon/attach-journal`. A beacon whose
/// process died, or whose PID now names another process, is ignored: Claude
/// leaves the beacon behind when the viewer is killed.
pub fn viewers(dir: &Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(dir.join("daemon").join("attach-journal")) else {
        return Vec::new();
    };
    let mut out: Vec<u32> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let o = Object::parse(&read_small(&p)?)?;
            if o.str("surface") != Some("fleet") {
                return None;
            }
            let pid = u32::try_from(o.num("pid")?).ok()?;
            live(pid, o.str("procStart")?, o.num("startedAtEpochMs")).then_some(pid)
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// True when `pid` is the process that wrote a beacon: alive, with Claude's
/// `procStart`. On Linux that is our start time (field 22 of
/// `/proc/<pid>/stat`). Where it is not (macOS, unverified), a live process
/// that started before the beacon was written is accepted.
fn live(pid: u32, start: &str, at_ms: Option<u64>) -> bool {
    if start.is_empty() {
        return false;
    }
    owner::alive(pid, start)
        || (owner::alive(pid, "")
            && at_ms.is_some_and(|at| owner::epoch_start(pid).is_some_and(|s| s <= at / 1000)))
}

/// The name of the daemon job that engine `pid`, started at `start`, runs:
/// from its registry file, only for a job that has a real name (one the user
/// gave or Claude chose). An unnamed job's name is its id, which no title
/// shows; a spare runs no job yet.
pub fn job_name(dir: &Path, pid: u32, start: &str) -> Option<String> {
    let o = Object::parse(&read_small(
        &dir.join("sessions").join(format!("{pid}.json")),
    )?)?;
    // Claude's procStart is our start time on Linux; elsewhere its format is
    // unverified (see `live`), so only the PID is checked there.
    let same = o.num("pid").is_none_or(|p| p == u64::from(pid))
        && o.str("procStart")
            .is_none_or(|s| start.is_empty() || s == start || !cfg!(target_os = "linux"));
    if !same || o.str("kind") != Some("bg") || o.bool("spare") == Some(true) {
        return None;
    }
    o.str("nameSource")?;
    o.str("name").filter(|n| !n.is_empty()).map(str::to_owned)
}

/// True when a pane title shows job `name`. At attach the viewer sets the
/// bare name; the job's own title, once it changes, is a status glyph, a
/// space and the name.
pub fn titled(title: &str, name: &str) -> bool {
    if title == name {
        return true;
    }
    let mut cs = title.chars();
    match cs.next() {
        Some(c) if !c.is_alphanumeric() && !c.is_whitespace() => {
            cs.as_str().strip_prefix(' ') == Some(name)
        }
        _ => false,
    }
}

fn read_small(path: &Path) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    if f.metadata().ok()?.len() > MAX_FILE {
        return None;
    }
    let mut s = String::new();
    f.take(MAX_FILE).read_to_string(&mut s).ok()?;
    Some(s)
}

/// The top-level fields of a JSON object that are strings, numbers or
/// booleans. Nested values are checked for syntax and skipped. Enough for
/// Claude's beacon and registry files, with no dependency.
#[derive(Debug, Default)]
struct Object(Vec<(String, Scalar)>);

#[derive(Debug, PartialEq)]
enum Scalar {
    Str(String),
    Num(f64),
    Bool(bool),
    Other,
}

impl Object {
    fn parse(text: &str) -> Option<Object> {
        let mut p = Parser {
            s: text.as_bytes(),
            i: 0,
        };
        let o = p.object()?;
        p.ws();
        (p.i == p.s.len()).then_some(o)
    }

    fn get(&self, key: &str) -> Option<&Scalar> {
        // A repeated key: the last wins, as in JavaScript.
        self.0.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Scalar::Str(s) => Some(s),
            _ => None,
        }
    }

    /// A non-negative whole number.
    fn num(&self, key: &str) -> Option<u64> {
        match self.get(key)? {
            Scalar::Num(n) if *n >= 0.0 && n.fract() == 0.0 && *n < 9.0e15 => Some(*n as u64),
            _ => None,
        }
    }

    fn bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            Scalar::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

/// Deeper nesting than this is not a Claude file.
const MAX_DEPTH: usize = 32;

impl Parser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn eat(&mut self, b: u8) -> Option<()> {
        self.ws();
        (self.s.get(self.i) == Some(&b)).then(|| self.i += 1)
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }

    fn object(&mut self) -> Option<Object> {
        self.eat(b'{')?;
        let mut o = Object::default();
        if self.eat(b'}').is_some() {
            return Some(o);
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.eat(b':')?;
            let v = self.value(0)?;
            o.0.push((k, v));
            if self.eat(b',').is_none() {
                self.eat(b'}')?;
                return Some(o);
            }
        }
    }

    fn value(&mut self, depth: usize) -> Option<Scalar> {
        if depth > MAX_DEPTH {
            return None;
        }
        match self.peek()? {
            b'"' => self.string().map(Scalar::Str),
            b'{' | b'[' => {
                let close = if self.s[self.i] == b'{' { b'}' } else { b']' };
                let keyed = close == b'}';
                self.i += 1;
                if self.eat(close).is_some() {
                    return Some(Scalar::Other);
                }
                loop {
                    if keyed {
                        self.ws();
                        self.string()?;
                        self.eat(b':')?;
                    }
                    self.value(depth + 1)?;
                    if self.eat(b',').is_none() {
                        self.eat(close)?;
                        return Some(Scalar::Other);
                    }
                }
            }
            b't' => self.word("true").map(|()| Scalar::Bool(true)),
            b'f' => self.word("false").map(|()| Scalar::Bool(false)),
            b'n' => self.word("null").map(|()| Scalar::Other),
            _ => self.number(),
        }
    }

    fn word(&mut self, w: &str) -> Option<()> {
        let end = self.i + w.len();
        (self.s.get(self.i..end) == Some(w.as_bytes())).then(|| self.i = end)
    }

    fn number(&mut self) -> Option<Scalar> {
        let start = self.i;
        while self
            .s
            .get(self.i)
            .is_some_and(|b| b.is_ascii_digit() || b"+-.eE".contains(b))
        {
            self.i += 1;
        }
        let t = std::str::from_utf8(&self.s[start..self.i]).ok()?;
        t.parse().ok().map(Scalar::Num)
    }

    /// A string at the cursor, unescaped.
    fn string(&mut self) -> Option<String> {
        if self.s.get(self.i) != Some(&b'"') {
            return None;
        }
        self.i += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let b = *self.s.get(self.i)?;
            self.i += 1;
            match b {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let e = *self.s.get(self.i)?;
                    self.i += 1;
                    let c = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode()?,
                        _ => return None,
                    };
                    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                }
                0..0x20 => return None,
                _ => out.push(b),
            }
        }
    }

    /// The code point after `\u`, joining a surrogate pair.
    fn unicode(&mut self) -> Option<char> {
        let hi = self.hex4()?;
        if !(0xD800..0xDC00).contains(&hi) {
            return char::from_u32(hi);
        }
        if self.s.get(self.i..self.i + 2) != Some(b"\\u") {
            return None;
        }
        self.i += 2;
        let lo = self.hex4()?;
        if !(0xDC00..0xE000).contains(&lo) {
            return None;
        }
        char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
    }

    fn hex4(&mut self) -> Option<u32> {
        let h = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(h, 16).ok()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Where tests put scratch dirs: /var/tmp, which is on disk, else /tmp
    /// (the Nix build sandbox has no /var/tmp).
    pub fn scratch() -> &'static str {
        if Path::new("/var/tmp").is_dir() {
            "/var/tmp"
        } else {
            "/tmp"
        }
    }

    /// A scratch Claude config dir, removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            let d = PathBuf::from(format!(
                "{}/tmux-agent-fleet-{}-{tag}",
                scratch(),
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(d.join("daemon/attach-journal")).unwrap();
            std::fs::create_dir_all(d.join("sessions")).unwrap();
            Dir(d)
        }

        fn write(&self, rel: &str, text: &str) {
            std::fs::write(self.0.join(rel), text).unwrap();
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn beacon(pid: u32, start: &str) -> String {
        format!(
            "{{\"gestureId\":\"fd36231f\",\"surface\":\"fleet\",\"startedAtEpochMs\":{},\
             \"attempt\":1,\"pid\":{pid},\"procStart\":\"{start}\",\"marksExpected\":true,\
             \"via\":\"cold\",\"interactiveReached\":true,\"attachMs\":19}}",
            u64::MAX >> 12
        )
    }

    #[test]
    fn parses_top_level_scalars_and_skips_nested_values() {
        let o = Object::parse(
            r#" {"a":"x\"\\\u00e9\ud83d\ude00","n":12,"f":-1.5,"t":true,"z":null,
                "nest":{"name":"inner","l":[1,{"x":[]},"s"]},"e":{},"name":"outer","b":false} "#,
        )
        .unwrap();
        assert_eq!(o.str("a"), Some("x\"\\é😀"));
        assert_eq!(o.num("n"), Some(12));
        assert_eq!(o.num("f"), None);
        assert_eq!(o.bool("t"), Some(true));
        assert_eq!(o.bool("b"), Some(false));
        assert_eq!(o.str("z"), None);
        // A nested key never shadows a top-level one.
        assert_eq!(o.str("name"), Some("outer"));
        assert_eq!(o.str("nest"), None);
        for bad in [
            "",
            "{}x",
            "{\"a\":}",
            "{\"a\":\"\n\"}",
            "[1]",
            "{\"a\":1,}",
            "{\"a\":\"\\q\"}",
        ] {
            assert!(Object::parse(bad).is_none(), "{bad:?}");
        }
        assert!(Object::parse("{}").is_some());
        let deep = format!("{{\"a\":{}{}}}", "[".repeat(40), "]".repeat(40));
        assert!(Object::parse(&deep).is_none());
    }

    #[test]
    fn viewers_are_live_fleet_beacons() {
        let d = Dir::new("viewers");
        let me = std::process::id();
        let start = owner::start_time(me).unwrap();
        d.write("daemon/attach-journal/a.json", &beacon(me, &start));
        // A duplicate, another surface, a dead process, a reused PID, a
        // claimed copy, an empty beacon and junk are all ignored.
        d.write("daemon/attach-journal/b.json", &beacon(me, &start));
        d.write(
            "daemon/attach-journal/c.json",
            &beacon(me, &start).replace("fleet", "bg_cli"),
        );
        d.write("daemon/attach-journal/d.json", &beacon(0x7fff_fff0, "1"));
        // Written before the epoch, so no fallback can accept it.
        d.write(
            "daemon/attach-journal/e.json",
            &beacon(1, "no-such-start").replace(&(u64::MAX >> 12).to_string(), "0"),
        );
        d.write(
            "daemon/attach-journal/f.json.1.claimed",
            &beacon(me, &start),
        );
        d.write("daemon/attach-journal/g.json", "{}");
        d.write("daemon/attach-journal/h.json", "not json");
        assert_eq!(viewers(&d.0), vec![me]);
        assert_eq!(
            viewers(Path::new("/nonexistent/tmux-agent")),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn a_job_name_needs_a_named_bg_engine() {
        let d = Dir::new("names");
        let reg = |pid: u32, extra: &str| {
            format!(
                "{{\"pid\":{pid},\"sessionId\":\"661643ba-6033\",\"procStart\":\"55125366\",\
                 \"kind\":\"bg\",\"name\":\"krate-paa-docs\",\"jobId\":\"661643ba\",\
                 \"formerNames\":[{{\"name\":\"old\"}}]{extra}}}"
            )
        };
        d.write("sessions/10.json", &reg(10, ",\"nameSource\":\"user\""));
        d.write("sessions/11.json", &reg(11, ""));
        d.write(
            "sessions/12.json",
            &reg(12, ",\"nameSource\":\"auto\",\"spare\":true"),
        );
        d.write("sessions/13.json", &reg(99, ",\"nameSource\":\"auto\""));
        d.write(
            "sessions/14.json",
            &reg(14, ",\"nameSource\":\"auto\"").replace("\"bg\"", "\"interactive\""),
        );
        assert_eq!(
            job_name(&d.0, 10, "55125366").as_deref(),
            Some("krate-paa-docs")
        );
        assert_eq!(job_name(&d.0, 10, "").as_deref(), Some("krate-paa-docs"));
        // Another process now holds the PID (checked where procStart is ours).
        if cfg!(target_os = "linux") {
            assert_eq!(job_name(&d.0, 10, "1"), None);
        }
        // Unnamed, a spare, a file for another PID, not a job, no file.
        for pid in [11, 12, 13, 14, 15] {
            assert_eq!(job_name(&d.0, pid, "55125366"), None, "{pid}");
        }
    }

    #[test]
    fn titles_show_the_name_bare_or_after_a_glyph() {
        assert!(titled("krate-paa-docs", "krate-paa-docs"));
        assert!(titled("✳ krate-paa-docs", "krate-paa-docs"));
        assert!(titled("⠂ krate-paa (3) ⑂", "krate-paa (3) ⑂"));
        assert!(!titled("krate-paa-docs2", "krate-paa-docs"));
        assert!(!titled("akrate-paa-docs", "krate-paa-docs"));
        assert!(!titled("x krate-paa-docs", "krate-paa-docs"));
        assert!(!titled("✳krate-paa-docs", "krate-paa-docs"));
        assert!(!titled("✳ ✳ krate-paa-docs", "krate-paa-docs"));
        assert!(!titled("current session", "krate-paa-docs"));
        assert!(!titled("", "krate-paa-docs"));
    }
}
