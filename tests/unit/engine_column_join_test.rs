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
                 CREATE TABLE pairs(id INTEGER, region TEXT, tag TEXT);\
                 INSERT INTO pairs VALUES (1, 'west', 'w1');\
                 INSERT INTO pairs VALUES (2, 'north', 'n2');\
                 INSERT INTO pairs VALUES (3, 'east', 'e3');\
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

/// #919: a filter on the Parquet table of a join written SQLite-first was
/// read as a lookup filter ("no such table: production").
#[test]
fn a_lookup_first_join_filters_and_orders_by_either_side() {
    let db = LookupDb::new("swapped-where");
    let mut e = db.engine();
    for (lookup_first, probe_first) in [
        (
            "SELECT production.id, labels.label FROM labels \
             JOIN production ON labels.id = production.id \
             WHERE production.id >= 2 ORDER BY labels.label DESC LIMIT 1",
            "SELECT production.id, labels.label FROM production \
             JOIN labels ON labels.id = production.id \
             WHERE production.id >= 2 ORDER BY labels.label DESC LIMIT 1",
        ),
        (
            "SELECT COUNT(*) FROM labels JOIN production USING (id) \
             WHERE production.id < 3 AND labels.label <> 'one'",
            "SELECT COUNT(*) FROM production JOIN labels USING (id) \
             WHERE production.id < 3 AND labels.label <> 'one'",
        ),
    ] {
        let got = rows(&mut e, lookup_first);
        assert_eq!(got, rows(&mut e, probe_first), "{lookup_first}");
        assert!(!got.is_empty(), "{lookup_first}");
        assert!(e.explain_plan(lookup_first).is_ok(), "{lookup_first}");
    }
    assert_eq!(
        rows(
            &mut e,
            "SELECT production.id FROM labels JOIN production ON labels.id = production.id \
             WHERE production.id >= 2 ORDER BY labels.label DESC LIMIT 1"
        ),
        vec![vec![Cell::Int(2)]]
    );
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
fn explain_plan_labels_both_sides_by_mode_and_file() {
    let db = LookupDb::new("explain-plan");
    let e = db.engine();
    let plan = e
        .explain_plan(
            "SELECT production.id FROM production JOIN labels ON production.id = labels.id",
        )
        .unwrap();
    let text: Vec<&str> = plan.iter().map(|r| r.detail.as_str()).collect();
    assert!(
        text.iter()
            .any(|d| d.contains("batch tests/fixtures/parquet/production.parquet")),
        "{text:?}"
    );
    assert!(text.iter().any(|d| d.contains("sqlite ")), "{text:?}");
    assert!(
        !text.iter().any(|d| d.contains("PRUNE")),
        "no lookup filter, nothing to prune: {text:?}"
    );
}

/// #788: `ROUND`/`ABS` work in a Parquet ⋈ SQLite join -- over a probe
/// column, over a lookup column's join, and wrapped round an aggregate.
#[test]
fn numeric_scalars_work_in_a_parquet_sqlite_join() {
    let db = LookupDb::new("round-join");
    let mut e = db.engine();
    let raw = rows(
        &mut e,
        "SELECT production.amount FROM production \
         JOIN labels ON production.id = labels.id ORDER BY production.id",
    );
    let half_away = |x: f64, places: i32| {
        let scale = 10f64.powi(places);
        (x * scale).round() / scale
    };
    let want: Vec<Vec<Cell>> = raw
        .iter()
        .map(|r| match &r[0] {
            Cell::Real(x) => vec![Cell::Real(half_away(*x, 1)), Cell::Real(x.abs())],
            other => panic!("amount is not a REAL: {other:?}"),
        })
        .collect();
    assert_eq!(
        rows(
            &mut e,
            "SELECT round(production.amount, 1), abs(production.amount) FROM production \
             JOIN labels ON production.id = labels.id ORDER BY production.id",
        ),
        want
    );

    // The money shape: a rounded aggregate over the join.
    let total: f64 = raw
        .iter()
        .map(|r| match &r[0] {
            Cell::Real(x) => *x,
            other => panic!("{other:?}"),
        })
        .sum();
    assert_eq!(
        rows(
            &mut e,
            "SELECT round(sum(production.amount) / 100.0, 2) FROM production \
             JOIN labels ON production.id = labels.id",
        ),
        vec![vec![Cell::Real(half_away(total / 100.0, 2))]]
    );
}

/// #789: the SQLite side's `SCAN` line reports the table's real size and
/// no row groups, with or without `ANALYZE` statistics, whatever the case
/// the query spells the table in.
#[test]
fn explain_plan_reports_the_lookup_tables_real_row_count() {
    let db = LookupDb::new("explain-rows");
    let scan = |sql: &str| -> String {
        db.engine()
            .explain_plan(sql)
            .unwrap()
            .into_iter()
            .map(|r| r.detail)
            .find(|d| d.contains("[sqlite "))
            .expect("a SQLite SCAN line")
    };
    let labels = "SELECT production.id FROM production JOIN labels ON production.id = labels.id";
    let line = scan(labels);
    assert!(line.starts_with("SCAN labels (~3 rows) [sqlite "), "{line}");
    assert!(!line.contains("row group"), "{line}");
    let tiers = "SELECT production.id FROM production JOIN TIERS ON production.id = TIERS.id";
    assert!(scan(tiers).contains("(~2 rows)"), "{}", scan(tiers));

    // After `ANALYZE` the row count comes from `sqlite_stat1`.
    RowEngine::open(&db.0)
        .unwrap()
        .run_query("ANALYZE")
        .unwrap();
    assert!(scan(labels).contains("(~3 rows)"), "{}", scan(labels));
}

#[test]
fn explain_plan_reports_the_row_group_pruning_a_lookup_filter_implies() {
    let db = LookupDb::new("explain-prune");
    let e = db.engine();
    let plan = e
        .explain_plan(
            "SELECT production.id FROM production JOIN labels ON production.id = labels.id \
             WHERE labels.label = 'nope'",
        )
        .unwrap();
    let prune: Vec<&str> = plan
        .iter()
        .map(|r| r.detail.as_str())
        .filter(|d| d.contains("PRUNE"))
        .collect();
    assert_eq!(prune.len(), 1, "{plan:?}");
    assert!(prune[0].contains("0 of "), "{prune:?}");

    let plan = e
        .explain_plan(
            "SELECT production.id FROM production JOIN labels ON production.id = labels.id \
             WHERE labels.label = 'two'",
        )
        .unwrap();
    assert!(
        plan.iter()
            .any(|r| r.detail.contains("PRUNE") && r.detail.contains("1 of ")),
        "{plan:?}"
    );
}

#[test]
fn explain_opcodes_show_the_sqlite_build_and_the_batch_probe_and_body() {
    let db = LookupDb::new("explain-opcodes");
    let e = db.engine();
    let sections = e
        .explain_opcodes(
            "SELECT production.id, labels.label FROM production \
             JOIN labels ON production.id = labels.id",
        )
        .unwrap();
    let lanes: Vec<(&str, &str)> = sections
        .iter()
        .map(|s| (s.lane, s.label.as_str()))
        .collect();
    assert_eq!(
        lanes,
        [
            ("row", "JOIN build (sqlite)"),
            ("batch", "JOIN probe (parquet production)"),
            ("batch", "JOIN body"),
        ]
    );
    assert!(sections.iter().all(|s| !s.rows.is_empty()));
}

/// #782: a star join's `EXPLAIN` lists every joined table and its hash
/// join (it used to be refused).
#[test]
fn explain_of_a_star_join_lists_every_joined_table() {
    let db = LookupDb::new("explain-star");
    let e = db.engine();
    let sql = "SELECT production.id FROM production \
               JOIN labels ON production.id = labels.id \
               JOIN tiers ON production.id = tiers.id";
    let plan: Vec<String> = e
        .explain_plan(sql)
        .unwrap()
        .into_iter()
        .map(|r| r.detail)
        .collect();
    for table in ["labels", "tiers"] {
        assert!(
            plan.iter()
                .any(|d| d.starts_with(&format!("SCAN {table} (")) && d.contains("[sqlite ")),
            "{table}: {plan:?}"
        );
        assert!(
            plan.iter()
                .any(|d| d.starts_with("HASH JOIN") && d.ends_with(&format!("{table}.id"))),
            "{table}: {plan:?}"
        );
    }
    let sections = e.explain_opcodes(sql).unwrap();
    let labels: Vec<&str> = sections.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "JOIN build (sqlite labels)",
            "JOIN build (sqlite tiers)",
            "JOIN probe (parquet production)",
            "JOIN body"
        ]
    );
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

