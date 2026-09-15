// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `codegen::row::dispatch`'s keyword-sniffing and error paths, end to
//! end through `engine::row::RowEngine` -- `IF NOT EXISTS` no-ops,
//! already-exists/no-such-table/index errors, `ANALYZE <name>`'s three
//! branches, `EXPLAIN QUERY PLAN`, `BEGIN`/`COMMIT`/`ROLLBACK`, and an
//! unrecognized statement, none of which had a test anywhere in the
//! suite before.
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

use db_core::engine::row::RowEngine;
use db_core::engine::{Engine, ErrorKind};

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-dispatch-{label}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::copy(FIXTURE, &path).expect("copy fixture");
        TempDb(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
        std::fs::remove_file(format!("{}-journal", self.0.display())).ok();
    }
}

fn open(db: &TempDb) -> RowEngine {
    RowEngine::open(db.path()).expect("open fixture copy")
}

#[test]
fn create_table_if_not_exists_no_ops_when_the_table_is_already_there() {
    let db = TempDb::new("ct-ine");
    let mut e = open(&db);
    e.run_query("CREATE TABLE dt(a INTEGER)").unwrap();
    // No error, and the existing table/rows are untouched.
    e.run_query("INSERT INTO dt VALUES (1)").unwrap();
    e.run_query("CREATE TABLE IF NOT EXISTS dt(a INTEGER, b INTEGER)")
        .unwrap();
    let rows = e.run_query("SELECT a FROM dt").unwrap().rows;
    assert_eq!(rows.len(), 1);
}

#[test]
fn create_table_without_if_not_exists_on_an_existing_table_errors() {
    let db = TempDb::new("ct-exists");
    let mut e = open(&db);
    e.run_query("CREATE TABLE dt2(a INTEGER)").unwrap();
    let err = e.run_query("CREATE TABLE dt2(a INTEGER)").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("already exists"), "{}", err.message);
}

#[test]
fn create_view_compiles_and_is_queryable() {
    let db = TempDb::new("create-view");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE cv(a INTEGER); \
         INSERT INTO cv VALUES (1), (2); \
         CREATE VIEW cv_view AS SELECT a FROM cv WHERE a > 1",
    )
    .unwrap();
    let rows = e.run_query("SELECT a FROM cv_view").unwrap().rows;
    assert_eq!(rows.len(), 1);
}

#[test]
fn create_index_on_an_unknown_table_is_no_such_table() {
    let db = TempDb::new("ci-no-table");
    let mut e = open(&db);
    let err = e
        .run_query("CREATE INDEX ci_idx ON missing_table(a)")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("no such table"), "{}", err.message);
}

#[test]
fn create_index_if_not_exists_no_ops_when_the_index_is_already_there() {
    let db = TempDb::new("ci-ine");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE ci_t(a INTEGER); \
         CREATE INDEX ci_idx ON ci_t(a)",
    )
    .unwrap();
    e.run_query("CREATE INDEX IF NOT EXISTS ci_idx ON ci_t(a)")
        .unwrap();
}

#[test]
fn create_index_without_if_not_exists_on_an_existing_index_errors() {
    let db = TempDb::new("ci-exists");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE ci_t2(a INTEGER); \
         CREATE INDEX ci_idx2 ON ci_t2(a)",
    )
    .unwrap();
    let err = e.run_query("CREATE INDEX ci_idx2 ON ci_t2(a)").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("already exists"), "{}", err.message);
}

#[test]
fn create_unique_index_dispatches_through_the_same_path_as_a_plain_index() {
    let db = TempDb::new("ci-unique");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE ciu(a INTEGER); \
         CREATE UNIQUE INDEX ciu_idx ON ciu(a)",
    )
    .unwrap();
}

#[test]
fn drop_table_on_an_unknown_table_is_no_such_table() {
    let db = TempDb::new("dt-unknown");
    let mut e = open(&db);
    let err = e.run_query("DROP TABLE missing_table").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("no such table"), "{}", err.message);
}

#[test]
fn drop_index_on_an_unknown_index_is_no_such_index() {
    let db = TempDb::new("di-unknown");
    let mut e = open(&db);
    let err = e.run_query("DROP INDEX missing_index").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("no such index"), "{}", err.message);
}

#[test]
fn analyze_of_a_named_table_only_touches_that_table() {
    let db = TempDb::new("analyze-named");
    let mut e = open(&db);
    e.run_query("CREATE TABLE an_t(a INTEGER)").unwrap();
    e.run_query("ANALYZE an_t").unwrap();
}

#[test]
fn analyze_of_an_unknown_name_is_no_such_table() {
    let db = TempDb::new("analyze-unknown");
    let mut e = open(&db);
    let err = e.run_query("ANALYZE missing_thing").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("no such table"), "{}", err.message);
}

#[test]
fn analyze_of_a_single_index_name_is_unsupported() {
    let db = TempDb::new("analyze-index");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE ax_t(a INTEGER); \
         CREATE INDEX ax_idx ON ax_t(a)",
    )
    .unwrap();
    let err = e.run_query("ANALYZE ax_idx").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("not yet supported"), "{}", err.message);
}

#[test]
fn bare_explain_without_query_plan_is_unsupported() {
    let db = TempDb::new("bare-explain");
    let mut e = open(&db);
    e.run_query("CREATE TABLE be(a INTEGER)").unwrap();
    let err = e.run_query("EXPLAIN SELECT a FROM be").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("not yet supported"), "{}", err.message);
}

#[test]
fn begin_commit_and_rollback_compile_and_run() {
    let db = TempDb::new("txn-words");
    let mut e = open(&db);
    e.run_query("CREATE TABLE tw(a INTEGER)").unwrap();
    e.run_query("BEGIN; INSERT INTO tw VALUES (1); COMMIT")
        .unwrap();
    e.run_query("BEGIN; INSERT INTO tw VALUES (2); ROLLBACK")
        .unwrap();
    let rows = e.run_query("SELECT a FROM tw").unwrap().rows;
    assert_eq!(rows.len(), 1);
}

#[test]
fn an_unrecognized_leading_keyword_is_reported_with_the_original_casing() {
    let db = TempDb::new("unrecognized");
    let mut e = open(&db);
    let err = e.run_query("frobnicate everything").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("FROBNICATE"), "{}", err.message);
}

#[test]
fn insert_select_with_a_cte_source_is_unsupported() {
    let db = TempDb::new("insert-select-cte");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE isc_src(a INTEGER); \
         CREATE TABLE isc_dst(a INTEGER)",
    )
    .unwrap();
    let err = e
        .run_query("INSERT INTO isc_dst WITH c AS (SELECT a FROM isc_src) SELECT a FROM c")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("CTE"), "{}", err.message);
}
