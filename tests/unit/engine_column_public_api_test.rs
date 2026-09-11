// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for `db_core::engine::column::BatchEngine` (#325, #326,
//! #327): one Parquet file as one table through the `Engine` seam, including
//! as `Box<dyn Engine>`. The fixture is column-rs's `production.parquet`
//! (5000 rows; columns `region` BYTE_ARRAY, `amount` DOUBLE, `id` INT64;
//! first rows `west 2.5 1`, `north 5 2`, `south 7.5 3`), read in place --
//! the engine never writes.
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
    clippy::float_cmp,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::path::Path;

use db_core::engine::column::BatchEngine;
use db_core::engine::{Cell, Engine, ErrorKind, FileStats, Mode, PlanRow};

const FIXTURE: &str = "tests/corpus/fixtures/parquet/production.parquet";

fn open() -> BatchEngine {
    BatchEngine::open(Path::new(FIXTURE)).expect("open fixture")
}

fn rows(e: &mut BatchEngine, sql: &str) -> Vec<Vec<Cell>> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
}

#[test]
fn open_reports_batch_mode_footer_stats_and_the_stem_named_table() {
    let e = open();
    assert_eq!(e.mode(), Mode::Batch);
    assert_eq!(e.table_name(), "production");
    assert_eq!(e.path(), Path::new(FIXTURE));
    match e.stats() {
        FileStats::Batch { row_groups, rows } => {
            assert!(row_groups >= 1, "{row_groups}");
            assert_eq!(rows, 5000);
        }
        other => panic!("batch engine reported {other:?}"),
    }
}

#[test]
fn tables_lists_the_one_table_with_its_real_columns_and_physical_types() {
    let e = open();
    let tables = e.tables().unwrap();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name, "production");
    let cols: Vec<(&str, &str)> = tables[0]
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.type_name.as_str()))
        .collect();
    assert_eq!(
        cols,
        [
            ("region", "BYTE_ARRAY"),
            ("amount", "DOUBLE"),
            ("id", "INT64")
        ]
    );
}

#[test]
fn open_missing_or_non_parquet_file_is_an_open_error() {
    let err = BatchEngine::open(Path::new("/nonexistent/dir/no-such.parquet")).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Open, "{err}");
    assert!(err.message.contains("no-such.parquet"), "{err}");

    // A real file that is not Parquet (a SQLite fixture).
    let err = BatchEngine::open(Path::new(
        "tests/corpus/fixtures/btrees/table_single_page.db",
    ))
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Open, "{err}");
}

#[test]
fn run_query_returns_real_rows_as_cells_with_output_column_names() {
    let mut e = open();
    let r = e
        .run_query("SELECT region, amount, id FROM production ORDER BY id LIMIT 3")
        .unwrap();
    assert_eq!(r.columns, ["region", "amount", "id"]);
    assert_eq!(
        r.rows,
        vec![
            vec![Cell::Text("west".into()), Cell::Real(2.5), Cell::Int(1)],
            vec![Cell::Text("north".into()), Cell::Real(5.0), Cell::Int(2)],
            vec![Cell::Text("south".into()), Cell::Real(7.5), Cell::Int(3)],
        ]
    );
}

#[test]
fn star_expands_against_the_file_schema() {
    let mut e = open();
    let r = e
        .run_query("SELECT * FROM production ORDER BY id LIMIT 1")
        .unwrap();
    assert_eq!(r.columns, ["region", "amount", "id"]);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].len(), 3);
}

#[test]
fn aggregates_and_group_by_run_across_every_row_group() {
    let mut e = open();
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM production"),
        vec![vec![Cell::Int(5000)]]
    );
    let grouped = rows(
        &mut e,
        "SELECT region, count(*) FROM production GROUP BY region ORDER BY region",
    );
    assert!(!grouped.is_empty());
    let total: i64 = grouped
        .iter()
        .map(|r| match &r[1] {
            Cell::Int(n) => *n,
            other => panic!("count is {other:?}"),
        })
        .sum();
    assert_eq!(total, 5000, "group counts must sum to the row count");
    let filtered = rows(
        &mut e,
        "SELECT id FROM production WHERE id > 4990 ORDER BY id",
    );
    assert_eq!(filtered.len(), 10);
    assert_eq!(filtered[0], vec![Cell::Int(4991)]);
}

#[test]
fn like_matches_contains_prefix_and_suffix_patterns() {
    let mut e = open();
    // Distinct regions: east, north, south, west.
    let contains = rows(
        &mut e,
        "SELECT region FROM production WHERE region LIKE '%out%' GROUP BY region",
    );
    assert_eq!(contains, vec![vec![Cell::Text("south".into())]]);

    let prefix = rows(
        &mut e,
        "SELECT region FROM production WHERE region LIKE 'wes%' GROUP BY region",
    );
    assert_eq!(prefix, vec![vec![Cell::Text("west".into())]]);

    // Both "north" and "south" end in "th".
    let suffix = rows(
        &mut e,
        "SELECT region FROM production WHERE region LIKE '%th' GROUP BY region ORDER BY region",
    );
    assert_eq!(
        suffix,
        vec![
            vec![Cell::Text("north".into())],
            vec![Cell::Text("south".into())],
        ]
    );
}

