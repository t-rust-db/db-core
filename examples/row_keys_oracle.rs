// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Manual oracle aid for unsigned row keys (#787, criterion 3 of #600):
//! writes a table whose rowids lie on both sides of 2^63 -- through db-core's
//! writer, keys at or above 2^63 stored as their signed bit pattern -- and
//! prints what db-core itself reads back. `tools/check_row_keys_oracle.sh`
//! has the `sqlite3` binary read the same file and compares.
//!
//! Usage: `row_keys_oracle <out.db>` (the file is replaced)

use std::process::ExitCode;

use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine};

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

/// Rows in each cluster: enough that the table's b-tree has interior
/// pages, so keys on both sides of 2^63 meet in interior cells too.
const CLUSTER: u64 = 1500;

/// The u64 keys written, in the (scrambled) order they are inserted: the
/// top cluster descending, then the low one ascending, then the one
/// across 2^63.
fn keys() -> Vec<u64> {
    let top = (0..CLUSTER).map(|i| u64::MAX.saturating_sub(i));
    let low = 0..CLUSTER;
    let mid = (1u64 << 63) - CLUSTER / 2..(1u64 << 63) + CLUSTER / 2;
    top.chain(low).chain(mid).collect()
}

fn main() -> ExitCode {
    let Some(out) = std::env::args().nth(1) else {
        eprintln!("usage: row_keys_oracle <out.db>");
        return ExitCode::from(2);
    };
    if std::fs::copy(FIXTURE, &out).is_err() {
        eprintln!("cannot copy {FIXTURE} to {out}");
        return ExitCode::from(2);
    }
    let mut engine = match RowEngine::open(std::path::Path::new(&out)) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("open: {e}");
            return ExitCode::from(2);
        }
    };
    let mut sql = String::from("CREATE TABLE keys(id INTEGER PRIMARY KEY, label TEXT);BEGIN;");
    for key in keys() {
        // A key at or above 2^63 is its signed bit pattern.
        let id = key.cast_signed();
        sql.push_str(&format!("INSERT INTO keys VALUES ({id}, 'k{key}');"));
    }
    sql.push_str("COMMIT;");
    if let Err(e) = engine.run_query(&sql) {
        eprintln!("write: {e}");
        return ExitCode::from(2);
    }
    // What db-core reads back, in sqlite3's `.mode list` form.
    for query in [
        "SELECT count(*), min(id), max(id) FROM keys",
        "SELECT id, label FROM keys ORDER BY id LIMIT 3",
        "SELECT id, label FROM keys WHERE id >= -1 ORDER BY id LIMIT 3",
        "SELECT id, label FROM keys WHERE id < 0 ORDER BY id DESC LIMIT 3",
        "SELECT label FROM keys WHERE id = -9223372036854775808",
        "SELECT label FROM keys WHERE id = -1",
    ] {
        match engine.run_query(query) {
            Ok(result) => {
                println!("-- {query}");
                for row in &result.rows {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|c| match c {
                            Cell::Int(i) => i.to_string(),
                            other => other.to_string(),
                        })
                        .collect();
                    println!("{}", cells.join("|"));
                }
            }
            Err(e) => {
                eprintln!("{query}: {e}");
                return ExitCode::from(2);
            }
        }
    }
    ExitCode::SUCCESS
}
