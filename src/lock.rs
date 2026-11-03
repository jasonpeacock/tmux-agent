//! One writer at a time per tmux server.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;

/// Held until dropped; the OS releases it if the process dies.
pub struct Lock(#[allow(dead_code)] File);

impl Lock {
    /// Locks `<socket>.tmux-agent.lock`, beside the socket in tmux's private
    /// directory, so every process naming the same server agrees on the path.
    pub fn acquire(socket: &str) -> io::Result<Lock> {
        let socket = std::fs::canonicalize(socket).unwrap_or_else(|_| PathBuf::from(socket));
        let mut path = socket.into_os_string();
        path.push(".tmux-agent.lock");
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        f.lock()?;
        Ok(Lock(f))
    }
}
