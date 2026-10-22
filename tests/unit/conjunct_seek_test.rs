// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `AND` conjunctions and BLOB literals use an index (#695), end to end
//! through `engine::row::RowEngine`. Every predicate runs against an
//! indexed table and an identical unindexed twin; the rows must agree
//! (so `NULL` keys, duplicates, exclusive bounds and residual filters are
//! all checked against the engine's own scan), and the indexed table must
//! plan as `SEARCH`, worded as sqlite3 words it.
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

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-conj-{label}-{}-{}.db",
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

/// `ct` (indexed) and `cm` (no index) hold the same 144 rows: every
/// combination of `a`, `b`, `n` and `bl` over a few values and `NULL`.
fn seeded(label: &str) -> (TempDb, RowEngine) {
    let db = TempDb::new(label);
    let mut e = RowEngine::open(db.path()).expect("open");
    e.run_query(
        "CREATE TABLE ct(id INTEGER PRIMARY KEY, a TEXT, b TEXT, n INTEGER, bl BLOB, pad TEXT); \
         CREATE INDEX ct_ab ON ct(a, b, n); \
         CREATE INDEX ct_n ON ct(n); \
         CREATE INDEX ct_bl ON ct(bl); \
         CREATE INDEX ct_b ON ct(b); \
         CREATE TABLE cm(id INTEGER PRIMARY KEY, a TEXT, b TEXT, n INTEGER, bl BLOB, pad TEXT)",
    )
    .expect("create");
    let texts = ["'p'", "'q'", "NULL"];
    let bs = ["'x'", "'y'", "NULL"];
    let ns = ["1", "2", "3", "NULL"];
    let bls = ["X'01'", "X'0102'", "X'02'", "NULL"];
    for a in texts {
        for b in bs {
            for n in ns {
                for bl in bls {
                    for table in ["ct", "cm"] {
                        e.run_query(&format!(
                            "INSERT INTO {table}(a, b, n, bl, pad) VALUES ({a}, {b}, {n}, {bl}, 'z')"
                        ))
                        .expect("insert");
                    }
                }
            }
        }
    }
    (db, e)
}

