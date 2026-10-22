// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `INSERT`, `UPDATE` and `DELETE` on `WITHOUT ROWID` tables (#722), end to
//! end through `engine::row::RowEngine`. Every scenario runs on db-core and,
//! where the `sqlite3` CLI is installed, the same statements run on sqlite3
//! over its own copy of the file: the tables they leave must agree, sqlite3
//! must read (and accept the integrity of) the file db-core wrote, and a
//! statement that fails must fail in both. `CREATE TABLE ... WITHOUT ROWID`
//! is still refused, so the tables come from a file sqlite3 wrote.
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
use std::sync::atomic::{AtomicU32, Ordering};

use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine, ErrorKind};

/// Written by sqlite3 3.53.4. `w1(a, b, c, PRIMARY KEY (c, a))`, 3000 rows;
/// `w2(k INTEGER PRIMARY KEY, v)`, 300 rows (`k` = 3, 6, ...); `w3(a, b,
/// PRIMARY KEY (a, b))`, 200 rows; `w4` with secondary indexes; a rowid
/// table `r(id, k)`, 60 rows; `w5(id PRIMARY KEY, n NOT NULL DEFAULT 5, c
/// CHECK (c > 0), t)`, 3 rows; and two tables whose key this crate cannot
/// write yet, `w6` (`DESC`) and `w7` (`COLLATE NOCASE`).
const FIXTURE: &str = "tests/fixtures/btrees/without_rowid_tables.db";

static NEXT: AtomicU32 = AtomicU32::new(0);

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-wr-write-{label}-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::copy(FIXTURE, &path).expect("copy fixture");
        TempDb(path)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
        std::fs::remove_file(format!("{}-journal", self.0.display())).ok();
    }
}

fn cell(c: &Cell) -> String {
    match c {
        Cell::Null => String::new(),
        Cell::Int(i) => i.to_string(),
        Cell::Text(s) => s.clone(),
        other => panic!("{other:?}"),
    }
}

fn sqlite3(path: &Path, sql: &str) -> Option<Result<Vec<String>, String>> {
    let out = Command::new("sqlite3").arg(path).arg(sql).output().ok()?;
    if out.status.success() {
        Some(Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()))
    } else {
        Some(Err(String::from_utf8_lossy(&out.stderr).to_string()))
    }
}

/// db-core and sqlite3, each on its own copy of the fixture.
struct Pair {
    ours_file: TempDb,
    engine: Option<RowEngine>,
    theirs_file: TempDb,
}

impl Pair {
    fn new(label: &str) -> Self {
        let ours_file = TempDb::new(label);
        let engine = RowEngine::open(&ours_file.0).expect("open");
        Pair {
            ours_file,
            engine: Some(engine),
            theirs_file: TempDb::new(&format!("{label}-oracle")),
        }
    }

    fn engine(&mut self) -> &mut RowEngine {
        self.engine.as_mut().expect("engine is open")
    }

    fn ours(&mut self, sql: &str) -> Vec<String> {
        self.engine()
            .run_query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"))
            .rows
            .iter()
            .map(|row| row.iter().map(cell).collect::<Vec<_>>().join("|"))
            .collect()
    }

    /// `sql` succeeds in both.
    fn exec(&mut self, sql: &str) {
        self.ours(sql);
        if let Some(result) = sqlite3(&self.theirs_file.0, sql) {
            result.unwrap_or_else(|e| panic!("sqlite3 rejected {sql}: {e}"));
        }
    }

    /// `query` returns the same rows in both.
    fn agree(&mut self, query: &str) -> Vec<String> {
        let rows = self.ours(query);
        if let Some(result) = sqlite3(&self.theirs_file.0, query) {
            assert_eq!(rows, result.unwrap(), "{query}");
        }
        rows
    }

    /// `sql` fails in both, with a message containing `message`.
    fn fails(&mut self, sql: &str, message: &str) {
        let err = self.engine().run_query(sql).expect_err(sql);
        assert_eq!(err.kind, ErrorKind::Execute, "{sql}: {}", err.message);
        assert!(err.message.contains(message), "{sql}: {}", err.message);
        if let Some(result) = sqlite3(&self.theirs_file.0, sql) {
            let theirs = result.expect_err(sql);
            assert!(theirs.contains(message), "{sql}: {theirs}");
        }
    }

