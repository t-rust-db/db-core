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
