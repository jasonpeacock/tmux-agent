//! Finding and running tmux.

use std::cell::OnceCell;
use std::env;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Tmux {
    pub bin: PathBuf,
    /// The server socket, passed to every call as `-S`.
    pub socket: String,
    /// Whether the server prints `$` as `\$` in command output (3.4 only),
    /// checked the first time output holds `\$`.
    escapes_dollar: OnceCell<bool>,
}

impl Tmux {
    /// The tmux client for `socket`, or for the server in `$TMUX`.
    pub fn new(socket: Option<String>) -> Option<Tmux> {
        let tmux_env = env::var("TMUX").ok();
        let socket = socket.or_else(|| tmux_env.as_deref().and_then(socket_from_env))?;
        let server_pid = tmux_env.as_deref().and_then(server_pid_from_env);
        Some(Tmux {
            bin: find_tmux(server_pid)?,
            socket,
            escapes_dollar: OnceCell::new(),
        })
    }

    /// Runs one tmux call: `cmds` may hold several commands separated by `;`
    /// arguments. Returns stdout, or an error when tmux fails.
    pub fn run<S: AsRef<str>>(&self, cmds: &[S]) -> io::Result<String> {
        // -u: without a UTF-8 locale, tmux prints tabs and non-ASCII as '_'.
        let out = Command::new(&self.bin)
            .arg("-u")
            .arg("-S")
            .arg(&self.socket)
            .args(cmds.iter().map(|s| s.as_ref()))
            .output()?;
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            return Err(io::Error::other(msg));
        }
        let out = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.contains("\\$") && *self.escapes_dollar.get_or_init(|| self.is_3_4()) {
            return Ok(unescape_dollar(&out));
        }
        Ok(out)
    }

    /// The server escapes, so its version counts, not this client's: a
    /// background engine's report finds tmux on PATH, which may differ.
    fn is_3_4(&self) -> bool {
        Command::new(&self.bin)
            .arg("-S")
            .arg(&self.socket)
            .args(["display-message", "-p", "#{version}"])
            .output()
            .is_ok_and(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).trim_end() == "3.4"
            })
    }
}

/// tmux 3.4 prints each `$` in a value as `\$` (3.3 and 3.5 do not). Each
/// `$` gains exactly one backslash, so dropping it restores the value.
fn unescape_dollar(out: &str) -> String {
    out.replace("\\$", "$")
}

/// Builds one tmux call out of several commands.
#[derive(Default)]
pub struct Cmds(Vec<String>);

