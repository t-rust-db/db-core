// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Implicit `PRIMARY KEY`/`UNIQUE` indexes (`sqlite_autoindex_<t>_<n>`),
//! end to end through `engine::row::RowEngine` (#694): `CREATE TABLE`
//! allocates them, `INSERT`/`UPDATE`/`DELETE` maintain and enforce them,
//! and a file written by sqlite3 keeps its own indexes consistent. Where
//! the `sqlite3` CLI is installed it is the oracle for file validity
//! (`PRAGMA integrity_check`) and for the constraint messages; without
//! it those checks are skipped.
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
use std::process::Command;

use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine, ErrorKind};

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";
/// Written by sqlite3 3.53.4: `a(x TEXT PRIMARY KEY, y)`, `b(x, y, UNIQUE(x, y))`,
/// `c(n TEXT COLLATE NOCASE UNIQUE, m)` and `d(k TEXT PRIMARY KEY, v INT
/// UNIQUE)` (two implicit indexes), each with a row or two.
const SQLITE_FILE: &str = "tests/fixtures/btrees/autoindex.db";

struct TempDb(PathBuf);

impl TempDb {
    fn new(fixture: &str, label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-autoindex-{label}-{}-{}.db",
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

fn fresh(label: &str) -> (TempDb, RowEngine) {
    let db = TempDb::new(FIXTURE, label);
    let e = RowEngine::open(db.path()).expect("open");
    (db, e)
}

fn run(e: &mut RowEngine, sql: &str) {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"));
}

fn fails(e: &mut RowEngine, sql: &str) -> String {
    let err = e.run_query(sql).expect_err(sql);
    assert_eq!(err.kind, ErrorKind::Execute, "{sql}");
    err.message
}

fn ints(e: &mut RowEngine, sql: &str) -> Vec<i64> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
        .iter()
        .map(|r| match r[0] {
            Cell::Int(i) => i,
            ref other => panic!("{sql}: {other:?}"),
        })
        .collect()
}

/// sqlite3's verdict on the file, `None` when the CLI is not installed.
fn sqlite3(path: &Path, sql: &str) -> Option<String> {
    let out = Command::new("sqlite3").arg(path).arg(sql).output().ok()?;
    Some(
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .trim()
        .to_string(),
    )
}

fn assert_sqlite3_integrity_ok(path: &Path) {
    if let Some(verdict) = sqlite3(path, "PRAGMA integrity_check") {
        assert_eq!(verdict, "ok", "sqlite3 integrity_check of {path:?}");
    }
}

#[test]
fn create_table_writes_a_file_sqlite3_accepts() {
    for ddl in [
        "CREATE TABLE a (x TEXT PRIMARY KEY, y INT)",
        "CREATE TABLE a (x TEXT UNIQUE, y INT)",
        "CREATE TABLE a (x TEXT, y INT, PRIMARY KEY (x, y))",
        "CREATE TABLE a (x INTEGER PRIMARY KEY, y INT)",
        "CREATE TABLE a (x INTEGER PRIMARY KEY DESC, y INT)",
        "CREATE TABLE a (x TEXT PRIMARY KEY, y INT UNIQUE, z UNIQUE)",
    ] {
        let (db, mut e) = fresh("create");
        run(&mut e, ddl);
        drop(e);
        assert_sqlite3_integrity_ok(db.path());
    }
}

#[test]
fn create_table_lays_out_the_schema_as_sqlite3_does() {
    let (db, mut e) = fresh("layout");
    run(&mut e, "CREATE TABLE a (x TEXT PRIMARY KEY, y INT UNIQUE)");
    drop(e);
    let layout = |path: &Path| {
        sqlite3(
            path,
            "SELECT type, name, tbl_name, sql IS NULL FROM sqlite_schema \
             WHERE name NOT LIKE 'sqlite_sequence' AND name <> 't' AND tbl_name <> 't' \
             ORDER BY rowid",
        )
    };
    let Some(ours) = layout(db.path()) else {
        return;
    };
    assert_eq!(
        ours,
        "table|a|a|0\n\
         index|sqlite_autoindex_a_1|a|1\n\
         index|sqlite_autoindex_a_2|a|1"
    );
}

#[test]
fn a_duplicate_key_fails_with_sqlite3s_message() {
    let (db, mut e) = fresh("dup");
    run(
        &mut e,
        "CREATE TABLE a (x TEXT PRIMARY KEY, y INT, z UNIQUE); \
         CREATE TABLE d (p, q, UNIQUE(p, q)); \
         INSERT INTO a VALUES ('p', 1, 'u'); \
         INSERT INTO d VALUES (1, 2)",
    );
    assert!(fails(&mut e, "INSERT INTO a VALUES ('p', 2, 'v')")
        .contains("UNIQUE constraint failed: a.x"));
    assert!(fails(&mut e, "INSERT INTO a VALUES ('q', 2, 'u')")
        .contains("UNIQUE constraint failed: a.z"));
    assert!(
        fails(&mut e, "INSERT INTO d VALUES (1, 2)").contains("UNIQUE constraint failed: d.p, d.q")
    );
    run(&mut e, "INSERT INTO d VALUES (1, 3)");
    assert_eq!(ints(&mut e, "SELECT count(*) FROM a"), vec![1]);
    drop(e);
    assert_sqlite3_integrity_ok(db.path());
}

#[test]
fn null_keys_never_conflict() {
    let (db, mut e) = fresh("nulls");
    run(
        &mut e,
        "CREATE TABLE n (a TEXT UNIQUE, b, c, UNIQUE(b, c)); \
         INSERT INTO n VALUES (NULL, 1, NULL); \
         INSERT INTO n VALUES (NULL, 1, NULL); \
         INSERT INTO n VALUES ('x', NULL, 5); \
         INSERT INTO n VALUES ('y', NULL, 5)",
    );
    assert_eq!(ints(&mut e, "SELECT count(*) FROM n"), vec![4]);
    assert!(fails(&mut e, "INSERT INTO n VALUES ('x', 9, 9)").contains("n.a"));
    drop(e);
    assert_sqlite3_integrity_ok(db.path());
}

#[test]
fn update_and_delete_keep_the_index_consistent() {
    let (db, mut e) = fresh("update");
    run(
        &mut e,
        "CREATE TABLE a (x TEXT PRIMARY KEY, y INT); \
         INSERT INTO a VALUES ('p', 1); \
         INSERT INTO a VALUES ('q', 2); \
         INSERT INTO a VALUES ('r', 3)",
    );
    // Onto another row's key: refused, nothing changes.
    assert!(fails(&mut e, "UPDATE a SET x = 'p' WHERE x = 'q'")
        .contains("UNIQUE constraint failed: a.x"));
    assert_eq!(ints(&mut e, "SELECT y FROM a WHERE x = 'q'"), vec![2]);
    // Onto its own key, or an unrelated column: fine.
    run(&mut e, "UPDATE a SET x = x WHERE x = 'q'");
    run(&mut e, "UPDATE a SET y = y + 10");
    // A genuinely new key moves the entry.
    run(&mut e, "UPDATE a SET x = 'z' WHERE x = 'q'");
    assert!(fails(&mut e, "INSERT INTO a VALUES ('z', 0)").contains("a.x"));
    run(&mut e, "INSERT INTO a VALUES ('q', 0)");
    // A deleted key can be used again.
    run(&mut e, "DELETE FROM a WHERE x = 'p'");
    run(&mut e, "INSERT INTO a VALUES ('p', 99)");
    assert_eq!(ints(&mut e, "SELECT count(*) FROM a"), vec![4]);
    // The index answers the lookups.
    assert_eq!(ints(&mut e, "SELECT y FROM a WHERE x = 'p'"), vec![99]);
    // A statement that changes several rows to one key stops at the second.
    assert!(fails(&mut e, "UPDATE a SET x = 'same'").contains("a.x"));
    drop(e);
    assert_sqlite3_integrity_ok(db.path());
}

#[test]
fn update_or_ignore_skips_a_conflicting_row() {
    let (_db, mut e) = fresh("update-ignore");
    run(
        &mut e,
        "CREATE TABLE a (x TEXT UNIQUE, y INT); \
         INSERT INTO a VALUES ('p', 1); \
         INSERT INTO a VALUES ('q', 2)",
    );
    run(&mut e, "UPDATE OR IGNORE a SET x = 'p' WHERE x = 'q'");
    assert_eq!(ints(&mut e, "SELECT y FROM a WHERE x = 'q'"), vec![2]);
}

#[test]
fn update_or_replace_on_a_unique_index_is_refused_not_ignored() {
    let (_db, mut e) = fresh("update-replace");
    run(
        &mut e,
        "CREATE TABLE a (x TEXT UNIQUE, y INT); INSERT INTO a VALUES ('p', 1)",
    );
    let err = e
        .run_query("UPDATE OR REPLACE a SET x = 'p'")
        .expect_err("refused");
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("UPDATE OR REPLACE"), "{}", err.message);
}

