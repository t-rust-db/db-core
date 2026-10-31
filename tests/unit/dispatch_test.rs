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

/// #650: ROLLBACK also undoes DDL, so the cached catalog must follow the
/// file back; it kept the in-transaction schema, and a dropped table
/// stayed "no such table" while a rolled-back CREATE stayed visible.
#[test]
fn rollback_restores_a_dropped_table() {
    let db = TempDb::new("rollback-drop-table");
    let mut e = open(&db);
    e.run_query("CREATE TABLE rdt(a INTEGER); INSERT INTO rdt VALUES (7)")
        .unwrap();
    e.run_query("BEGIN; DROP TABLE rdt; ROLLBACK").unwrap();
    assert_eq!(e.run_query("SELECT a FROM rdt").unwrap().rows.len(), 1);
    // sqlite3 checks the kind before `IF EXISTS`.
    let err = e.run_query("DROP VIEW IF EXISTS rdt").unwrap_err();
    assert!(
        err.message.contains("use DROP TABLE to delete table rdt"),
        "{}",
        err.message
    );
}

#[test]
fn rollback_forgets_a_created_table_and_view() {
    let db = TempDb::new("rollback-create");
    let mut e = open(&db);
    e.run_query("BEGIN; CREATE TABLE rct(a INTEGER); CREATE VIEW rcv AS SELECT 1; ROLLBACK")
        .unwrap();
    for sql in ["SELECT a FROM rct", "SELECT * FROM rcv"] {
        let err = e.run_query(sql).unwrap_err();
        assert!(
            err.message.contains("no such table"),
            "{sql}: {}",
            err.message
        );
    }
    // Free again: nothing is left to collide with.
    e.run_query("CREATE TABLE rct(a INTEGER); CREATE VIEW rcv AS SELECT 1")
        .unwrap();
    e.run_query("INSERT INTO rct VALUES (3)").unwrap();
    assert_eq!(e.run_query("SELECT a FROM rct").unwrap().rows.len(), 1);
}

#[test]
fn rollback_restores_a_dropped_view() {
    let db = TempDb::new("rollback-drop-view");
    let mut e = open(&db);
    e.run_query("CREATE VIEW rdv AS SELECT 1").unwrap();
    e.run_query("BEGIN; DROP VIEW rdv; ROLLBACK").unwrap();
    assert_eq!(e.run_query("SELECT * FROM rdv").unwrap().rows.len(), 1);
}

/// #650: flattening a subquery left an outer `*` to expand over the base
/// table, so `SELECT * FROM (SELECT a FROM t) s` returned every column.
#[test]
fn outer_star_over_an_explicit_subquery_projection_keeps_its_columns() {
    let db = TempDb::new("flatten-outer-star");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE fs(a INTEGER, b INTEGER, c INTEGER); INSERT INTO fs VALUES (1, 2, 3)",
    )
    .unwrap();
    for (sql, expected) in [
        ("SELECT * FROM (SELECT a FROM fs) s", "[Int(1)]"),
        ("SELECT * FROM (SELECT c, a FROM fs) s", "[Int(3), Int(1)]"),
        ("SELECT * FROM (SELECT b AS x FROM fs) s", "[Int(2)]"),
        (
            "SELECT * FROM (SELECT * FROM fs) s",
            "[Int(1), Int(2), Int(3)]",
        ),
        (
            "SELECT * FROM (SELECT fs.* FROM fs) s",
            "[Int(1), Int(2), Int(3)]",
        ),
    ] {
        let rows = e.run_query(sql).unwrap().rows;
        assert_eq!(format!("{:?}", rows[0]), expected, "{sql}");
    }
}

/// #650: a flattened `SELECT *` subquery lost its alias, so a reference to
/// it next to a joined table with a same-named column was "ambiguous".
#[test]
fn a_flattened_star_subquery_keeps_its_alias_next_to_a_join() {
    let db = TempDb::new("flatten-star-join");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE js(a INTEGER, b INTEGER); INSERT INTO js VALUES (1, 2); \
         CREATE TABLE ju(a INTEGER, z INTEGER); INSERT INTO ju VALUES (1, 9)",
    )
    .unwrap();
    for (sql, expected) in [
        (
            "SELECT * FROM (SELECT * FROM js) s JOIN ju ON s.a = ju.a",
            "[Int(1), Int(2), Int(1), Int(9)]",
        ),
        (
            "SELECT * FROM (SELECT js.* FROM js) s JOIN ju ON s.a = ju.a",
            "[Int(1), Int(2), Int(1), Int(9)]",
        ),
        (
            "SELECT s.b, ju.z FROM (SELECT * FROM js) s JOIN ju ON s.a = ju.a WHERE s.a = 1",
            "[Int(2), Int(9)]",
        ),
    ] {
        let rows = e
            .run_query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"))
            .rows;
        assert_eq!(format!("{:?}", rows[0]), expected, "{sql}");
    }
}

