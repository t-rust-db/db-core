//! `RowExecutor`: the cursor-driven, row-at-a-time query VM -- one of
//! `sql-vm`'s three executors (see crate root docs).
//!
//! **Partial (db-core#18, phase 1 of the tracking issue).** This module
//! currently carries only the value-semantics slice mechanically ported
//! from sqlite-rs's VDBE -- [`value`] (the row `Value`/`Collation` model
//! and REAL formatting), [`compare`] (cross-type ordering), [`logic`]
//! (three-valued logic / NULL propagation), [`affinity`] (column type
//! affinity), [`cast`] (`CAST` conversion), [`coerce`] (text-to-numeric
//! coercion and checked arithmetic), and [`program`] (a partial
//! `Opcode`/`Instruction`/`Program` skeleton covering just those
//! opcodes).
//!
//! **Not yet ported**: the fetch-decode-execute loop (`exec.rs`), a
//! storage-agnostic cursor trait plus real cursor opcodes (`cursor.rs`,
//! sqlite-rs's largest VDBE file), control flow (`control.rs`), result
//! rows (`result.rs`), aggregation (`aggregate.rs`, `hash_agg.rs`),
//! sorting (`sorter.rs`), scalar functions (`functions.rs`), and
//! `EXPLAIN`/`PRAGMA` rendering (`explain.rs`, `pragma.rs`). Tracked as
//! a follow-up sub-ticket of db-core#18.
//!
//! **Opcode-set identity and cursor design: see ADR 0008.** `Opcode` is
//! a mechanical port of sqlite-rs's VDBE opcode set (not a new design),
//! with typed operands rather than raw `p1..p5` slots -- the same
//! departure ADR 0007 made for [`super::batch`]. The eventual cursor
//! abstraction is a storage-agnostic trait `vm::row` defines and an
//! adapter crate implements over `db-storage`, not a direct `db-core` ->
//! `db-storage` dependency.
//!
//! [`super::batch`] is the reference for what "done" looks like once
//! this module is complete: one `Opcode` enum, a `Vm`, and real
//! end-to-end test coverage over that `Vm`.

#![forbid(unsafe_code)]

pub mod affinity;
pub mod cast;
pub mod coerce;
pub mod compare;
pub mod logic;
pub mod program;
pub mod value;

pub use affinity::{affinity_of, apply_affinity, comparison_affinity, Affinity};
pub use cast::cast_to;
pub use compare::compare;
pub use program::{ArithOp, CompareOp, Instruction, LogicOp, Opcode, Program};
pub use value::{compare_text, format_real, Collation, TextEncoding, Value};
