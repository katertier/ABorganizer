//! Daemon lockfile via POSIX `fcntl` advisory exclusive lock.
//!
//! Kernel holds the lock for the lifetime of the file descriptor;
//! when the daemon exits (cleanly, panics, SIGKILL, power loss) the
//! lock auto-releases. No stale-PID detection needed.
//!
//! The PID is written to the file as a courtesy so operators can
//! `cat daemon.lock` to see who owns it. The PID is informational —
//! ownership is determined by the lock, not by the PID value.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use fd_lock::RwLock;

/// Held lock guard. Drop releases the lock automatically.
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct DaemonLock {
    _lock: RwLock<std::fs::File>,
}

/// Try to acquire the daemon lockfile at `path`. Fails immediately
/// if another daemon already holds it.
pub(crate) fn acquire(path: &Path) -> Result<DaemonLock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;

    let mut lock = RwLock::new(file);

    // `try_write` is non-blocking: returns `Err(WouldBlock)` if held.
    let mut guard = match lock.try_write() {
        Ok(guard) => guard,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            return Err(anyhow!(
                "another daemon is already running (lock held: {})",
                path.display()
            ));
        }
        Err(e) => return Err(e.into()),
    };

    // Best-effort PID write — only a hint for operators.
    let pid_str = std::process::id().to_string();
    let _ = (*guard).set_len(0);
    let _ = (*guard).write_all(pid_str.as_bytes());

    // Drop the temporary guard but keep the lock alive via the RwLock.
    // The RwLock will release the lock on drop.
    drop(guard);

    Ok(DaemonLock { _lock: lock })
}
