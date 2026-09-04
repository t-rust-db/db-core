//! Shared query codegen for t-rust-db, as three emitters -- one per
//! `sql-vm` executor (db-core#20), mirroring that crate's own
//! `batch`/`row`/`stream` split (ADR 0001):
//!
//! - [`batch`] -- renders a compiled `crate::vm::batch::Opcode` program (or
//!   a reconstructed `crate::expr::Query` literal, for the join/semi-join/
//!   window shapes that bypass the VM program entirely) to standalone
//!   Rust source text. **Implemented** -- extracted from column-rs's
//!   private `src/codegen.rs`, so other `crate::vm::batch` consumers don't
//!   reimplement it.
//! - [`row`] -- the eventual home for sqlite-rs-style codegen (AST to
//!   VDBE-shaped bytecode). **Not yet implemented** -- see its own doc
//!   comment for why, and what sqlite-rs's own `src/codegen/*` (20,548
//!   lines) actually looks like.
//! - [`stream`] -- push-driven codegen for live/unbounded sources. **Not
//!   yet implemented.**
//!
//! Each emitter is gated behind its own Cargo feature (`batch`/`row`/
//! `stream`, `batch` on by default) -- a consumer enables only the
//! one(s) it uses, exactly like `sql-vm`'s own feature split.
//!
//! column-rs's own query *planning* (`compile()`, deciding which
//! `crate::expr::Query` shape needs the VM-program path vs. the join/
//! semi-join/window bypass, materializing a flat query's `Opcode`
//! program) is NOT part of this crate -- it's product-specific query
//! optimization, not code generation, and stays in column-rs's own
//! `src/query.rs` for now. [`batch`]'s rendering functions take an
//! already-planned program/`Query` and turn it into text; they don't
//! plan anything themselves.

#![forbid(unsafe_code)]

#[cfg(feature = "codegen-batch")]
pub mod batch;
#[cfg(feature = "codegen-row")]
pub mod row;
#[cfg(feature = "codegen-stream")]
pub mod stream;