    /// `sql` is refused by db-core before it runs.
    fn refused(&mut self, sql: &str, message: &str) {
        let err = self.engine().run_query(sql).expect_err(sql);
        assert_eq!(err.kind, ErrorKind::Compile, "{sql}: {}", err.message);
        assert!(err.message.contains(message), "{sql}: {}", err.message);
    }

    /// The tables agree, both integrity checks pass, and sqlite3 reads the
    /// file db-core wrote the way it reads its own.
    fn finish(mut self) {
        for table in ["w1", "w2", "w3", "w5"] {
            self.agree(&format!("SELECT count(*) FROM {table}"));
        }
        assert_eq!(self.ours("PRAGMA integrity_check"), ["ok"]);
        drop(self.engine.take());
        let dump = "SELECT group_concat(a || ',' || b || ',' || c, ';') FROM (SELECT * FROM w1 ORDER BY c, a); \
                    SELECT group_concat(k || ',' || v, ';') FROM (SELECT * FROM w2 ORDER BY k); \
                    SELECT group_concat(a || ',' || b, ';') FROM (SELECT * FROM w3 ORDER BY a, b); \
                    SELECT group_concat(id || ',' || n || ',' || ifnull(c, '') || ',' || ifnull(t, ''), ';') FROM (SELECT * FROM w5 ORDER BY id)";
        if let (Some(ours), Some(theirs)) = (
            sqlite3(&self.ours_file.0, "PRAGMA integrity_check"),
            sqlite3(&self.theirs_file.0, "PRAGMA integrity_check"),
        ) {
            assert_eq!(ours.unwrap(), ["ok"], "sqlite3 on db-core's file");
            assert_eq!(theirs.unwrap(), ["ok"]);
        }
        if let (Some(ours), Some(theirs)) = (
            sqlite3(&self.ours_file.0, dump),
            sqlite3(&self.theirs_file.0, dump),
        ) {
            assert_eq!(ours.unwrap(), theirs.unwrap(), "table contents differ");
        }
    }
}

#[test]
fn insert_into_every_key_layout() {
    let mut p = Pair::new("insert-layouts");
    // The key is not the leading columns (c, a), is an INTEGER PRIMARY KEY,
    // or covers every column.
    p.exec("INSERT INTO w1 VALUES ('new1', 1, 'cx'), ('new2', 2, 'cx'), ('new1', 3, 'cy')");
    p.exec("INSERT INTO w2 VALUES (4, 'four'), (1, 'one'), (2000, 'big')");
    p.exec("INSERT INTO w3 VALUES (50, 1), (50, 2), (-1, 7)");
    p.exec("INSERT INTO w5(id, c) VALUES (4, 4)");
    p.exec("INSERT INTO w1(c, a, b) VALUES ('cz', 'reordered', 9)");
    assert_eq!(
        p.agree("SELECT a, b, c FROM w1 WHERE c IN ('cx', 'cy', 'cz') ORDER BY c, a"),
        ["new1|1|cx", "new2|2|cx", "new1|3|cy", "reordered|9|cz"]
    );
    assert_eq!(
        p.agree("SELECT k, v FROM w2 WHERE k < 7 OR k > 900 ORDER BY k"),
        ["1|one", "3|v1", "4|four", "6|v2", "2000|big"]
    );
    p.agree("SELECT a, b FROM w3 WHERE a < 0 OR a = 50 ORDER BY a, b");
    // The DEFAULT fills the column the statement leaves out.
    assert_eq!(
        p.agree("SELECT id, n, ifnull(c, 'null'), ifnull(t, 'null') FROM w5 WHERE id = 4"),
        ["4|5|4|null"]
    );
    p.finish();
}

#[test]
fn a_repeated_key_fails_and_changes_nothing() {
    let mut p = Pair::new("duplicate");
    p.fails(
        "INSERT INTO w1 VALUES ('a5', 99, 'c5')",
        "UNIQUE constraint failed: w1.c, w1.a",
    );
    p.fails(
        "INSERT INTO w2 VALUES (9, 'dup')",
        "UNIQUE constraint failed: w2.k",
    );
    p.fails(
        "INSERT INTO w3 VALUES (3, 3)",
        "UNIQUE constraint failed: w3.a, w3.b",
    );
    // The statement fails as a whole: its first row is not left behind.
    p.fails(
        "INSERT INTO w2 VALUES (1, 'first'), (9, 'dup')",
        "UNIQUE constraint failed: w2.k",
    );
    assert_eq!(p.agree("SELECT count(*) FROM w2 WHERE k < 5"), ["1"]);
    assert_eq!(p.agree("SELECT b FROM w1 WHERE a = 'a5'"), ["5"]);
    p.finish();
}

