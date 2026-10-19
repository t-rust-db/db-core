// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The row engine's scalar functions against sqlite3 3.53.4's answers
//! (#651), without sqlite3: each `tools/row_functions_cases*.sql` runs as
//! `examples/row_oracle` runs it, and must print what
//! `tools/check_row_functions_oracle.py` wrote to the matching
//! `.expected` file when every line agreed with sqlite3. Reals compare
//! within a few ulps: `sin`, `exp`, `pow`, ... come from the platform's
//! libm, which may round the last bit differently than the Mac the
//! expectations were taken on.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    reason = "test code fails fast (db-core#230)"
)]

use std::path::PathBuf;

use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine};

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

/// `examples/row_oracle`'s `.mode quote` rendering of a cell.
fn quote(c: &Cell) -> String {
    match c {
        Cell::Null => "NULL".to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::Real(x) => format!("{x:?}"),
        Cell::Bool(b) => i64::from(*b).to_string(),
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

/// The case file's statements, one line each, run in order against one
/// scratch copy of the fixture: `row_oracle`'s output lines.
fn run(cases: &str) -> Vec<String> {
    let db: PathBuf = std::env::temp_dir().join(format!(
        "db-core-row-functions-{}-{}.db",
        std::process::id(),
        cases.replace('/', "_")
    ));
    std::fs::copy(FIXTURE, &db).unwrap();
    let mut engine = RowEngine::open(&db).unwrap();
    let out = std::fs::read_to_string(cases)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("--"))
        .map(|sql| match engine.run_query(sql) {
            Ok(result) => result
                .rows
                .iter()
                .map(|r| r.iter().map(quote).collect::<Vec<_>>().join(","))
                .collect::<Vec<_>>()
                .join(" | "),
            Err(_) => "ERROR".to_string(),
        })
        .collect();
    drop(engine);
    std::fs::remove_file(&db).ok();
    out
}

/// A `.mode quote` row split on the commas outside quotes.
fn cells(row: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, ch) in row.char_indices() {
        match ch {
            '\'' => quoted = !quoted,
            ',' if !quoted => {
                out.push(&row[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&row[start..]);
    out
}

/// A cell `row_oracle` printed as a REAL.
fn real(cell: &str) -> Option<f64> {
    if cell.starts_with('\'') || cell.starts_with("X'") || !cell.contains(['.', 'e', 'E', 'i', 'N'])
    {
        return None;
    }
    cell.parse().ok()
}

/// `got` and `want` are the same output, reals within 4 ulps.
fn same(got: &str, want: &str) -> bool {
    if got == want {
        return true;
    }
    let (got_rows, want_rows): (Vec<_>, Vec<_>) =
        (got.split(" | ").collect(), want.split(" | ").collect());
    got_rows.len() == want_rows.len()
        && got_rows.iter().zip(&want_rows).all(|(g, w)| {
            let (g, w) = (cells(g), cells(w));
            g.len() == w.len()
                && g.iter().zip(&w).all(|(g, w)| {
                    g == w
                        || matches!((real(g), real(w)), (Some(a), Some(b))
                            if a.is_finite() && b.is_finite()
                                && (a.to_bits() as i64).abs_diff(b.to_bits() as i64) <= 4)
                })
        })
}

fn check(cases: &str) {
    let expected_path = cases.replace(".sql", ".expected");
    let expected = std::fs::read_to_string(&expected_path).unwrap();
    let expected: Vec<&str> = expected.lines().collect();
    let sql: Vec<String> = std::fs::read_to_string(cases)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("--"))
        .map(str::to_string)
        .collect();
    let got = run(cases);
    assert_eq!(
        got.len(),
        expected.len(),
        "{expected_path} is stale: re-run make check-row-functions-oracle"
    );
    let mut bad = Vec::new();
    for ((sql, got), want) in sql.iter().zip(&got).zip(&expected) {
        let ok = match *want {
            // A nondeterministic line only has to error alike.
            "OK" | "ERROR" if sql.trim_end().ends_with("-- nondeterministic") => {
                (got == "ERROR") == (*want == "ERROR")
            }
            _ => same(got, want),
        };
        if !ok {
            bad.push(format!("{sql}\n  got:  {got}\n  want: {want}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} differ from sqlite3:\n{}",
        bad.len(),
        got.len(),
        bad.join("\n")
    );
}

#[test]
fn core_text_math_and_connection_state_functions() {
    check("tools/row_functions_cases.sql");
}

#[test]
fn date_and_time_functions() {
    check("tools/row_functions_cases_datetime.sql");
}

#[test]
fn json_functions() {
    check("tools/row_functions_cases_json.sql");
}
