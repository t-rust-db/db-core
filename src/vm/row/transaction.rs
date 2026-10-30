//! The transaction-boundary hook a consumer's pager can install to
//! observe/react to `BEGIN`/`COMMIT`/`ROLLBACK` (`Opcode::Transaction`/
//! `AutoCommit`, db-core#81). `db-core` has no pager of its own (ADR
//! 0008/0006) -- with no hook installed, these opcodes are a pure
//! no-op beyond toggling `Vm::autocommit`, the flag `Opcode::
//! SetJournalMode` already needed (db-core#89).

use std::fmt;

/// A hook a consumer's pager implements to drive real transaction
/// semantics when `Opcode::Transaction`/`AutoCommit` run
/// (`super::vm::Vm::set_transaction_hook`). With none installed, those
/// opcodes only toggle `Vm::autocommit` -- matching `db-core` having no
/// pager of its own to actually begin/commit/roll back anything yet.
pub trait Transaction {
    /// `BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE]` -- `mode` is one of the
    /// `TRANSACTION_MODE_*` constants (`Opcode::Transaction`'s `p1`).
    fn begin(&mut self, mode: i32) -> Result<(), TransactionError>;

    /// `COMMIT` (`Opcode::AutoCommit` with `p2` nonzero).
    fn commit(&mut self) -> Result<(), TransactionError>;

    /// `ROLLBACK` (`Opcode::AutoCommit` with `p2` zero).
    fn rollback(&mut self) -> Result<(), TransactionError>;

    /// `SetJournalMode p1` (`JOURNAL_MODE_DELETE`/`JOURNAL_MODE_WAL`):
    /// switch the attached pager's journal mode. Default: no-op, the
    /// read-only fallback sqlite-rs itself uses without a writer (#134).
    fn set_journal_mode(&mut self, mode: i32) -> Result<(), TransactionError> {
        let _ = mode;
        Ok(())
    }

    /// `PRAGMA synchronous` query form: the pager's current level
    /// (`SYNCHRONOUS_OFF`/`NORMAL`/`FULL`); `None` reports `FULL` (#134).
    fn synchronous(&self) -> Option<i32> {
        None
    }

    /// `PRAGMA synchronous = level`. Default: no-op (#134).
    fn set_synchronous(&mut self, level: i32) -> Result<(), TransactionError> {
        let _ = level;
        Ok(())
    }

    /// `PRAGMA wal_autocheckpoint`: with `Some(frames)` sets the threshold,
    /// either way returns the value now in force. `None` reports SQLite's
    /// default of 1000, as for a connection with no writer (#896).
    fn wal_autocheckpoint(&mut self, frames: Option<i32>) -> Result<Option<i32>, TransactionError> {
        let _ = frames;
        Ok(None)
    }

    /// `PRAGMA wal_checkpoint(mode)` (`mode` one of `CHECKPOINT_*`): the
    /// `(busy, log, checkpointed)` row. `None` reports `(0, -1, -1)`, what
    /// SQLite answers for a database that is not in WAL mode (#896).
    fn wal_checkpoint(&mut self, mode: i32) -> Result<Option<(i64, i64, i64)>, TransactionError> {
        let _ = mode;
        Ok(None)
    }

    /// `PRAGMA integrity_check` / `quick_check` (`quick`): the problem
    /// lines to report, `["ok"]` for a clean file. `None` leaves the
    /// opcode `Unimplemented`, as without any hook (#134).
    fn integrity_check(&mut self, quick: bool) -> Option<Result<Vec<String>, TransactionError>> {
        let _ = quick;
        None
    }
}

/// Why a [`Transaction`] hook's `begin`/`commit`/`rollback` failed --
/// deliberately just a carried message: the hook lives in the
/// consumer, which knows its own failure modes (disk I/O, lock
/// contention, ...) far better than this crate could model
/// generically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionError(pub String);

impl fmt::Display for TransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TransactionError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hook that implements only the required methods, so the defaults
    /// (no pager attached) are what the calls below exercise.
    struct NoPager;

    impl Transaction for NoPager {
        fn begin(&mut self, _mode: i32) -> Result<(), TransactionError> {
            Ok(())
        }
        fn commit(&mut self) -> Result<(), TransactionError> {
            Ok(())
        }
        fn rollback(&mut self) -> Result<(), TransactionError> {
            Ok(())
        }
    }

    #[test]
    fn defaults_report_no_pager() {
        let mut hook = NoPager;
        hook.begin(0).unwrap();
        hook.commit().unwrap();
        hook.rollback().unwrap();
        assert_eq!(hook.wal_autocheckpoint(Some(20)).unwrap(), None);
        assert_eq!(hook.wal_autocheckpoint(None).unwrap(), None);
        assert_eq!(hook.wal_checkpoint(0).unwrap(), None);
        hook.set_journal_mode(0).unwrap();
        assert_eq!(hook.synchronous(), None);
        hook.set_synchronous(0).unwrap();
        assert_eq!(hook.integrity_check(false), None);
    }

    #[test]
    fn transaction_error_displays_its_message() {
        let error = TransactionError("disk full".into());
        assert_eq!(error.to_string(), "disk full");
    }
}
