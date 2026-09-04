//! `db-core`: the shared SQL language/execution layer for t-rust-db --
//! types, expression AST, parser, join primitives, VM, planner (`codegen`)
//! and AOT source emitter (`emit`), all
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
//! - [`codegen`] -- the planner, AST -> executable `Program` (sqlite-rs's
//!   sense of "codegen", ADR 0007): `codegen-batch` (default, needs
//!   `vm-batch`) / `codegen-row` / `codegen-stream`.
//! - [`emit`] -- ahead-of-time Rust-source emitter (a planned `Program`
//!   -> `const PROGRAM` source text; batch-only, no sqlite-rs
//!   equivalent): `emit-batch` (default, needs `codegen-batch`) /
//!   `emit-row` / `emit-stream`.

pub mod expr;
pub mod join;
pub mod types;

pub mod codegen;
pub mod emit;
pub mod parser;
pub mod vm;
