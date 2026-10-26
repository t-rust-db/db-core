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
//! The first argument may name several tables, comma-separated, each a
//! Parquet file or a table directory (#782): a query joining two of them
//! probes with the larger. `--seed-extra-dir <dir>` writes the second
//! directory the script compares: `extra(id, n)`, ids 1..=1000 three times
//! each, flushed every 500 rows.
//!
//! Usage: `column_join_oracle <table>[,<more>...] <lookup.db> <sql>`
//! or `column_join_oracle --seed-dir <dir>` / `--seed-extra-dir <dir>`

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

/// `extra(id, n)`: `n` in 0..3000, `id` = `n % 1000 + 1`, ordered by id then
/// n, flushed every 500 rows.
fn seed_extra_dir(dir: &Path) -> Result<(), String> {
    let schema = vec![
        ColumnSpec::new("id", ColumnType::Int64, false),
        ColumnSpec::new("n", ColumnType::Int64, false),
    ];
    let mut w = TableWriter::open(
        dir,
        TableOptions {
            schema: Some(schema),
            ..TableOptions::default()
        },
    )
    .map_err(|e| e.to_string())?;
    let mut rows: Vec<(i64, i64)> = (0..3000i64)
        .map(|n| (n.rem_euclid(1000).saturating_add(1), n))
        .collect();
    rows.sort_unstable();
    for (i, (id, n)) in rows.iter().enumerate() {
        w.append_row(
            vec![RecordValue::Int(*id), RecordValue::Int(*n)],
            Ack::Durable,
        )
        .map_err(|e| e.to_string())?;
        if i.saturating_add(1).checked_rem(500) == Some(0) {
            w.flush_all().map_err(|e| e.to_string())?;
        }
    }
    w.close().map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if let [_, flag, dir] = args.as_slice() {
        let seed = match flag.as_str() {
            "--seed-dir" => Some(seed_dir as fn(&Path) -> Result<(), String>),
            "--seed-extra-dir" => Some(seed_extra_dir as fn(&Path) -> Result<(), String>),
            _ => None,
        };
        if let Some(seed) = seed {
            return match seed(Path::new(dir)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("seed {dir}: {err}");
                    ExitCode::FAILURE
                }
            };
        }
    }
    let [_, parquet, lookup, sql] = args.as_slice() else {
        eprintln!(
            "usage: column_join_oracle <path.parquet>[,<more.parquet>...] | <table dir> <lookup.db> <sql>"
        );
        return ExitCode::FAILURE;
    };

    let tables: Vec<&Path> = parquet.split(',').map(Path::new).collect();
    let opened = match tables.as_slice() {
        [only] if only.is_dir() => CrossModeEngine::open_table_sqlite(only, Path::new(lookup)),
        _ => CrossModeEngine::open_column_tables_sqlite(&tables, Path::new(lookup)),
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
