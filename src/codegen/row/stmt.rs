//! `Insert`/`Update`/`Delete` AST -> `Program` compilation (db-core#96,
//! mirroring sqlite-rs's `codegen/stmt.rs` + `codegen/stmt/
//! {insert,update,delete}.rs`). See `super`'s module doc for this
//! compiler's general scope, and [`super::index_maintenance`] for the
//! secondary-index bookkeeping all three share.
//!
//! **Cursor convention:** each compiled `Program` opens its own table
//! cursor at a fixed slot (`TABLE_CURSOR`), followed by one index
//! cursor per `TableSchema::indexes` entry starting at
//! `FIRST_INDEX_CURSOR` -- the caller must `Vm::open_cursor` the same
//! slots before running it (see each module's tests for the pattern).
//!
//! **Scoped down** (mirrors #91/#92's precedent for `Expr`/`Query`):
//! no `INSERT ... SELECT`, `ON CONFLICT`/upsert, multi-table `UPDATE
//! ... FROM`, or `DELETE`'s rowid-seek fast path (sqlite-rs's #336;
//! deferred alongside #94's index/range-scan codegen) -- every `Program`
//! here does a full table scan.

pub mod delete;
pub mod insert;
pub mod update;

pub use delete::compile_delete;
pub use insert::compile_insert;
pub use update::compile_update;

/// The table cursor's fixed slot in every `Program` this module emits.
pub(crate) const TABLE_CURSOR: i32 = 0;
/// The first secondary-index cursor's slot; index `i` on
/// `TableSchema::indexes` gets `FIRST_INDEX_CURSOR + i`.
pub(crate) const FIRST_INDEX_CURSOR: i32 = 1;
