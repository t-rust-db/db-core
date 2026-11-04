// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The column engine's silent wrong answers (#679), as SQL: each query runs
//! through `BatchEngine::run_query` and must give DuckDB 1.5.5's answer on
//! the same file -- the column engine's reference (ADR 0000 (f)). Every
//! expectation here was produced by `duckdb` on these fixtures, not typed
//! by hand; where #679 chose to refuse a shape instead, the test asserts
//! the refusal.
//!
//! - `tests/fixtures/parquet/null_types.parquet` (table `null_types`):
//!   NULLs in every typed column, mixed-case and empty text, a decimal and a
//!   timestamp column (`tools/gen_parquet_fixtures.sh`).
//! - `tests/fixtures/parquet/production.parquet` (table `production`):
//!   ids 1..=5000, four regions, amounts.
//! - `tests/fixtures/parquet/int96_events.parquet` (table `int96_events`):
//!   a legacy INT96 timestamp column, dictionary-encoded by pyarrow, with a
//!   NULL and a pre-1970 instant (`tools/gen_parquet_fixtures.sh`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use db_core::engine::column::BatchEngine;
use db_core::engine::{Cell, Engine, ErrorKind};

const NULL_TYPES: &str = "tests/fixtures/parquet/null_types.parquet";
const PRODUCTION: &str = "tests/fixtures/parquet/production.parquet";
const INT96_EVENTS: &str = "tests/fixtures/parquet/int96_events.parquet";

fn query(file: &str, sql: &str) -> Result<Vec<Vec<Cell>>, db_core::engine::EngineError> {
    let mut engine = BatchEngine::open(Path::new(file)).expect("open fixture");
    engine.run_query(sql).map(|r| r.rows)
}

