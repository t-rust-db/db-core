// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Manual oracle-parity aid for the row engine's scalar functions (#651,
//! ADR 0015: a manual aid, not a suite). Runs each line of a file as one
//! statement against a scratch copy of the row fixture and prints one
//! line per statement: the rows in sqlite3's `.mode quote` form (cells
//! joined by `,`, rows by ` | `), or `ERROR` -- what
//! `tools/check_row_functions_oracle.py` compares against sqlite3.
//!
//! Usage: `row_oracle <cases.sql>`

use std::path::PathBuf;
use std::process::ExitCode;

use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine};

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

/// sqlite3's `.mode quote`, except reals: printed at round-trip precision
/// for the script to compare as doubles.
fn quote(c: &Cell) -> String {
    match c {
        Cell::Null => "NULL".to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::Real(x) => format!("{x:?}"),
        Cell::Bool(b) => i64::from(*b).to_string(),
        // sqlite3's CLI prints text as a C string: a NUL ends it.
        Cell::Text(s) => {
            let s = s.split('\0').next().unwrap_or_default();
            format!("'{}'", s.replace('\'', "''"))
        }
        Cell::Blob(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
            format!("X'{hex}'")
        }
        other => other.to_string(),
    }
}

fn main() -> ExitCode {
    let Some(cases) = std::env::args().nth(1) else {
        eprintln!("usage: row_oracle <cases.sql>");
        return ExitCode::from(2);
    };
    let Ok(text) = std::fs::read_to_string(&cases) else {
        eprintln!("cannot read {cases}");
        return ExitCode::from(2);
    };
    let db: PathBuf = std::env::temp_dir().join(format!("row-oracle-{}.db", std::process::id()));
    if std::fs::copy(FIXTURE, &db).is_err() {
        eprintln!("cannot copy {FIXTURE}");
        return ExitCode::from(2);
    }
    let mut engine = match RowEngine::open(&db) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("open: {e}");
            return ExitCode::from(2);
        }
    };
    for sql in text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("--"))
    {
        match engine.run_query(sql) {
            Ok(result) => {
                let rows: Vec<String> = result
                    .rows
                    .iter()
                    .map(|r| r.iter().map(quote).collect::<Vec<_>>().join(","))
                    .collect();
                println!("{}", rows.join(" | "));
            }
            Err(_) => println!("ERROR"),
        }
    }
    std::fs::remove_file(&db).ok();
    ExitCode::SUCCESS
}
