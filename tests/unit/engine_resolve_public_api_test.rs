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
    explain_plan, explain_stream_stream_plan, run_query, run_stream_stream_query, CrossModeEngine,
};
use db_core::engine::row::RowEngine;
use db_core::engine::stream::StreamEngine;
use db_core::engine::{Cell, Engine, ErrorKind, Mode};

const LOG_FIXTURE: &str = "tests/fixtures/stream/syslog-1k.log";
const ROW_FIXTURE: &str = "tests/corpus/fixtures/btrees/table_single_page.db";

/// A writable copy of the SQLite fixture, removed on drop, with a `hosts`
/// dimension table: `web01`/`eu`, `web02`/`us`; `db01` deliberately absent
/// (an unmatched driving key must surface as `NULL`, never a silent drop).
/// Optionally also seeds an `owners` table (`web01`/`alice`, `web02`/`bob`)
/// keyed the same way, for the multi-way star-join tests (#394): a second,
/// independent SQLite lookup table joined to `log` on the same driving
/// column `hosts` is joined on.
struct HostsDb(PathBuf);

impl HostsDb {
    fn new(label: &str) -> Self {
        Self::with_owners(label, false)
    }

    fn with_owners(label: &str, seed_owners: bool) -> Self {
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
        if seed_owners {
            engine
                .run_query(
                    "CREATE TABLE owners(host TEXT PRIMARY KEY, owner TEXT);\
                     INSERT INTO owners VALUES ('web01', 'alice');\
                     INSERT INTO owners VALUES ('web02', 'bob');",
                )
                .expect("seed owners table");
        }
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

/// #387: the same stream/SQLite cross-mode join, but routed through
/// `CrossModeEngine`'s `Engine` impl instead of the free functions --
/// `run_query`/`explain_opcodes` must agree with the free-function path
/// above, and `explain_opcodes` (previously "not available" for cross-mode
/// queries, db-studio#54) must return a real, non-empty opcode dump naming
/// the join.
#[test]
fn cross_mode_engine_routes_run_query_and_explain_opcodes() {
    let db = HostsDb::new("cross-mode-engine");
    let mut engine = CrossModeEngine::open_stream_sqlite(Path::new(LOG_FIXTURE), db.path())
        .expect("open cross-mode engine");
    assert_eq!(engine.mode(), Mode::Cross);

    let sql = "SELECT log.hostname, hosts.region FROM log \
               JOIN hosts ON log.hostname = hosts.name \
               WHERE severity >= 'WARN'";
    let result = engine.run_query(sql).expect("cross-mode join via Engine");
    assert_eq!(result.columns, vec!["log.hostname", "hosts.region"]);
    assert!(!result.rows.is_empty());

    let sections = engine
        .explain_opcodes(sql)
        .expect("explain_opcodes via Engine");
    assert!(!sections.is_empty());
    assert!(sections.iter().any(|s| s.label.contains("JOIN build")));
    assert!(sections.iter().any(|s| s.label.contains("JOIN probe")));
    assert!(sections.iter().any(|s| !s.rows.is_empty()));

    // #388: each section's lane names which physical engine executes it --
    // the SQLite lookup build side is "row", the driving stream probe side
    // is "stream", and the join body always runs on vm::batch ("batch")
    // regardless of either side's origin.
    let build = sections
        .iter()
        .find(|s| s.label.contains("JOIN build"))
        .expect("build section");
    assert_eq!(build.lane, "row");
    let probe = sections
        .iter()
        .find(|s| s.label.contains("JOIN probe"))
        .expect("probe section");
    assert_eq!(probe.lane, "stream");
    let body = sections
        .iter()
        .find(|s| s.label.contains("JOIN body"))
        .expect("body section");
    assert_eq!(body.lane, "batch");
}

/// `Engine::open`'s single `path` cannot express a cross-mode engine's two
/// files -- it must fail with a typed error, not panic or silently open
/// only one side.
#[test]
fn cross_mode_engine_open_is_unsupported() {
    let err = CrossModeEngine::open(Path::new(LOG_FIXTURE)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
}

/// #366: `CrossModeEngine::StreamSqlite`'s `explain_plan`/`stats`/`tables`
/// -- routed through the `Engine` trait, not the free functions -- were
/// untested. `explain_plan` must label both sides by mode (mirroring
/// `explain_plan`'s own doc comment); `stats` reports the driving
/// (stream) side's `FileStats::Stream`; `tables` merges both engines'
/// schemas (`log` plus whatever the SQLite lookup file declares).
#[test]
fn cross_mode_engine_stream_sqlite_explain_plan_stats_and_tables() {
    let db = HostsDb::new("cross-mode-engine-explain-plan");
    let engine = CrossModeEngine::open_stream_sqlite(Path::new(LOG_FIXTURE), db.path())
        .expect("open cross-mode engine");

    let plan = engine
        .explain_plan(
            "SELECT log.hostname, hosts.region FROM log \
             JOIN hosts ON log.hostname = hosts.name",
        )
        .expect("explain_plan via Engine");
    assert!(!plan.is_empty());
    let details: Vec<&str> = plan.iter().map(|n| n.detail.as_str()).collect();
    assert!(details.iter().any(|d| d.contains("[stream")), "{details:?}");
    assert!(details.iter().any(|d| d.contains("[sqlite")), "{details:?}");

    assert!(matches!(
        engine.stats(),
        db_core::engine::FileStats::Stream { .. }
    ));

    let tables = engine.tables().expect("tables via Engine");
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"log"), "{names:?}");
    assert!(names.contains(&"hosts"), "{names:?}");
}

/// #366: same as above, for the `StreamStream` (windowed self-join)
/// variant -- `explain_plan` labels both aliases by stream file path,
/// `stats` reports the `left` engine's stats, `tables` merges `left` and
/// `right` (both `log`, since a self-join has one physical table each).
#[test]
fn cross_mode_engine_stream_stream_explain_plan_stats_and_tables() {
    let (a_path, b_path) = stream_stream_fixtures();
    let engine =
        CrossModeEngine::open_stream_stream(&a_path, &b_path).expect("open cross-mode engine");

    let plan = engine
        .explain_plan(
            "SELECT a.hostname, b.tag FROM log AS a \
             JOIN log AS b ON a.hostname = b.hostname \
             SINCE 1 hour",
        )
        .expect("explain_plan via Engine");
    assert!(!plan.is_empty());
    let details: Vec<&str> = plan.iter().map(|n| n.detail.as_str()).collect();
    assert_eq!(
        details.iter().filter(|d| d.contains("[stream")).count(),
        2,
        "{details:?}"
    );

    assert!(matches!(
        engine.stats(),
        db_core::engine::FileStats::Stream { .. }
    ));

    let tables = engine.tables().expect("tables via Engine");
    assert_eq!(tables.len(), 2);
    assert!(tables.iter().all(|t| t.name == "log"));
}

#[test]
fn accepts_sqlite_as_the_driving_side_for_inner_join() {
    let db = HostsDb::new("driving-side");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    // Same join as `joins_warn_log_lines_against_a_sqlite_hosts_lookup_table`,
    // with `hosts` written as the FROM table (ADR-0021, #371): only matched
    // hosts survive (INNER JOIN), so `db01` -- absent from `hosts` -- must
    // not appear.
    let result = run_query(
        &driving,
        &lookup,
        "SELECT hosts.region, log.hostname FROM hosts \
         JOIN log ON hosts.name = log.hostname \
         WHERE log.severity >= 'WARN'",
    )
    .expect("cross-mode join with SQLite as the driving side");

    assert_eq!(result.columns, vec!["hosts.region", "log.hostname"]);
    assert!(!result.rows.is_empty());
    for row in &result.rows {
        let region = text(&row[0]);
        let host = text(&row[1]);
        match host {
            "web01" => assert_eq!(region, "eu"),
            "web02" => assert_eq!(region, "us"),
            other => panic!("unmatched host {other} must not survive an INNER JOIN"),
        }
    }
}

#[test]
fn rejects_left_join_with_sqlite_as_the_driving_side() {
    let db = HostsDb::new("driving-side-left-join");
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts db");

    // `hosts LEFT JOIN log` would need to keep all `hosts` rows, but the
    // stream side must always be the probe side (see module docs) -- that
    // shape needs RIGHT JOIN semantics, which isn't implemented.
    let err = run_query(
        &driving,
        &lookup,
        "SELECT hosts.region FROM hosts LEFT JOIN log ON hosts.name = log.hostname",
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

/// #394: multiple `JOIN`s against the same physical table are still
/// rejected -- aliasing a repeated lookup target isn't supported by the
/// multi-way star-join path, unlike joining N *distinct* SQLite tables
/// (see `joins_warn_log_lines_against_two_independent_sqlite_lookup_tables`).
#[test]
fn rejects_more_than_one_join_clause_against_the_same_table() {
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

    // `hosts LEFT JOIN log` with SQLite as the driving side is rejected by
    // `run_query` too (`rejects_left_join_with_sqlite_as_the_driving_side`,
    // ADR-0021, #371) -- unlike the plain `JOIN` shape, which is now
    // accepted (`accepts_sqlite_as_the_driving_side_for_inner_join`).
    let err = explain_plan(
        &driving,
        &lookup,
        db.path(),
        "SELECT hosts.region FROM hosts LEFT JOIN log ON hosts.name = log.hostname",
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
    // A bare pid-scoped name collides across parallel `#[test]` threads
    // sharing this process (`stream_stream_fixtures` is called by several
    // tests): one thread's `write` can race another's `open`/`write` on the
    // exact same path, torn-reading a corrupt file (#381 investigation).
    // A per-call counter makes every call's path unique regardless of
    // thread interleaving.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "db-core-resolve-stream-stream-{}-{n}-{name}.log",
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

/// #387/#388: the same windowed stream-to-stream join routed through
/// `CrossModeEngine`, with both sides' opcode sections lane-labeled
/// `"stream"` (ADR-0022 has no SQLite side) and the body `"batch"`.
#[test]
fn cross_mode_engine_routes_stream_stream_explain_opcodes() {
    let (a_path, b_path) = stream_stream_fixtures();
    let mut engine =
        CrossModeEngine::open_stream_stream(&a_path, &b_path).expect("open cross-mode engine");
    assert_eq!(engine.mode(), Mode::Cross);

    let sql = "SELECT a.hostname, b.tag FROM log AS a \
               LEFT JOIN log AS b ON a.hostname = b.hostname \
               SINCE 1 hour";
    let result = engine
        .run_query(sql)
        .expect("windowed self-join via Engine");
    assert_eq!(result.columns, vec!["a.hostname", "b.tag"]);
    assert_eq!(result.rows.len(), 2);

    let sections = engine
        .explain_opcodes(sql)
        .expect("explain_opcodes via Engine");
    let build = sections
        .iter()
        .find(|s| s.label.contains("JOIN build"))
        .expect("build section");
    assert_eq!(build.lane, "stream");
    let probe = sections
        .iter()
        .find(|s| s.label.contains("JOIN probe"))
        .expect("probe section");
    assert_eq!(probe.lane, "stream");
    let body = sections
        .iter()
        .find(|s| s.label.contains("JOIN body"))
        .expect("body section");
    assert_eq!(body.lane, "batch");
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

/// #366: `resolve_stream_stream_sides`'s "not a join" branch -- a
/// single-table query has no `from.joins` entry at all, distinct from
/// (and checked before) any alias/window validation.
#[test]
fn rejects_a_stream_stream_query_with_no_join_at_all() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    let err = run_stream_stream_query(&a, &b, "SELECT hostname FROM log").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("single-table"), "{}", err.message);
}

/// #366: a `FROM` subquery is rejected the same way on the stream-stream
/// path as on the stream/SQLite path (`rejects_a_subquery_from_clause`,
/// above) -- `resolve_stream_stream_sides` has its own copy of this check.
#[test]
fn rejects_a_stream_stream_join_with_a_subquery_from_clause() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    let err = run_stream_stream_query(
        &a,
        &b,
        "SELECT x.hostname FROM (SELECT hostname FROM log) x \
         JOIN log AS b ON x.hostname = b.hostname SINCE 1 hour",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert!(err.message.contains("subquery"), "{}", err.message);
}

/// #366: the `FROM`-side alias check short-circuits before the `JOIN`-side
/// one ever runs, so `rejects_a_stream_stream_join_missing_an_alias` (with
/// *neither* side aliased) never actually exercises the `JOIN`-side
/// rejection. This aliases the `FROM` side only, isolating it.
#[test]
fn rejects_a_stream_stream_join_missing_the_join_side_alias() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    let err = run_stream_stream_query(
        &a,
        &b,
        "SELECT a.hostname FROM log AS a JOIN log ON a.hostname = log.hostname SINCE 1 hour",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert!(err.message.contains("JOIN"), "{}", err.message);
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

/// A thin `Clock` forwarding to a shared `Arc<FakeClock>`, matching
/// `engine_stream_standing_queries_test.rs`'s own convention (`Box<dyn
/// Clock>` needs a concrete forwarding type since `FakeClock` itself
/// isn't boxed by the test).
struct FakeClockHandle(std::sync::Arc<db_core::clock::FakeClock>);

impl db_core::clock::Clock for FakeClockHandle {
    fn now_ns(&self) -> i64 {
        self.0.now_ns()
    }
}

/// Opens the fixture as usual (so format detection sees real content, not
/// an empty file), then swaps in a fake clock pinned to the real time the
/// segments were actually observed at -- so the clock can be moved
/// forward deterministically afterward without re-detecting or
/// re-admitting anything.
fn open_with_fake_clock_at_now(
    path: &Path,
) -> (StreamEngine, std::sync::Arc<db_core::clock::FakeClock>) {
    let now_ns = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap();
    let clock = std::sync::Arc::new(db_core::clock::FakeClock::new(now_ns));
    let mut e = StreamEngine::open(path).expect("open");
    e.set_clock(Box::new(FakeClockHandle(clock.clone())));
    (e, clock)
}

/// A window excluding every segment on both sides (the fake clock jumped
/// 2 hours past the real observation time, then queried with a 1-hour
/// `SINCE`) must materialize the (empty) build side to a zero-row,
/// zero-column `Batch` (`materialize_segments`'s `loaded.first()` is
/// `None`) and produce zero output rows -- not an error -- rather than
/// panicking or erroring on the column-less batch.
#[test]
fn windowed_self_join_with_a_window_excluding_every_segment_returns_zero_rows() {
    let (a_path, b_path) = stream_stream_fixtures();
    let (a, a_clock) = open_with_fake_clock_at_now(&a_path);
    let (b, b_clock) = open_with_fake_clock_at_now(&b_path);

    let two_hours_ns = 2 * 3_600 * 1_000_000_000;
    a_clock.advance(two_hours_ns);
    b_clock.advance(two_hours_ns);

    let result = run_stream_stream_query(
        &a,
        &b,
        "SELECT a.hostname, b.tag FROM log AS a \
         LEFT JOIN log AS b ON a.hostname = b.hostname \
         SINCE 1 hour",
    )
    .expect("windowed stream-to-stream join with a window matching no segment");

    assert_eq!(result.rows.len(), 0, "{:?}", result.rows);
}

/// INNER JOIN through the stream-stream path: unmatched rows on either
/// side must be dropped, not surfaced as NULL (distinguishing this from
/// the LEFT JOIN coverage above).
#[test]
fn windowed_self_join_inner_join_drops_unmatched_rows() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    let result = run_stream_stream_query(
        &a,
        &b,
        "SELECT a.hostname, b.tag FROM log AS a \
         JOIN log AS b ON a.hostname = b.hostname \
         SINCE 1 hour",
    )
    .expect("windowed stream-to-stream inner join");

    assert_eq!(result.rows.len(), 1, "{:?}", result.rows);
    assert_eq!(text(&result.rows[0][0]), "web01");
}

/// More than one `JOIN` is rejected for the stream-stream path
/// specifically (mirrors the stream/SQLite path's own rejection, but
/// exercised through `run_stream_stream_query` rather than inferred).
#[test]
fn rejects_a_stream_stream_query_with_more_than_one_join() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    let err = run_stream_stream_query(
        &a,
        &b,
        "SELECT x.hostname FROM log AS x \
         JOIN log AS y ON x.hostname = y.hostname \
         JOIN log AS z ON y.hostname = z.hostname \
         SINCE 1 hour",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert!(err.message.contains("JOIN"), "{}", err.message);
}

/// A `LINES`/`BYTES` scope passes the "some scope is present" check but
/// has no fixed time edge (`StreamEngine::segments_in_range`), so it
/// can't actually bound either side finitely -- must be rejected the same
/// as no scope at all, with a message distinguishing it from a plain
/// missing-window rejection.
#[test]
fn rejects_a_stream_stream_join_with_a_non_time_scope() {
    let (a_path, b_path) = stream_stream_fixtures();
    let a = StreamEngine::open(&a_path).expect("open a");
    let b = StreamEngine::open(&b_path).expect("open b");

    let err = run_stream_stream_query(
        &a,
        &b,
        "SELECT a.hostname, b.tag FROM log AS a \
         LEFT JOIN log AS b ON a.hostname = b.hostname \
         SINCE 10 lines",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert!(
        err.message.contains("time-based"),
        "expected a time-based-scope-specific message, got: {}",
        err.message
    );
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

/// #394: `log` joined to two independent SQLite lookup tables in one
/// query (`hosts` and `owners`, both keyed on `hostname`) -- the star-join
/// shape `resolve_multi_sides`/`compile_cross_mode_multi_join` add on top
/// of the single-`JOIN` path above.
#[test]
fn joins_warn_log_lines_against_two_independent_sqlite_lookup_tables() {
    let db = HostsDb::with_owners("multi-join", true);
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts+owners db");

    let result = run_query(
        &driving,
        &lookup,
        "SELECT log.hostname, hosts.region, owners.owner FROM log \
         JOIN hosts ON log.hostname = hosts.name \
         JOIN owners ON log.hostname = owners.host \
         WHERE severity >= 'WARN'",
    )
    .expect("multi-way cross-mode join");

    assert_eq!(
        result.columns,
        vec!["log.hostname", "hosts.region", "owners.owner"]
    );
    assert!(!result.rows.is_empty());
    for row in &result.rows {
        let host = text(&row[0]);
        let (region, owner) = (&row[1], &row[2]);
        match host {
            "web01" => {
                assert_eq!(region, &Cell::Text("eu".to_string()));
                assert_eq!(owner, &Cell::Text("alice".to_string()));
            }
            "web02" => {
                assert_eq!(region, &Cell::Text("us".to_string()));
                assert_eq!(owner, &Cell::Text("bob".to_string()));
            }
            _ => {
                assert_eq!(region, &Cell::Null, "unmatched host {host} must be NULL");
                assert_eq!(owner, &Cell::Null, "unmatched host {host} must be NULL");
            }
        }
    }
}

/// #394: a three-way star join (`log` against `hosts`, `owners`, and a
/// third ad hoc lookup table created inline) still resolves and executes
/// correctly -- proves the generalization isn't hard-coded to two sides.
#[test]
fn joins_warn_log_lines_against_three_independent_sqlite_lookup_tables() {
    let db = HostsDb::with_owners("three-way-join", true);
    let mut seed = RowEngine::open(db.path()).expect("open db to seed a third table");
    seed.run_query(
        "CREATE TABLE tiers(host TEXT PRIMARY KEY, tier TEXT);\
         INSERT INTO tiers VALUES ('web01', 'gold');\
         INSERT INTO tiers VALUES ('web02', 'silver');",
    )
    .expect("seed tiers table");
    drop(seed);

    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts+owners+tiers db");

    let result = run_query(
        &driving,
        &lookup,
        "SELECT log.hostname, hosts.region, owners.owner, tiers.tier FROM log \
         JOIN hosts ON log.hostname = hosts.name \
         JOIN owners ON log.hostname = owners.host \
         JOIN tiers ON log.hostname = tiers.host \
         WHERE severity >= 'WARN'",
    )
    .expect("three-way cross-mode join");

    assert_eq!(
        result.columns,
        vec!["log.hostname", "hosts.region", "owners.owner", "tiers.tier"]
    );
    assert!(!result.rows.is_empty());
    for row in &result.rows {
        let host = text(&row[0]);
        if host == "web01" {
            assert_eq!(row[3], Cell::Text("gold".to_string()));
        } else if host == "web02" {
            assert_eq!(row[3], Cell::Text("silver".to_string()));
        } else {
            assert_eq!(row[3], Cell::Null, "unmatched host {host} must be NULL");
        }
    }
}

/// #394: the stream table must be written first (`FROM log`) once there is
/// more than one `JOIN` -- there's no single "other side" to swap into with
/// N lookup tables, unlike the single-`JOIN` case.
#[test]
fn rejects_a_multi_join_with_the_sqlite_table_written_first() {
    let db = HostsDb::with_owners("multi-join-wrong-order", true);
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts+owners db");

    let err = run_query(
        &driving,
        &lookup,
        "SELECT hosts.region FROM hosts \
         JOIN log ON hosts.name = log.hostname \
         JOIN owners ON log.hostname = owners.host",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
}

/// #394: every `JOIN` target in a multi-way cross-mode query must be a
/// known SQLite table -- a second `log` (the stream table again) is
/// rejected, not silently treated as a third lookup side.
#[test]
fn rejects_a_multi_join_that_repeats_the_stream_table() {
    let db = HostsDb::with_owners("multi-join-repeat-log", true);
    let driving = StreamEngine::open(Path::new(LOG_FIXTURE)).expect("open log fixture");
    let lookup = RowEngine::open(db.path()).expect("open hosts+owners db");

    let err = run_query(
        &driving,
        &lookup,
        "SELECT hosts.region FROM log \
         JOIN hosts ON log.hostname = hosts.name \
         JOIN log AS l2 ON log.hostname = l2.hostname",
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
}