fn rows(file: &str, sql: &str) -> Vec<Vec<Cell>> {
    query(file, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// The first column of every row, as integers.
fn ids(file: &str, sql: &str) -> Vec<i64> {
    rows(file, sql)
        .into_iter()
        .map(|r| match r.first() {
            Some(Cell::Int(i)) => *i,
            other => panic!("{sql}: expected an integer, got {other:?}"),
        })
        .collect()
}

fn count(file: &str, sql: &str) -> i64 {
    match ids(file, sql).as_slice() {
        [n] => *n,
        other => panic!("{sql}: expected one count, got {other:?}"),
    }
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

/// `sql` is refused with an error of `kind` (or any kind when `None`).
fn refused(file: &str, sql: &str, kind: Option<ErrorKind>) {
    match query(file, sql) {
        Ok(rows) => panic!("{sql}: expected an error, got {rows:?}"),
        Err(e) => {
            if let Some(kind) = kind {
                assert_eq!(e.kind, kind, "{sql}: {e}");
            }
        }
    }
}

/// Step 1: AND/OR/NOT follow SQL's three-valued logic -- `TRUE OR NULL`
/// is TRUE, `FALSE AND NULL` is FALSE, `NOT NULL` is NULL.
#[test]
fn and_or_not_follow_three_valued_logic() {
    // Row 4: n = 3, d NULL -- `n > 0 OR d > 100` is TRUE OR NULL.
    assert_eq!(
        ids(
            NULL_TYPES,
            "SELECT id FROM null_types WHERE n > 0 OR d > 100 ORDER BY id"
        ),
        [1, 4]
    );
    // Row 4: `n > 5 AND d > 0` is FALSE AND NULL = FALSE, so NOT is TRUE.
    assert_eq!(
        ids(
            NULL_TYPES,
            "SELECT id FROM null_types WHERE NOT (n > 5 AND d > 0) ORDER BY id"
        ),
        [3, 4, 5]
    );
    assert_eq!(
        ids(
            NULL_TYPES,
            "SELECT id FROM null_types WHERE n < 100 AND (d > 100 OR n > 0) ORDER BY id"
        ),
        [1, 4]
    );
    assert_eq!(
        ids(
            NULL_TYPES,
            "SELECT id FROM null_types WHERE NOT (n > 5 OR d > 100) ORDER BY id"
        ),
        [3, 5]
    );
    assert_eq!(
        rows(
            NULL_TYPES,
            "SELECT s, count(*) FROM null_types GROUP BY s \
             HAVING count(*) > 5 OR max(d) > 1 ORDER BY s"
        ),
        [[text("a"), Cell::Int(1)], [text("b"), Cell::Int(1)]]
    );
}

/// Step 2: OFFSET skips rows -- on the top-N path, after a multi-key sort,
/// after grouping, and past the end.
#[test]
fn offset_skips_rows() {
    assert_eq!(
        ids(
            PRODUCTION,
            "SELECT id FROM production ORDER BY id LIMIT 2 OFFSET 3"
        ),
        [4, 5]
    );
    assert_eq!(
        ids(
            PRODUCTION,
            "SELECT id FROM production ORDER BY id DESC LIMIT 3 OFFSET 4998"
        ),
        [2, 1]
    );
    assert_eq!(
        rows(
            PRODUCTION,
            "SELECT region, id FROM production ORDER BY region, id LIMIT 2 OFFSET 1250"
        ),
        [[text("north"), Cell::Int(2)], [text("north"), Cell::Int(6)]]
    );
    assert_eq!(
        rows(
            PRODUCTION,
            "SELECT region, count(*) FROM production GROUP BY region ORDER BY region \
             LIMIT 2 OFFSET 1"
        ),
        [
            [text("north"), Cell::Int(1250)],
            [text("south"), Cell::Int(1250)]
        ]
    );
    assert!(ids(
        PRODUCTION,
        "SELECT id FROM production ORDER BY id LIMIT 5 OFFSET 6000"
    )
    .is_empty());
}

/// Step 3: `IN (SELECT ...)` and `EXISTS` inside a larger condition are
/// refused -- they used to compile to FALSE.
#[test]
fn a_nested_in_subquery_or_exists_is_refused() {
    for sql in [
        "SELECT count(*) FROM production WHERE id > 0 AND id IN (SELECT id FROM production WHERE id < 5)",
        "SELECT count(*) FROM production WHERE EXISTS (SELECT id FROM production WHERE id < 5)",
        "SELECT count(*) FROM production WHERE id < 3 OR id IN (SELECT id FROM production WHERE id > 4998)",
        "SELECT count(*) FROM production WHERE NOT EXISTS (SELECT id FROM production)",
    ] {
        refused(PRODUCTION, sql, Some(ErrorKind::Unsupported));
    }
}

/// Step 4: a quoted value compared with a number column is converted to
/// the column's type, as DuckDB does; one that does not convert is an
/// error.
#[test]
fn a_quoted_value_compares_as_the_column_type() {
    for (sql, want) in [
        ("SELECT count(*) FROM production WHERE id = '5'", 1),
        ("SELECT count(*) FROM production WHERE id <> '5'", 4999),
        ("SELECT count(*) FROM production WHERE id < '5'", 4),
        ("SELECT count(*) FROM production WHERE '5' = id", 1),
        ("SELECT count(*) FROM production WHERE amount > '100'", 4960),
        ("SELECT count(*) FROM production WHERE amount = '12.5'", 1),
    ] {
        assert_eq!(count(PRODUCTION, sql), want, "{sql}");
    }
    assert_eq!(
        ids(
            NULL_TYPES,
            "SELECT id FROM null_types WHERE d >= '1.5' ORDER BY id"
        ),
        [1, 3]
    );
    refused(
        PRODUCTION,
        "SELECT count(*) FROM production WHERE region <> 5",
        None,
    );
    refused(
        PRODUCTION,
        "SELECT count(*) FROM production WHERE id = 'abc'",
        None,
    );
}

/// Step 5: a TIMESTAMP column compared with a quoted date or timestamp
/// compares as timestamps; a quoted value that is no timestamp is an
/// error.
#[test]
fn a_timestamp_column_compares_with_a_quoted_timestamp() {
    assert_eq!(
        count(
            NULL_TYPES,
            "SELECT count(*) FROM null_types WHERE ts >= '2026-01-02'"
        ),
        3
    );
    assert_eq!(
        ids(
            NULL_TYPES,
            "SELECT id FROM null_types WHERE ts < '2026-01-02 12:00:00' ORDER BY id"
        ),
        [1, 2]
    );
    assert_eq!(
        ids(
            NULL_TYPES,
            "SELECT id FROM null_types WHERE ts = '2026-01-03 00:00:00' ORDER BY id"
        ),
        [5]
    );
    refused(
        NULL_TYPES,
        "SELECT count(*) FROM null_types WHERE ts > 'not a time'",
        None,
    );
}

/// A legacy INT96 timestamp column reads as epoch microseconds, like a
/// TIMESTAMP_MICROS column: whole (eager load) and at the rows a filter
/// keeps (the two-phase load's positional decode), compared with a quoted
/// timestamp, under `date_trunc` and in aggregates. It used to fail with
/// "unexpected end of page data" (the string reader). Expectations are
/// DuckDB's `epoch_us` of the same values.
#[test]
fn a_legacy_int96_timestamp_column_reads_as_a_timestamp() {
    let ts = |us: i64| Cell::Int(us);
    assert_eq!(
        rows(INT96_EVENTS, "SELECT ts FROM int96_events ORDER BY id"),
        [
            vec![ts(1_767_609_000_000_000)],
            vec![ts(1_767_657_599_999_999)],
            vec![Cell::Null],
            vec![ts(-43_200_000_000)],
            vec![ts(1_767_609_000_000_000)],
            vec![ts(1_769_904_000_000_001)],
        ]
    );
    assert_eq!(
        rows(INT96_EVENTS, "SELECT ts FROM int96_events WHERE id >= 3"),
        [
            vec![ts(-43_200_000_000)],
            vec![ts(1_767_609_000_000_000)],
            vec![ts(1_769_904_000_000_001)],
        ]
    );
    assert_eq!(
        ids(
            INT96_EVENTS,
            "SELECT id FROM int96_events WHERE ts >= '2026-01-05' ORDER BY id"
        ),
        [0, 1, 4, 5]
    );
    assert_eq!(
        rows(
            INT96_EVENTS,
            "SELECT date_trunc('day', ts) AS d, count(*) FROM int96_events GROUP BY 1 ORDER BY 1"
        ),
        [
            vec![ts(-86_400_000_000), Cell::Int(1)],
            vec![ts(1_767_571_200_000_000), Cell::Int(3)],
            vec![ts(1_769_904_000_000_000), Cell::Int(1)],
            vec![Cell::Null, Cell::Int(1)],
        ]
    );
    assert_eq!(
        rows(
            INT96_EVENTS,
            "SELECT min(ts), max(ts), count(ts) FROM int96_events"
        ),
        [vec![
            ts(-43_200_000_000),
            ts(1_769_904_000_000_001),
            Cell::Int(5)
        ]]
    );
}

/// The public hook for a caller that parses and plans a query itself
/// (column-rs): `resolve_timestamps` with a file's
/// `parquet_timestamp_columns` turns a quoted timestamp into the column's
/// integer units and `date_trunc` into the dialect's `__date_trunc`, as
/// this engine does to its own parsed queries.
#[test]
fn resolve_timestamps_rewrites_a_query_parsed_outside_the_engine() {
    use db_core::engine::column::{parquet_timestamp_columns, resolve_timestamps};
    use db_core::storage::parquet::ParquetFile;

    let bytes = std::fs::read(INT96_EVENTS).unwrap();
    let file = ParquetFile::open(&bytes).unwrap();
    let timestamps = parquet_timestamp_columns(&file);
    assert_eq!(timestamps, [("ts".to_string(), 1_000_000)]);

    let mut select = db_core::parser::parse(
        "SELECT date_trunc('day', ts) FROM int96_events WHERE ts >= '2026-01-05'",
    )
    .unwrap();
    resolve_timestamps(&mut select, &timestamps).unwrap();
    let sql = select.to_string();
    assert!(sql.contains("__date_trunc('day', ts, 1000000)"), "{sql}");
    assert!(sql.contains("ts >= 1767571200000000"), "{sql}");

    // A query without TIMESTAMP columns is left alone; a bad literal or
    // `date_trunc` over a column that is no TIMESTAMP is an error.
    let mut plain = db_core::parser::parse("SELECT id FROM t WHERE id > 1").unwrap();
    let before = plain.to_string();
    resolve_timestamps(&mut plain, &timestamps).unwrap();
    assert_eq!(plain.to_string(), before);
    let mut bad = db_core::parser::parse("SELECT id FROM t WHERE ts > 'soon'").unwrap();
    assert!(resolve_timestamps(&mut bad, &timestamps).is_err());
    let mut not_ts = db_core::parser::parse("SELECT date_trunc('day', id) FROM t").unwrap();
    assert!(resolve_timestamps(&mut not_ts, &timestamps).is_err());
}

/// Step 6: LIKE is case-sensitive, as in DuckDB; ILIKE is the
/// case-insensitive form. GLOB was already case-sensitive.
#[test]
fn like_is_case_sensitive_and_ilike_is_not() {
    assert_eq!(
        count(
            PRODUCTION,
            "SELECT count(*) FROM production WHERE region LIKE 'NORTH'"
        ),
        0
    );
    assert_eq!(
        count(
            PRODUCTION,
            "SELECT count(*) FROM production WHERE region LIKE 'north'"
        ),
        1250
    );
    assert_eq!(
        ids(
            NULL_TYPES,
            "SELECT id FROM null_types WHERE lvl LIKE 'err%' ORDER BY id"
        ),
        [2]
    );
    assert_eq!(
        ids(
            NULL_TYPES,
            "SELECT id FROM null_types WHERE lvl ILIKE 'err%' ORDER BY id"
        ),
        [1, 2]
    );
    assert_eq!(
        ids(
            NULL_TYPES,
            "SELECT id FROM null_types WHERE lvl NOT ILIKE 'err%' ORDER BY id"
        ),
        [3, 4]
    );
    assert_eq!(
        ids(
            NULL_TYPES,
            "SELECT id FROM null_types WHERE s GLOB 'B' ORDER BY id"
        ),
        [4]
    );
}

/// Step 7: `regexp_extract` gives '' when the pattern does not match (NULL
/// only for a NULL input), as DuckDB does.
#[test]
fn regexp_extract_without_a_match_is_empty() {
    assert_eq!(
        count(
            PRODUCTION,
            "SELECT count(*) FROM production WHERE regexp_extract(region, '(n)', 1) = ''"
        ),
        3750
    );
    assert_eq!(
        rows(
            NULL_TYPES,
            "SELECT id, regexp_extract(s, '(b)', 1) FROM null_types ORDER BY id"
        ),
        [
            [Cell::Int(1), text("")],
            [Cell::Int(2), Cell::Null],
            [Cell::Int(3), text("b")],
            [Cell::Int(4), text("")],
            [Cell::Int(5), text("")],
        ]
    );
}
