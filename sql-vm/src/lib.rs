//! Shared query execution engines for t-rust-db, as three executors over
//! `sql_expr`-compiled queries:
//!
//! - [`batch`] -- `BatchExecutor`: vectorized/columnar, pull-based (row
//!   groups, batches of ~1024 rows at a time). **Implemented** -- this is
//!   column-rs's VM, extracted here so other batch/columnar consumers
//!   (loglume, reading historical log files) don't reimplement it.
//! - [`row`] -- `RowExecutor`: cursor-driven, row-at-a-time. **Not yet
//!   implemented** -- the eventual home for sqlite-rs-style execution.
//! - [`stream`] -- `StreamExecutor`: push-driven, for live/unbounded
//!   sources (loglume tailing Docker/journald/K8s). **Not yet
//!   implemented.**
//!
//! Why all three live in one crate rather than `batch`/`stream` here and
//! `row` staying inside sqlite-rs (the original design, see
//! `t-rust-db/grammar/ALIGNMENT.md` §3 for that history and why it was
//! reversed): consolidating the engines that execute a compiled query --
//! regardless of execution strategy -- in one place, rather than splitting
//! "the VM" across two repos by strategy.
//!
//! Each executor has its own opcode set (`batch::Opcode` and a future
//! `row::Opcode` are NOT the same type, and are not expected to become
//! one) -- see each module's own docs.

#![forbid(unsafe_code)]

pub mod batch;
pub mod row;
pub mod stream;
