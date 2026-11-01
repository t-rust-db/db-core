// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for a cross-mode engine over several SQLite files
//! (#924): `CrossModeEngine::open_column_tables_sqlite` with two named
//! files. Each SQLite table a query names builds from its own file; a bare
//! name resolves when one file has the table, `name.table` picks the file
//! otherwise, and `EXPLAIN QUERY PLAN` labels each table with its file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use db_core::engine::column::table::TableEngine;
use db_core::engine::resolve::CrossModeEngine;
use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine, ErrorKind};
use db_core::storage::ingest::{Ack, RecordValue, TableOptions, TableWriter};
use db_core::storage::parquet::schema::{ColumnSpec, ColumnType};

const ROW_FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

/// A scratch directory holding the `events` table directory (ids 1..=10,
/// `channel_id` alternating 2, 1) and two SQLite files: `machine.db` with
/// `machine` and `region`, `session.db` with `session_live` and its own
/// `region`. Removed on drop.
struct Fixture {
    root: PathBuf,
    _writer: TableWriter,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "db-core-multi-sqlite-join-{}-{n}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        seed(
            &root.join("machine.db"),
            "CREATE TABLE machine(event_id INTEGER PRIMARY KEY, host TEXT);\
             INSERT INTO machine VALUES (2, 'h2');\
             INSERT INTO machine VALUES (5, 'h5');\
             INSERT INTO machine VALUES (9, 'h9');\
             CREATE TABLE region(id INTEGER PRIMARY KEY, name TEXT);\
             INSERT INTO region VALUES (1, 'm-one');\
             INSERT INTO region VALUES (2, 'm-two');",
        );
        seed(
            &root.join("session.db"),
            "CREATE TABLE session_live(event_id INTEGER PRIMARY KEY, user TEXT);\
             INSERT INTO session_live VALUES (5, 'u5');\
             INSERT INTO session_live VALUES (9, 'u9');\
             INSERT INTO session_live VALUES (10, 'u10');\
             CREATE TABLE region(id INTEGER PRIMARY KEY, name TEXT);\
             INSERT INTO region VALUES (1, 's-one');",
        );
        let mut w = TableWriter::open(
            &root.join("events"),
            TableOptions {
                schema: Some(vec![
                    ColumnSpec::new("event_id", ColumnType::Int64, false),
                    ColumnSpec::new("channel_id", ColumnType::Int64, false),
                ]),
                ..TableOptions::default()
            },
        )
        .unwrap();
        for id in 1..=10 {
            w.append_row(
                vec![RecordValue::Int(id), RecordValue::Int(id % 2 + 1)],
                Ack::Durable,
            )
            .unwrap();
        }
        w.flush_all().unwrap();
        Fixture { root, _writer: w }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn engine(&self) -> CrossModeEngine {
        CrossModeEngine::open_column_tables_sqlite(
            &[self.path("events").as_path()],
            &[
                ("m", self.path("machine.db").as_path()),
                ("s", self.path("session.db").as_path()),
            ],
        )
        .expect("open")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
    }
}

/// A copy of the SQLite fixture at `path` with `sql` run on it.
fn seed(path: &Path, sql: &str) {
    std::fs::copy(ROW_FIXTURE, path).expect("copy fixture");
    RowEngine::open(path)
        .expect("open fixture copy")
        .run_query(sql)
        .expect("seed");
}

fn rows(e: &mut CrossModeEngine, sql: &str) -> Vec<Vec<Cell>> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
}

fn error(e: &mut CrossModeEngine, sql: &str) -> (ErrorKind, String) {
    let err = e.run_query(sql).expect_err(sql);
    (err.kind, err.to_string())
}

fn text(s: &str) -> Cell {
    Cell::Text(s.into())
}

