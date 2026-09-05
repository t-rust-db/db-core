//! `RowExecutor`: the cursor-driven, row-at-a-time query VM -- one of
//! `sql-vm`'s three executors (see crate root docs).
//!
//! **Partial (db-core#18/#51/#56/#59/#62/#64/#68/#69, tracking issue
//! db-core#18).** A literal, opcode-for-opcode port of sqlite-rs's VDBE
//! (ADR 0008, revised for full parity -- see that ADR's history: an
//! earlier draft mistakenly generalized [`super::batch`]'s
//! typed-operand design to `row`, which does not apply here). Ported
//! so far:
//!
//! - [`value`] -- `Value`/`Collation`/`compare_text`/`format_real`.
//! - [`compare`] -- cross-type ordering (NULL < numeric < text < blob).
//! - [`logic`] -- three-valued logic / NULL propagation (codegen-side
//!   helpers; not used by the exec loop itself, same as sqlite-rs).
//! - [`affinity`] -- column type affinity.
//! - [`cast`] -- `CAST` conversion.
//! - [`coerce`] -- text-to-numeric coercion and checked arithmetic.
//! - [`aggregate`] -- `AggState`/`step`/`finalize` (`COUNT`/`SUM`/
//!   `AVG`/`MIN`/`MAX`), backing `Opcode::AggStep`/`AggFinal` (single
//!   accumulator per slot) and, via [`cursor::HashAggCursor`]
//!   (db-core#86), `Opcode::HashAggStep`'s per-group accumulators.
//! - [`functions`] -- scalar functions, backing `Opcode::Function`,
//!   now closing the gap against sqlite-rs's `vdbe::functions`
//!   entirely: `abs`/`length`/`upper`/`lower`/`coalesce`/`ifnull`/
//!   `nullif`/`typeof` (db-core#64), `sign`/`zeroblob`/`iif`/scalar
//!   `min`/`max`/`sqlite_version`/`round`/`hex`/`unhex`/`instr`/
//!   `quote` (db-core#68), and `substr`/`trim`/`ltrim`/`rtrim`/
//!   `replace`/`like`/`glob` (db-core#90, whose `like_match`/
//!   `glob_match` are exposed for a future `LIKE`/`GLOB` operator to
//!   call directly).
//! - [`program`] -- `Opcode`/`Instruction`/`Program`, sqlite-rs's raw
//!   `p1..p5` operand shape (not typed named fields).
//! - [`cursor`] -- the storage-agnostic [`cursor::Cursor`] trait ADR
//!   0008 calls for, [`cursor::InMemoryCursor`] (a read-only test
//!   fixture), [`cursor::EphemeralTableCursor`] (a real,
//!   `Opcode::Insert`-writable in-memory table backing `Opcode::
//!   OpenEphemeral`'s table-mode cursor), and [`cursor::SorterCursor`]
//!   (db-core#69, extended by db-core#87 to multi-key `ORDER BY` and an
//!   optional `LIMIT`-derived top-K bound), backing `Opcode::
//!   SorterOpen`/`Insert`/`Sort`/`Next`/`Data`, and
//!   [`cursor::HashAggCursor`] (db-core#86, backing `Opcode::
//!   HashAggOpen`/`Find`/`Step`/`Rewind`/`Data`/`Next` -- the O(n)
//!   `GROUP BY` alternative to the sort strategy; group lookup is a
//!   linear scan rather than an actual hash table, a deliberate
//!   simplification documented on the type itself).
//! - [`record`] -- on-disk record encoding/decoding (`encode_record`/
//!   `decode_record`/`decode_column`), backing `Opcode::MakeRecord`,
//!   ephemeral-cursor `Insert`/`Column`, and sorter key decoding.
//! - [`explain`] -- `EXPLAIN` output rendering (db-core#88):
//!   [`explain::explain`] renders a [`program::Program`] as one
//!   [`explain::ExplainRow`] per instruction (addr/opcode/p1-p5/p4/
//!   comment), preferring an instruction's own `comment` field (ADR
//!   0007) over a computed default. `EXPLAIN QUERY PLAN`'s tree
//!   renderer is out of scope here -- in sqlite-rs it lives in the
//!   query *planner* (`codegen/select/eqp.rs`), not the VDBE.
//! - [`vm`] -- the register file, cursor-slot table, aggregate-context
//!   slot table, and fetch-decode-execute loop (`Vm`, [`vm::execute`])
//!   -- control flow, compare/cast/arithmetic, result-row loads
//!   including `MakeRecord`, `Rewind`/`Next`/`Column`/`Rowid` over
//!   [`cursor::Cursor`], `OpenEphemeral`/`Insert` over
//!   [`cursor::EphemeralTableCursor`], `AggStep`/`AggFinal` over
//!   [`aggregate::AggState`], `Function` over [`functions::call`], and
//!   the sorter opcodes over [`cursor::SorterCursor`]. `OpenRead`/
//!   `OpenWrite` (db-core#76) are also dispatched, but only as an
//!   assertion that the caller pre-wired the cursor slot via
//!   `Vm::open_cursor` -- real root-page/pager semantics against
//!   `db-storage` are still not implemented (blocked on
//!   t-rust-db/db-storage#8, which adds the read-only `TableCursor`
//!   this trait's eventual adapter will wrap). `SetJournalMode`/
//!   `Synchronous` (db-core#89) are also dispatched, but -- since
//!   `db-core` has no pager of its own -- reduce to the no-writer
//!   fallback sqlite-rs itself defines for a read-only connection:
//!   `SetJournalMode` is a no-op (erroring only on `Vm::autocommit`),
//!   and `Synchronous`'s query form always reports `FULL`.
//!   `Opcode::Transaction`/`AutoCommit` (db-core#81) toggle
//!   `Vm::autocommit` and, when one is installed via [`vm::Vm::
//!   set_transaction_hook`], call into a [`transaction::Transaction`]
//!   hook -- with none installed, they reduce to the same no-op
//!   `SetJournalMode` already assumed. `Opcode::SeekRowid` (db-core#81)
//!   is also dispatched, over [`cursor::Cursor::seek`]. `IntegrityCheck`
//!   is *not* dispatched -- it has no no-writer fallback in sqlite-rs
//!   (it always needs a real page source), so it stays
//!   `ExecError::Unimplemented` until `db-core` has one to attach.
//!
//! [`cursor::Cursor`] also grew `prev`/`last`/`delete` (db-core#76) and
//! `seek`/`payload` (db-core#81), matching the shape a real
//! storage-backed cursor will need -- proven sufficient by a mock
//! `TableCursor`-shaped implementor in [`cursor`]'s own tests, ahead of
//! the real `db-storage` adapter (which lives in the consumer,
//! t-rust-db/sqlite-rs, per ADR 0008's amendment) landing there.
//!
//! - [`transaction`] -- the [`transaction::Transaction`] hook surface
//!   (db-core#81) a consumer's pager installs to observe/react to
//!   `BEGIN`/`COMMIT`/`ROLLBACK`.
//! - [`cursor_conformance`] -- trait-level [`cursor::Cursor`]
//!   conformance checks (db-core#81), public so a real adapter can run
//!   the same checks this crate runs against its own fixtures.
//!
//! - [`cursor_factory`] -- the [`cursor_factory::CursorFactory`] hook
//!   (db-core#125) a consumer's pager installs via [`vm::Vm::
//!   set_cursor_factory`] to resolve `OpenRead`/`OpenWrite`'s `p2` root
//!   page to a real cursor at run time; `OpenDup` re-opens an
//!   already-open slot's root a second time, and `OpenPseudo` opens
//!   [`cursor::PseudoCursor`] over an already-`MakeRecord`-encoded
//!   register. With no factory installed, `OpenRead`/`OpenWrite` keep
//!   the earlier pre-wired-slot fallback.
//!
//! - [`schema_storage`] -- the [`schema_storage::SchemaStorage`] hook
//!   (db-core#128) a consumer's pager installs via [`vm::Vm::
//!   set_schema_storage`] to back `CreateTable`/`CreateIndex`/
//!   `DropTable`/`DropIndex`/`Analyze` -- allocating/freeing b-tree
//!   roots, writing/deleting `sqlite_master` rows, bumping the schema
//!   cookie, and computing `sqlite_stat1` rows, none of which
//!   `db-core` can do itself (ADR 0008). With no hook installed, those
//!   five opcodes fail with `ExecError::SchemaStorageMissing`.
//!
//! [`vm::Vm`] also now dispatches (db-core#127): `Opcode::AutoIndexInsert`/
//! `Seek`/`Rowid`/`Next` over [`cursor::AutoIndexCursor`] (a transient,
//! in-memory join index that never touches storage -- `AutoIndexInsert`
//! opens the slot itself on first use, since there's no dedicated
//! `OpenAutoIndex` opcode); `Opcode::Count` (a fast path via
//! [`cursor::Cursor::count`], falling back to a full `rewind`/`next`
//! scan when a cursor kind doesn't track one); `Opcode::Last` (mirrors
//! `Rewind`, over [`cursor::Cursor::last`]); `Opcode::NullRow` (marks a
//! cursor slot's `Column`/`Rowid` reads as NULL/0 until the next
//! repositioning opcode clears it -- `LEFT JOIN`'s unmatched side);
//! `Opcode::Sequence` (a plain per-slot monotonic counter, independent
//! of any open cursor).
//!
//! **Not yet ported**: real `db-storage` cursor wiring (`cursor.rs`'s
//! largest file), index-mode cursors, `IntegrityCheck`. `Opcode`
//! already lists every variant sqlite-rs has (parity of identity);
//! dispatch for the rest lands in later phases (tracked against
//! db-core#18).
//!
//! [`super::batch`] is a *structural* reference only (module layout,
//! doc density, in-module tests) -- its typed-operand `Opcode` design
//! is `vm::batch`-specific (ADR 0007) and does not extend to `row`.