impl Cmds {
    pub fn push<I, S>(&mut self, cmd: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if !self.0.is_empty() {
            self.0.push(";".into());
        }
        self.0.extend(cmd.into_iter().map(|s| {
            // tmux reads an argument ending in ';' as a command separator,
            // and one ending in `\;` as a literal ';'.
            let s: String = s.into();
            match s.strip_suffix(';') {
                Some(head) => format!("{head}\\;"),
                None => s,
            }
        }));
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn args(&self) -> &[String] {
        &self.0
    }
}

/// `$TMUX` is `<socket>,<server pid>,<session index>`.
pub fn socket_from_env(tmux: &str) -> Option<String> {
    let (rest, _) = tmux.rsplit_once(',')?;
    let (socket, _) = rest.rsplit_once(',')?;
    (!socket.is_empty()).then(|| socket.to_owned())
}

/// The server in `$TMUX`, else tmux's default server.
pub fn socket_or_default() -> Option<String> {
    env::var("TMUX")
        .ok()
        .as_deref()
        .and_then(socket_from_env)
        .or_else(|| default_socket(env::var_os("TMUX_TMPDIR"), uid()))
}

/// The socket tmux uses with no `-L` or `-S`, as tmux works it out: the
/// first of `$TMUX_TMPDIR` and `/tmp` that `realpath` resolves (on macOS
/// `/tmp` is `/private/tmp`), then `tmux-<uid>/default`, not resolved again.
pub fn default_socket(tmpdir: Option<OsString>, uid: u32) -> Option<String> {
    let dir = tmpdir
        .filter(|d| !d.is_empty())
        .and_then(|d| std::fs::canonicalize(d).ok())
        .or_else(|| std::fs::canonicalize("/tmp").ok())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join(format!("tmux-{uid}"))
        .join("default")
        .into_os_string()
        .into_string()
        .ok()
}

fn uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

fn server_pid_from_env(tmux: &str) -> Option<u32> {
    let (rest, _) = tmux.rsplit_once(',')?;
    rest.rsplit_once(',')?.1.parse().ok()
}

/// The tmux binary: `$TMUX_AGENT_TMUX`, the running server's own executable,
/// `PATH`, then the usual install locations.
fn find_tmux(server_pid: Option<u32>) -> Option<PathBuf> {
    if let Some(p) = env::var_os("TMUX_AGENT_TMUX").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    if let Some(p) = server_pid.and_then(exe_of) {
        return Some(p);
    }
    let home = env::var_os("HOME").map(PathBuf::from);
    let user = env::var_os("USER");
    let mut dirs: Vec<PathBuf> = env::var_os("PATH")
        .map(|p| env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(user) = &user {
        let mut p = OsString::from("/etc/profiles/per-user/");
        p.push(user);
        dirs.push(PathBuf::from(p).join("bin"));
    }
    if let Some(home) = &home {
        dirs.push(home.join(".local/state/nix/profile/bin"));
        dirs.push(home.join(".nix-profile/bin"));
    }
    dirs.extend(
        [
            "/run/current-system/sw/bin",
            "/nix/var/nix/profiles/default/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
        ]
        .map(PathBuf::from),
    );
    dirs.into_iter()
        .map(|d| d.join("tmux"))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(target_os = "linux")]
fn exe_of(pid: u32) -> Option<PathBuf> {
    let p = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    // A binary replaced on disk (e.g. by an upgrade) reads as "<path> (deleted)".
    let s = p.to_str()?;
    let p = PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(s));
    is_executable(&p).then_some(p)
}

#[cfg(target_os = "macos")]
fn exe_of(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buf is writable for its whole length.
    let n = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    let p = PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..n as usize]));
    // Only trust it if it is tmux: the PID in $TMUX may have been reused.
    (p.file_name().is_some_and(|f| f == "tmux") && is_executable(&p)).then_some(p)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn exe_of(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tmux_env() {
        let v = "/private/tmp/tmux-503/default,30010,0";
        assert_eq!(
            socket_from_env(v).as_deref(),
            Some("/private/tmp/tmux-503/default")
        );
        assert_eq!(server_pid_from_env(v), Some(30010));
        assert_eq!(
            socket_from_env("/a,b/sock,1,2").as_deref(),
            Some("/a,b/sock")
        );
        assert_eq!(socket_from_env("garbage"), None);
        assert_eq!(socket_from_env(",1,2"), None);
    }

    #[test]
    fn default_socket_follows_tmux() {
        let d = std::env::temp_dir().join(format!("ta-sock-{}", std::process::id()));
        let _ = std::fs::create_dir_all(d.join("tmux-7"));
        let real = std::fs::canonicalize(&d).unwrap();
        let want = real.join("tmux-7/default").to_str().unwrap().to_owned();
        assert_eq!(default_socket(Some(d.clone().into()), 7), Some(want));
        // tmux-<uid> need not exist yet: the server makes it.
        assert_eq!(
            default_socket(Some(d.clone().into()), 8).unwrap(),
            real.join("tmux-8/default").to_str().unwrap()
        );
        // A $TMUX_TMPDIR that does not resolve, or an empty one, falls back
        // to /tmp, as tmux does.
        let tmp = std::fs::canonicalize("/tmp").unwrap();
        let want = tmp.join("tmux-7/default").to_str().unwrap().to_owned();
        assert_eq!(
            default_socket(Some(d.join("nope").into()), 7),
            Some(want.clone())
        );
        assert_eq!(default_socket(Some(OsString::new()), 7), Some(want.clone()));
        assert_eq!(default_socket(None, 7), Some(want));
        let _ = std::fs::remove_dir(d.join("tmux-7"));
        let _ = std::fs::remove_dir(&d);
    }

    #[test]
    fn unescapes_tmux_3_4_dollars() {
        assert_eq!(unescape_dollar("a\\$b"), "a$b");
        // A stored `\$` prints as `\\$` on 3.4.
        assert_eq!(unescape_dollar("a\\\\$b"), "a\\$b");
        assert_eq!(unescape_dollar("no dollars"), "no dollars");
    }

    #[test]
    fn cmds_separates_and_guards_semicolons() {
        let mut c = Cmds::default();
        c.push(["set", "-g", "@a", "x;"]);
        c.push(["set", "-g", "@b", "y"]);
        assert_eq!(
            c.args(),
            ["set", "-g", "@a", "x\\;", ";", "set", "-g", "@b", "y"]
        );
    }
}