#[test]
fn a_file_written_by_sqlite3_is_enforced_and_stays_consistent() {
    let db = TempDb::new(SQLITE_FILE, "from-sqlite3");
    let mut e = RowEngine::open(db.path()).expect("open");
    assert!(
        fails(&mut e, "INSERT INTO a VALUES ('p', 9)").contains("UNIQUE constraint failed: a.x")
    );
    assert!(
        fails(&mut e, "INSERT INTO b VALUES (1, 2)").contains("UNIQUE constraint failed: b.x, b.y")
    );
    assert!(fails(&mut e, "INSERT INTO d VALUES ('k1', 1)").contains("d.k"));
    assert!(fails(&mut e, "INSERT INTO d VALUES ('k9', 10)").contains("d.v"));
    // A NOCASE key: the index b-tree orders keys byte for byte, so
    // writing it would corrupt the index. Refused, and reads still work.
    for sql in [
        "INSERT INTO c VALUES ('ALICE', 2)",
        "UPDATE c SET n = 'x'",
        "DELETE FROM c",
    ] {
        let err = e.run_query(sql).expect_err(sql);
        assert_eq!(err.kind, ErrorKind::Compile, "{sql}");
        assert!(err.message.contains("non-BINARY"), "{sql}: {}", err.message);
    }
    assert_eq!(ints(&mut e, "SELECT m FROM c WHERE n = 'Alice'"), vec![1]);
    // An update that leaves the key alone does not touch the index.
    run(&mut e, "UPDATE c SET m = 3");
    assert_eq!(ints(&mut e, "SELECT m FROM c"), vec![3]);
    run(&mut e, "INSERT INTO a VALUES ('r', 3)");
    run(&mut e, "INSERT INTO b VALUES (2, 1)");
    run(&mut e, "INSERT INTO d VALUES ('k3', 30)");
    run(&mut e, "UPDATE d SET v = 21 WHERE k = 'k2'");
    run(&mut e, "DELETE FROM d WHERE k = 'k1'");
    run(&mut e, "INSERT INTO d VALUES ('k1', 10)");
    assert!(fails(&mut e, "UPDATE d SET v = 21 WHERE k = 'k3'").contains("d.v"));
    drop(e);
    assert_sqlite3_integrity_ok(db.path());
    // sqlite3 reads back what we wrote through the indexes.
    if let Some(rows) = sqlite3(db.path(), "SELECT count(*) FROM d WHERE v = 21") {
        assert_eq!(rows, "1");
    }
}

