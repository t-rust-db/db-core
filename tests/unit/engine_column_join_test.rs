// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for a Parquet table joined to SQLite lookup tables
//! (#598): `CrossModeEngine::open_column_sqlite`. The Parquet file is the
//! probe side and the SQLite tables always build, wherever the query
//! writes them. `JOIN` stays inner; a `LEFT JOIN` that would keep the
//! SQLite table's rows is rejected.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::path::{Path, PathBuf};

use db_core::engine::resolve::CrossModeEngine;
use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine, ErrorKind, Mode};

const PARQUET: &str = "tests/fixtures/parquet/production.parquet";
const ROW_FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

/// A writable copy of the SQLite fixture, removed on drop, seeded with
/// `labels(id, label)` for ids 1..=3 and `tiers(id, tier)` for ids 1..=2
/// -- `production` has ids 1..=5000, so most probe rows have no match.
struct LookupDb(PathBuf);

impl LookupDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-column-join-{label}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::copy(ROW_FIXTURE, &path).expect("copy fixture");
        let mut engine = RowEngine::open(&path).expect("open fixture copy");
        engine
            .run_query(
                "CREATE TABLE labels(id INTEGER PRIMARY KEY, label TEXT);\
                 INSERT INTO labels VALUES (1, 'one');\
                 INSERT INTO labels VALUES (2, 'two');\
                 INSERT INTO labels VALUES (3, 'three');\
                 CREATE TABLE tiers(id INTEGER PRIMARY KEY, tier TEXT);\
                 INSERT INTO tiers VALUES (1, 'gold');\
                 INSERT INTO tiers VALUES (2, 'silver');",
            )
            .expect("seed lookup tables");
        LookupDb(path)
    }

    fn engine(&self) -> CrossModeEngine {
        CrossModeEngine::open_column_sqlite(Path::new(PARQUET), &self.0).expect("open")
    }
}

impl Drop for LookupDb {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

fn rows(e: &mut CrossModeEngine, sql: &str) -> Vec<Vec<Cell>> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
}

fn text(s: &str) -> Cell {
    Cell::Text(s.into())
}

#[test]
fn open_reports_cross_mode_and_lists_both_sides_tables() {
    let db = LookupDb::new("tables");
    let e = db.engine();
    assert_eq!(e.mode(), Mode::Cross);
    let names: Vec<String> = e.tables().unwrap().into_iter().map(|t| t.name).collect();
    assert!(names.contains(&"production".to_string()), "{names:?}");
    assert!(names.contains(&"labels".to_string()), "{names:?}");
}

#[test]
fn plain_join_is_inner_and_keeps_only_matching_probe_rows() {
    let db = LookupDb::new("inner");
    let mut e = db.engine();
    let got = rows(
        &mut e,
        "SELECT production.id, labels.label FROM production \
         JOIN labels ON production.id = labels.id ORDER BY production.id",
    );
    assert_eq!(
        got,
        vec![
            vec![Cell::Int(1), text("one")],
            vec![Cell::Int(2), text("two")],
            vec![Cell::Int(3), text("three")],
        ]
    );
}

#[test]
fn left_join_keeps_every_probe_row_with_null_for_the_unmatched() {
    let db = LookupDb::new("left");
    let mut e = db.engine();
    let got = rows(
        &mut e,
        "SELECT production.id, labels.label FROM production \
         LEFT JOIN labels ON production.id = labels.id ORDER BY production.id LIMIT 5",
    );
    assert_eq!(
        got,
        vec![
            vec![Cell::Int(1), text("one")],
            vec![Cell::Int(2), text("two")],
            vec![Cell::Int(3), text("three")],
            vec![Cell::Int(4), Cell::Null],
            vec![Cell::Int(5), Cell::Null],
        ]
    );
    let count = rows(
        &mut e,
        "SELECT COUNT(*) FROM production LEFT JOIN labels ON production.id = labels.id",
    );
    assert_eq!(count, vec![vec![Cell::Int(5000)]]);
}

#[test]
fn inner_join_written_with_the_lookup_table_first_gives_the_same_rows() {
    let db = LookupDb::new("swapped");
    let mut e = db.engine();
    let got = rows(
        &mut e,
        "SELECT production.id, labels.label FROM labels \
         JOIN production ON labels.id = production.id ORDER BY production.id",
    );
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got[0], vec![Cell::Int(1), text("one")]);
}