fn ids(e: &mut RowEngine, table: &str, pred: &str) -> Vec<Vec<Cell>> {
    let sql = format!("SELECT id FROM {table} WHERE {pred} ORDER BY id");
    e.run_query(&sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
}

fn plan(e: &RowEngine, pred: &str) -> String {
    // `pad` is not in any index, so the plan is the seek, not a covering scan.
    e.explain_plan(&format!("SELECT pad FROM ct WHERE {pred}"))
        .unwrap_or_else(|err| panic!("{pred}: {err}"))
        .iter()
        .map(|r| r.detail.clone())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// `pred` on `ct` plans as `expected` and returns what the scan of `cm` does.
fn check(e: &mut RowEngine, pred: &str, expected: &str) {
    assert_eq!(plan(e, pred), expected, "plan of {pred}");
    let seek = ids(e, "ct", pred);
    let scan = ids(e, "cm", pred);
    assert!(!scan.is_empty(), "{pred} matched nothing");
    assert_eq!(seek, scan, "{pred}");
}

#[test]
fn equality_prefix_of_a_composite_index() {
    let (_db, mut e) = seeded("prefix");
    check(&mut e, "a = 'p'", "SEARCH ct USING INDEX ct_ab (a=?)");
    check(
        &mut e,
        "a = 'p' AND b = 'x'",
        "SEARCH ct USING INDEX ct_ab (a=? AND b=?)",
    );
    check(
        &mut e,
        "a = 'p' AND b = 'x' AND n = 2",
        "SEARCH ct USING INDEX ct_ab (a=? AND b=? AND n=?)",
    );
    check(
        &mut e,
        "b = 'x' AND a = 'q'",
        "SEARCH ct USING INDEX ct_ab (a=? AND b=?)",
    );
}

#[test]
fn equality_prefix_then_a_range() {
    let (_db, mut e) = seeded("prefix-range");
    for (pred, wording) in [
        ("a = 'p' AND b = 'x' AND n > 1", "(a=? AND b=? AND n>?)"),
        ("a = 'p' AND b = 'x' AND n >= 2", "(a=? AND b=? AND n>?)"),
        ("a = 'p' AND b = 'x' AND n < 3", "(a=? AND b=? AND n<?)"),
        ("a = 'p' AND b = 'x' AND n <= 2", "(a=? AND b=? AND n<?)"),
        (
            "a = 'p' AND b = 'x' AND n > 1 AND n <= 3",
            "(a=? AND b=? AND n>? AND n<?)",
        ),
        (
            "a = 'p' AND b = 'x' AND n BETWEEN 2 AND 3",
            "(a=? AND b=? AND n>? AND n<?)",
        ),
        ("a = 'q' AND b > 'x'", "(a=? AND b>?)"),
        ("a = 'q' AND b < 'y'", "(a=? AND b<?)"),
    ] {
        check(
            &mut e,
            pred,
            &format!("SEARCH ct USING INDEX ct_ab {wording}"),
        );
    }
}

#[test]
fn text_range_on_one_column() {
    let (_db, mut e) = seeded("text-range");
    check(
        &mut e,
        "b >= 'x' AND b < 'y'",
        "SEARCH ct USING INDEX ct_b (b>? AND b<?)",
    );
    check(
        &mut e,
        "b > 'x' AND b <= 'y'",
        "SEARCH ct USING INDEX ct_b (b>? AND b<?)",
    );
}

#[test]
fn half_open_range_on_one_column() {
    let (_db, mut e) = seeded("half-open");
    check(
        &mut e,
        "n >= 1 AND n < 3",
        "SEARCH ct USING INDEX ct_n (n>? AND n<?)",
    );
    check(
        &mut e,
        "n > 1 AND n <= 2",
        "SEARCH ct USING INDEX ct_n (n>? AND n<?)",
    );
    check(
        &mut e,
        "3 > n AND 1 <= n",
        "SEARCH ct USING INDEX ct_n (n>? AND n<?)",
    );
}

#[test]
fn terms_the_index_does_not_serve_are_a_residual_filter() {
    let (_db, mut e) = seeded("residual");
    check(
        &mut e,
        "a = 'p' AND pad = 'z'",
        "SEARCH ct USING INDEX ct_ab (a=?)",
    );
    check(
        &mut e,
        "a = 'p' AND n = 2",
        "SEARCH ct USING INDEX ct_ab (a=?)",
    );
    check(
        &mut e,
        "n BETWEEN 1 AND 2 AND pad = 'z' AND id > 3",
        "SEARCH ct USING INDEX ct_n (n>? AND n<?)",
    );
    check(
        &mut e,
        "a = 'p' AND b = 'x' AND (n = 1 OR n = 3)",
        "SEARCH ct USING INDEX ct_ab (a=? AND b=?)",
    );
}

#[test]
fn a_one_sided_range_with_a_residual_keeps_its_scan() {
    let (_db, mut e) = seeded("one-sided");
    for pred in ["n > 1 AND pad = 'z'", "n < 3 AND pad = 'z'"] {
        assert_eq!(plan(&e, pred), "SCAN ct", "plan of {pred}");
        assert_eq!(ids(&mut e, "ct", pred), ids(&mut e, "cm", pred), "{pred}");
    }
}

#[test]
fn contradictory_and_null_terms_match_nothing_like_a_scan() {
    let (_db, mut e) = seeded("empty");
    for pred in [
        "a = 'p' AND a = 'q'",
        "a = 'p' AND b = NULL",
        "n > 3 AND n < 1",
        "n >= 2 AND n < 2",
    ] {
        assert_eq!(ids(&mut e, "ct", pred), ids(&mut e, "cm", pred), "{pred}");
        assert!(ids(&mut e, "ct", pred).is_empty(), "{pred}");
    }
}

#[test]
fn blob_literals_use_an_index() {
    let (_db, mut e) = seeded("blob");
    check(&mut e, "bl = X'0102'", "SEARCH ct USING INDEX ct_bl (bl=?)");
    check(&mut e, "bl > X'01'", "SEARCH ct USING INDEX ct_bl (bl>?)");
    check(&mut e, "bl < X'02'", "SEARCH ct USING INDEX ct_bl (bl<?)");
    check(
        &mut e,
        "bl BETWEEN X'01' AND X'0102'",
        "SEARCH ct USING INDEX ct_bl (bl>? AND bl<?)",
    );
    check(
        &mut e,
        "bl >= X'01' AND bl < X'02'",
        "SEARCH ct USING INDEX ct_bl (bl>? AND bl<?)",
    );
}

#[test]
fn update_and_delete_with_a_conjunction_seek_the_index() {
    let (_db, mut e) = seeded("write");
    e.run_query("UPDATE ct SET pad = 'u' WHERE a = 'p' AND b = 'x' AND n = 2")
        .unwrap();
    e.run_query("UPDATE cm SET pad = 'u' WHERE a = 'p' AND b = 'x' AND n = 2")
        .unwrap();
    assert_eq!(
        ids(&mut e, "ct", "pad = 'u'"),
        ids(&mut e, "cm", "pad = 'u'")
    );
    e.run_query("DELETE FROM ct WHERE n >= 2 AND n < 3 AND a = 'q'")
        .unwrap();
    e.run_query("DELETE FROM cm WHERE n >= 2 AND n < 3 AND a = 'q'")
        .unwrap();
    assert_eq!(ids(&mut e, "ct", "1 = 1"), ids(&mut e, "cm", "1 = 1"));
}
