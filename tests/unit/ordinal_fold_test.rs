// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The integer a GROUP BY / ORDER BY term folds to (#954), end to end through
//! `engine::row::RowEngine`. Each script also runs through the `sqlite3`
//! CLI when it is installed, and the tables it leaves behind must agree;
//! without the CLI only the expectations written here are checked.
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

struct TempDb(PathBuf);

/// Tells apart temp files made in the same clock tick.
static NEXT: AtomicU32 = AtomicU32::new(0);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-ordinal-fold-{label}-{}-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
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

fn cell(c: &Cell) -> String {
    match c {
        Cell::Null => String::new(),
        Cell::Int(i) => i.to_string(),
        Cell::Text(s) => s.clone(),
        other => panic!("{other:?}"),
    }
}

/// `(term, GROUP BY outcome, ORDER BY outcome)`, as sqlite3 3.54.1 answers
/// them over `t(a, b)` = (3,2), (1,3), (2,1):
/// - GROUP BY: `SELECT count(*) FROM (SELECT a, count(*) FROM t GROUP BY <term>)`;
/// - ORDER BY: `SELECT a || b FROM t ORDER BY <term>`, joined by commas.
///
/// `ERR` is any error. A term sqlite3 reads as column 1 groups into 3 and
/// sorts by `a` (13,21,32); a plain constant groups into 1 and keeps the
/// scan order (32,13,21).
const TERMS: &[(&str, &str, &str)] = &[
    ("1", "3", "13,21,32"),
    ("2", "ERR", "ERR"),
    ("3", "ERR", "ERR"),
    ("0", "ERR", "ERR"),
    ("+1", "3", "13,21,32"),
    ("+2", "ERR", "ERR"),
    ("- 1", "ERR", "ERR"),
    ("-0", "ERR", "ERR"),
    ("(1)", "3", "13,21,32"),
    ("1 IS NOT NULL", "3", "13,21,32"),
    ("2 IS NOT NULL", "3", "13,21,32"),
    ("'a' IS NOT NULL", "3", "13,21,32"),
    ("1 NOTNULL", "3", "13,21,32"),
    ("1 IS NULL", "ERR", "ERR"),
    ("1.5 IS NULL", "ERR", "ERR"),
    ("x'31' IS NULL", "ERR", "ERR"),
    ("1 ISNULL", "ERR", "ERR"),
    ("1 IS NULL IS NULL", "ERR", "ERR"),
    ("(1 IS NOT NULL) IS NULL", "ERR", "ERR"),
    ("0 AND a", "ERR", "ERR"),
    ("a AND 0", "ERR", "ERR"),
    ("NULL AND 0", "ERR", "ERR"),
    ("0 AND 0", "ERR", "ERR"),
    ("(0) AND a", "ERR", "ERR"),
    ("0x0 AND a", "ERR", "ERR"),
    ("(1 IS NULL) AND a", "ERR", "ERR"),
    ("1 AND a", "1", "32,13,21"),
    ("1 AND 1", "1", "32,13,21"),
    ("+0 AND a", "1", "32,13,21"),
    ("-0 AND a", "1", "32,13,21"),
    ("NULL IS NULL", "1", "32,13,21"),
    ("TRUE IS NULL", "1", "32,13,21"),
    ("2147483647", "ERR", "ERR"),
    ("2147483648", "1", "32,13,21"),
    ("-2147483648", "1", "32,13,21"),
    ("0 COLLATE NOCASE AND a", "1", "32,13,21"),
    ("1 COLLATE NOCASE", "3", "13,21,32"),
    ("1+1", "1", "32,13,21"),
    ("NOT 1", "1", "32,13,21"),
    ("1 = 1", "1", "32,13,21"),
    ("-(1 IS NULL)", "ERR", "ERR"),
    ("+(1 IS NOT NULL)", "3", "13,21,32"),
];

const SETUP: &str = "CREATE TABLE t(a, b); INSERT INTO t VALUES (3,2),(1,3),(2,1)";

fn ours(setup_and_query: &[&str]) -> String {
    let db = TempDb::new("fold");
    let mut e = RowEngine::open(db.path()).expect("open");
    e.run_query("DROP TABLE t").expect("drop the fixture table");
    let (last, setup) = setup_and_query.split_last().unwrap();
    for stmt in setup {
        e.run_query(stmt).unwrap();
    }
    match e.run_query(last) {
        Err(_) => "ERR".to_string(),
        Ok(r) => r
            .rows
            .iter()
            .map(|row| row.iter().map(cell).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// The pinned sqlite3 3.54 (Homebrew's keg, or any `sqlite3` that reports
/// 3.54): older ones fold fewer terms, so they are not an oracle for this.
fn oracle_bin() -> Option<String> {
    ["/opt/homebrew/opt/sqlite/bin/sqlite3", "sqlite3"]
        .into_iter()
        .find(|bin| {
            Command::new(bin)
                .arg("--version")
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).starts_with("3.54."))
        })
        .map(str::to_string)
}

fn oracle(query: &str) -> Option<String> {
    let out = Command::new(oracle_bin()?)
        .arg(":memory:")
        .arg(format!("{SETUP}; {query}"))
        .output()
        .ok()?;
    Some(if out.status.success() {
        String::from_utf8_lossy(&out.stdout)
            .trim_end()
            .replace('\n', ",")
    } else {
        "ERR".to_string()
    })
}

#[test]
fn a_group_by_term_that_folds_to_an_integer_is_a_result_position() {
    for (term, group, _) in TERMS {
        let q = format!("SELECT count(*) FROM (SELECT a, count(*) FROM t GROUP BY {term})");
        assert_eq!(ours(&[SETUP, &q]), *group, "db-core: GROUP BY {term}");
        if let Some(theirs) = oracle(&q) {
            assert_eq!(theirs, *group, "sqlite3: GROUP BY {term}");
        }
    }
}

#[test]
fn an_order_by_term_that_folds_to_an_integer_is_a_result_position() {
    for (term, _, order) in TERMS {
        let q = format!("SELECT a || b FROM t ORDER BY {term}");
        assert_eq!(ours(&[SETUP, &q]), *order, "db-core: ORDER BY {term}");
        if let Some(theirs) = oracle(&q) {
            assert_eq!(theirs, *order, "sqlite3: ORDER BY {term}");
        }
    }
}
