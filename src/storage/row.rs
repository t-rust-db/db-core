//! Physical storage for row-oriented (sqlite-style) execution: paged
//! files, WAL/rollback-journal, and the on-disk record format. Migrated
//! in from `db-core`'s `sql-vfs`/`sql-pager`/`sql-header`/`sql-record`
//! crates (db-core#39, per ADR 0006) as a single feature-gated module,
//! matching this crate's existing `column` module.
//!
//! Carries forward the strict `[lints.clippy]` baseline those four
//! crates each had in `db-core` (`unwrap_used`/`expect_used`/
//! `indexing_slicing`/`panic`/`arithmetic_side_effects` denied) as inner
//! attributes scoped to this module tree, rather than in `Cargo.toml`
//! crate-wide -- `column` predates that baseline and isn't written to
//! it. `mod_module_files` is dropped from the ported set: this module
//! deliberately uses `mod.rs`-per-submodule, matching `column`'s own
//! existing convention in this crate.
#![deny(clippy::let_underscore_must_use)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::panic)]
#![deny(clippy::arithmetic_side_effects)]

pub mod btree;
pub mod format;
pub mod header;
pub mod integrity;
pub mod pager;
pub mod record;
pub mod schema;
pub mod vfs;
