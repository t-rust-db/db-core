// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Index seeks on a **non-unique** index, end to end through
//! `engine::row::RowEngine` (#298): `col = lit` plans as `SEARCH` and
//! returns every matching row, and so does `IN (...)` -- which before
//! #298 did one `SeekIndexEq` per value and silently returned only the
//! first of several rows sharing a key. Every seek shape is checked
//! against the same query run as a full scan (an unindexed predicate on
//! the same rows), so the expectation is the engine's own scan, not a
//! hand-typed list.
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
use db_core::engine::{Cell, Engine};

const FIXTURE: &str = "tests/corpus/fixtures/btrees/table_single_page.db";
/// Built by sqlite3 3.53.4 (db-core's own DDL does not accept `COLLATE`
/// inside `CREATE INDEX`): `n(id INTEGER PRIMARY KEY, name TEXT COLLATE
/// NOCASE, tag TEXT)`, `CREATE INDEX n_name ON n(name COLLATE NOCASE)`,
/// rows Alice/alice/ALICE/bob/Bob/carol.
const NOCASE_FIXTURE: &str = "tests/corpus/fixtures/btrees/collate_nocase.db";

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        Self::from(FIXTURE, label)
    }
    fn from(fixture: &str, label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-seek-{label}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::copy(fixture, &path).expect("copy fixture");
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

/// `k` is indexed (non-unique) and `tag` is not; `mirror` duplicates `k`
/// unindexed so the same predicate can be run as a plain scan.
/// Rows: k=1 x3, k=2 x1, k=3 x2, plus a text-keyed twin table.
fn seeded(label: &str) -> (TempDb, RowEngine) {
    let db = TempDb::new(label);
    let mut e = RowEngine::open(db.path()).expect("open");
    e.run_query(
        "CREATE TABLE s(id INTEGER PRIMARY KEY, k INTEGER, mirror INTEGER, tag TEXT); \
         CREATE INDEX s_k ON s(k); \
         INSERT INTO s(k, mirror, tag) VALUES (1, 1, 'a'); \
         INSERT INTO s(k, mirror, tag) VALUES (2, 2, 'b'); \
         INSERT INTO s(k, mirror, tag) VALUES (1, 1, 'c'); \
         INSERT INTO s(k, mirror, tag) VALUES (3, 3, 'd'); \
         INSERT INTO s(k, mirror, tag) VALUES (1, 1, 'e'); \
         INSERT INTO s(k, mirror, tag) VALUES (3, 3, 'f'); \
         CREATE TABLE w(id INTEGER PRIMARY KEY, name TEXT, mirror TEXT); \
         CREATE INDEX w_name ON w(name); \
         INSERT INTO w(name, mirror) VALUES ('x', 'x'); \
         INSERT INTO w(name, mirror) VALUES ('y', 'y'); \
         INSERT INTO w(name, mirror) VALUES ('x', 'x'); \
         INSERT INTO w(name, mirror) VALUES ('z', 'z'); \
         INSERT INTO w(name, mirror) VALUES ('x', 'x')",
    )
    .expect("seed");
    (db, e)
}

fn rows(e: &mut RowEngine, sql: &str) -> Vec<Vec<Cell>> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
}

