// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for a star join of two Parquet tables and a SQLite
//! lookup table (#782): `CrossModeEngine::open_column_tables_sqlite`. The
//! larger Parquet table probes whatever order the query writes them in, the
//! other builds in memory, and a filter on the built one prunes the probe's
//! row groups. Every result is checked against a reference computed here
//! from the same rows.
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

use db_core::engine::resolve::CrossModeEngine;
use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine, ErrorKind};
use db_core::storage::parquet::writer::{
    ColumnSpec, ColumnType, ColumnValues, Compression, ParquetWriter, WriterOptions,
};

const ROW_FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

/// One session of `events`: its id (`None` for a NULL key) and channel.
fn event_rows(n: i64) -> Vec<(Option<i64>, i64)> {
    (1..=n)
        .map(|i| ((i % 50 != 0).then_some(i), i % 4))
        .collect()
}

/// `transactions`: two rows for each session 1..=`sessions` (so the build
/// side has duplicate keys), amounts 0.0, 1.0, ..., and a NULL key row.
fn transaction_rows(sessions: i64) -> Vec<(Option<i64>, f64)> {
    let mut rows: Vec<(Option<i64>, f64)> = (0..sessions * 2)
        .map(|j| (Some(j / 2 + 1), j as f64))
        .collect();
    rows.push((None, 1_000_000.0));
    rows
}

fn write_parquet(
    path: &Path,
    schema: Vec<ColumnSpec>,
    row_group_rows: usize,
    batches: Vec<Vec<ColumnValues>>,
) {
    let options = WriterOptions {
        compression: Compression::Uncompressed,
        row_group_rows,
        dictionary: false,
        ..WriterOptions::default()
    };
    let mut writer = ParquetWriter::new(Vec::new(), schema, options).unwrap();
    for batch in batches {
        writer.write_columns(batch).unwrap();
    }
    std::fs::write(path, writer.finish().unwrap().0).unwrap();
}

/// The data of one session of the tests: files and a SQLite lookup table,
/// removed on drop.
struct Dataset {
    dir: PathBuf,
    events: Vec<(Option<i64>, i64)>,
    transactions: Vec<(Option<i64>, f64)>,
}

impl Dataset {
    /// `events` of `event_count` sessions in row groups of 200, and
    /// `transactions` for the first `tx_sessions` of them, in groups of 250.
    fn new(label: &str, event_count: i64, tx_sessions: i64) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "db-core-column-tables-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let events = event_rows(event_count);
        let transactions = transaction_rows(tx_sessions);

        let schema = vec![
            ColumnSpec::new("session_id", ColumnType::Int64, true),
            ColumnSpec::new("channel_id", ColumnType::Int64, false),
        ];
        let batches = events
            .chunks(200)
            .map(|chunk| {
                vec![
                    ColumnValues::Int64(chunk.iter().map(|e| e.0).collect()),
                    ColumnValues::Int64(chunk.iter().map(|e| Some(e.1)).collect()),
                ]
            })
            .collect();
        write_parquet(&dir.join("events.parquet"), schema, 200, batches);

        let schema = vec![
            ColumnSpec::new("session_id", ColumnType::Int64, true),
            ColumnSpec::new("amount", ColumnType::Double, false),
        ];
        let batches = transactions
            .chunks(250)
            .map(|chunk| {
                vec![
                    ColumnValues::Int64(chunk.iter().map(|t| t.0).collect()),
                    ColumnValues::Double(chunk.iter().map(|t| Some(t.1)).collect()),
                ]
            })
            .collect();
        write_parquet(&dir.join("transactions.parquet"), schema, 250, batches);

        std::fs::copy(ROW_FIXTURE, dir.join("lookup.db")).unwrap();
        RowEngine::open(&dir.join("lookup.db"))
            .unwrap()
            .run_query(
                "CREATE TABLE channels(channel_id INTEGER PRIMARY KEY, name TEXT);\
                 INSERT INTO channels VALUES (0, 'web');\
                 INSERT INTO channels VALUES (1, 'app');\
                 INSERT INTO channels VALUES (2, 'store');\
                 INSERT INTO channels VALUES (3, 'phone');",
            )
            .unwrap();
        Dataset {
            dir,
            events,
            transactions,
        }
    }

    fn engine(&self) -> CrossModeEngine {
        // The files are named so either could be the first one opened.
        let events = self.dir.join("events.parquet");
        let transactions = self.dir.join("transactions.parquet");
        CrossModeEngine::open_column_tables_sqlite(
            &[events.as_path(), transactions.as_path()],
            &self.dir.join("lookup.db"),
        )
        .unwrap()
    }
}