#[test]
fn drop_table_frees_the_implicit_indexes() {
    let db = TempDb::new(SQLITE_FILE, "drop");
    let mut e = RowEngine::open(db.path()).expect("open");
    run(&mut e, "DROP TABLE d");
    run(&mut e, "CREATE TABLE d (k TEXT PRIMARY KEY, v INT UNIQUE)");
    run(&mut e, "INSERT INTO d VALUES ('k1', 10)");
    assert!(fails(&mut e, "INSERT INTO d VALUES ('k1', 11)").contains("d.k"));
    drop(e);
    assert_sqlite3_integrity_ok(db.path());
    if let Some(rows) = sqlite3(
        db.path(),
        "SELECT count(*) FROM sqlite_schema WHERE name LIKE 'sqlite_autoindex_d_%'",
    ) {
        assert_eq!(rows, "2");
    }
}

#[test]
fn an_implicit_index_cannot_be_dropped() {
    let db = TempDb::new(SQLITE_FILE, "drop-index");
    let mut e = RowEngine::open(db.path()).expect("open");
    let err = e
        .run_query("DROP INDEX sqlite_autoindex_a_1")
        .expect_err("refused");
    assert!(
        err.message
            .contains("index associated with UNIQUE or PRIMARY KEY constraint cannot be dropped"),
        "{}",
        err.message
    );
}
