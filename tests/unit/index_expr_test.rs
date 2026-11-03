// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Expression index key columns (#945): db-core builds and maintains them
//! like sqlite3. After every statement sqlite3 checks
//! the file db-core wrote (where the CLI is installed) and so does
//! db-core; query results must agree with a copy sqlite3 itself edited.
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
use db_core::engine::{Cell, Engine};

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

static NEXT: AtomicU32 = AtomicU32::new(0);

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-idx-expr-{label}-{}-{}.db",
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

fn sqlite3(path: &Path, sql: &str) -> Option<Vec<String>> {
    let out = Command::new("sqlite3").arg(path).arg(sql).output().ok()?;
    assert!(
        out.status.success(),
        "sqlite3 failed on {sql}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

fn rows(e: &mut RowEngine, sql: &str) -> Vec<String> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| match c {
                    Cell::Null => String::new(),
                    Cell::Int(i) => i.to_string(),
                    Cell::Text(s) => s.clone(),
                    other => panic!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// Runs `steps` on db-core and on sqlite3 (each on its own copy of a
/// database built by `setup`), reopening db-core after every statement so
/// sqlite3 can read the file it wrote; `probes` must answer the same on
/// both after each step.
fn run_steps(setup: &str, steps: &[&str], probes: &[&str]) {
    let ours = TempDb::new("ours");
    let theirs = TempDb::new("theirs");
    {
        let mut e = RowEngine::open(&ours.0).unwrap();
        e.run_query("DROP TABLE t").unwrap();
        e.run_query(setup).unwrap();
    }
    sqlite3(&theirs.0, "DROP TABLE t");
    sqlite3(&theirs.0, setup);
    let check = |label: &str| {
        let mut e = RowEngine::open(&ours.0).unwrap();
        assert_eq!(rows(&mut e, "PRAGMA integrity_check"), ["ok"], "{label}");
        if let Some(verdict) = sqlite3(&ours.0, "PRAGMA integrity_check") {
            assert_eq!(verdict, ["ok"], "sqlite3 on db-core's file: {label}");
        }
        for probe in probes {
            if let Some(expected) = sqlite3(&theirs.0, probe) {
                assert_eq!(rows(&mut e, probe), expected, "{probe} {label}");
            }
        }
    };
    check("after setup");
    for step in steps {
        {
            let mut e = RowEngine::open(&ours.0).unwrap();
            e.run_query(step)
                .unwrap_or_else(|err| panic!("{step}: {err}"));
        }
        sqlite3(&theirs.0, step);
        check(step);
    }
}

fn fill(count: u32) -> String {
    (0..count)
        .map(|i| {
            let key = (i * 7919) % count;
            let pad = "X".repeat((i % 5) as usize);
            format!(
                "INSERT INTO t VALUES ({i}, 'k{key:05}{pad}', {});",
                key % 17
            )
        })
        .collect()
}

const PROBES: &[&str] = &[
    "SELECT count(*), sum(c) FROM t",
    "SELECT id FROM t WHERE lower(a) = 'k00042xx' ORDER BY id",
    "SELECT id FROM t WHERE c + 1 = 5 ORDER BY id",
    "SELECT id FROM t WHERE lower(a) >= 'k00300' AND lower(a) < 'k00320' ORDER BY id",
    "SELECT id FROM t WHERE a || c = 'k0000010' ORDER BY id",
    "SELECT id FROM t ORDER BY lower(a) DESC, id LIMIT 7",
    "SELECT id FROM t ORDER BY c * 2, id LIMIT 9",
];

#[test]
fn expression_indexes_are_built_and_maintained_like_sqlite3() {
    let setup = format!(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, c INT); \
         CREATE INDEX t_l ON t(lower(a)); CREATE INDEX t_c ON t(c + 1, a || c DESC); {}",
        fill(1200)
    );
    run_steps(
        &setup,
        &[
            "INSERT INTO t VALUES (5000, 'K00042xx', 4), (5001, 'ZZZ', 99), (5002, '', -5)",
            "UPDATE t SET a = 'K9' || a WHERE id % 4 = 0",
            "UPDATE t SET c = c + 1 WHERE id % 3 = 0",
            "DELETE FROM t WHERE id % 5 = 0",
            "DELETE FROM t WHERE c > 10",
            "DELETE FROM t",
            "INSERT INTO t VALUES (1, 'again', 1)",
        ],
        PROBES,
    );
}

#[test]
fn an_expression_index_built_over_existing_rows_and_a_unique_one() {
    run_steps(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, c INT);",
        &[
            &fill(600),
            "CREATE INDEX t_l ON t(lower(a))",
            "CREATE UNIQUE INDEX t_u ON t(id * 3, c)",
            "INSERT INTO t VALUES (9000, 'late', 4)",
            "DELETE FROM t WHERE c = 4",
        ],
        PROBES,
    );
}

#[test]
fn a_unique_expression_index_rejects_duplicates() {
    let ours = TempDb::new("unique");
    let mut e = RowEngine::open(&ours.0).unwrap();
    e.run_query("DROP TABLE t").unwrap();
    e.run_query(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT); CREATE UNIQUE INDEX t_a ON t(lower(a)); \
         INSERT INTO t VALUES (1, 'x'), (2, 'y')",
    )
    .unwrap();
    assert!(e.run_query("INSERT INTO t VALUES (3, 'X')").is_err());
    assert!(e.run_query("UPDATE t SET a = 'Y' WHERE id = 1").is_err());
    assert_eq!(rows(&mut e, "SELECT count(*) FROM t"), ["2"]);
    assert_eq!(rows(&mut e, "PRAGMA integrity_check"), ["ok"]);
}

