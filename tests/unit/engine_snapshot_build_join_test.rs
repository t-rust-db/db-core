// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for table directories next to Parquet files in a star
//! join (#782): `CrossModeEngine::open_column_tables_sqlite` with a
//! directory among its paths. The largest table the query is a star around
//! probes and the others build in memory, whether each is a Parquet file or
//! a directory read as a fresh snapshot (archive files and live pages
//! alike). Every result is checked against a reference from the same rows.
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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use db_core::engine::resolve::CrossModeEngine;
use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine, FileStats};
use db_core::storage::ingest::{Ack, RecordValue, TableOptions, TableWriter};
use db_core::storage::parquet::schema::{ColumnSpec, ColumnType, ColumnValues};
use db_core::storage::parquet::writer::{Compression, ParquetWriter, WriterOptions};

const ROW_FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

/// `events` as a Parquet file (`event_count` sessions, ten row groups when
/// 2000), `transactions` as a table directory (two rows for each of the
/// first `tx_sessions` sessions: the first `flushed` sessions in archive
/// files of 100 sessions, the rest still in the live page), and a SQLite
/// `channels` table, in a scratch directory removed on drop.
struct Fixture {
    root: PathBuf,
    events: Vec<(i64, i64)>,
    transactions: Vec<(i64, f64)>,
    writer: TableWriter,
}

impl Fixture {
    fn new(event_count: i64, tx_sessions: i64, flushed: i64) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("db-core-snapshot-build-{}-{n}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();

        let events: Vec<(i64, i64)> = (1..=event_count).map(|i| (i, i % 4)).collect();
        let options = WriterOptions {
            compression: Compression::Uncompressed,
            row_group_rows: 200,
            dictionary: false,
            ..WriterOptions::default()
        };
        let schema = vec![
            ColumnSpec::new("session_id", ColumnType::Int64, false),
            ColumnSpec::new("channel_id", ColumnType::Int64, false),
        ];
        let mut parquet = ParquetWriter::new(Vec::new(), schema, options).unwrap();
        for chunk in events.chunks(200) {
            parquet
                .write_columns(vec![
                    ColumnValues::Int64(chunk.iter().map(|e| Some(e.0)).collect()),
                    ColumnValues::Int64(chunk.iter().map(|e| Some(e.1)).collect()),
                ])
                .unwrap();
        }
        std::fs::write(root.join("events.parquet"), parquet.finish().unwrap().0).unwrap();

        let transactions: Vec<(i64, f64)> = (0..tx_sessions * 2)
            .map(|j| (j / 2 + 1, j as f64))
            .collect();
        let mut writer = TableWriter::open(
            &root.join("transactions"),
            TableOptions {
                schema: Some(vec![
                    ColumnSpec::new("session_id", ColumnType::Int64, false),
                    ColumnSpec::new("amount", ColumnType::Double, false),
                ]),
                ..TableOptions::default()
            },
        )
        .unwrap();
        for (i, (session, amount)) in transactions.iter().enumerate() {
            // Only the last append waits for its fsync: commits are ordered,
            // so it makes every earlier one durable too.
            let ack = if i + 1 == transactions.len() {
                Ack::Durable
            } else {
                Ack::Memory
            };
            writer
                .append_row(
                    vec![RecordValue::Int(*session), RecordValue::Float(*amount)],
                    ack,
                )
                .unwrap();
            // An archive file per 100 sessions, up to `flushed` sessions.
            let done = (i as i64 + 1) % 200 == 0 && (i as i64 + 1) / 2 <= flushed;
            if done {
                writer.flush_all().unwrap();
            }
        }

        std::fs::copy(ROW_FIXTURE, root.join("lookup.db")).unwrap();
        RowEngine::open(&root.join("lookup.db"))
            .unwrap()
            .run_query(
                "CREATE TABLE channels(channel_id INTEGER PRIMARY KEY, name TEXT);\
                 INSERT INTO channels VALUES (0, 'web');\
                 INSERT INTO channels VALUES (1, 'app');\
                 INSERT INTO channels VALUES (2, 'store');\
                 INSERT INTO channels VALUES (3, 'phone');",
            )
            .unwrap();
        Fixture {
            root,
            events,
            transactions,
            writer,
        }
    }

    fn events(&self) -> PathBuf {
        self.root.join("events.parquet")
    }

    fn transactions(&self) -> PathBuf {
        self.root.join("transactions")
    }

    fn lookup(&self) -> PathBuf {
        self.root.join("lookup.db")
    }

    fn engine(&self, paths: &[&Path]) -> CrossModeEngine {
        CrossModeEngine::open_column_tables_sqlite(paths, &self.lookup()).unwrap()
    }

