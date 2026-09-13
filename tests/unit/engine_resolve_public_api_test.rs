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

use db_core::engine::resolve::{
    explain_plan, explain_stream_stream_plan, run_query, run_stream_stream_query,
};
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
fn a_select_with_no_from_clause_does_not_parse() {
    // The grammar itself requires a FROM clause for any statement
    // reaching this far (a bare `SELECT 1` fails to parse), so
    // `resolve_sides`'s own "SELECT without FROM" guard is defensive
    // for a shape this parser never actually produces.
    let db = HostsDb::new("no-from");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    let err = run_query(&driving, &lookup, "SELECT 1").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Parse);
}

#[test]
fn rejects_more_than_one_join_clause() {
    let db = HostsDb::new("two-joins");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    let err = run_query(
        &driving,
        &lookup,
        "SELECT log.hostname FROM log \
         JOIN hosts ON log.hostname = hosts.name \
         JOIN hosts h2 ON log.hostname = h2.name",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
}

#[test]
fn rejects_neither_side_being_the_stream_table() {
    let db = HostsDb::new("neither-log");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    let err = run_query(
        &driving,
        &lookup,
        "SELECT h1.name FROM hosts h1 JOIN hosts h2 ON h1.name = h2.name",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
}

#[test]
fn rejects_a_subquery_from_clause() {
    let db = HostsDb::new("subquery-from");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    let err = run_query(
        &driving,
        &lookup,
        "SELECT x.name FROM (SELECT name FROM hosts) x JOIN hosts ON x.name = hosts.name",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
}

#[test]
fn a_subquery_join_target_does_not_parse() {
    // Same story as the FROM-subquery case, mirrored on the JOIN side:
    // this grammar doesn't accept a subquery there at all.
    let db = HostsDb::new("subquery-join");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    let err = run_query(
        &driving,
        &lookup,
        "SELECT log.hostname FROM log \
         JOIN (SELECT name FROM hosts) x ON log.hostname = x.name",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Parse);
}

#[test]
fn rejects_an_unsupported_join_kind_via_the_batch_planner() {
    // `resolve_sides` accepts this (log drives, hosts looks up); the
    // rejection comes from `codegen::batch::compile_join` itself
    // (`PlanError::UnsupportedJoinKind`), routed through `plan_err`.
    let db = HostsDb::new("unsupported-join-kind");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    let err = run_query(
        &driving,
        &lookup,
        "SELECT log.hostname FROM log FULL JOIN hosts ON log.hostname = hosts.name",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
}

#[test]
fn a_malformed_statement_is_a_parse_error() {
    let db = HostsDb::new("parse-error");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    let err = run_query(&driving, &lookup, "SELECT FROM FROM").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Parse);
}

#[test]
fn explain_plan_shares_run_querys_side_resolution_errors() {
    let db = HostsDb::new("explain-errors");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    let err = explain_plan(&driving, &lookup, db.path(), "SELECT 1").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Parse);

    let err = explain_plan(
        &driving,
        &lookup,
        db.path(),
        "SELECT hosts.region FROM hosts JOIN log ON hosts.name = log.hostname",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
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

// --- Windowed stream-to-stream joins (ADR-0022, #372) ---

fn temp_log(name: &str, text: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "db-core-resolve-stream-stream-{}-{name}.log",
        std::process::id()
    ));
    std::fs::write(&p, text).unwrap();
    p
}

/// `a` (requests): `web01`, `web02`. `b` (auth): `web01` only -- `web02`
/// has no match on `b`, so a LEFT JOIN must surface it as NULL, never a
/// silent drop, exactly like the stream/SQLite join's unmatched-host case.
fn stream_stream_fixtures() -> (PathBuf, PathBuf) {
    let a = temp_log(
        "requests",
        "<134>Sep 10 08:00:01 web01 nginx[1]: request 1\n\
         <134>Sep 10 08:00:02 web02 nginx[2]: request 2\n",
    );
    let b = temp_log(
        "auth",
        "<38>Sep 10 08:00:03 web01 sshd[10]: session opened\n",
    );
    (a, b)
}

#[test]
fn windowed_self_join_matches_on_hostname_and_nulls_unmatched_rows() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    let result = run_stream_stream_query(
        &a,
        &b,
        "SELECT a.hostname, b.tag FROM log AS a \
         LEFT JOIN log AS b ON a.hostname = b.hostname \
         SINCE 1 hour",
    )
    .expect("windowed stream-to-stream join");

    assert_eq!(result.columns, vec!["a.hostname", "b.tag"]);
    assert_eq!(result.rows.len(), 2);
    for row in &result.rows {
        let host = text(&row[0]);
        let tag = &row[1];
        match host {
            "web01" => assert_eq!(tag, &Cell::Text("sshd".to_string())),
            "web02" => assert_eq!(tag, &Cell::Null, "unmatched host must be NULL, not dropped"),
            other => panic!("unexpected host {other}"),
        }
    }
}

#[test]
fn rejects_a_stream_stream_join_with_no_window() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    let err = run_stream_stream_query(
        &a,
        &b,
        "SELECT a.hostname, b.tag FROM log AS a LEFT JOIN log AS b ON a.hostname = b.hostname",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert!(
        err.message.contains("window"),
        "expected a window-specific message, got: {}",
        err.message
    );
}

#[test]
fn rejects_a_stream_stream_join_missing_an_alias() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    // Neither side aliased: both a `FROM`/`JOIN` name of literally `log`.
    let err = run_stream_stream_query(
        &a,
        &b,
        "SELECT log.hostname FROM log JOIN log ON log.hostname = log.hostname SINCE 1 hour",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert!(err.message.contains("alias"), "{}", err.message);
}

/// MC/DC vector (`resolve_stream_stream_sides`'s `!from_is_log ||
/// !join_is_log` guard): leaf A (`!from_is_log`) true alone.
#[test]
#[allow(non_snake_case)]
fn mcdc__engine_resolve_resolve_stream_stream_sides_0cfb5f70__v1_from_side_not_log() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    let err = run_stream_stream_query(
        &a,
        &b,
        "SELECT x.hostname FROM hosts AS x JOIN log AS y ON x.hostname = y.hostname SINCE 1 hour",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
}

/// MC/DC vector: leaf B (`!join_is_log`) true alone.
#[test]
#[allow(non_snake_case)]
fn mcdc__engine_resolve_resolve_stream_stream_sides_0cfb5f70__v2_join_side_not_log() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    let err = run_stream_stream_query(
        &a,
        &b,
        "SELECT x.hostname FROM log AS x JOIN hosts AS y ON x.hostname = y.hostname SINCE 1 hour",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
}

/// MC/DC vector: both leaves false -- both sides are `log`, so this check
/// passes (the subsequent alias check is what actually rejects here).
#[test]
#[allow(non_snake_case)]
fn mcdc__engine_resolve_resolve_stream_stream_sides_0cfb5f70__v3_both_sides_log() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    let err = run_stream_stream_query(
        &a,
        &b,
        "SELECT log.hostname FROM log JOIN log ON log.hostname = log.hostname SINCE 1 hour",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert!(err.message.contains("alias"));
}

#[test]
fn rejects_a_stream_stream_join_with_duplicate_aliases() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    let err = run_stream_stream_query(
        &a,
        &b,
        "SELECT x.hostname FROM log AS x JOIN log AS x ON x.hostname = x.hostname SINCE 1 hour",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
}

#[test]
fn explain_stream_stream_labels_each_side_by_alias_and_file() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    let plan = explain_stream_stream_plan(
        &a,
        &b,
        "SELECT a.hostname, b.tag FROM log AS a \
         LEFT JOIN log AS b ON a.hostname = b.hostname \
         SINCE 1 hour",
    )
    .expect("explain stream-to-stream join");

    let details: Vec<&str> = plan.iter().map(|n| n.detail.as_str()).collect();
    assert!(
        details
            .iter()
            .any(|d| d.starts_with("SCAN a") && d.contains("[stream")),
        "{details:?}"
    );
    assert!(
        details
            .iter()
            .any(|d| d.starts_with("SCAN b") && d.contains("[stream")),
        "{details:?}"
    );
}
