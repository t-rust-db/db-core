// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for a table directory joined to SQLite lookup tables
//! (#772): `CrossModeEngine::open_table_sqlite`. The directory's archive
//! files and live pages are the probe side and the SQLite tables always
//! build, wherever the query writes them, as for a Parquet file (#598).
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
use std::sync::atomic::{AtomicU64, Ordering};

use db_core::engine::resolve::CrossModeEngine;
use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine, ErrorKind};
use db_core::storage::ingest::{Ack, RecordValue, TableOptions, TableWriter};
use db_core::storage::parquet::schema::{ColumnSpec, ColumnType};

const ROW_FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

/// A scratch directory holding the `events` table directory and a writable
/// copy of the SQLite fixture seeded with lookup tables, removed on drop.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("db-core-table-join-{}-{n}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::copy(ROW_FIXTURE, root.join("lookup.db")).expect("copy fixture");
        let mut lookup = RowEngine::open(&root.join("lookup.db")).expect("open fixture copy");
        lookup
            .run_query(
                "CREATE TABLE event_def(event_id INTEGER PRIMARY KEY, name TEXT);\
                 CREATE TABLE channels(channel_id INTEGER PRIMARY KEY, channel TEXT);\
                 INSERT INTO channels VALUES (1, 'web');\
                 INSERT INTO channels VALUES (2, 'app');",
            )
            .expect("seed lookup tables");
        Fixture { root }
    }

    fn dir(&self) -> PathBuf {
        self.root.join("events")
    }

    fn lookup(&self) -> PathBuf {
        self.root.join("lookup.db")
    }

    fn define(&self, rows: &[(i64, &str)]) {
        let mut lookup = RowEngine::open(&self.lookup()).unwrap();
        for (id, name) in rows {
            lookup
                .run_query(&format!("INSERT INTO event_def VALUES ({id}, '{name}')"))
                .unwrap();
        }
    }

    fn engine(&self) -> CrossModeEngine {
        CrossModeEngine::open_table_sqlite(&self.dir(), &self.lookup()).expect("open")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
    }
}

fn schema() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("event_id", ColumnType::Int64, false),
        ColumnSpec::new("channel_id", ColumnType::Int64, false),
        ColumnSpec::new("n", ColumnType::Int64, false),
    ]
}

fn writer(dir: &Path, schema: Vec<ColumnSpec>) -> TableWriter {
    TableWriter::open(
        dir,
        TableOptions {
            schema: Some(schema),
            ..TableOptions::default()
        },
    )
    .unwrap()
}

/// Appends the rows `ids` (`channel_id` alternating 1, 2; `n` = the id),
/// then flushes them to their own archive file when `flush` is set.
fn append(w: &mut TableWriter, ids: std::ops::RangeInclusive<i64>, flush: bool) {
    for id in ids {
        w.append_row(
            vec![
                RecordValue::Int(id),
                RecordValue::Int(id % 2 + 1),
                RecordValue::Int(id),
            ],
            Ack::Durable,
        )
        .unwrap();
    }
    if flush {
        w.flush_all().unwrap();
    }
}

/// `events` with four archive files of ten events each and the live page
/// holding ids 41..=45.
fn four_files_and_a_page(f: &Fixture) -> TableWriter {
    let mut w = writer(&f.dir(), schema());
    for start in [1, 11, 21, 31] {
        append(&mut w, start..=start + 9, true);
    }
    append(&mut w, 41..=45, false);
    w
}

fn rows(e: &mut CrossModeEngine, sql: &str) -> Vec<Vec<Cell>> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
}

fn text(s: &str) -> Cell {
    Cell::Text(s.into())
}

fn plan(e: &CrossModeEngine, sql: &str) -> Vec<String> {
    e.explain_plan(sql)
        .unwrap()
        .into_iter()
        .map(|r| r.detail)
        .collect()
}

#[test]
fn a_plain_join_is_inner_over_archive_files_and_live_pages() {
    let f = Fixture::new();
    let _w = four_files_and_a_page(&f);
    f.define(&[
        (3, "three"),
        (25, "twenty-five"),
        (42, "forty-two"),
        (99, "nowhere"),
    ]);
    let mut e = f.engine();
    let got = rows(
        &mut e,
        "SELECT events.event_id, event_def.name FROM events \
         JOIN event_def ON events.event_id = event_def.event_id ORDER BY events.event_id",
    );
    assert_eq!(
        got,
        vec![
            vec![Cell::Int(3), text("three")],
            vec![Cell::Int(25), text("twenty-five")],
            vec![Cell::Int(42), text("forty-two")],
        ]
    );
}