/// #650: a subquery projecting `t.*` twice repeats the star outside too,
/// flattened or not, alone or next to a joined table.
#[test]
fn repeated_table_stars_keep_every_copy_after_flattening() {
    let db = TempDb::new("flatten-repeated-star");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE rs(a INTEGER, b INTEGER); INSERT INTO rs VALUES (1, 2); \
         CREATE TABLE ru(a INTEGER, z INTEGER); INSERT INTO ru VALUES (1, 9)",
    )
    .unwrap();
    for (sql, expected) in [
        (
            "SELECT * FROM (SELECT rs.*, rs.* FROM rs) s",
            "[Int(1), Int(2), Int(1), Int(2)]",
        ),
        (
            "SELECT * FROM (SELECT x.*, x.*, x.* FROM rs x) s",
            "[Int(1), Int(2), Int(1), Int(2), Int(1), Int(2)]",
        ),
        (
            "SELECT * FROM (SELECT rs.*, rs.* FROM rs) s JOIN ru ON s.a = ru.a",
            "[Int(1), Int(2), Int(1), Int(2), Int(1), Int(9)]",
        ),
        (
            "SELECT * FROM ru JOIN (SELECT rs.*, rs.* FROM rs) s ON s.a = ru.a",
            "[Int(1), Int(9), Int(1), Int(2), Int(1), Int(2)]",
        ),
    ] {
        let rows = e
            .run_query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"))
            .rows;
        assert_eq!(format!("{:?}", rows[0]), expected, "{sql}");
    }
    let plan = e
        .explain_plan("SELECT * FROM (SELECT rs.*, rs.* FROM rs) s")
        .unwrap();
    assert!(
        plan.iter().all(|r| !r.detail.contains("(subquery)")),
        "{plan:?}"
    );
}

/// #650: `SELECT t.* FROM t` projects what `SELECT * FROM t` does, so a
/// subquery of that shape flattens, as it does in sqlite3.
#[test]
fn a_single_table_star_subquery_flattens_like_a_plain_star() {
    let db = TempDb::new("flatten-table-star");
    let mut e = open(&db);
    e.run_query("CREATE TABLE ft(a INTEGER, b INTEGER)")
        .unwrap();
    for sql in [
        "SELECT * FROM (SELECT * FROM ft) s",
        "SELECT * FROM (SELECT ft.* FROM ft) s",
        "SELECT * FROM (SELECT x.* FROM ft x) s",
    ] {
        let plan = e.explain_plan(sql).unwrap();
        assert!(
            plan.iter().all(|r| !r.detail.contains("(subquery)")),
            "{sql}: {plan:?}"
        );
    }
}

/// #650: sqlite3 keeps the first value of a column listed twice in an
/// INSERT, and still prepares (so validates) the others.
#[test]
fn insert_with_a_column_listed_twice_keeps_the_first_value_and_validates_the_rest() {
    let db = TempDb::new("insert-dup-column");
    let mut e = open(&db);
    e.run_query("CREATE TABLE dc(a INTEGER, s INTEGER)")
        .unwrap();
    e.run_query("INSERT INTO dc(s, s) VALUES (1, 2)").unwrap();
    let rows = e.run_query("SELECT a, s FROM dc").unwrap().rows;
    assert_eq!(format!("{:?}", rows[0]), "[Null, Int(1)]");
    for (sql, message) in [
        (
            "INSERT INTO dc(s, s) VALUES (nosuch(1), 2)",
            "no such function: nosuch",
        ),
        (
            "INSERT INTO dc(s, s) VALUES (1, nosuch(2))",
            "no such function: nosuch",
        ),
        ("INSERT INTO dc(s, s) VALUES (pj, 2)", "pj"),
        ("INSERT INTO dc(s, s) VALUES (1, pj)", "pj"),
    ] {
        let err = e.run_query(sql).unwrap_err();
        assert!(err.message.contains(message), "{sql}: {}", err.message);
    }
    // Nothing was added by the refused statements.
    assert_eq!(e.run_query("SELECT a FROM dc").unwrap().rows.len(), 1);
}

/// #650: statements sqlite3 refuses at prepare, with its messages.
#[test]
fn having_check_parameters_and_explain_star_are_refused_like_sqlite3() {
    let db = TempDb::new("prepare-refusals");
    let mut e = open(&db);
    e.run_query("CREATE TABLE pr(a INTEGER, b INTEGER); INSERT INTO pr VALUES (1, 2)")
        .unwrap();
    for (sql, message) in [
        (
            "SELECT a FROM pr HAVING a > 0",
            "HAVING clause on a non-aggregate query",
        ),
        (
            "SELECT a FROM pr HAVING count(*) > 0",
            "HAVING clause on a non-aggregate query",
        ),
        (
            "SELECT * FROM (SELECT a FROM pr HAVING a) s",
            "HAVING clause on a non-aggregate query",
        ),
        (
            "CREATE TABLE pc(a CHECK (a > ?))",
            "parameters prohibited in CHECK constraints",
        ),
        ("SELECT *, pr.* FROM (VALUES (1))", "no such table: pr"),
    ] {
        let err = e.run_query(sql).unwrap_err();
        assert!(err.message.contains(message), "{sql}: {}", err.message);
        // EXPLAIN QUERY PLAN refuses what the statement itself refuses.
        if sql.starts_with("SELECT") {
            let err = e.explain_plan(sql).unwrap_err();
            assert!(err.message.contains(message), "{sql}: {}", err.message);
        }
    }
    // The accepted shapes still run.
    for sql in [
        "SELECT count(*) FROM pr HAVING count(*) > 0",
        "SELECT a FROM pr GROUP BY a HAVING a > 0",
        "SELECT a, count(*) FROM pr HAVING 1",
    ] {
        e.run_query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
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
