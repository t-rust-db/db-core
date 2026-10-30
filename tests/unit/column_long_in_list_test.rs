// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! A long literal `IN` list runs in the column engine (#901): the parser used
//! to lower it to an `OR` chain as deep as the list is long, and the passes
//! over that tree overflowed a 2 MiB stack at about 1,000 items, which aborts
//! the host process. Every query here runs on a **2 MiB thread** (what a
//! tokio worker has) against integer, text and blob columns.

#![allow(clippy::unwrap_used, clippy::panic)]

use db_core::engine::column::BatchEngine;
use db_core::engine::{Cell, Engine};
use db_core::storage::parquet::writer::{
    ColumnSpec, ColumnType, ColumnValues, ParquetWriter, WriterOptions,
};

const ROWS: i64 = 6_000;

/// A SQL literal for the value `i`.
type Literal = fn(i64) -> String;

fn engine() -> BatchEngine {
    let schema = vec![
        ColumnSpec::new("id", ColumnType::Int64, false),
        ColumnSpec::new("name", ColumnType::Utf8, false),
        ColumnSpec::new("ref", ColumnType::Binary, false),
    ];
    let mut w = ParquetWriter::new(Vec::new(), schema, WriterOptions::default()).unwrap();
    w.write_columns(vec![
        ColumnValues::Int64((0..ROWS).map(Some).collect()),
        ColumnValues::Utf8((0..ROWS).map(|i| Some(format!("v{i}"))).collect()),
        ColumnValues::Binary(
            (0..ROWS)
                .map(|i| Some(u32::try_from(i).unwrap().to_be_bytes().to_vec()))
                .collect(),
        ),
    ])
    .unwrap();
    BatchEngine::from_bytes("t", w.finish().unwrap().0).unwrap()
}

/// `IN`/`NOT IN` over `column` with the literals of ids `0, 2, 4, ...`
/// (`n` of them): the even ids below `ROWS` match.
fn count(engine: &mut BatchEngine, column: &str, negated: bool, n: i64, lit: Literal) -> i64 {
    let items: Vec<String> = (0..n).map(|i| lit(i.saturating_mul(2))).collect();
    let not = if negated { "NOT " } else { "" };
    let sql = format!(
        "SELECT count(*) FROM t WHERE {column} {not}IN ({})",
        items.join(", ")
    );
    match engine.run_query(&sql).unwrap().rows.as_slice() {
        [row] => match row.as_slice() {
            [Cell::Int(c)] => *c,
            other => panic!("{column}: expected one count, got {other:?}"),
        },
        other => panic!("{column}: expected one row, got {other:?}"),
    }
}

fn int(i: i64) -> String {
    i.to_string()
}
fn text(i: i64) -> String {
    format!("'v{i}'")
}
fn blob(i: i64) -> String {
    format!("x'{i:08x}'")
}

#[test]
fn a_long_in_list_runs_on_a_2mib_stack_for_integer_text_and_blob_columns() {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            let mut engine = engine();
            let cases: [(&str, Literal); 3] = [("id", int), ("name", text), ("ref", blob)];
            for (column, lit) in cases {
                // Short lists keep their answer: the first 5 literals are
                // 0, 2, 4, 6, 8.
                assert_eq!(count(&mut engine, column, false, 5, lit), 5, "{column}");
                assert_eq!(
                    count(&mut engine, column, true, 5, lit),
                    ROWS - 5,
                    "{column}"
                );
                // 1,000 and 10,000 literals: of the even ids 0, 2, ..., only the 3,000
                // below `ROWS` exist; the rest match nothing.
                for (n, matching) in [(1_000, 1_000), (10_000, ROWS / 2)] {
                    assert_eq!(
                        count(&mut engine, column, false, n, lit),
                        matching,
                        "{column} {n}"
                    );
                    assert_eq!(
                        count(&mut engine, column, true, n, lit),
                        ROWS - matching,
                        "{column} NOT {n}"
                    );
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