    /// The Parquet file first, then the directory.
    fn mixed(&self) -> CrossModeEngine {
        self.engine(&[&self.events(), &self.transactions()])
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
    }
}

fn rows(e: &mut CrossModeEngine, sql: &str) -> Vec<Vec<Cell>> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
}

fn plan(e: &CrossModeEngine, sql: &str) -> Vec<String> {
    e.explain_plan(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .into_iter()
        .map(|r| r.detail)
        .collect()
}

const STAR: &str = "SELECT channels.name, count(*), sum(transactions.amount) \
                    FROM events \
                    JOIN transactions ON transactions.session_id = events.session_id \
                    JOIN channels ON events.channel_id = channels.channel_id \
                    GROUP BY channels.name ORDER BY channels.name";

fn star_reference(f: &Fixture) -> Vec<Vec<Cell>> {
    let names = ["web", "app", "store", "phone"];
    let mut by_name: BTreeMap<&str, (i64, f64)> = BTreeMap::new();
    for (session, channel) in &f.events {
        for (tx, amount) in &f.transactions {
            if tx == session {
                let slot = by_name.entry(names[*channel as usize]).or_default();
                slot.0 += 1;
                slot.1 += amount;
            }
        }
    }
    by_name
        .into_iter()
        .map(|(name, (n, sum))| vec![Cell::Text(name.into()), Cell::Int(n), Cell::Real(sum)])
        .collect()
}

#[test]
fn a_table_directory_builds_next_to_a_parquet_probe_and_a_lookup() {
    // events (2000 rows) probes; transactions (1000 rows: 800 archived in
    // four files, 200 in the live page) builds.
    let f = Fixture::new(2000, 500, 400);
    let mut e = f.mixed();
    assert_eq!(rows(&mut e, STAR), star_reference(&f));
    let sides = plan(&f.mixed(), STAR);
    assert!(
        sides.iter().any(|d| d
            .starts_with("JOIN SIDES: PROBE events (~2000 rows), BUILD transactions (~1000 rows)")),
        "{sides:?}"
    );
}

#[test]
fn the_probe_is_the_larger_table_whether_it_is_a_file_or_a_directory() {
    // transactions (1000 rows) is larger than events (300): the directory
    // probes and the Parquet file builds, in either written order.
    let f = Fixture::new(300, 500, 400);
    let mut e = f.mixed();
    let events_first = "SELECT count(*), sum(transactions.amount), sum(events.channel_id) \
                        FROM events JOIN transactions \
                        ON transactions.session_id = events.session_id";
    let tx_first = "SELECT count(*), sum(transactions.amount), sum(events.channel_id) \
                    FROM transactions JOIN events \
                    ON events.session_id = transactions.session_id";
    let (mut n, mut sum, mut channels) = (0i64, 0f64, 0i64);
    for (session, channel) in &f.events {
        for (tx, amount) in &f.transactions {
            if tx == session {
                n += 1;
                sum += amount;
                channels += channel;
            }
        }
    }
    let want = vec![vec![Cell::Int(n), Cell::Real(sum), Cell::Int(channels)]];
    assert_eq!(rows(&mut e, events_first), want);
    assert_eq!(rows(&mut e, tx_first), want);
    for sql in [events_first, tx_first] {
        let sides = plan(&f.mixed(), sql);
        assert!(
            sides.iter().any(|d| d.starts_with(
                "JOIN SIDES: PROBE transactions (~1000 rows), BUILD events (~300 rows)"
            )),
            "{sql}: {sides:?}"
        );
    }
}

#[test]
fn two_table_directories_join_and_the_larger_probes() {
    let f = Fixture::new(300, 500, 400);
    // The second directory is the same data, so the join is a self-match.
    let other = f.root.join("sessions");
    let mut w = TableWriter::open(
        &other,
        TableOptions {
            schema: Some(vec![
                ColumnSpec::new("session_id", ColumnType::Int64, false),
                ColumnSpec::new("score", ColumnType::Int64, false),
            ]),
            ..TableOptions::default()
        },
    )
    .unwrap();
    for id in 1..=100i64 {
        let ack = if id == 100 { Ack::Durable } else { Ack::Memory };
        w.append_row(vec![RecordValue::Int(id), RecordValue::Int(id * 10)], ack)
            .unwrap();
    }
    w.flush_all().unwrap();
    let mut e = f.engine(&[&other, &f.transactions()]);
    let got = rows(
        &mut e,
        "SELECT count(*), sum(sessions.score) FROM sessions \
         JOIN transactions ON transactions.session_id = sessions.session_id",
    );
    // Two transactions for each of sessions 1..=100.
    assert_eq!(got, vec![vec![Cell::Int(200), Cell::Int(2 * 10 * 5050)]]);
    let sides = plan(&e, "SELECT count(*) FROM sessions JOIN transactions ON transactions.session_id = sessions.session_id");
    assert!(
        sides
            .iter()
            .any(|d| d.starts_with("JOIN SIDES: PROBE transactions ")),
        "{sides:?}"
    );
}

#[test]
fn a_filter_on_a_built_directory_prunes_the_probes_row_groups() {
    let f = Fixture::new(2000, 500, 400);
    let mut e = f.mixed();
    // Session 7: one of events' ten row groups holds it.
    let sql = "SELECT events.session_id, transactions.amount FROM events \
               JOIN transactions ON transactions.session_id = events.session_id \
               WHERE transactions.session_id = 7 ORDER BY transactions.amount";
    assert_eq!(
        rows(&mut e, sql),
        vec![
            vec![Cell::Int(7), Cell::Real(12.0)],
            vec![Cell::Int(7), Cell::Real(13.0)]
        ]
    );
    let lines = plan(&e, sql);
    assert!(
        lines
            .iter()
            .any(|d| d
                .starts_with("PRUNE ROW GROUPS OF events BY transactions FILTER (1 of 10 kept)")),
        "{lines:?}"
    );
    // A filter no built row passes empties an inner join.
    let none = "SELECT events.session_id FROM events \
                JOIN transactions ON transactions.session_id = events.session_id \
                WHERE transactions.amount > 9999999";
    assert!(rows(&mut e, none).is_empty());
}

#[test]
fn rows_in_the_directorys_live_page_take_part_and_later_appends_are_seen() {
    let mut f = Fixture::new(2000, 500, 0);
    // Nothing flushed: every transaction is in the live page.
    let mut e = f.mixed();
    assert_eq!(rows(&mut e, STAR), star_reference(&f));

    // Another session's transactions, appended after the engine opened, are
    // read by the next query.
    f.writer
        .append_row(
            vec![RecordValue::Int(600), RecordValue::Float(5.0)],
            Ack::Durable,
        )
        .unwrap();
    f.transactions.push((600, 5.0));
    assert_eq!(rows(&mut e, STAR), star_reference(&f));
}

#[test]
fn a_left_join_keeps_the_from_table_as_the_probe() {
    let f = Fixture::new(300, 100, 100);
    let mut e = f.mixed();
    let sql = "SELECT count(*), count(transactions.amount) FROM events \
               LEFT JOIN transactions ON transactions.session_id = events.session_id";
    let mut total = 0i64;
    let mut matched = 0i64;
    for (session, _) in &f.events {
        let n = f
            .transactions
            .iter()
            .filter(|(tx, _)| tx == session)
            .count() as i64;
        total += n.max(1);
        matched += n;
    }
    assert_eq!(
        rows(&mut e, sql),
        vec![vec![Cell::Int(total), Cell::Int(matched)]]
    );
    assert!(plan(&f.mixed(), sql)
        .iter()
        .any(|d| d.starts_with("JOIN SIDES: PROBE events ")));
}

#[test]
fn explain_opcodes_stats_and_tables_cover_every_table() {
    let f = Fixture::new(2000, 500, 400);
    let e = f.mixed();
    let sections = e
        .explain_opcodes(
            "SELECT count(*) FROM events \
             JOIN transactions ON transactions.session_id = events.session_id",
        )
        .unwrap();
    let labels: Vec<&str> = sections.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "JOIN build (parquet transactions)",
            "JOIN probe (parquet events)",
            "JOIN body"
        ]
    );
    let names: Vec<String> = e.tables().unwrap().into_iter().map(|t| t.name).collect();
    for want in ["events", "transactions", "channels"] {
        assert!(names.contains(&want.to_string()), "{want}: {names:?}");
    }
    assert!(matches!(e.stats(), FileStats::Batch { .. }));
    // The directory first reports the directory's stats.
    let e = f.engine(&[&f.transactions(), &f.events()]);
    assert!(!matches!(e.stats(), FileStats::Stream { .. }));
}

#[test]
fn all_files_still_open_the_parquet_only_session() {
    let f = Fixture::new(300, 100, 100);
    // No directory among the paths: the earlier session shape, unchanged.
    let e = f.engine(&[&f.events()]);
    assert!(e.tables().unwrap().iter().any(|t| t.name == "events"));
    assert!(CrossModeEngine::open_column_tables_sqlite(&[], &f.lookup()).is_err());
}
