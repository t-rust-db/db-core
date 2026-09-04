//! `RowExecutor`: the cursor-driven, row-at-a-time query VM -- one of
//! `sql-vm`'s three executors (see crate root docs).
//!
//! **Partial (db-core#18/#51/#56/#59, tracking issue db-core#18).** A
//! literal, opcode-for-opcode port of sqlite-rs's VDBE (ADR 0008,
//! revised for full parity -- see that ADR's history: an earlier draft
//! mistakenly generalized [`super::batch`]'s typed-operand design to
//! `row`, which does not apply here). Ported so far:
//!
//! - [`value`] -- `Value`/`Collation`/`compare_text`/`format_real`.
//! - [`compare`] -- cross-type ordering (NULL < numeric < text < blob).
//! - [`logic`] -- three-valued logic / NULL propagation (codegen-side
//!   helpers; not used by the exec loop itself, same as sqlite-rs).
//! - [`affinity`] -- column type affinity.
//! - [`cast`] -- `CAST` conversion.
//! - [`coerce`] -- text-to-numeric coercion and checked arithmetic.
//! - [`program`] -- `Opcode`/`Instruction`/`Program`, sqlite-rs's raw
//!   `p1..p5` operand shape (not typed named fields).
//! - [`cursor`] -- the storage-agnostic [`cursor::Cursor`] trait ADR
//!   0008 calls for, [`cursor::InMemoryCursor`] (a read-only test
//!   fixture), and [`cursor::EphemeralTableCursor`] (a real,
//!   `Opcode::Insert`-writable in-memory table backing `Opcode::
//!   OpenEphemeral`'s table-mode cursor).
//! - [`record`] -- on-disk record encoding/decoding (`encode_record`/
//!   `decode_record`/`decode_column`), backing `Opcode::MakeRecord` and
//!   ephemeral-cursor `Insert`/`Column`.
//! - [`vm`] -- the register file, cursor-slot table, and
//!   fetch-decode-execute loop (`Vm`, [`vm::execute`]) -- control flow,
//!   compare/cast/arithmetic, result-row loads including `MakeRecord`,
//!   `Rewind`/`Next`/`Column`/`Rowid` over [`cursor::Cursor`], and
//!   `OpenEphemeral`/`Insert` over [`cursor::EphemeralTableCursor`].
//!
//! **Not yet ported**: real `db-storage` cursor wiring (`cursor.rs`'s
//! largest file, real `OpenRead`/`OpenWrite`), `OpenDup`/`OpenPseudo`
//! and index-mode ephemeral cursors, DDL, real transactions, sorter,
//! hash aggregation, scalar functions, `EXPLAIN`/`PRAGMA` rendering.
//! `Opcode` already lists every variant sqlite-rs has (parity of
//! identity); dispatch for the rest lands in later phases (tracked
//! against db-core#18).
//!
//! [`super::batch`] is a *structural* reference only (module layout,
//! doc density, in-module tests) -- its typed-operand `Opcode` design
//! is `vm::batch`-specific (ADR 0007) and does not extend to `row`.

#![forbid(unsafe_code)]

pub mod affinity;
pub mod cast;
pub mod coerce;
pub mod compare;
pub mod cursor;
pub mod logic;
pub mod program;
pub mod record;
pub mod value;
pub mod vm;

pub use affinity::{affinity_of, apply_affinity, comparison_affinity, Affinity};
pub use cast::cast_to;
pub use compare::compare;
pub use cursor::{Cursor, EphemeralTableCursor, InMemoryCursor};
pub use program::{Instruction, Opcode, Program, P4};
pub use record::{decode_column, decode_record, encode_record, RecordError};
pub use value::{compare_text, format_real, Collation, TextEncoding, Value};
pub use vm::{execute, ExecError, Step, Vm};
