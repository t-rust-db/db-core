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
