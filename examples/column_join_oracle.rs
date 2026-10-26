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
//! The first argument may be a table directory instead of a Parquet file
//! (#772): its archive files and live pages probe the lookup tables through
//! `CrossModeEngine::open_table_sqlite`. `--seed-dir <dir>` writes the
//! directory the script compares: `production(id, region)`, ids 1..=5000,
//! flushed in five files so DuckDB, which cannot read a WAL, sees every row.
//!
//! Usage: `column_join_oracle <path.parquet | table dir> <lookup.db> <sql>`
//! or `column_join_oracle --seed-dir <dir>`

use std::env;
use std::path::Path;
use std::process::ExitCode;

use db_core::engine::resolve::CrossModeEngine;
use db_core::engine::{Cell, Engine};
use db_core::storage::ingest::{Ack, RecordValue, TableOptions, TableWriter};
use db_core::storage::parquet::schema::{ColumnSpec, ColumnType};

/// `production(id, region)`, ids 1..=5000, the region cycling north, south,
/// east, west, flushed every 1000 ids.
fn seed_dir(dir: &Path) -> Result<(), String> {
    let schema = vec![
        ColumnSpec::new("id", ColumnType::Int64, false),
        ColumnSpec::new("region", ColumnType::Utf8, false),
    ];
    let mut w = TableWriter::open(
        dir,
        TableOptions {
            schema: Some(schema),
            ..TableOptions::default()
        },
    )
    .map_err(|e| e.to_string())?;
    let regions = ["north", "south", "east", "west"];
    for (id, region) in (1..=5000i64).zip(regions.iter().cycle()) {
        w.append_row(
            vec![
                RecordValue::Int(id),
                RecordValue::Str((*region).to_string()),
            ],
            Ack::Durable,
        )
        .map_err(|e| e.to_string())?;
        if id.checked_rem(1000) == Some(0) {
            w.flush_all().map_err(|e| e.to_string())?;
        }
    }
    w.close().map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if let [_, flag, dir] = args.as_slice() {
        if flag == "--seed-dir" {
            return match seed_dir(Path::new(dir)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("seed {dir}: {err}");
                    ExitCode::FAILURE
                }
            };
        }
    }
    let [_, parquet, lookup, sql] = args.as_slice() else {
        eprintln!("usage: column_join_oracle <path.parquet | table dir> <lookup.db> <sql>");
        return ExitCode::FAILURE;
    };

    let opened = if Path::new(parquet).is_dir() {
        CrossModeEngine::open_table_sqlite(Path::new(parquet), Path::new(lookup))
    } else {
        CrossModeEngine::open_column_sqlite(Path::new(parquet), Path::new(lookup))
    };
    let mut engine = match opened {
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
