// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Manual oracle-parity aid (db-core#406, ADR 0015: "`examples/
//! oracle_check.rs` is a manual aid, not a suite"): runs one SQL statement
//! against a Parquet file through `engine::column::BatchEngine` and prints
//! the result as CSV, one row per line. `tools/check_column_oracle.sh` runs
//! the same statement through DuckDB reading the same file and diffs the
//! two outputs -- this binary is that script's db-core-side half, not a
//! test target of its own.
//!
//! Usage: `column_oracle <path.parquet> <sql>`

use std::env;
use std::path::Path;
use std::process::ExitCode;

use db_core::engine::column::BatchEngine;
use db_core::engine::{Cell, Engine};

/// Reals print at a fixed 2-decimal precision (not `Cell`'s own
/// SQLite-`%!.15g`-style `Display`) so summation-order differences between
/// this engine and DuckDB -- both correct, neither canonical -- don't read
/// as an oracle mismatch. The fixture's amounts have at most one decimal
/// digit, so 2 decimals loses no real precision.
fn format_cell(c: &Cell) -> String {
    match c {
        Cell::Real(x) => format!("{x:.2}"),
        other => other.to_string(),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let [_, path, sql] = args.as_slice() else {
        eprintln!("usage: column_oracle <path.parquet> <sql>");
        return ExitCode::FAILURE;
    };

    let mut engine = match BatchEngine::open(Path::new(path)) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("open {path}: {err}");
            return ExitCode::FAILURE;
        }
    };

    let result = match engine.run_query(sql) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("query failed: {err}");
            return ExitCode::FAILURE;
        }
    };

    for row in &result.rows {
        let line: Vec<String> = row.iter().map(format_cell).collect();
        println!("{}", line.join(","));
    }
    ExitCode::SUCCESS
}
