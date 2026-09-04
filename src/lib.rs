//! `db-core`: the shared SQL language/execution layer for t-rust-db --
//! types, expression AST, parser, join primitives, VM, and codegen, all
//! storage-agnostic (per ADR 0006 -- physical storage lives in
//! `db-storage`, structured as `row`/`column`/`stream` there).
//!
//! Was six separate crates (`sql-types`, `sql-expr`, `sql-parser`,
//! `sql-join`, `sql-vm`, `sql-codegen`); merged into one, feature-gated
//! by module, matching the pattern already used *inside* `vm`/`parser`/
//! `codegen` for their own `batch`/`row`/`stream` splits. See this
//! crate's `CHANGELOG.md` for the migration.
//!
//! - [`types`] / [`expr`] -- always compiled, no feature gate (small,
//!   no dependencies, needed by everything else).
//! - [`join`] -- always compiled (small, no dependencies); its only
//!   consumer today is `vm`'s `vm-batch` feature, but gating it
//!   separately isn't worth the complexity for ~250 lines with zero
//!   deps.
//! - [`parser`] -- `parser-column` (default) / `parser-row`.
//! - [`vm`] -- `vm-batch` (default) / `vm-row` / `vm-stream`.
//! - [`codegen`] -- `codegen-batch` (default, needs `vm-batch`) /
//!   `codegen-row` / `codegen-stream`.

pub mod expr;
pub mod join;
pub mod types;

pub mod codegen;
pub mod parser;
pub mod vm;
