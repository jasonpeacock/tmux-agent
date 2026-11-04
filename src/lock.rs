//! One writer at a time per tmux server.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;

/// `<socket><suffix>`, beside the socket in tmux's private directory, with
/// the socket path resolved so every process naming the same server agrees.
pub fn beside(socket: &str, suffix: &str) -> PathBuf {
    let socket = std::fs::canonicalize(socket).unwrap_or_else(|_| PathBuf::from(socket));
    let mut path = socket.into_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

/// Held until dropped; the OS releases it if the process dies.
pub struct Lock(#[allow(dead_code)] File);

impl Lock {
    /// Locks `<socket>.tmux-agent.lock` (see `beside`).
    pub fn acquire(socket: &str) -> io::Result<Lock> {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(beside(socket, ".tmux-agent.lock"))?;
        f.lock()?;
        Ok(Lock(f))
    }
}