impl Drop for Dataset {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
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

/// `STAR`'s answer from the rows: sessions with a key, matched to every
/// transaction of the same key (a NULL key matches nothing).
fn star_reference(data: &Dataset) -> Vec<Vec<Cell>> {
    let names = ["web", "app", "store", "phone"];
    let mut by_name: BTreeMap<&str, (i64, f64)> = BTreeMap::new();
    for (session, channel) in &data.events {
        let Some(session) = session else { continue };
        for (tx, amount) in &data.transactions {
            if *tx == Some(*session) {
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
fn a_star_of_two_parquet_tables_and_a_lookup_matches_the_reference() {
    let data = Dataset::new("star", 2000, 500);
    let mut e = data.engine();
    let want = star_reference(&data);
    assert_eq!(want.len(), 4, "every channel has matched sessions");
    assert_eq!(rows(&mut e, STAR), want);
}

#[test]
fn the_probe_is_the_larger_table_whatever_order_the_query_writes_them_in() {
    // events (2000 rows) is larger than transactions (1001): events probes.
    let data = Dataset::new("larger", 2000, 500);
    let mut e = data.engine();
    let events_first = "SELECT events.session_id, transactions.amount FROM events \
         JOIN transactions ON transactions.session_id = events.session_id \
         ORDER BY transactions.amount LIMIT 5";
    let transactions_first = "SELECT events.session_id, transactions.amount FROM transactions \
         JOIN events ON events.session_id = transactions.session_id \
         ORDER BY transactions.amount LIMIT 5";
    let a = rows(&mut e, events_first);
    assert_eq!(a, rows(&mut e, transactions_first));
    assert_eq!(a.len(), 5);
    for sql in [events_first, transactions_first] {
        let sides = plan(&data.engine(), sql);
        assert!(
            sides.iter().any(|d| d.starts_with(
                "JOIN SIDES: PROBE events (~2000 rows), BUILD transactions (~1001 rows)"
            )),
            "{sql}: {sides:?}"
        );
    }

    // Now transactions (1001 rows) is the larger one: it probes.
    let data = Dataset::new("larger-tx", 300, 500);
    let mut e = data.engine();
    let b = rows(&mut e, events_first);
    assert_eq!(b, rows(&mut e, transactions_first));
    assert_eq!(b.len(), 5);
    let sides = plan(&data.engine(), events_first);
    assert!(
        sides.iter().any(|d| d
            .starts_with("JOIN SIDES: PROBE transactions (~1001 rows), BUILD events (~300 rows)")),
        "{sides:?}"
    );
}

#[test]
fn the_result_does_not_depend_on_which_table_probes() {
    let sql = "SELECT count(*), sum(transactions.amount), sum(events.channel_id) FROM events \
               JOIN transactions ON transactions.session_id = events.session_id";
    // The same join, once with events probing and once with transactions.
    let small_events = Dataset::new("swap-a", 300, 500);
    let large_events = Dataset::new("swap-b", 2000, 500);
    for data in [&small_events, &large_events] {
        let mut want_n = 0i64;
        let mut want_sum = 0f64;
        let mut want_channels = 0i64;
        for (session, channel) in &data.events {
            let Some(session) = session else { continue };
            for (tx, amount) in &data.transactions {
                if *tx == Some(*session) {
                    want_n += 1;
                    want_sum += amount;
                    want_channels += channel;
                }
            }
        }
        let mut e = data.engine();
        assert_eq!(
            rows(&mut e, sql),
            vec![vec![
                Cell::Int(want_n),
                Cell::Real(want_sum),
                Cell::Int(want_channels)
            ]]
        );
    }
}

#[test]
fn a_filter_on_the_built_table_prunes_the_probes_row_groups() {
    let data = Dataset::new("prune", 2000, 500);
    let mut e = data.engine();
    // Session 7 only: one of events' ten row groups can hold it.
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

    // A filter that no built row passes leaves an inner join empty.
    let none = "SELECT events.session_id FROM events \
                JOIN transactions ON transactions.session_id = events.session_id \
                WHERE transactions.amount > 9999999";
    assert!(rows(&mut e, none).is_empty());
    let lines = plan(&e, none);
    assert!(
        lines.iter().any(|d| d.contains("FILTER (0 of 10 kept)")),
        "{lines:?}"
    );

    // A range filter keeps the groups that can hold a matching key.
    let range = "SELECT count(*) FROM events \
                 JOIN transactions ON transactions.session_id = events.session_id \
                 WHERE transactions.amount < 10";
    let mut expected = 0;
    for (session, _) in &data.events {
        let Some(session) = session else { continue };
        expected += data
            .transactions
            .iter()
            .filter(|(tx, amount)| *tx == Some(*session) && *amount < 10.0)
            .count() as i64;
    }
    assert_eq!(rows(&mut e, range), vec![vec![Cell::Int(expected)]]);
}

#[test]
fn a_left_join_keeps_the_from_table_as_the_probe_and_its_unmatched_rows() {
    // transactions is larger here, but the LEFT JOIN's left side must probe.
    let data = Dataset::new("left", 300, 500);
    let mut e = data.engine();
    let sql = "SELECT count(*), count(transactions.amount) FROM events \
               LEFT JOIN transactions ON transactions.session_id = events.session_id";
    let mut total = 0i64;
    let mut matched = 0i64;
    for (session, _) in &data.events {
        let n = data
            .transactions
            .iter()
            .filter(|(tx, _)| session.is_some() && *tx == *session)
            .count() as i64;
        total += n.max(1);
        matched += n;
    }
    assert_eq!(
        rows(&mut e, sql),
        vec![vec![Cell::Int(total), Cell::Int(matched)]]
    );
    let sides = plan(&data.engine(), sql);
    assert!(
        sides
            .iter()
            .any(|d| d.starts_with("JOIN SIDES: PROBE events ")),
        "{sides:?}"
    );

    // With the larger table on the preserved side, it probes as written.
    let swapped = "SELECT count(*), count(events.session_id) FROM transactions \
                   LEFT JOIN events ON events.session_id = transactions.session_id";
    let mut total = 0i64;
    let mut matched = 0i64;
    for (tx, _) in &data.transactions {
        let n = data
            .events
            .iter()
            .filter(|(session, _)| tx.is_some() && *session == *tx)
            .count() as i64;
        total += n.max(1);
        matched += n;
    }
    assert_eq!(
        rows(&mut e, swapped),
        vec![vec![Cell::Int(total), Cell::Int(matched)]]
    );
}

#[test]
fn a_join_that_is_not_a_star_around_a_parquet_table_is_refused() {
    let data = Dataset::new("chain", 400, 100);
    let mut e = data.engine();
    // channels joins transactions, not events, and events joins channels:
    // a chain events - channels - transactions, no star centre among the
    // Parquet tables.
    let sql = "SELECT count(*) FROM events \
               JOIN channels ON events.channel_id = channels.channel_id \
               JOIN transactions ON transactions.session_id = channels.channel_id";
    let err = e.run_query(sql).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");
    assert!(err.message.contains("star"), "{err}");
}

#[test]
fn one_parquet_table_and_a_lookup_still_work_through_the_tables_constructor() {
    let data = Dataset::new("single", 400, 100);
    let events = data.dir.join("events.parquet");
    let mut e = CrossModeEngine::open_column_tables_sqlite(
        &[events.as_path()],
        &data.dir.join("lookup.db"),
    )
    .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT channels.name, count(*) FROM events \
             JOIN channels ON events.channel_id = channels.channel_id \
             GROUP BY channels.name ORDER BY channels.name",
        ),
        vec![
            vec![Cell::Text("app".into()), Cell::Int(100)],
            vec![Cell::Text("phone".into()), Cell::Int(100)],
            vec![Cell::Text("store".into()), Cell::Int(100)],
            vec![Cell::Text("web".into()), Cell::Int(100)],
        ]
    );
    let none = CrossModeEngine::open_column_tables_sqlite(&[], &data.dir.join("lookup.db"));
    assert!(none.is_err());
}

#[test]
fn explain_opcodes_label_the_built_parquet_table() {
    let data = Dataset::new("opcodes", 2000, 500);
    let e = data.engine();
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
}

#[test]
fn tables_lists_every_parquet_file_and_the_lookup_tables() {
    let data = Dataset::new("tables", 400, 100);
    let names: Vec<String> = data
        .engine()
        .tables()
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    for want in ["events", "transactions", "channels"] {
        assert!(names.contains(&want.to_string()), "{want}: {names:?}");
    }
}

// MC/DC vectors for `sides::choose_probe` (obligation
// `engine_resolve_sides_choose_probe_d959e87b`, `!allowed || !is_star_around`).

#[test]
#[allow(non_snake_case)]
fn mcdc__engine_resolve_sides_choose_probe_d959e87b__v1_a_left_joined_table_cannot_probe() {
    let data = Dataset::new("mcdc-v1", 300, 100);
    let mut e = data.engine();
    // `events` is the right side of a LEFT JOIN, so only `transactions`
    // (the FROM table) may probe.
    let sql = "SELECT count(*) FROM transactions \
               LEFT JOIN events ON events.session_id = transactions.session_id";
    assert!(rows(&mut e, sql).len() == 1);
    assert!(plan(&data.engine(), sql)
        .iter()
        .any(|d| d.starts_with("JOIN SIDES: PROBE transactions ")),);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__engine_resolve_sides_choose_probe_d959e87b__v2_an_inner_star_may_probe() {
    let data = Dataset::new("mcdc-v2", 300, 100);
    let mut e = data.engine();
    let sql = "SELECT count(*) FROM events \
               JOIN transactions ON transactions.session_id = events.session_id";
    assert!(rows(&mut e, sql).len() == 1);
    assert!(plan(&data.engine(), sql)
        .iter()
        .any(|d| d.starts_with("JOIN SIDES: PROBE events ")),);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__engine_resolve_sides_choose_probe_d959e87b__v3_an_inner_chain_has_no_probe() {
    let data = Dataset::new("mcdc-v3", 300, 100);
    let mut e = data.engine();
    let sql = "SELECT count(*) FROM events \
               JOIN channels ON events.channel_id = channels.channel_id \
               JOIN transactions ON transactions.session_id = channels.channel_id";
    assert_eq!(e.run_query(sql).unwrap_err().kind, ErrorKind::Unsupported);
}
