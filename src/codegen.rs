//! Query planners -- [`crate::parser::ast::Select`] (the crate's single
//! AST, #153) to an executable [`crate::vm`] `Program` -- one per `vm`
//! executor, mirroring `vm`'s own `batch`/`row`/`stream` split (ADR 0001).
//!
//! **Naming (ADR 0007):** *codegen* here means exactly what sqlite-rs's
//! `src/codegen/*` means -- planning. The ahead-of-time *Rust-source*
//! emitter that column-rs used to call "codegen" is `batch::emit`
//! (db-core#192 folded the former crate-level `emit` module in here,
//! since it renders only [`batch`]'s planner output and had no `row`/
//! `stream` counterpart in practice -- see ADR 0007's addendum).
//!
//! - [`batch`] -- the columnar planner: `compile()` turns a flat/`GROUP
//!   BY`/`ORDER BY`/`LIMIT` query into a [`crate::vm::batch::Program`]
//!   ending in [`crate::vm::batch::Opcode::Combine`] (plus an optional
//!   trailing `Sort`/`Limit`, db-core#48), plus the join/
//!   semi-join/window program assembly and `EXPLAIN` plan-tree
//!   construction. **Implemented** -- moved from column-rs's `src/query.rs`,
//!   which never touched Parquet in these parts. Consumes
//!   [`crate::parser::ast::Select`] directly, the same AST [`row`] does
//!   (#153 retired the private `expr::Query` module it used to consume).
//!   Its `emit` submodule is the AOT Rust-source renderer over this
//!   planner's output, gated by its own `emit-batch` feature.
//! - [`row`] -- sqlite-rs's own planner (AST to VDBE-shaped bytecode),
//!   moved in verbatim by db-core#219 (ADR 0013) after #20/#91-#97's
//!   re-derivation was retired -- see its own doc comment for the source
//!   sha and the two db-core-owned additions.
//! - [`stream`] -- push-driven planner for live/unbounded sources. **Not
//!   yet implemented.**
//!
//! Each planner is gated behind its own Cargo feature (`codegen-batch`/
//! `codegen-row`/`codegen-stream`, `codegen-batch` on by default);
//! `codegen-batch` needs `vm-batch` for the `Program`/`Opcode` types it
//! builds.

#![forbid(unsafe_code)]

#[cfg(feature = "codegen-batch")]
// Lives in `batch_planner.rs`, not `batch.rs`: `cargo-mvl-mcdc` ids
// obligations by file stem + line, so this file and `src/vm/batch.rs`
// collided on every same-numbered decision (#363). The module path
// stays `codegen::batch`.
#[path = "codegen/batch_planner.rs"]
pub mod batch;
#[cfg(feature = "codegen-row")]
pub mod row;
#[cfg(feature = "codegen-stream")]
pub mod stream;
