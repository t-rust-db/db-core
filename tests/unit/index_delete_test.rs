// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Deleting rows from an indexed table keeps the index's b-tree valid.
//! Emptying a page of an index used to leave it in the tree, and SQLite
//! refuses a page with no cells: a table of a few thousand rows, once its
//! rows were deleted, was a file `sqlite3` called malformed. After every
//! statement here sqlite3 checks the file db-core wrote (where the CLI is
//! installed) and so does db-core, and the two tables must agree with a
//! copy sqlite3 itself edited.
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
            "db-core-idx-delete-{label}-{}-{}.db",
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

/// `count` rows whose indexed text is out of order and of varying length,
/// so the index fills and splits all over rather than at its right edge.
fn fill(table: &str, count: u32) -> String {
    (0..count)
        .map(|i| {
            let key = (i * 7919) % count;
            let pad = "x".repeat((i % 13 * 3) as usize);
            format!(
                "INSERT INTO {table} VALUES ({i}, 'k{key:05}{pad}', {});",
                i * 3
            )
        })
        .collect()
}

/// Runs `steps` on db-core and on sqlite3 (each on its own copy of a
/// database built by `setup`), reopening db-core after every statement so
/// sqlite3 can read the file it wrote.
fn run_steps(setup: &str, table: &str, steps: &[&str]) {
    let ours = TempDb::new("ours");
    let theirs = TempDb::new("theirs");
    {
        let mut e = RowEngine::open(&ours.0).unwrap();
        e.run_query("DROP TABLE t").unwrap();
        e.run_query(setup).unwrap();
    }
    sqlite3(&theirs.0, "DROP TABLE t");
    sqlite3(&theirs.0, setup);
    let probe = format!("SELECT count(*), sum(b), min(a), max(a) FROM {table}");
    for step in steps {
        {
            let mut e = RowEngine::open(&ours.0).unwrap();
            e.run_query(step)
                .unwrap_or_else(|err| panic!("{step}: {err}"));
            assert_eq!(rows(&mut e, "PRAGMA integrity_check"), ["ok"], "{step}");
        }
        sqlite3(&theirs.0, step);
        if let Some(verdict) = sqlite3(&ours.0, "PRAGMA integrity_check") {
            assert_eq!(verdict, ["ok"], "sqlite3 on db-core's file after: {step}");
        }
        let mut e = RowEngine::open(&ours.0).unwrap();
        if let Some(expected) = sqlite3(&theirs.0, &probe) {
            assert_eq!(rows(&mut e, &probe), expected, "after: {step}");
        }
        // The index answers as the table does.
        let by_index = format!("SELECT count(*) FROM {table} WHERE a >= ''");
        assert_eq!(
            rows(&mut e, &by_index),
            rows(&mut e, &format!("SELECT count(*) FROM {table}"))
        );
    }
}

#[test]
fn deleting_the_rows_of_a_secondary_index_keeps_the_file_valid() {
    let setup = format!(
        "CREATE TABLE big(id INTEGER PRIMARY KEY, a TEXT, b INT); \
         CREATE INDEX big_a ON big(a); CREATE UNIQUE INDEX big_b ON big(b); {}",
        fill("big", 1500)
    );
    run_steps(
        &setup,
        "big",
        &[
            "DELETE FROM big WHERE id % 5 = 0",
            "DELETE FROM big WHERE id BETWEEN 500 AND 800",
            "DELETE FROM big WHERE b % 7 = 1",
            "DELETE FROM big WHERE a < 'k01000'",
            "DELETE FROM big WHERE id > 1200",
            "DELETE FROM big WHERE id % 2 = 1",
            "DELETE FROM big",
        ],
    );
}

#[test]
fn deleting_every_row_at_once_leaves_an_empty_valid_index() {
    // The original report: all of a few thousand rows, in one statement.
    let setup = format!(
        "CREATE TABLE big(id INTEGER PRIMARY KEY, a TEXT, b INT); \
         CREATE INDEX big_a ON big(a); {}",
        fill("big", 1500)
    );
    run_steps(
        &setup,
        "big",
        &["DELETE FROM big", "INSERT INTO big VALUES (1, 'again', 1)"],
    );
}

#[test]
fn a_primary_key_or_unique_constraint_index_survives_deletes_too() {
    let setup = format!(
        "CREATE TABLE big(id INTEGER PRIMARY KEY, a TEXT UNIQUE, b INT UNIQUE); {}",
        fill("big", 1200)
    );
    run_steps(
        &setup,
        "big",
        &[
            "DELETE FROM big WHERE id % 3 = 0",
            "DELETE FROM big WHERE b > 2000",
            "DELETE FROM big WHERE a LIKE 'k0%'",
            "DELETE FROM big",
        ],
    );
}

#[test]
fn updates_that_move_index_entries_keep_the_file_valid() {
    // An UPDATE of an indexed column is a delete and an insert in the
    // index, so rows leave pages too.
    let setup = format!(
        "CREATE TABLE big(id INTEGER PRIMARY KEY, a TEXT, b INT); \
         CREATE INDEX big_a ON big(a); {}",
        fill("big", 1000)
    );
    run_steps(
        &setup,
        "big",
        &[
            "UPDATE big SET a = 'z' || a WHERE id % 2 = 0",
            "UPDATE big SET a = substr(a, 2) WHERE id % 4 = 0",
            "DELETE FROM big WHERE a LIKE 'z%'",
            "UPDATE big SET a = a || 'tail' WHERE id < 400",
        ],
    );
}
