// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for `db_core::engine::resolve` (#342, epic #317): the
//! runtime resolver that routes a cross-mode `SELECT` -- the stream
//! engine's `log` table joined to a SQLite lookup table -- to
//! `codegen::batch::compile_join` / `vm::engine::run_join_segments`. This
//! is the piece the epic's own PR (#340) left open: #312/#314/#315/#316
//! only built the plumbing, no path from SQL text to a routed join.
//!
//! The fixture is `tests/fixtures/stream/syslog-1k.log` (the same corpus
//! `engine_stream_public_api_test.rs` uses), whose RFC 3164 hostnames are
//! `web01`/`web02`/`db01`/... -- exactly the epic's own example: "severity
//! >= WARN log lines enriched from a hosts/services dimension file".
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

use db_core::engine::resolve::{explain_plan, run_query};
use db_core::engine::row::RowEngine;
use db_core::engine::stream::StreamEngine;
use db_core::engine::{Cell, Engine, ErrorKind};

const LOG_FIXTURE: &str = "tests/fixtures/stream/syslog-1k.log";
const ROW_FIXTURE: &str = "tests/corpus/fixtures/btrees/table_single_page.db";

/// A writable copy of the SQLite fixture, removed on drop, with a `hosts`
/// dimension table: `web01`/`eu`, `web02`/`us`; `db01` deliberately absent
/// (an unmatched driving key must surface as `NULL`, never a silent drop).
struct HostsDb(PathBuf);

impl HostsDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-resolve-{label}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::copy(ROW_FIXTURE, &path).expect("copy fixture");
        let mut engine = RowEngine::open(&path).expect("open fixture copy");
        engine
            .run_query(
                "CREATE TABLE hosts(name TEXT PRIMARY KEY, region TEXT);\
                 INSERT INTO hosts VALUES ('web01', 'eu');\
                 INSERT INTO hosts VALUES ('web02', 'us');",
            )
            .expect("seed hosts table");
        HostsDb(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for HostsDb {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
        std::fs::remove_file(format!("{}-journal", self.0.display())).ok();
    }
}

fn text(cell: &Cell) -> &str {
    match cell {
        Cell::Text(s) => s,
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn joins_warn_log_lines_against_a_sqlite_hosts_lookup_table() {
    let db = HostsDb::new("join");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    let result = run_query(
        &driving,
        &lookup,
        "SELECT log.hostname, hosts.region FROM log \
         JOIN hosts ON log.hostname = hosts.name \
         WHERE severity >= 'WARN'",
    )
    .expect("cross-mode join");

    assert_eq!(result.columns, vec!["log.hostname", "hosts.region"]);
    assert!(!result.rows.is_empty());
    for row in &result.rows {
        let host = text(&row[0]);
        let region = &row[1];
        match host {
            "web01" => assert_eq!(region, &Cell::Text("eu".to_string())),
            "web02" => assert_eq!(region, &Cell::Text("us".to_string())),
            // `db01` (and any other unmatched host) has no `hosts` row: LEFT
            // JOIN semantics via HashProbe surface this as NULL, not a drop.
            _ => assert_eq!(region, &Cell::Null, "unmatched host {host} must be NULL"),
        }
    }
}

#[test]
fn rejects_sqlite_as_the_driving_side() {
    let db = HostsDb::new("driving-side");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    let err = run_query(
        &driving,
        &lookup,
        "SELECT hosts.region FROM hosts JOIN log ON hosts.name = log.hostname",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
}

#[test]
fn rejects_an_unknown_lookup_table() {
    let db = HostsDb::new("unknown-table");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    let err = run_query(
        &driving,
        &lookup,
        "SELECT log.hostname FROM log JOIN nope ON log.hostname = nope.name",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
}

#[test]
fn rejects_a_single_table_query() {
    let db = HostsDb::new("single-table");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    let err = run_query(&driving, &lookup, "SELECT * FROM log").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
}

#[test]
fn explain_labels_each_side_by_source_mode_and_file() {
    let db = HostsDb::new("explain");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    let plan = explain_plan(
        &driving,
        &lookup,
        db.path(),
        "SELECT log.hostname, hosts.region FROM log JOIN hosts ON log.hostname = hosts.name",
    )
    .expect("explain cross-mode join");

    let details: Vec<&str> = plan.iter().map(|n| n.detail.as_str()).collect();
    assert!(
        details
            .iter()
            .any(|d| d.starts_with("SCAN log") && d.contains("[stream")),
        "{details:?}"
    );
    assert!(
        details
            .iter()
            .any(|d| d.starts_with("SCAN hosts") && d.contains("[sqlite")),
        "{details:?}"
    );
}