#[test]
fn flushing_the_live_pages_does_not_change_the_result() {
    let f = Fixture::new();
    let mut w = four_files_and_a_page(&f);
    f.define(&[(5, "five"), (44, "forty-four")]);
    let sql = "SELECT events.event_id, event_def.name FROM events \
               JOIN event_def ON events.event_id = event_def.event_id ORDER BY events.event_id";
    let mut e = f.engine();
    let before = rows(&mut e, sql);
    assert_eq!(
        before.len(),
        2,
        "the row in the live page takes part: {before:?}"
    );
    w.flush_all().unwrap();
    assert_eq!(rows(&mut e, sql), before);
}

#[test]
fn a_left_join_keeps_every_probe_row_and_the_lookup_first_form_is_rejected() {
    let f = Fixture::new();
    let _w = four_files_and_a_page(&f);
    f.define(&[(1, "one"), (45, "forty-five")]);
    let mut e = f.engine();
    let got = rows(
        &mut e,
        "SELECT events.event_id, event_def.name FROM events \
         LEFT JOIN event_def ON events.event_id = event_def.event_id \
         ORDER BY events.event_id LIMIT 3",
    );
    assert_eq!(
        got,
        vec![
            vec![Cell::Int(1), text("one")],
            vec![Cell::Int(2), Cell::Null],
            vec![Cell::Int(3), Cell::Null],
        ]
    );
    let count = rows(
        &mut e,
        "SELECT COUNT(*) FROM events LEFT JOIN event_def ON events.event_id = event_def.event_id",
    );
    assert_eq!(count, vec![vec![Cell::Int(45)]]);

    let err = e
        .run_query(
            "SELECT event_def.name, events.event_id FROM event_def \
             LEFT JOIN events ON event_def.event_id = events.event_id",
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");
    assert!(err.message.contains("events LEFT JOIN"), "{err}");
}

#[test]
fn a_star_join_probes_several_lookup_tables() {
    let f = Fixture::new();
    let _w = four_files_and_a_page(&f);
    f.define(&[(2, "two"), (3, "three"), (43, "forty-three")]);
    let mut e = f.engine();
    let got = rows(
        &mut e,
        "SELECT events.event_id, event_def.name, channels.channel FROM events \
         JOIN event_def ON events.event_id = event_def.event_id \
         JOIN channels ON events.channel_id = channels.channel_id ORDER BY events.event_id",
    );
    assert_eq!(
        got,
        vec![
            vec![Cell::Int(2), text("two"), text("web")],
            vec![Cell::Int(3), text("three"), text("app")],
            vec![Cell::Int(43), text("forty-three"), text("app")],
        ]
    );
}

#[test]
fn a_lookup_filter_skips_archive_row_groups_whose_keys_cannot_match() {
    let f = Fixture::new();
    let _w = four_files_and_a_page(&f);
    f.define(&[(5, "x"), (25, "y"), (42, "y"), (43, "z")]);
    let mut e = f.engine();
    let sql = "SELECT events.event_id FROM events \
               JOIN event_def ON events.event_id = event_def.event_id \
               WHERE event_def.name = 'y' ORDER BY events.event_id";
    // Only ids 25 (the third file) and 42 (a live page) match.
    assert_eq!(
        rows(&mut e, sql),
        vec![vec![Cell::Int(25)], vec![Cell::Int(42)]]
    );
    let lines = plan(&e, sql);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("PRUNE ROW GROUPS OF events BY event_def FILTER (1 of 4 kept)")),
        "the keys 25 and 42 keep only the file holding 25; 42 is in a live page: {lines:?}"
    );

    // A filter matching no lookup row leaves an inner join empty.
    let none =
        "SELECT COUNT(*) FROM events JOIN event_def ON events.event_id = event_def.event_id \
                WHERE event_def.name = 'nothing'";
    assert_eq!(rows(&mut e, none), vec![vec![Cell::Int(0)]]);
    assert!(plan(&e, none)
        .iter()
        .any(|l| l.contains("FILTER (0 of 4 kept)")));
}

