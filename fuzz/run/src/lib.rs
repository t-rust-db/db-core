//! Totality runner for the ROW frontend (db-core#545, second slice of
//! epic #543): feeds grammar-generated statements through `parser::row`,
//! `codegen::row` and `vm::row` one stage at a time, each under
//! `catch_unwind` with a per-statement timeout, and records only
//! panics and hangs as findings -- typed rejections at any stage are the
//! expected, counted outcome.
//!
//! Stages are probed separately on purpose: a single `run_query` call
//! that panics cannot say whether the parser, the code generator or the
//! VM blew up. Running parse first, then compile-only, then execute means
//! a panic in stage *k* implies stages `< k` already returned normally on
//! the same input, so the finding names the layer at fault.

pub mod dialect;
pub mod findings;
pub mod runner;
pub mod stage;

pub use dialect::RowDialect;
pub use findings::{Finding, FindingsSink};
pub use runner::{
    catalog_of, install_panic_capture, probe, run_parallel, RunConfig, RunError, RunSummary,
    Runner, TempDb,
};
pub use stage::{Outcome, Rejection, Stage};

/// Fixture every worker starts from: `t(id INTEGER PRIMARY KEY, i INTEGER,
/// s TEXT, r REAL, b BLOB)`, one column per storage class.
pub const FIXTURE_REL: &str = "tests/fixtures/btrees/select_parity.db";

/// Absolute path of [`FIXTURE_REL`] resolved from this crate's manifest
/// directory (`fuzz/run` -> repo root is two levels up).
pub fn fixture_path(manifest_dir: &str) -> std::path::PathBuf {
    std::path::Path::new(manifest_dir)
        .join("..")
        .join("..")
        .join(FIXTURE_REL)
}
