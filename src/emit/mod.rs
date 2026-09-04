//! Ahead-of-time Rust-source emitters -- one per `vm` executor, mirroring
//! `vm`'s own `batch`/`row`/`stream` split (ADR 0001).
//!
//! **Naming (ADR 0007):** in this family, *codegen* means what sqlite-rs
//! means by it -- the planner turning an AST into an executable
//! [`crate::vm`] `Program` ([`crate::codegen`]). *emit* is this module:
//! it takes an already-planned program and renders it to standalone Rust
//! **source text** (`const PROGRAM` in a `.rs` file for rustc), so a query
//! can be baked into a binary with no runtime parser. sqlite-rs has no
//! equivalent step; column-rs's `codegen` CLI subcommand is the consumer.
//!
//! - [`batch`] -- renders a planned [`crate::codegen::batch`] program (or a
//!   reconstructed [`crate::expr::Query`] literal, for the join/semi-join/
//!   window shapes that bypass the flat program) to Rust source.
//!   **Implemented** -- extracted from column-rs's private `src/codegen.rs`.
//! - [`row`] -- emitter for a future `vm::row` program. **Not yet
//!   implemented.**
//! - [`stream`] -- emitter for a future `vm::stream` program. **Not yet
//!   implemented.**
//!
//! Each emitter is gated behind its own Cargo feature (`emit-batch`/
//! `emit-row`/`emit-stream`, `emit-batch` on by default). `emit-batch`
//! pulls in `codegen-batch`: rendering a planned program presupposes the
//! planner that produced it.

#![forbid(unsafe_code)]

#[cfg(feature = "emit-batch")]
pub mod batch;
#[cfg(feature = "emit-row")]
pub mod row;
#[cfg(feature = "emit-stream")]
pub mod stream;