#[test]
fn a_null_in_a_key_column_is_refused() {
    let mut p = Pair::new("null-key");
    p.fails(
        "INSERT INTO w1 VALUES ('x', 1, NULL)",
        "NOT NULL constraint failed: w1.c",
    );
    p.fails(
        "INSERT INTO w1 VALUES (NULL, 1, 'c')",
        "NOT NULL constraint failed: w1.a",
    );
    // An INTEGER PRIMARY KEY is no rowid alias here: NULL is not replaced
    // by the next number.
    p.fails(
        "INSERT INTO w2 VALUES (NULL, 'x')",
        "NOT NULL constraint failed: w2.k",
    );
    p.fails(
        "INSERT INTO w3 VALUES (1, NULL)",
        "NOT NULL constraint failed: w3.b",
    );
    p.fails(
        "INSERT INTO w5(id) VALUES (NULL)",
        "NOT NULL constraint failed: w5.id",
    );
    // A non-key column keeps its own NOT NULL and CHECK.
    p.fails(
        "INSERT INTO w5 VALUES (9, NULL, 1, 'x')",
        "NOT NULL constraint failed: w5.n",
    );
    p.fails(
        "INSERT INTO w5 VALUES (9, 1, 0, 'x')",
        "CHECK constraint failed",
    );
    p.finish();
}

#[test]
fn insert_conflict_clauses() {
    let mut p = Pair::new("insert-clauses");
    p.exec("INSERT OR IGNORE INTO w1 VALUES ('a5', 555, 'c5'), ('fresh', 1, 'cq')");
    assert_eq!(p.agree("SELECT b FROM w1 WHERE a = 'a5'"), ["5"]);
    assert_eq!(p.agree("SELECT b FROM w1 WHERE a = 'fresh'"), ["1"]);
    // REPLACE removes the row that has the key, then adds the new one.
    p.exec("INSERT OR REPLACE INTO w1 VALUES ('a5', 555, 'c5'), ('a6', 666, 'c6')");
    assert_eq!(
        p.agree("SELECT b FROM w1 WHERE a IN ('a5', 'a6') ORDER BY a"),
        ["555", "666"]
    );
    assert_eq!(p.agree("SELECT count(*) FROM w1"), ["3001"]);
    p.exec("INSERT OR REPLACE INTO w3 VALUES (3, 3)");
    assert_eq!(p.agree("SELECT count(*) FROM w3"), ["200"]);
    p.fails(
        "INSERT OR ABORT INTO w2 VALUES (3, 'x')",
        "UNIQUE constraint failed: w2.k",
    );
    p.fails(
        "INSERT OR FAIL INTO w2 VALUES (3, 'x')",
        "UNIQUE constraint failed: w2.k",
    );
    p.finish();
}

#[test]
fn the_key_is_probed_with_the_values_column_affinity_gives_it() {
    let mut p = Pair::new("affinity");
    // INTEGER affinity turns '7' into 7 and 12.0 into 12 before the key is
    // looked for, so these meet the rows that are already there.
    p.exec("INSERT INTO w2 VALUES ('7', 'seven')");
    assert_eq!(p.agree("SELECT typeof(k) FROM w2 WHERE k = 7"), ["integer"]);
    p.fails(
        "INSERT INTO w2 VALUES ('9', 'dup')",
        "UNIQUE constraint failed: w2.k",
    );
    p.exec("INSERT OR REPLACE INTO w2 VALUES ('12.0', 'twelve')");
    assert_eq!(p.agree("SELECT v FROM w2 WHERE k = 12"), ["twelve"]);
    assert_eq!(p.agree("SELECT count(*) FROM w2"), ["301"]);
    p.fails(
        "INSERT INTO w3 VALUES ('3', '3')",
        "UNIQUE constraint failed: w3.a, w3.b",
    );
    p.finish();
}

#[test]
fn insert_from_a_select() {
    let mut p = Pair::new("insert-select");
    // Row 5 of `r` is already a key of `w3`.
    p.exec("INSERT OR IGNORE INTO w3 SELECT id, k FROM r");
    p.agree("SELECT count(*), sum(a), sum(b) FROM w3");
    p.exec("INSERT OR IGNORE INTO w2 SELECT id, g FROM w4 WHERE id <= 12");
    assert_eq!(
        p.agree("SELECT k, v FROM w2 WHERE k <= 12 ORDER BY k"),
        [
            "1|g1", "2|g2", "3|v1", "4|g4", "5|g5", "6|v2", "7|g7", "8|g8", "9|v3", "10|g10",
            "11|g11", "12|v4"
        ]
    );
    p.fails(
        "INSERT INTO w3 SELECT a, b FROM r JOIN w3 ON w3.a = r.id",
        "UNIQUE constraint failed",
    );
    p.finish();
    // A source that is the table being written is refused outright.
    let mut p = Pair::new("insert-select-self");
    p.refused("INSERT INTO w3 SELECT a + 1000, b FROM w3", "into itself");
}