/// The planner serves `WHERE <index expression> = ?` / `BETWEEN` from the
/// index, worded as sqlite3 words it (`<expr>`), and a different
/// expression does not match.
#[test]
fn the_planner_uses_an_expression_index_when_the_expression_matches() {
    let ours = TempDb::new("plan");
    let mut e = RowEngine::open(&ours.0).unwrap();
    e.run_query("DROP TABLE t").unwrap();
    e.run_query(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, c INT); CREATE INDEX t_l ON t(lower(a)); \
         INSERT INTO t VALUES (1, 'Ab', 1), (2, 'aB', 2), (3, 'cd', 3), (4, 'AB', 4)",
    )
    .unwrap();
    let plan = |e: &RowEngine, sql: &str| -> String {
        e.explain_plan(sql)
            .unwrap()
            .iter()
            .map(|r| r.detail.clone())
            .collect::<Vec<_>>()
            .join(" | ")
    };
    assert_eq!(
        plan(&e, "SELECT * FROM t WHERE lower(a) = 'ab'"),
        "SEARCH t USING INDEX t_l (<expr>=?)"
    );
    assert_eq!(
        plan(&e, "SELECT * FROM t WHERE 'ab' = lower( a )"),
        "SEARCH t USING INDEX t_l (<expr>=?)"
    );
    assert_eq!(plan(&e, "SELECT * FROM t WHERE upper(a) = 'AB'"), "SCAN t");
    assert_eq!(
        rows(&mut e, "SELECT id FROM t WHERE lower(a) = 'ab' ORDER BY id"),
        ["1", "2", "4"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM t WHERE lower(a) BETWEEN 'ab' AND 'b' ORDER BY id"
        ),
        ["1", "2", "4"]
    );
}

/// `CREATE UNIQUE INDEX` over rows that already collide fails, for a plain
/// column as for an expression, and leaves no half-built index behind;
/// sqlite3's other index-expression rules are refused too.
#[test]
fn create_unique_index_checks_existing_rows_and_expression_rules_hold() {
    let ours = TempDb::new("create-unique");
    let mut e = RowEngine::open(&ours.0).unwrap();
    e.run_query("DROP TABLE t").unwrap();
    e.run_query(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT); \
         INSERT INTO t VALUES (1, 'x'), (2, 'X'), (3, NULL), (4, NULL)",
    )
    .unwrap();
    assert!(e
        .run_query("CREATE UNIQUE INDEX u1 ON t(lower(a))")
        .is_err());
    assert!(e
        .run_query("CREATE UNIQUE INDEX u2 ON t(a COLLATE BINARY, id / id)")
        .is_ok());
    e.run_query("INSERT INTO t VALUES (5, 'y')").unwrap();
    assert!(e.run_query("CREATE UNIQUE INDEX u3 ON t(id % 1)").is_err());
    for bad in [
        "CREATE INDEX b1 ON t(random())",
        "CREATE INDEX b2 ON t(a || ?)",
        "CREATE INDEX b3 ON t((SELECT 1))",
    ] {
        assert!(e.run_query(bad).is_err(), "{bad}");
    }
    // The failed statements left no index behind: their names are free.
    for name in ["u1", "u3", "b1", "b2", "b3"] {
        e.run_query(&format!("CREATE INDEX {name} ON t(id)"))
            .unwrap();
    }
    assert_eq!(rows(&mut e, "PRAGMA integrity_check"), ["ok"]);
    drop(e);
    if let Some(verdict) = sqlite3(&ours.0, "PRAGMA integrity_check") {
        assert_eq!(verdict, ["ok"]);
    }
}