#[test]
fn a_column_an_older_file_lacks_joins_as_null_and_is_never_pruned() {
    let f = Fixture::new();
    // The first file predates `channel_id`.
    let old = vec![
        ColumnSpec::new("event_id", ColumnType::Int64, false),
        ColumnSpec::new("n", ColumnType::Int64, false),
    ];
    let mut w = writer(&f.dir(), old);
    for id in 1..=5 {
        w.append_row(
            vec![RecordValue::Int(id), RecordValue::Int(id)],
            Ack::Durable,
        )
        .unwrap();
    }
    w.flush_all().unwrap();
    w.close().unwrap();
    let mut evolved = vec![
        ColumnSpec::new("event_id", ColumnType::Int64, false),
        ColumnSpec::new("n", ColumnType::Int64, false),
    ];
    evolved.push(ColumnSpec::new("channel_id", ColumnType::Int64, true));
    let mut w = writer(&f.dir(), evolved);
    for id in 6..=8 {
        w.append_row(
            vec![
                RecordValue::Int(id),
                RecordValue::Int(id),
                RecordValue::Int(id % 2 + 1),
            ],
            Ack::Durable,
        )
        .unwrap();
    }
    w.flush_all().unwrap();

    let mut e = f.engine();
    let sql = "SELECT events.event_id, channels.channel FROM events \
               JOIN channels ON events.channel_id = channels.channel_id \
               WHERE channels.channel = 'app' ORDER BY events.event_id";
    // The old file has no `channel_id`: its rows read NULL and match nothing.
    assert_eq!(rows(&mut e, sql), vec![vec![Cell::Int(7), text("app")]]);
    let lines = plan(&e, sql);
    assert!(
        lines.iter().any(|l| l.contains("(2 of 2 kept)")),
        "a file without the key column is never pruned: {lines:?}"
    );
}

#[test]
fn a_uint64_key_above_i64_max_joins_to_the_signed_row_key() {
    let f = Fixture::new();
    let schema = vec![
        ColumnSpec::new("event_id", ColumnType::UInt64, false),
        ColumnSpec::new("n", ColumnType::Int64, false),
    ];
    let mut w = writer(&f.dir(), schema);
    let keys = [5u64, 1 << 63, u64::MAX];
    for (n, k) in keys.iter().enumerate() {
        w.append_row(
            vec![RecordValue::UInt(*k), RecordValue::Int(n as i64)],
            Ack::Durable,
        )
        .unwrap();
        if n == 0 {
            w.flush_all().unwrap(); // the first in an archive file, the rest in a page
        }
    }
    f.define(&[(5, "five"), (1 << 63, "two63")]);
    let mut lookup = RowEngine::open(&f.lookup()).unwrap();
    lookup
        .run_query(&format!(
            "INSERT INTO event_def VALUES ({}, 'max')",
            u64::MAX as i64
        ))
        .unwrap();
    let mut e = f.engine();
    let got = rows(
        &mut e,
        "SELECT event_def.name FROM events JOIN event_def ON events.event_id = event_def.event_id \
         ORDER BY events.n",
    );
    assert_eq!(
        got,
        vec![vec![text("five")], vec![text("two63")], vec![text("max")]]
    );
}

#[test]
fn explain_plan_and_opcodes_work_for_the_join() {
    let f = Fixture::new();
    let _w = four_files_and_a_page(&f);
    f.define(&[(1, "one")]);
    let e = f.engine();
    let sql = "SELECT events.event_id, event_def.name FROM events \
               JOIN event_def ON events.event_id = event_def.event_id";
    let lines = plan(&e, sql);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("events") && l.contains("batch")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("event_def") && l.contains("sqlite")),
        "{lines:?}"
    );
    let sections = e.explain_opcodes(sql).unwrap();
    assert!(
        sections.len() >= 2,
        "build and probe sections: {sections:?}"
    );
}

#[test]
fn tables_and_stats_list_both_sides() {
    let f = Fixture::new();
    let _w = four_files_and_a_page(&f);
    let e = f.engine();
    let names: Vec<String> = e.tables().unwrap().into_iter().map(|t| t.name).collect();
    assert!(names.contains(&"events".to_string()), "{names:?}");
    assert!(names.contains(&"event_def".to_string()), "{names:?}");
    assert_eq!(e.mode(), db_core::engine::Mode::Cross);
    let _ = e.stats();
}