#[test]
fn many_rows_split_the_b_tree() {
    let mut p = Pair::new("splits");
    let script: String = (0..1500)
        .map(|i| format!("INSERT INTO w1 VALUES ('z{i}', {i}, 'cz{}');", i % 37))
        .collect();
    p.exec(&script);
    p.agree("SELECT count(*), sum(b), min(a), max(a), min(c), max(c) FROM w1");
    p.agree("SELECT c, count(*) FROM w1 GROUP BY c ORDER BY c LIMIT 12");
    p.agree("SELECT a, b FROM w1 WHERE c = 'cz5' ORDER BY a");
    p.finish();
}

#[test]
fn delete_rows_by_key_by_other_columns_and_all() {
    let mut p = Pair::new("delete");
    p.exec("DELETE FROM w1 WHERE c = 'c5'");
    assert_eq!(p.agree("SELECT count(*) FROM w1"), ["2970"]);
    p.exec("DELETE FROM w1 WHERE b % 7 = 0");
    p.agree("SELECT count(*), sum(b), min(a), max(a) FROM w1");
    p.exec("DELETE FROM w2 WHERE k IN (SELECT k FROM r)");
    p.agree("SELECT count(*), sum(k) FROM w2");
    p.exec("DELETE FROM w2 WHERE k > 100 AND v LIKE 'v1%'");
    p.agree("SELECT count(*), sum(k) FROM w2");
    p.exec("DELETE FROM w3");
    assert_eq!(p.agree("SELECT count(*) FROM w3"), ["0"]);
    // The emptied tree takes rows again.
    p.exec("INSERT INTO w3 VALUES (1, 1), (0, 5)");
    assert_eq!(p.agree("SELECT a, b FROM w3 ORDER BY a, b"), ["0|5", "1|1"]);
    p.exec("DELETE FROM w1 WHERE c = 'nope'");
    p.exec("DELETE FROM w5 WHERE id = 2");
    assert_eq!(p.agree("SELECT id FROM w5 ORDER BY id"), ["1", "3"]);
    p.finish();
}

#[test]
fn delete_every_row_of_a_multi_level_table() {
    let mut p = Pair::new("delete-all");
    p.exec("DELETE FROM w1");
    assert_eq!(p.agree("SELECT count(*) FROM w1"), ["0"]);
    p.exec("INSERT INTO w1 VALUES ('again', 1, 'c')");
    assert_eq!(p.agree("SELECT a, b, c FROM w1"), ["again|1|c"]);
    p.finish();
}

#[test]
fn update_columns_outside_the_key() {
    let mut p = Pair::new("update-non-key");
    p.exec("UPDATE w1 SET b = b + 1000 WHERE c = 'c7'");
    p.agree("SELECT count(*), sum(b) FROM w1 WHERE c = 'c7'");
    p.exec("UPDATE w1 SET b = length(a) + b");
    p.agree("SELECT sum(b) FROM w1");
    p.exec("UPDATE w2 SET v = v || '!' WHERE k % 2 = 0");
    p.agree("SELECT count(*), group_concat(v) FROM (SELECT v FROM w2 WHERE v LIKE '%!' ORDER BY k LIMIT 6)");
    p.exec("UPDATE w3 SET b = b WHERE a = 4");
    p.exec("UPDATE w5 SET t = upper(t), n = n * 2");
    assert_eq!(
        p.agree("SELECT id, n, t FROM w5 ORDER BY id"),
        ["1|20|ONE", "2|40|TWO", "3|60|THREE"]
    );
    p.finish();
}