#[test]
fn a_star_join_reads_each_sqlite_table_from_its_own_file() {
    let f = Fixture::new();
    let mut e = f.engine();
    let want = vec![
        vec![Cell::Int(5), text("h5"), text("u5")],
        vec![Cell::Int(9), text("h9"), text("u9")],
    ];
    for sql in [
        "SELECT events.event_id, machine.host, session_live.user FROM events \
         JOIN machine ON events.event_id = machine.event_id \
         JOIN session_live ON events.event_id = session_live.event_id \
         ORDER BY events.event_id",
        "SELECT events.event_id, machine.host, session_live.user FROM events \
         JOIN m.machine ON events.event_id = machine.event_id \
         JOIN s.session_live ON events.event_id = session_live.event_id \
         ORDER BY events.event_id",
        "SELECT e.event_id, mc.host, sl.user FROM events AS e \
         JOIN m.machine AS mc ON e.event_id = mc.event_id \
         JOIN s.session_live AS sl ON e.event_id = sl.event_id \
         ORDER BY e.event_id",
    ] {
        assert_eq!(rows(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn a_single_join_reads_a_table_of_the_second_file() {
    let f = Fixture::new();
    let mut e = f.engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT events.event_id, session_live.user FROM session_live \
             JOIN events ON events.event_id = session_live.event_id ORDER BY events.event_id"
        ),
        vec![
            vec![Cell::Int(5), text("u5")],
            vec![Cell::Int(9), text("u9")],
            vec![Cell::Int(10), text("u10")],
        ]
    );
}

#[test]
fn an_ambiguous_table_name_names_both_files_unless_qualified() {
    let f = Fixture::new();
    let mut e = f.engine();
    let (kind, message) = error(
        &mut e,
        "SELECT COUNT(*) FROM events JOIN region ON events.channel_id = region.id",
    );
    assert_eq!(kind, ErrorKind::Compile, "{message}");
    assert!(
        message.contains("ambiguous table name: region"),
        "{message}"
    );
    for file in ["machine.db", "session.db"] {
        assert!(message.contains(file), "{message}");
    }
    let names = |e: &mut CrossModeEngine, file: &str| {
        rows(
            e,
            &format!(
                "SELECT region.name, COUNT(*) FROM events JOIN {file}.region \
                 ON events.channel_id = region.id GROUP BY region.name ORDER BY region.name"
            ),
        )
    };
    assert_eq!(
        names(&mut e, "m"),
        vec![
            vec![text("m-one"), Cell::Int(5)],
            vec![text("m-two"), Cell::Int(5)],
        ]
    );
    assert_eq!(names(&mut e, "S"), vec![vec![text("s-one"), Cell::Int(5)]]);
}

#[test]
fn a_qualifier_names_a_file_that_has_the_table() {
    let f = Fixture::new();
    let mut e = f.engine();
    let (kind, message) = error(
        &mut e,
        "SELECT COUNT(*) FROM events JOIN x.machine ON events.event_id = machine.event_id",
    );
    assert_eq!(kind, ErrorKind::Compile);
    assert!(message.contains("no such table: x.machine"), "{message}");
    let (kind, message) = error(
        &mut e,
        "SELECT COUNT(*) FROM events JOIN s.machine ON events.event_id = machine.event_id",
    );
    assert_eq!(kind, ErrorKind::Compile);
    assert!(message.contains("no such table: s.machine"), "{message}");
}

#[test]
fn one_query_reads_a_table_name_from_one_file() {
    let f = Fixture::new();
    let mut e = f.engine();
    let (kind, message) = error(
        &mut e,
        "SELECT COUNT(*) FROM events JOIN m.region AS a ON events.channel_id = a.id \
         JOIN s.region AS b ON events.channel_id = b.id",
    );
    assert_eq!(kind, ErrorKind::Unsupported, "{message}");
    assert!(message.contains("table region of both"), "{message}");
}

#[test]
fn explain_query_plan_labels_each_sqlite_table_with_its_file() {
    let f = Fixture::new();
    let e = f.engine();
    let plan: Vec<String> = e
        .explain_plan(
            "SELECT COUNT(*) FROM events \
             JOIN machine ON events.event_id = machine.event_id \
             JOIN s.session_live ON events.event_id = session_live.event_id",
        )
        .unwrap()
        .into_iter()
        .map(|r| r.detail)
        .collect();
    let scan = |table: &str| {
        plan.iter()
            .find(|d| d.starts_with(&format!("SCAN {table} (")))
            .unwrap_or_else(|| panic!("no SCAN {table}: {plan:?}"))
            .clone()
    };
    let machine = scan("machine");
    assert!(
        machine.ends_with(&format!("[sqlite {}]", f.path("machine.db").display())),
        "{machine}"
    );
    let session = scan("session_live");
    assert!(
        session.ends_with(&format!("[sqlite {}]", f.path("session.db").display())),
        "{session}"
    );
}

#[test]
fn tables_lists_the_tables_of_every_file() {
    let f = Fixture::new();
    let e = f.engine();
    let names: Vec<String> = e.tables().unwrap().into_iter().map(|t| t.name).collect();
    for table in ["events", "machine", "session_live"] {
        assert!(names.iter().any(|n| n == table), "{names:?}");
    }
    assert_eq!(
        names.iter().filter(|n| *n == "region").count(),
        2,
        "{names:?}"
    );
}

#[test]
fn the_sqlite_files_are_at_least_one_and_named_apart() {
    let f = Fixture::new();
    let events = f.path("events");
    let machine = f.path("machine.db");
    let session = f.path("session.db");
    let none = CrossModeEngine::open_column_tables_sqlite(&[events.as_path()], &[]);
    assert_eq!(none.unwrap_err().kind, ErrorKind::Open);
    let twice = CrossModeEngine::open_column_tables_sqlite(
        &[events.as_path()],
        &[("a", machine.as_path()), ("A", session.as_path())],
    );
    let err = twice.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Open);
    assert!(
        err.to_string().contains("two SQLite files are named A"),
        "{err}"
    );
}

#[test]
fn a_single_file_engine_qualifies_its_tables_with_the_file_stem() {
    let f = Fixture::new();
    let mut e =
        CrossModeEngine::open_table_sqlite(&f.path("events"), &f.path("machine.db")).expect("open");
    assert_eq!(
        rows(
            &mut e,
            "SELECT COUNT(*) FROM events JOIN machine.machine ON events.event_id = machine.event_id"
        ),
        vec![vec![Cell::Int(3)]]
    );
}

#[test]
fn a_qualified_table_outside_a_cross_mode_engine_is_unknown() {
    let f = Fixture::new();
    let mut table = TableEngine::open(&f.path("events")).unwrap();
    let err = table
        .run_query("SELECT COUNT(*) FROM m.events")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.to_string().contains("unknown table: m.events"), "{err}");

    // The row engine names its own file `main` (#924, ADR 0043).
    let mut row = RowEngine::open(&f.path("machine.db")).unwrap();
    let hosts = row.run_query("SELECT host FROM main.machine").unwrap().rows;
    assert_eq!(hosts.len(), 3);
}
