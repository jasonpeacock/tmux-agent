//! Process liveness and start time, used to tell a pane's owning Claude
//! process from a dead one or a reused PID.

/// The process's start time as an opaque string, or `None` when it does not
/// exist. Compared only for equality.
pub fn start_time(pid: u32) -> Option<String> {
    imp::start_time(pid)
}

/// The process's start time in whole seconds since the epoch, where the
/// start time is kept in that form (macOS). `None` elsewhere.
pub fn epoch_start(pid: u32) -> Option<u64> {
    if cfg!(target_os = "macos") {
        start_time(pid)?.split('.').next()?.parse().ok()
    } else {
        None
    }
}

/// True when `pid` names a live process that started at `start`. An empty
/// `start` (never recorded) checks liveness alone.
pub fn alive(pid: u32, start: &str) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: kill with signal 0 sends nothing; it only checks the PID.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if r != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
        return false;
    }
    start.is_empty() || start_time(pid).is_some_and(|s| s == start)
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn start_time(pid: u32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name, field 2, is in parentheses and may hold spaces;
        // the fields after its closing parenthesis start at field 3.
        let rest = &stat[stat.rfind(')')? + 1..];
        // starttime is field 22.
        rest.split_whitespace().nth(22 - 3).map(str::to_owned)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    pub fn start_time(pid: u32) -> Option<String> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: info is a properly sized, writable proc_bsdinfo.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        (n == size).then(|| format!("{}.{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    pub fn start_time(_pid: u32) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive_with_its_start_time() {
        let pid = std::process::id();
        let start = start_time(pid).expect("start time of own process");
        assert!(alive(pid, &start));
        assert!(alive(pid, ""));
        assert!(!alive(pid, "0.000000"));
    }

    #[test]
    fn exited_process_is_dead() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!alive(pid, ""));
        assert_eq!(start_time(pid), None);
    }

    #[test]
    fn zero_and_huge_pids_are_dead() {
        assert!(!alive(0, ""));
        assert!(!alive(u32::MAX, ""));
    }
}
