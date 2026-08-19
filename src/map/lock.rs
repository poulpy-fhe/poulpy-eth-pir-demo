//! One writer per snapshot.
//!
//! Two processes writing one snapshot both do write-temp-then-rename, so the
//! later `rename` silently discards the other's work — no error, no corruption,
//! just lost blocks. `flock` turns that into a startup failure.
//!
//! Advisory and process-scoped: the kernel drops it when the file descriptor
//! closes, including on a crash or `kill -9`, so there is no stale lock to clean
//! up. It does not protect against a writer that ignores the lock, which is why
//! only the commands that write take one.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Held for as long as the process may write the snapshot.
#[derive(Debug)]
pub struct SnapshotLock {
    _lock: AdvisoryLock,
}

impl SnapshotLock {
    /// Take the lock for `snapshot`, or fail if another process holds it.
    pub fn acquire(snapshot: &Path) -> Result<Self> {
        let path = lock_path(snapshot);
        AdvisoryLock::acquire(&path, &format!("writing {snapshot:?}")).map(|_lock| Self { _lock })
    }
}

/// A non-blocking advisory lock for another single-owner artifact.
#[derive(Debug)]
pub(crate) struct AdvisoryLock {
    _file: std::fs::File,
}

impl AdvisoryLock {
    pub(crate) fn acquire(path: &Path, activity: &str) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {path:?}"))?;

        // SAFETY: `file` owns a live descriptor for the duration of the call.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked != 0 {
            let err = std::io::Error::last_os_error();
            anyhow::ensure!(
                err.kind() != std::io::ErrorKind::WouldBlock,
                "another process is already {activity} (lock {path:?})",
            );
            return Err(err).with_context(|| format!("locking {path:?}"));
        }

        tracing::debug!("holding advisory lock at {path:?}");
        Ok(Self { _file: file })
    }
}

pub(crate) fn lock_path(snapshot: &Path) -> PathBuf {
    appended_path(snapshot, ".lock")
}

pub(crate) fn appended_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK_PROBE_PATH: &str = "USDT_PIR_TEST_CONTENDED_LOCK_PATH";

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("usdt-pir-lock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p.push("balances.snapshot");
        p
    }

    #[test]
    fn the_lock_sits_beside_the_snapshot() {
        assert_eq!(
            lock_path(Path::new("data/balances.snapshot")),
            Path::new("data/balances.snapshot.lock"),
            "must not collide with the .tmp the writer renames from"
        );
    }

    #[test]
    fn a_lock_can_be_taken_and_released() {
        let snapshot = tmp("release");
        let first = SnapshotLock::acquire(&snapshot).expect("first");
        assert!(lock_path(&snapshot).exists());
        drop(first);
        SnapshotLock::acquire(&snapshot).expect("second, after the first went away");
    }

    /// `flock` is per-descriptor, so a second `acquire` in this same process must
    /// still be refused — otherwise the guard would only work across processes
    /// and a threaded caller could double-write.
    #[test]
    fn a_second_lock_is_refused_while_the_first_lives() {
        let snapshot = tmp("contended");
        let _held = SnapshotLock::acquire(&snapshot).expect("first");
        let err = SnapshotLock::acquire(&snapshot).expect_err("second must fail");
        assert!(
            format!("{err:#}").contains("already writing"),
            "message should name the problem: {err:#}"
        );
    }

    #[test]
    fn child_process_lock_probe() {
        let Some(snapshot) = std::env::var_os(LOCK_PROBE_PATH) else {
            return;
        };
        let snapshot = PathBuf::from(snapshot);
        let error = SnapshotLock::acquire(&snapshot).expect_err("parent must hold the lock");
        assert!(
            format!("{error:#}").contains("already writing"),
            "child should observe process-level contention: {error:#}"
        );
    }

    #[test]
    fn another_process_is_refused_while_the_lock_lives() {
        let snapshot = tmp("cross-process");
        let _held = SnapshotLock::acquire(&snapshot).expect("parent lock");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("map::lock::tests::child_process_lock_probe")
            .arg("--exact")
            .arg("--nocapture")
            .env(LOCK_PROBE_PATH, &snapshot)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "child lock probe failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
