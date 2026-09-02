//! `RowExecutor`: the cursor-driven, row-at-a-time query VM -- one of
//! `sql-vm`'s three executors (see crate root docs).
//!
//! **Not yet implemented.** This is the eventual home for a VDBE-style
//! bytecode machine (in the sense of "executes one row's worth of work per
//! instruction pass," the sqlite-rs sense of the term -- not a port of
//! sqlite-rs's actual VDBE, whose ~65 opcodes are deeply SQLite-specific,
//! spec-driven, and independently tested there). Whether this module ends
//! up sharing an opcode *set* with sqlite-rs, or just the row-at-a-time
//! *execution strategy* while keeping its own opcodes, is an open design
//! question -- see `t-rust-db/grammar/ALIGNMENT.md` §3 for the history:
//! this crate previously kept `RowExecutor` entirely inside sqlite-rs,
//! unshared; that call was reversed in favor of housing all three
//! executors here, but the reversal is a placeholder for now, not a
//! finished design.
//!
//! [`super::batch`] is the reference for what "done" looks like here:
//! `Opcode`, a `Vm`, register/row semantics, and real test coverage.