fn plan(e: &RowEngine, sql: &str) -> String {
    e.explain_plan(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .iter()
        .map(|r| r.detail.clone())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Seek shape vs. the same predicate on the unindexed mirror column, both
/// ordered by rowid so row order is not part of the comparison.
fn assert_seek_matches_scan(e: &mut RowEngine, table: &str, seek_pred: &str, scan_pred: &str) {
    let seek = rows(
        e,
        &format!("SELECT id FROM {table} WHERE {seek_pred} ORDER BY id"),
    );
    let scan = rows(
        e,
        &format!("SELECT id FROM {table} WHERE {scan_pred} ORDER BY id"),
    );
    assert!(
        !scan.is_empty(),
        "scan predicate {scan_pred} matched nothing"
    );
    assert_eq!(seek, scan, "{seek_pred} vs {scan_pred}");
}

#[test]
fn equality_on_integer_index_plans_as_search_and_returns_every_duplicate() {
    let (_db, mut e) = seeded("eq-int");
    // A projection the index does not cover takes #298's seek; a covered
    // one (`id` is the rowid, `k` the key) takes the pre-existing covering
    // path -- both are SEARCHes, worded as sqlite3 words them.
    assert_eq!(
        plan(&e, "SELECT tag FROM s WHERE k = 1"),
        "SEARCH s USING INDEX s_k (k=?)"
    );
    assert_eq!(
        plan(&e, "SELECT tag FROM s WHERE 1 = k"),
        "SEARCH s USING INDEX s_k (k=?)"
    );
    assert_eq!(
        plan(&e, "SELECT id FROM s WHERE k = 1"),
        "SEARCH s USING COVERING INDEX s_k (k=?)"
    );
    assert_seek_matches_scan(&mut e, "s", "k = 1", "mirror = 1");
    assert_seek_matches_scan(&mut e, "s", "1 = k", "mirror = 1");
    assert_seek_matches_scan(&mut e, "s", "k = 3", "mirror = 3");
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM s WHERE k = 1"),
        vec![vec![Cell::Int(3)]]
    );
    assert!(rows(&mut e, "SELECT id FROM s WHERE k = 99").is_empty());
}

#[test]
fn equality_on_text_index_plans_as_search_and_returns_every_duplicate() {
    let (_db, mut e) = seeded("eq-text");
    assert_eq!(
        plan(&e, "SELECT mirror FROM w WHERE name = 'x'"),
        "SEARCH w USING INDEX w_name (name=?)"
    );
    assert_seek_matches_scan(&mut e, "w", "name = 'x'", "mirror = 'x'");
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM w WHERE name = 'x'"),
        vec![vec![Cell::Int(3)]]
    );
    assert_seek_matches_scan(&mut e, "w", "name = 'y'", "mirror = 'y'");
}

#[test]
fn in_list_returns_every_duplicate_for_each_value() {
    // The pre-#298 bug: `IN (1)` returned one of the three k=1 rows.
    let (_db, mut e) = seeded("in-dups");
    assert_eq!(
        plan(&e, "SELECT tag FROM s WHERE k IN (1)"),
        "SEARCH s USING INDEX s_k (k=?)"
    );
    assert_seek_matches_scan(&mut e, "s", "k IN (1)", "mirror IN (1)");
    assert_seek_matches_scan(&mut e, "s", "k IN (1, 3)", "mirror IN (1, 3)");
    assert_seek_matches_scan(&mut e, "s", "k IN (3, 1, 1)", "mirror IN (3, 1, 1)");
    assert_seek_matches_scan(&mut e, "s", "k IN (2, 99)", "mirror IN (2, 99)");
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM s WHERE k IN (1, 3)"),
        vec![vec![Cell::Int(5)]]
    );
    assert_seek_matches_scan(&mut e, "w", "name IN ('x')", "mirror IN ('x')");
    assert_seek_matches_scan(&mut e, "w", "name IN ('x', 'z')", "mirror IN ('x', 'z')");
}

#[test]
fn between_is_unchanged_and_degenerate_between_equals_equality() {
    let (_db, mut e) = seeded("between");
    assert_eq!(
        plan(&e, "SELECT tag FROM s WHERE k BETWEEN 1 AND 2"),
        "SEARCH s USING INDEX s_k (k>? AND k<?)"
    );
    assert_seek_matches_scan(&mut e, "s", "k BETWEEN 1 AND 2", "mirror BETWEEN 1 AND 2");
    assert_seek_matches_scan(&mut e, "s", "k BETWEEN 1 AND 1", "mirror = 1");
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM s WHERE k BETWEEN 1 AND 1 ORDER BY id"
        ),
        rows(&mut e, "SELECT id FROM s WHERE k = 1 ORDER BY id")
    );
}