#[test]
fn using_joins_like_the_equivalent_on_and_lists_the_key_once_under_star() {
    let db = LookupDb::new("using");
    let mut e = db.engine();
    let on = rows(
        &mut e,
        "SELECT production.id, labels.label FROM production \
         JOIN labels ON production.id = labels.id ORDER BY production.id",
    );
    let using = rows(
        &mut e,
        "SELECT production.id, labels.label FROM production \
         JOIN labels USING (id) ORDER BY production.id",
    );
    assert_eq!(using, on);

    // An unqualified key is the FROM table's, on a LEFT JOIN too.
    let left = rows(
        &mut e,
        "SELECT id, labels.label FROM production LEFT JOIN labels USING (id) \
         ORDER BY id LIMIT 4",
    );
    assert_eq!(left[3], vec![Cell::Int(4), Cell::Null]);

    let star = e
        .run_query("SELECT * FROM production JOIN labels USING (id) ORDER BY id")
        .unwrap();
    assert_eq!(star.columns, ["region", "amount", "id", "label"]);
    assert_eq!(star.rows[0].len(), 4);
    assert_eq!(star.rows.len(), 3);
}

#[test]
fn star_over_an_on_join_lists_both_key_columns() {
    let db = LookupDb::new("star-on");
    let mut e = db.engine();
    let star = e
        .run_query(
            "SELECT * FROM production JOIN labels ON production.id = labels.id \
             ORDER BY production.id",
        )
        .unwrap();
    // Bare headers, duplicates included, as in SQLite and DuckDB.
    assert_eq!(star.columns, ["region", "amount", "id", "id", "label"]);
    assert_eq!(star.rows[0][2], star.rows[0][3]);
}

