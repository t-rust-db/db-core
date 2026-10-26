// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Manual oracle-parity aid (#598), the join sibling of `column_oracle`:
//! runs one `SELECT` joining a Parquet file to SQLite lookup tables through
//! `CrossModeEngine::open_column_sqlite` and prints the result as CSV.
//! `tools/check_column_join_oracle.sh` runs the same statement through
//! DuckDB with the Parquet file read directly and the SQLite file attached,
//! and diffs the two outputs.
//!
//! Cells print as `Cell`'s own `Display`: these queries sum nothing, so
//! reals match DuckDB's text without a fixed precision.
//!
//! The first argument may name several Parquet files, comma-separated
//! (#782): a query joining two of them probes with the larger.
//!
//! Usage: `column_join_oracle <path.parquet>[,<more.parquet>...] <lookup.db> <sql>`

use std::env;
use std::path::Path;
use std::process::ExitCode;

use db_core::engine::resolve::CrossModeEngine;
use db_core::engine::{Cell, Engine};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let [_, parquet, lookup, sql] = args.as_slice() else {
        eprintln!("usage: column_join_oracle <path.parquet>[,<more.parquet>...] <lookup.db> <sql>");
        return ExitCode::FAILURE;
    };

    let files: Vec<&Path> = parquet.split(',').map(Path::new).collect();
    let mut engine = match CrossModeEngine::open_column_tables_sqlite(&files, Path::new(lookup)) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("open {parquet} + {lookup}: {err}");
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
        let line: Vec<String> = row.iter().map(Cell::to_string).collect();
        println!("{}", line.join(","));
    }
    ExitCode::SUCCESS
}