#[test]
fn not_like_excludes_matching_rows() {
    let mut e = open();
    let remaining = rows(
        &mut e,
        "SELECT region FROM production WHERE region NOT LIKE 'wes%' GROUP BY region ORDER BY region",
    );
    assert_eq!(
        remaining,
        vec![
            vec![Cell::Text("east".into())],
            vec![Cell::Text("north".into())],
            vec![Cell::Text("south".into())],
        ]
    );
}

#[test]
fn unknown_table_and_unknown_column_are_compile_errors_naming_the_culprit() {
    let mut e = open();
    let err = e.run_query("SELECT id FROM no_such_table").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile, "{err}");
    assert!(err.message.contains("no_such_table"), "{err}");

    let err = e
        .run_query("SELECT no_such_column FROM production")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile, "{err}");
    assert!(err.message.contains("no_such_column"), "{err}");
}

#[test]
fn syntax_error_is_parse_kind_not_compile() {
    // column-rs's QueryEngine mislabels this as UnknownColumn; the engine
    // parses first and reports it for what it is.
    let mut e = open();
    let err = e.run_query("SELECT FROM WHERE").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Parse, "{err}");
}

#[test]
fn multi_table_shapes_are_unsupported_not_wrong() {
    let mut e = open();
    let err = e
        .run_query("SELECT p.id FROM production p JOIN production q ON p.id = q.id")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");
    let err = e
        .run_query("SELECT id FROM production WHERE id IN (SELECT id FROM production)")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");
}

#[test]
fn run_query_takes_exactly_one_statement() {
    let mut e = open();
    let err = e
        .run_query("SELECT id FROM production; SELECT id FROM production")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");
    let err = e.run_query("   ").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Parse, "{err}");
}

#[test]
fn explain_plan_is_a_tree_rooted_at_query_plan_with_a_scan_carrying_footer_stats() {
    let e = open();
    let plan: Vec<PlanRow> = e
        .explain_plan("SELECT region, SUM(amount) FROM production WHERE id > 1000 GROUP BY region")
        .unwrap();
    assert!(plan.len() >= 2, "{plan:?}");
    assert_eq!(plan[0].detail, "QUERY PLAN");
    assert_eq!(plan[0].parent, plan[0].id, "root's parent is itself");
    let scan = plan
        .iter()
        .find(|r| r.detail.starts_with("SCAN"))
        .unwrap_or_else(|| panic!("no SCAN node in {plan:?}"));
    assert!(scan.detail.contains("production"), "{}", scan.detail);
    // Footer stats reach the plan: 5000 rows.
    assert!(scan.detail.contains("5000"), "{}", scan.detail);
    // Every non-root node's parent is a node in the tree.
    let ids: Vec<i64> = plan.iter().map(|r| r.id).collect();
    for r in &plan[1..] {
        assert!(ids.contains(&r.parent), "{r:?} parent not in {ids:?}");
    }
}

#[test]
fn explain_opcodes_returns_the_sections_the_engine_runs() {
    let e = open();
    let sections = e
        .explain_opcodes("SELECT region, SUM(amount) FROM production GROUP BY region")
        .unwrap();
    assert!(!sections.is_empty());
    let total_rows: usize = sections.iter().map(|s| s.rows.len()).sum();
    assert!(total_rows > 0, "{sections:?}");
    for s in &sections {
        assert!(!s.label.is_empty());
        for (i, r) in s.rows.iter().enumerate() {
            assert_eq!(r.addr, i, "addresses dense per section: {sections:?}");
            assert!(!r.opcode.is_empty());
        }
    }
    let opcodes: Vec<&str> = sections
        .iter()
        .flat_map(|s| s.rows.iter().map(|r| r.opcode.as_str()))
        .collect();
    assert!(opcodes.iter().any(|o| o.starts_with("Load")), "{opcodes:?}");
}

#[test]
fn explain_rejects_what_run_query_rejects() {
    let e = open();
    assert_eq!(
        e.explain_plan("SELECT id FROM nope").unwrap_err().kind,
        ErrorKind::Compile
    );
    assert_eq!(
        e.explain_plan("SELECT id FROM production; SELECT 1")
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
    assert_eq!(
        e.explain_opcodes("SELECT FROM").unwrap_err().kind,
        ErrorKind::Parse
    );
}

#[test]
fn engine_is_object_safe_and_usable_through_dyn() {
    let mut boxed: Box<dyn Engine> = Box::new(open());
    assert_eq!(boxed.mode(), Mode::Batch);
    let r = boxed.run_query("SELECT count(*) FROM production").unwrap();
    assert_eq!(r.rows, vec![vec![Cell::Int(5000)]]);
    assert!(matches!(boxed.stats(), FileStats::Batch { rows: 5000, .. }));
    assert_eq!(boxed.tables().unwrap()[0].name, "production");
    assert!(!boxed
        .explain_opcodes("SELECT id FROM production")
        .unwrap()
        .is_empty());
}
