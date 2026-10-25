// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Manual oracle-parity aid (#598), the join sibling of `column_oracle`:
//! runs one `SELECT` joining a Parquet file to SQLite lookup tables through
//! `CrossModeEngine::open_column_sqlite` and prints the result as CSV.
//! `tools/check_column_join_oracle.sh` runs the same statement through
//! DuckDB with the Parquet file read directly and the SQLite file attached,
//! and diffs the two outputs.
//!
//! Usage: `column_join_oracle <path.parquet> <lookup.db> <sql>`

use std::env;
use std::path::Path;
use std::process::ExitCode;

use db_core::engine::resolve::CrossModeEngine;
use db_core::engine::{Cell, Engine};

fn format_cell(c: &Cell) -> String {
    match c {
        Cell::Real(x) => format!("{x:.2}"),
        other => other.to_string(),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let [_, parquet, lookup, sql] = args.as_slice() else {
        eprintln!("usage: column_join_oracle <path.parquet> <lookup.db> <sql>");
        return ExitCode::FAILURE;
    };

    let mut engine =
        match CrossModeEngine::open_column_sqlite(Path::new(parquet), Path::new(lookup)) {
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
        let line: Vec<String> = row.iter().map(format_cell).collect();
        println!("{}", line.join(","));
    }
    ExitCode::SUCCESS
}
