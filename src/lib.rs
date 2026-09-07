//! `db-core`: the shared SQL language/execution layer for t-rust-db --
//! types, parser (with its single AST, `parser::ast`), VM (with its
//! join primitives at `vm::join`), planner (`codegen`, whose `batch`
//! submodule includes the AOT Rust-source emitter at `codegen::batch::
//! emit`), all storage-agnostic (per ADR 0006 -- physical storage lives
//! in `db-storage`, structured as `row`/`column`/`stream` there).
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
//! - [`compare`]/[`coerce`]/[`functions`] -- always compiled, no feature
//!   gate: cross-type ordering, text-to-numeric coercion, and the ~30
//!   scalar functions (`substr`/`like`/`glob`/...), all pure `Value`-only
//!   logic with one correct behavior regardless of which `vm` executor
//!   calls it (db-core#122, mirroring [`value`]'s own ADR 0010 hoist).
//!   `vm::row` re-exports all three today; `vm-batch`/`vm-stream` have a
//!   documented path to call [`functions::call`] directly once either
//!   needs scalar functions of its own.
//! - [`parser`] -- `parser-column` (default) / `parser-row`.
//! - [`vm`] -- `vm-batch` (default) / `vm-row` / `vm-stream`. Its `join`
//!   submodule (db-core#193, moved from a former crate-level `join`
//!   module) is always compiled, ungated -- shared join infrastructure
//!   (`JoinHashTable`, `JoinKind`, `should_emit`), small and
//!   dependency-free, whose only consumer today is `vm-batch`, but
//!   available to `vm::row` once #117 needs a hash join.
//! - [`codegen`] -- the planner, AST -> executable `Program` (sqlite-rs's
//!   sense of "codegen", ADR 0007): `codegen-batch` (default, needs
//!   `vm-batch` and `parser-column`) / `codegen-row` / `codegen-stream`.
//!   `codegen::batch::emit` (db-core#192, folded in from a former
//!   crate-level `emit` module) is the ahead-of-time Rust-source
//!   emitter over the batch planner's output (a planned `Program` ->
//!   `const PROGRAM` source text; batch-only, no sqlite-rs equivalent),
//!   gated by its own `emit-batch` feature (default, needs
//!   `codegen-batch`).

#![deny(unsafe_code)]
#![warn(missing_docs)]

/// This crate's version, as embedded in `codegen::batch::emit`'s
/// generated-source headers.
///
/// A plain constant rather than `env!("CARGO_PKG_VERSION")`: the
/// qualified-subset gate (`make check-mvl-limit`) keeps `env!` out of
/// `src/`. Must match `Cargo.toml`'s `version` -- `tests/version.rs`
/// fails the build if the two drift, so bump both together on release.
pub const VERSION: &str = "0.63.0";

pub mod coerce;
pub mod compare;
pub mod functions;
pub mod types;
pub mod value;

pub mod codegen;
pub mod parser;
pub mod vm;
