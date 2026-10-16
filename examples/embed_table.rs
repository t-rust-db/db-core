// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! Embedding a table (ADR 0029, #574): append records, query them while
//! they are only in the WAL and pages, flush them to Parquet, query again.
//!
//! ```text
//! cargo run --example embed_table --features ingest,engine-column [DIR]
//! ```
//!
//! `DIR` defaults to a fresh directory under the system temp directory.
//! The second query reads the same answer from a Parquet file.

use std::path::PathBuf;

use db_core::engine::QueryResult;
use db_core::storage::ingest::{Ack, Table, TableOptions};

fn print(label: &str, result: &QueryResult) {
    println!("{label}: {}", result.columns.join(" | "));
    for row in &result.rows {
        let cells: Vec<String> = row.iter().map(ToString::to_string).collect();
        println!("  {}", cells.join(" | "));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args().nth(1).map_or_else(
        || std::env::temp_dir().join(format!("embed-table-{}/events", std::process::id())),
        PathBuf::from,
    );
    let table = Table::open(&dir, TableOptions::default())?;

    let events = [
        r#"{"user":"ana","event":"login"}"#,
        r#"{"user":"bo","event":"login"}"#,
        r#"{"user":"ana","event":"buy","amount":12.5}"#,
        r#"{"user":"ana","event":"logout"}"#,
    ];
    let last = table.append_batch(&events, Ack::Durable)?;
    println!("appended {} records, last LSN {last:?}", events.len());

    let sql = "SELECT event, count(*) AS n FROM events GROUP BY event ORDER BY event";
    let before = table.query(sql)?;
    print("before flush (WAL and pages)", &before);

    let files = table.flush()?;
    for f in &files {
        println!("flushed {} rows to {}", f.rows, f.path);
    }

    let after = table.query(sql)?;
    print("after flush (Parquet)", &after);
    assert_eq!(before.rows, after.rows, "a flush changes no answer");

    table.close()?;
    println!("table at {}", dir.display());
    Ok(())
}
