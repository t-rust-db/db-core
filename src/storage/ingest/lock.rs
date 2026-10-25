//! One writer per table (ADR 0027): an exclusive advisory lock on
//! `<table>/LOCK`, held for as long as the [`TableLock`] lives. The OS
//! drops the lock when the process dies, so a crashed writer never leaves
//! a table locked.
//!
//! The lock is `flock`-style: it belongs to the open file, and a forked
//! child shares it until it execs (the fd is close-on-exec). A process
//! that drops a table and reopens it while another of its threads is
//! spawning a child can therefore find its own lock briefly still held,
//! so [`TableLock::acquire`] retries for a short while before reporting
//! [`LockError::Held`].

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;
use std::time::Duration;

/// Attempts before [`LockError::Held`], [`RETRY_PAUSE`] apart: about 50 ms,
/// orders of magnitude longer than a fork-to-exec window. The library's own
/// tests wait about 2 s: with far more test threads than cores (#726) a
/// thread that is between `fork` and `exec` can be descheduled for longer
/// than 50 ms, and its child still holds a dropped table's lock.
#[cfg(not(test))]
const ATTEMPTS: u32 = 10;
#[cfg(test)]
const ATTEMPTS: u32 = 400;
const RETRY_PAUSE: Duration = Duration::from_millis(5);

/// The lock file's name inside the table directory.
pub const LOCK_FILE: &str = "LOCK";

/// Holds `<table>/LOCK` exclusively until dropped.
#[derive(Debug)]
pub struct TableLock {
    _file: File,
}

/// Why the lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another writer (this process or another) holds it.
    Held,
    /// Creating or locking the file failed.
    Io(io::Error),
}

impl TableLock {
    /// Takes the lock without waiting, creating the table directory and
    /// the lock file if needed.
    pub fn acquire(table: &Path) -> Result<Self, LockError> {
        std::fs::create_dir_all(table).map_err(LockError::Io)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(table.join(LOCK_FILE))
            .map_err(LockError::Io)?;
        for attempt in 1..=ATTEMPTS {
            match file.try_lock() {
                Ok(()) => return Ok(TableLock { _file: file }),
                Err(TryLockError::WouldBlock) => {
                    if attempt < ATTEMPTS {
                        std::thread::sleep(RETRY_PAUSE);
                    }
                }
                Err(TryLockError::Error(e)) => return Err(LockError::Io(e)),
            }
        }
        Err(LockError::Held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_writer_is_refused_until_the_first_drops() {
        let dir = std::env::temp_dir().join(format!("db-core-lock-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let first = TableLock::acquire(&dir).unwrap();
        assert!(matches!(TableLock::acquire(&dir), Err(LockError::Held)));
        drop(first);
        assert!(TableLock::acquire(&dir).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unusable_path_is_an_io_error() {
        // A table path under a regular file cannot be created.
        let file = std::env::temp_dir().join(format!("db-core-lock-file-{}", std::process::id()));
        std::fs::write(&file, b"").unwrap();
        assert!(matches!(
            TableLock::acquire(&file.join("t")),
            Err(LockError::Io(_))
        ));
        std::fs::remove_file(&file).unwrap();
    }
}