#[test]
fn update_the_key() {
    let mut p = Pair::new("update-key");
    // Every key moves, into room that is free.
    p.exec("UPDATE w3 SET b = b + 1000");
    p.agree("SELECT count(*), min(b), max(b), sum(a) FROM w3");
    p.exec("UPDATE w3 SET a = a + 100 WHERE b <= 1020");
    p.agree("SELECT a, b FROM w3 WHERE b <= 1030 ORDER BY b");
    p.exec("UPDATE w2 SET k = k + 1 WHERE k <= 30");
    assert_eq!(
        p.agree("SELECT k, v FROM w2 WHERE k <= 31 ORDER BY k"),
        ["4|v1", "7|v2", "10|v3", "13|v4", "16|v5", "19|v6", "22|v7", "25|v8", "28|v9", "31|v10"]
    );
    // A key set to what it already is does not collide with itself.
    p.exec("UPDATE w2 SET k = k WHERE k < 40");
    p.exec("UPDATE w1 SET c = c, a = a WHERE b < 50");
    // A composite key, one column of it.
    p.exec("UPDATE w1 SET c = 'moved' WHERE a = 'a10'");
    assert_eq!(
        p.agree("SELECT a, b, c FROM w1 WHERE a = 'a10'"),
        ["a10|10|moved"]
    );
    // The new key has the column's affinity before it is looked for.
    p.exec("UPDATE w2 SET k = '5000' WHERE k = 300");
    assert_eq!(
        p.agree("SELECT typeof(k), v FROM w2 WHERE k = 5000"),
        ["integer|v100"]
    );
    p.finish();
}

#[test]
fn update_to_a_key_another_row_has() {
    let mut p = Pair::new("update-conflict");
    p.fails(
        "UPDATE w2 SET k = 6 WHERE k = 3",
        "UNIQUE constraint failed: w2.k",
    );
    p.fails(
        "UPDATE w1 SET a = 'a6', c = 'c6' WHERE a = 'a5'",
        "UNIQUE constraint failed: w1.c, w1.a",
    );
    assert_eq!(
        p.agree("SELECT v FROM w2 WHERE k IN (3, 6) ORDER BY k"),
        ["v1", "v2"]
    );
    // OR IGNORE leaves the row; OR REPLACE removes the one in the way.
    p.exec("UPDATE OR IGNORE w2 SET k = 6 WHERE k = 3");
    assert_eq!(
        p.agree("SELECT k, v FROM w2 WHERE k <= 6 ORDER BY k"),
        ["3|v1", "6|v2"]
    );
    p.exec("UPDATE OR REPLACE w2 SET k = 6 WHERE k = 3");
    assert_eq!(
        p.agree("SELECT k, v FROM w2 WHERE k <= 6 ORDER BY k"),
        ["6|v1"]
    );
    assert_eq!(p.agree("SELECT count(*) FROM w2"), ["299"]);
    p.finish();
}

#[test]
fn update_keeps_the_constraints_of_the_table() {
    let mut p = Pair::new("update-constraints");
    p.fails(
        "UPDATE w5 SET c = 0 WHERE id = 1",
        "CHECK constraint failed",
    );
    p.fails("UPDATE w5 SET n = NULL", "NOT NULL constraint failed: w5.n");
    p.fails(
        "UPDATE w5 SET id = NULL WHERE id = 1",
        "NOT NULL constraint failed: w5.id",
    );
    p.fails(
        "UPDATE w1 SET a = NULL WHERE b = 1",
        "NOT NULL constraint failed: w1.a",
    );
    assert_eq!(
        p.agree("SELECT id, n, c FROM w5 ORDER BY id"),
        ["1|10|1", "2|20|2", "3|30|3"]
    );
    p.finish();
}

#[test]
fn what_cannot_be_written_yet_is_refused() {
    let mut p = Pair::new("refused");
    // A secondary index would have to be kept in step.
    for sql in [
        "INSERT INTO w4 VALUES (1000, 'g', 1)",
        "UPDATE w4 SET n = n + 1",
        "DELETE FROM w4 WHERE id = 1",
    ] {
        p.refused(sql, "secondary index");
    }
    // A key this b-tree cannot order the way sqlite3 does.
    p.refused("INSERT INTO w6 VALUES ('a', 1)", "descending");
    p.refused("INSERT INTO w7 VALUES ('a', 1)", "non-BINARY");
    p.refused("DELETE FROM w6", "descending");
    // Upsert follows.
    p.refused(
        "INSERT INTO w1 VALUES ('a5', 1, 'c5') ON CONFLICT DO NOTHING",
        "ON CONFLICT",
    );
    // Reads of those tables are as they were, and the table that is not
    // WITHOUT ROWID is untouched.
    p.agree("SELECT count(*) FROM w4");
    p.exec("INSERT INTO r VALUES (1000, 1)");
    p.finish();
}
