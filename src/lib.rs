//! `db-core`: the shared SQL language/execution layer for t-rust-db --
//! types, parser (with its single AST, `parser::ast`), join primitives,
//! VM, planner (`codegen`) and AOT source emitter (`emit`), all
//! storage-agnostic (per ADR 0006 -- physical storage lives in
//! `db-storage`, structured as `row`/`column`/`stream` there).
//!
//! Was six separate crates (`sql-types`, `sql-expr`, `sql-parser`,
//! `sql-join`, `sql-vm`, `sql-codegen`); merged into one, feature-gated
//! by module, matching the pattern already used *inside* `vm`/`parser`/
//! `codegen` for their own `batch`/`row`/`stream` splits. See this
//! crate's `CHANGELOG.md` for the migration.
//!
//! - [`types`] -- always compiled, no feature gate (small, no
//!   dependencies, needed by everything else). The former `expr` module
//!   (a private AST the batch planner alone consumed) was retired in
//!   #153: `parser::ast::Select` is now the crate's single AST, consumed
//!   directly by both `codegen::batch` and `codegen::row`.
//! - [`join`] -- always compiled (small, no dependencies); its only
//!   consumer today is `vm`'s `vm-batch` feature, but gating it
//!   separately isn't worth the complexity for ~250 lines with zero
//!   deps.
//! - [`parser`] -- `parser-column` (default) / `parser-row`.
//! - [`vm`] -- `vm-batch` (default) / `vm-row` / `vm-stream`.
//! - [`codegen`] -- the planner, AST -> executable `Program` (sqlite-rs's
//!   sense of "codegen", ADR 0007): `codegen-batch` (default, needs
//!   `vm-batch` and `parser-column`) / `codegen-row` / `codegen-stream`.
//! - [`emit`] -- ahead-of-time Rust-source emitter (a planned `Program`
//!   -> `const PROGRAM` source text; batch-only, no sqlite-rs
//!   equivalent): `emit-batch` (default, needs `codegen-batch`) /
//!   `emit-row` / `emit-stream`.

#![deny(unsafe_code)]
#![warn(missing_docs)]

/// This crate's version, as embedded in `emit`'s generated-source headers.
///
/// A plain constant rather than `env!("CARGO_PKG_VERSION")`: the
/// qualified-subset gate (`make check-mvl-limit`) keeps `env!` out of
/// `src/`. Must match `Cargo.toml`'s `version` -- `tests/version.rs`
/// fails the build if the two drift, so bump both together on release.
pub const VERSION: &str = "0.58.0";

pub mod join;
pub mod types;
pub mod value;

pub mod codegen;
pub mod emit;
pub mod parser;
pub mod vm;
