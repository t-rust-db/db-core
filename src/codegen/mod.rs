//! Query planners -- AST ([`crate::expr::Query`]) to an executable
//! [`crate::vm`] `Program` -- one per `vm` executor, mirroring `vm`'s own
//! `batch`/`row`/`stream` split (ADR 0001).
//!
//! **Naming (ADR 0007):** *codegen* here means exactly what sqlite-rs's
//! `src/codegen/*` means -- planning. The ahead-of-time *Rust-source*
//! emitter that column-rs used to call "codegen" is [`crate::emit`].
//!
//! - [`batch`] -- the columnar planner: `compile()` turns a flat/`GROUP
//!   BY`/`ORDER BY`/`LIMIT` query into a [`crate::vm::batch::Program`]
//!   ending in [`crate::vm::batch::Opcode::Finalize`], plus the join/
//!   semi-join/window program assembly and `EXPLAIN` plan-tree
//!   construction. **Implemented** -- moved from column-rs's `src/query.rs`,
//!   which never touched Parquet in these parts.
//! - [`row`] -- the eventual home for the sqlite-rs-style planner (AST to
//!   VDBE-shaped bytecode). **Not yet implemented** -- see its own doc
//!   comment for the port target.
//! - [`stream`] -- push-driven planner for live/unbounded sources. **Not
//!   yet implemented.**
//!
//! Each planner is gated behind its own Cargo feature (`codegen-batch`/
//! `codegen-row`/`codegen-stream`, `codegen-batch` on by default);
//! `codegen-batch` needs `vm-batch` for the `Program`/`Opcode` types it
//! builds.

#![forbid(unsafe_code)]

#[cfg(feature = "codegen-batch")]
pub mod batch;
#[cfg(feature = "codegen-row")]
pub mod row;
#[cfg(feature = "codegen-stream")]
pub mod stream;