#![forbid(unsafe_code)]

pub mod affinity;
pub mod aggregate;
pub mod cast;
pub mod coerce;
pub mod compare;
pub mod cursor;
pub mod cursor_conformance;
pub mod cursor_factory;
pub mod explain;
pub mod functions;
pub mod logic;
pub mod program;
pub mod record;
pub mod schema_storage;
pub mod transaction;
pub mod value;
pub mod vm;

pub use affinity::{affinity_of, apply_affinity, comparison_affinity, Affinity};
pub use aggregate::{AggState, AggregateError};
pub use cast::cast_to;
pub use compare::compare;
pub use cursor::{
    AutoIndexCursor, Cursor, EphemeralTableCursor, HashAggCursor, InMemoryCursor, PseudoCursor,
    SorterCursor,
};
pub use cursor_factory::{CursorFactory, CursorFactoryError};
pub use explain::{explain, ExplainRow};
pub use functions::FunctionError;
pub use program::{
    AnalyzeIndexTarget, AnalyzeTarget, GroupKeyColumn, Instruction, Opcode, Program, SortKeyColumn,
    JOURNAL_MODE_DELETE, JOURNAL_MODE_WAL, P4, SYNCHRONOUS_FULL, SYNCHRONOUS_NORMAL,
    SYNCHRONOUS_OFF, SYNCHRONOUS_QUERY, TRANSACTION_MODE_DEFERRED, TRANSACTION_MODE_EXCLUSIVE,
    TRANSACTION_MODE_IMMEDIATE,
};
pub use record::{decode_column, decode_record, encode_record, RecordError};
pub use schema_storage::{SchemaStorage, SchemaStorageError};
pub use transaction::{Transaction, TransactionError};
pub use value::{compare_text, format_real, Collation, TextEncoding, Value};
pub use vm::{execute, ExecError, Step, Vm};