#[test]
fn on_operands_in_either_order_and_using_with_the_lookup_first_join_the_same() {
    let db = LookupDb::new("operand-order");
    let mut e = db.engine();
    let want = rows(
        &mut e,
        "SELECT production.id, labels.label FROM production \
         JOIN labels ON production.id = labels.id ORDER BY production.id",
    );
    for sql in [
        "SELECT production.id, labels.label FROM production \
         JOIN labels ON labels.id = production.id ORDER BY production.id",
        "SELECT production.id, labels.label FROM labels \
         JOIN production ON production.id = labels.id ORDER BY production.id",
        "SELECT production.id, labels.label FROM labels \
         JOIN production USING (id) ORDER BY production.id",
    ] {
        assert_eq!(rows(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn using_with_several_columns_joins_on_all_of_them_for_an_inner_join_only() {
    let db = LookupDb::new("using-several");
    let mut e = db.engine();
    // `pairs` has (1, 'west'), (2, 'north'), (3, 'east'): only id 1 and 2
    // agree with production's region too.
    let got = rows(
        &mut e,
        "SELECT production.id, pairs.tag FROM production \
         JOIN pairs USING (id, region) ORDER BY production.id",
    );
    assert_eq!(
        got,
        vec![
            vec![Cell::Int(1), text("w1")],
            vec![Cell::Int(2), text("n2")],
        ]
    );
    let star = e
        .run_query("SELECT * FROM production JOIN pairs USING (id, region) ORDER BY id")
        .unwrap();
    assert_eq!(star.columns, ["region", "amount", "id", "tag"]);

    let err = e
        .run_query("SELECT production.id FROM production LEFT JOIN pairs USING (id, region)")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Parse, "{err}");
    assert!(
        err.message.contains("supported on an INNER JOIN only"),
        "{err}"
    );
}

#[test]
fn table_star_expands_one_tables_columns_by_name_or_alias() {
    let db = LookupDb::new("table-star");
    let mut e = db.engine();
    let labels = e
        .run_query("SELECT labels.* FROM production JOIN labels USING (id) ORDER BY labels.id")
        .unwrap();
    // `t.*` keeps the USING key, `*` would list it once.
    assert_eq!(labels.columns, ["id", "label"]);
    assert_eq!(labels.rows[0], vec![Cell::Int(1), text("one")]);

    let both = e
        .run_query(
            "SELECT p.*, l.label FROM production p JOIN labels l ON p.id = l.id \
             ORDER BY p.id LIMIT 1",
        )
        .unwrap();
    // An explicit `l.label` keeps the engine's qualified header.
    assert_eq!(both.columns, ["region", "amount", "id", "labels.label"]);
    assert_eq!(both.rows[0][3], text("one"));

    let err = e
        .run_query("SELECT nope.* FROM production JOIN labels USING (id)")
        .unwrap_err();
    assert!(err.message.contains("nope"), "{err}");
}

#[test]
fn expression_headers_stay_as_written() {
    let db = LookupDb::new("headers");
    let mut e = db.engine();
    let r = e
        .run_query(
            "SELECT sum(amount), count(label), max(production.id) FROM production \
             JOIN labels USING (id)",
        )
        .unwrap();
    assert_eq!(
        r.columns,
        ["SUM(amount)", "COUNT(label)", "MAX(production.id)"]
    );
}

#[test]
fn a_lookup_filter_with_using_still_keeps_the_matching_rows() {
    let db = LookupDb::new("using-filter");
    let mut e = db.engine();
    let got = rows(
        &mut e,
        "SELECT production.id FROM production JOIN labels USING (id) \
         WHERE labels.label = 'two'",
    );
    assert_eq!(got, vec![vec![Cell::Int(2)]]);
}

#[test]
fn an_unqualified_column_in_both_tables_is_ambiguous_unless_it_is_the_using_key() {
    let db = LookupDb::new("ambiguous");
    let mut e = db.engine();
    for sql in [
        "SELECT id FROM production JOIN labels ON production.id = labels.id",
        "SELECT production.id FROM production JOIN labels ON production.id = labels.id \
         WHERE id = 2",
        "SELECT region FROM production JOIN labels ON production.id = labels.id \
         ORDER BY id",
    ] {
        let err = e.run_query(sql).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Compile, "{sql}: {err}");
        assert!(err.message.contains("ambiguous column name: id"), "{err}");
        assert!(err.message.contains("production, labels"), "{err}");
    }
    // `ORDER BY id` names the output column `id`, not either table's.
    let got = rows(
        &mut e,
        "SELECT production.id FROM production JOIN labels ON production.id = labels.id \
         ORDER BY id DESC",
    );
    assert_eq!(got.first(), Some(&vec![Cell::Int(3)]));
    // `USING (id)` merges the two: the key is the table written first.
    let got = rows(
        &mut e,
        "SELECT id FROM production JOIN labels USING (id) WHERE id = 2",
    );
    assert_eq!(got, vec![vec![Cell::Int(2)]]);
}

#[test]
fn an_unqualified_column_of_one_table_resolves_to_it() {
    let db = LookupDb::new("owner");
    let mut e = db.engine();
    // `label` is only in the lookup table, `region` only in the Parquet one,
    // whichever table is written first, and the header stays as written.
    for sql in [
        "SELECT label, region FROM production JOIN labels ON production.id = labels.id \
         WHERE label = 'two'",
        "SELECT label, region FROM labels JOIN production ON labels.id = production.id \
         WHERE label = 'two'",
    ] {
        let r = e.run_query(sql).unwrap();
        assert_eq!(r.columns, ["label", "region"], "{sql}");
        assert_eq!(r.rows, vec![vec![text("two"), text("north")]], "{sql}");
    }
    let got = rows(
        &mut e,
        "SELECT label, count(*) FROM production JOIN labels USING (id) \
         GROUP BY label ORDER BY label",
    );
    assert_eq!(
        got,
        vec![
            vec![text("one"), Cell::Int(1)],
            vec![text("three"), Cell::Int(1)],
            vec![text("two"), Cell::Int(1)],
        ]
    );
    let err = e
        .run_query("SELECT nope FROM production JOIN labels USING (id)")
        .unwrap_err();
    assert!(err.message.contains("unknown column: nope"), "{err}");
}

/// A Parquet file whose `sid` and `other` columns are `UINT_64`, with
/// values on both sides of 2^63, and a SQLite `sessions` table keyed on
/// the same values stored as signed bit patterns (#600).
fn u64_key_files(label: &str) -> (PathBuf, LookupDb) {
    use db_core::storage::parquet::writer::{
        ColumnSpec, ColumnType, ColumnValues, ParquetWriter, WriterOptions,
    };
    let sids: [u64; 4] = [5, i64::MAX as u64, 1 << 63, u64::MAX];
    let schema = vec![
        ColumnSpec::new("sid", ColumnType::UInt64, false),
        ColumnSpec::new("other", ColumnType::UInt64, false),
        ColumnSpec::new("n", ColumnType::Int64, false),
    ];
    let mut w = ParquetWriter::new(Vec::new(), schema, WriterOptions::default()).unwrap();
    w.write_columns(vec![
        ColumnValues::UInt64(sids.iter().map(|s| Some(*s)).collect()),
        ColumnValues::UInt64(vec![Some(u64::MAX); 4]),
        ColumnValues::Int64((0..4).map(Some).collect()),
    ])
    .unwrap();
    let (bytes, _) = w.finish().unwrap();
    // The table is named after the file stem, so a unique directory.
    let mut parquet = std::env::temp_dir();
    parquet.push(format!("db-core-u64-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&parquet).unwrap();
    parquet.push("events.parquet");
    std::fs::write(&parquet, bytes).unwrap();

    let db = LookupDb::new(label);
    let mut lookup = RowEngine::open(&db.0).unwrap();
    for (sid, name) in sids.iter().zip(["five", "max_i64", "two63", "max_u64"]) {
        lookup
            .run_query(&format!(
                "INSERT INTO labels VALUES ({}, '{name}')",
                *sid as i64
            ))
            .unwrap();
    }
    (parquet, db)
}

#[test]
fn a_uint64_join_key_above_i64_max_matches_the_signed_row_key() {
    let (parquet, db) = u64_key_files("u64_join");
    let mut e = CrossModeEngine::open_column_sqlite(&parquet, &db.0).unwrap();
    let got = rows(
        &mut e,
        "SELECT labels.label FROM events JOIN labels ON events.sid = labels.id \
         ORDER BY events.n",
    )
    .into_iter()
    .map(|r| r[0].clone())
    .collect::<Vec<_>>();
    // ids 1..=3 of the seed also match no probe row; all four u64 keys do.
    assert_eq!(
        got,
        vec![
            text("five"),
            text("max_i64"),
            text("two63"),
            text("max_u64")
        ]
    );
    std::fs::remove_dir_all(parquet.parent().unwrap()).ok();
}

#[test]
fn a_uint64_column_outside_the_join_key_still_errors_above_i64_max() {
    let (parquet, db) = u64_key_files("u64_other");
    let mut e = CrossModeEngine::open_column_sqlite(&parquet, &db.0).unwrap();
    let err = e
        .run_query(
            "SELECT events.other FROM events \
             JOIN labels ON events.sid = labels.id",
        )
        .expect_err("a UINT_64 above i64::MAX outside a join key stays an error (#595)");
    assert!(err.to_string().contains("18446744073709551615"), "{err}");
    std::fs::remove_dir_all(parquet.parent().unwrap()).ok();
}

// MC/DC vectors for `sides::choose_probe`'s single-Parquet-table rewrite
// (obligation `engine_resolve_sides_choose_probe_580d7052`,
// `!only_inner || !is_star_around`, #919).

#[test]
#[allow(non_snake_case)]
fn mcdc__engine_resolve_sides_choose_probe_580d7052__v1_a_left_join_is_not_rewritten() {
    let db = LookupDb::new("mcdc-580d-v1");
    let mut e = db.engine();
    let got = rows(
        &mut e,
        "SELECT COUNT(*) FROM production LEFT JOIN labels ON production.id = labels.id \
         WHERE production.id < 5",
    );
    assert_eq!(got, vec![vec![Cell::Int(4)]]);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__engine_resolve_sides_choose_probe_580d7052__v2_an_inner_star_moves_the_probe_first() {
    let db = LookupDb::new("mcdc-580d-v2");
    let mut e = db.engine();
    let got = rows(
        &mut e,
        "SELECT COUNT(*) FROM labels JOIN production ON labels.id = production.id \
         WHERE production.id < 3",
    );
    assert_eq!(got, vec![vec![Cell::Int(2)]]);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__engine_resolve_sides_choose_probe_580d7052__v3_an_inner_chain_is_not_rewritten() {
    let db = LookupDb::new("mcdc-580d-v3");
    let mut e = db.engine();
    let sql = "SELECT COUNT(*) FROM labels JOIN production ON labels.id = production.id \
               JOIN tiers ON tiers.id = labels.id";
    // Not a star around `production`, so it stays where it is written and
    // the multi-way join keeps its probe-first refusal.
    let err = e.run_query(sql).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");
    assert!(err.message.contains("`production` written first"), "{err}");
}