#[test]
fn equality_seek_respects_limit_offset_and_projection() {
    let (_db, mut e) = seeded("limit");
    let all = rows(&mut e, "SELECT id, tag FROM s WHERE k = 1 ORDER BY id");
    assert_eq!(all.len(), 3);
    let limited = rows(&mut e, "SELECT id, tag FROM s WHERE k = 1 LIMIT 2");
    assert_eq!(limited.len(), 2);
    let offset = rows(&mut e, "SELECT id, tag FROM s WHERE k = 1 LIMIT 5 OFFSET 1");
    assert_eq!(offset.len(), 2);
    // Index-only read of the seeked column (#664 path) still yields the
    // right value for every duplicate.
    assert_eq!(
        rows(&mut e, "SELECT k FROM s WHERE k = 3"),
        vec![vec![Cell::Int(3)], vec![Cell::Int(3)]]
    );
}

#[test]
fn equality_still_scans_when_it_cannot_seek() {
    let (_db, mut e) = seeded("no-seek");
    // Unindexed column.
    assert_eq!(plan(&e, "SELECT id FROM s WHERE tag = 'a'"), "SCAN s");
    // Literal storage class does not match the column affinity: a raw
    // seek probe would be wrong, so #298's path must not take it (the
    // projection is deliberately not covered by the index).
    assert_eq!(plan(&e, "SELECT tag FROM s WHERE k = '1'"), "SCAN s");
    assert_eq!(
        plan(&e, "SELECT id, mirror FROM w WHERE name = 1"),
        "SCAN w"
    );
    // Both sides columns: not a constant bound.
    assert_eq!(plan(&e, "SELECT tag FROM s WHERE k = mirror"), "SCAN s");
    // And the scan path still answers correctly.
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM s WHERE k = mirror"),
        vec![vec![Cell::Int(6)]]
    );
}

#[test]
fn update_and_delete_with_equality_touch_exactly_the_duplicates() {
    let (_db, mut e) = seeded("write");
    e.run_query("UPDATE s SET tag = 'one' WHERE k = 1").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM s WHERE tag = 'one'"),
        vec![vec![Cell::Int(3)]]
    );
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM s WHERE tag <> 'one'"),
        vec![vec![Cell::Int(3)]]
    );
    e.run_query("DELETE FROM s WHERE k = 3").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM s"),
        vec![vec![Cell::Int(4)]]
    );
    assert!(rows(&mut e, "SELECT id FROM s WHERE k = 3").is_empty());
    assert!(rows(&mut e, "SELECT id FROM s WHERE mirror = 3").is_empty());
    // The index itself is consistent afterwards.
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM s WHERE k IN (1, 2, 3)"),
        vec![vec![Cell::Int(4)]]
    );
}

/// The index b-tree is BINARY-ordered (Tier 0), so no seek shape may be
/// used on a `NOCASE` index: the walk would land on the wrong leaf.
/// Before #298's guard, `=`/`IN`/`BETWEEN`/`>` on this fixture all
/// returned 0 rows. Expected counts are sqlite3's on the same file.
#[test]
fn non_binary_collated_index_is_never_seeked_and_results_match_the_oracle() {
    let db = TempDb::from(NOCASE_FIXTURE, "nocase");
    let mut e = RowEngine::open(db.path()).expect("open");
    for (sql, expected_rows) in [
        ("SELECT name FROM n WHERE name = 'alice'", 3),
        ("SELECT id, tag FROM n WHERE name = 'alice'", 3),
        ("SELECT id FROM n WHERE name IN ('bob')", 2),
        ("SELECT id FROM n WHERE name IN ('alice', 'BOB')", 5),
        ("SELECT id FROM n WHERE name > 'alice'", 3),
        ("SELECT id FROM n WHERE name BETWEEN 'alice' AND 'bob'", 5),
        ("SELECT id FROM n WHERE name LIKE 'al%'", 3),
    ] {
        assert_eq!(plan(&e, sql), "SCAN n", "{sql}");
        assert_eq!(rows(&mut e, sql).len(), expected_rows, "{sql}");
    }
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM n WHERE name = 'alice'"),
        vec![vec![Cell::Int(3)]]
    );
    // The rowid path is unaffected by the index's collation.
    assert!(
        plan(&e, "SELECT id FROM n WHERE id = 2").starts_with("SEARCH n USING INTEGER PRIMARY KEY")
    );
}
