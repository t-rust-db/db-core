//! One writer per table (ADR 0027): an exclusive advisory lock on
//! `<table>/LOCK`, held for as long as the [`TableLock`] lives. The OS
//! drops the lock when the process dies, so a crashed writer never leaves
//! a table locked.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

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
        match file.try_lock() {
            Ok(()) => Ok(TableLock { _file: file }),
            Err(TryLockError::WouldBlock) => Err(LockError::Held),
            Err(TryLockError::Error(e)) => Err(LockError::Io(e)),
        }
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