#[test]
fn left_join_that_would_keep_the_lookup_rows_is_rejected_with_a_clear_message() {
    let db = LookupDb::new("wrong-left");
    let mut e = db.engine();
    let err = e
        .run_query(
            "SELECT labels.label, production.id FROM labels \
             LEFT JOIN production ON labels.id = production.id",
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");
    assert!(
        err.message.contains("production LEFT JOIN"),
        "the message should say how to write it: {err}"
    );
}

#[test]
fn star_join_probes_several_lookup_tables() {
    let db = LookupDb::new("star");
    let mut e = db.engine();
    let got = rows(
        &mut e,
        "SELECT production.id, labels.label, tiers.tier FROM production \
         JOIN labels ON production.id = labels.id \
         JOIN tiers ON production.id = tiers.id ORDER BY production.id",
    );
    assert_eq!(
        got,
        vec![
            vec![Cell::Int(1), text("one"), text("gold")],
            vec![Cell::Int(2), text("two"), text("silver")],
        ]
    );
}

#[test]
fn aggregates_over_the_join_group_by_the_lookup_column() {
    let db = LookupDb::new("group");
    let mut e = db.engine();
    let got = rows(
        &mut e,
        "SELECT labels.label, COUNT(*) FROM production \
         JOIN labels ON production.id = labels.id GROUP BY labels.label ORDER BY labels.label",
    );
    assert_eq!(
        got,
        vec![
            vec![text("one"), Cell::Int(1)],
            vec![text("three"), Cell::Int(1)],
            vec![text("two"), Cell::Int(1)],
        ]
    );
}

#[test]
fn a_query_over_neither_table_is_a_compile_error() {
    let db = LookupDb::new("neither");
    let mut e = db.engine();
    let err = e
        .run_query("SELECT * FROM labels JOIN tiers ON labels.id = tiers.id")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile, "{err}");
}

#[test]
fn explain_is_unsupported_for_now() {
    let db = LookupDb::new("explain");
    let e = db.engine();
    let err = e
        .explain_plan("SELECT * FROM production JOIN labels ON production.id = labels.id")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");
}

#[test]
fn a_filter_on_the_lookup_table_keeps_only_the_matching_probe_rows() {
    let db = LookupDb::new("filter");
    let mut e = db.engine();
    let got = rows(
        &mut e,
        "SELECT production.id, labels.label FROM production \
         JOIN labels ON production.id = labels.id WHERE labels.label = 'two'",
    );
    assert_eq!(got, vec![vec![Cell::Int(2), text("two")]]);

    // OR within one lookup table, and a filter on each side of a star join.
    let got = rows(
        &mut e,
        "SELECT production.id FROM production \
         JOIN labels ON production.id = labels.id \
         JOIN tiers ON production.id = tiers.id \
         WHERE (labels.label = 'one' OR labels.label = 'three') AND tiers.tier = 'gold'",
    );
    assert_eq!(got, vec![vec![Cell::Int(1)]]);
}

#[test]
fn a_lookup_filter_matching_no_row_gives_an_empty_join_even_for_aggregates() {
    let db = LookupDb::new("filter-none");
    let mut e = db.engine();
    let got = rows(
        &mut e,
        "SELECT COUNT(*) FROM production \
         JOIN labels ON production.id = labels.id WHERE labels.label = 'nope'",
    );
    assert_eq!(got, vec![vec![Cell::Int(0)]]);
}

#[test]
fn a_left_join_filter_on_the_lookup_table_still_sees_the_unmatched_rows() {
    let db = LookupDb::new("left-filter");
    let mut e = db.engine();
    let got = rows(
        &mut e,
        "SELECT COUNT(*) FROM production \
         LEFT JOIN labels ON production.id = labels.id WHERE labels.label IS NULL",
    );
    assert_eq!(got, vec![vec![Cell::Int(4997)]]);
    let got = rows(
        &mut e,
        "SELECT production.id FROM production \
         LEFT JOIN labels ON production.id = labels.id WHERE labels.label = 'two'",
    );
    assert_eq!(got, vec![vec![Cell::Int(2)]]);
}
