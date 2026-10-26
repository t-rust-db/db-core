use super::*;
use crate::engine::Cell;
use crate::storage::ingest::layout::{DAY_NS, HOUR_NS};
use crate::storage::ingest::{
    Ack, Layout, RecordValue, SessionKey, TableOptions, TableWriter, TierOptions,
};
use crate::storage::parquet::writer::{ColumnSpec, ColumnType, WriterOptions};
use crate::test_rng::Rng;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

/// A table directory named `logs` in a fresh temp directory.
struct TempTable(PathBuf);

impl TempTable {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("db-core-table-engine-{}-{n}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        TempTable(root.join("logs"))
    }

    fn writer(&self) -> TableWriter {
        self.writer_with(TableOptions::default())
    }

    fn writer_with(&self, options: TableOptions) -> TableWriter {
        TableWriter::open(&self.0, options).unwrap()
    }
}

/// The tests' clock: one per test (see `test_clock`), starting at
/// 2026-10-11.
fn clock() -> i64 {
    crate::test_clock::now(20_737 * DAY_NS)
}

fn advance_clock(ns: i64) {
    crate::test_clock::set(clock() + ns);
}

fn append(w: &mut TableWriter, lines: &[&str]) {
    for l in lines {
        w.append(l.as_bytes(), Ack::Durable).unwrap();
    }
}

fn rows(w: &TableWriter, sql: &str) -> Vec<Vec<Cell>> {
    query_snapshot(&w.snapshot().unwrap(), "logs", sql)
        .unwrap()
        .rows
}

/// Rows or the error message: an unknown column must be unknown on both
/// sides alike.
fn answer(w: &TableWriter, sql: &str) -> Result<Vec<Vec<Cell>>, String> {
    query_snapshot(&w.snapshot().unwrap(), "logs", sql)
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

#[test]
fn queries_span_files_and_pages() {
    let t = TempTable::new();
    let mut w = t.writer();
    append(
        &mut w,
        &[r#"{"n":1,"level":"info"}"#, r#"{"n":2,"level":"error"}"#],
    );
    w.flush_all().unwrap();
    append(&mut w, &[r#"{"n":3,"level":"warn"}"#]);
    w.seal_head().unwrap();
    append(&mut w, &[r#"{"n":4,"level":"error"}"#]);
    assert_eq!(
        rows(&w, "SELECT COUNT(*), SUM(n), MIN(n), MAX(n) FROM logs"),
        vec![vec![
            Cell::Int(4),
            Cell::Int(10),
            Cell::Int(1),
            Cell::Int(4)
        ]]
    );
    assert_eq!(
        rows(
            &w,
            "SELECT severity_text, COUNT(*) FROM logs GROUP BY severity_text ORDER BY severity_text"
        ),
        vec![
            vec![text("ERROR"), Cell::Int(2)],
            vec![text("INFO"), Cell::Int(1)],
            vec![text("WARN"), Cell::Int(1)],
        ]
    );
    // Late materialization (WHERE plus a projection-only column) and a
    // top-N over a file and pages.
    assert_eq!(
        rows(
            &w,
            "SELECT raw FROM logs WHERE n >= 2 AND n <= 3 ORDER BY n"
        ),
        vec![
            vec![text(r#"{"n":2,"level":"error"}"#)],
            vec![text(r#"{"n":3,"level":"warn"}"#)],
        ]
    );
    assert_eq!(
        rows(&w, "SELECT n FROM logs ORDER BY n DESC LIMIT 2"),
        vec![vec![Cell::Int(4)], vec![Cell::Int(3)]]
    );
}

#[test]
fn a_column_missing_from_a_file_or_page_is_null() {
    let t = TempTable::new();
    let mut w = t.writer();
    append(&mut w, &[r#"{"a":1}"#]);
    w.flush_all().unwrap();
    append(&mut w, &[r#"{"b":"x"}"#]);
    w.flush_all().unwrap();
    append(&mut w, &[r#"{"a":3}"#, "not json"]);
    assert_eq!(
        rows(&w, "SELECT a, b FROM logs"),
        vec![
            vec![Cell::Int(1), Cell::Null],
            vec![Cell::Null, text("x")],
            vec![Cell::Int(3), Cell::Null],
            vec![Cell::Null, Cell::Null],
        ]
    );
    assert_eq!(
        rows(&w, "SELECT COUNT(*) FROM logs WHERE b IS NULL"),
        vec![vec![Cell::Int(3)]]
    );
    // The top-N's late materialization over a file lacking the column.
    assert_eq!(
        rows(
            &w,
            "SELECT b FROM logs WHERE a >= 1 ORDER BY a DESC LIMIT 1"
        ),
        vec![vec![Cell::Null]]
    );
}

#[test]
fn a_type_widened_after_a_flush_is_cast_and_never_pruned_wrongly() {
    let t = TempTable::new();
    let mut w = t.writer();
    // x: INT64 in files 1 and 3, a string in a page, so a string column.
    // z: INT64 in file 2, DOUBLE in file 3, so a DOUBLE column.
    append(&mut w, &[r#"{"x":5,"y":1}"#, r#"{"x":7,"y":2}"#]);
    w.flush_all().unwrap();
    append(&mut w, &[r#"{"y":3,"z":2}"#]);
    w.flush_all().unwrap();
    append(&mut w, &[r#"{"x":2,"y":4,"z":2.5}"#]);
    w.flush_all().unwrap();
    append(&mut w, &[r#"{"x":"high","y":5}"#]);
    assert_eq!(
        rows(&w, "SELECT x FROM logs ORDER BY y"),
        vec![
            vec![text("5")],
            vec![text("7")],
            vec![Cell::Null],
            vec![text("2")],
            vec![text("high")],
        ]
    );
    assert_eq!(
        rows(&w, "SELECT z FROM logs WHERE z IS NOT NULL ORDER BY y"),
        vec![vec![Cell::Real(2.0)], vec![Cell::Real(2.5)]]
    );
    // File 1's INT64 statistics say x <= 7, but the comparison is on
    // strings, where '5', '7' and '2' are all > '10'. Pruning by those
    // statistics would drop rows.
    assert_eq!(
        rows(&w, "SELECT COUNT(*) FROM logs WHERE x > '10'"),
        vec![vec![Cell::Int(4)]]
    );
    // Top-N bounds come only from columns stored with the table's type.
    assert_eq!(
        rows(&w, "SELECT x FROM logs ORDER BY x DESC LIMIT 1"),
        vec![vec![text("high")]]
    );
    assert_eq!(
        rows(&w, "SELECT y FROM logs ORDER BY y DESC LIMIT 2"),
        vec![vec![Cell::Int(5)], vec![Cell::Int(4)]]
    );
}

/// A random record: `i` makes `raw` unique; `n` is an integer, `f` a
/// float, `s` a string, `b` a boolean; `k` is an integer before line
/// `switch.0` and a string from it on, `g` an integer before `switch.1`
/// and a float from it on, and `m` any of the four, line by line. Each
/// may be missing; some lines are not JSON.
fn random_line(rng: &mut Rng, i: u64, switch: (u64, u64)) -> String {
    if rng.below(10) == 0 {
        return format!("plain text {i}");
    }
    let mut fields = vec![format!(r#""i":{i}"#)];
    let levels = ["debug", "info", "warn", "error"];
    if rng.below(2) == 0 {
        fields.push(format!(r#""level":"{}""#, levels[rng.below(4) as usize]));
    }
    if rng.below(3) > 0 {
        fields.push(format!(r#""n":{}"#, rng.below(100)));
    }
    if rng.below(2) == 0 {
        fields.push(format!(r#""f":{}.25"#, rng.below(50)));
    }
    if rng.below(2) == 0 {
        fields.push(format!(r#""s":"v{}""#, rng.below(5)));
    }
    if rng.below(2) == 0 {
        fields.push(format!(r#""b":{}"#, rng.below(2) == 0));
    }
    if rng.below(2) == 0 {
        fields.push(format!(r#""msg":"m{}""#, rng.below(3)));
    }
    if rng.below(2) == 0 {
        let k = rng.below(4);
        fields.push(if i < switch.0 {
            format!(r#""k":{k}"#)
        } else {
            format!(r#""k":"k{k}""#)
        });
    }
    if rng.below(2) == 0 {
        let g = rng.below(4);
        fields.push(if i < switch.1 {
            format!(r#""g":{g}"#)
        } else {
            format!(r#""g":{g}.5"#)
        });
    }
    match rng.below(5) {
        0 => fields.push(format!(r#""m":{}"#, rng.below(9))),
        1 => fields.push(format!(r#""m":{}.75"#, rng.below(9))),
        2 => fields.push(format!(r#""m":"m{}""#, rng.below(9))),
        3 => fields.push(format!(r#""m":{}"#, rng.below(2) == 0)),
        _ => {}
    }
    format!("{{{}}}", fields.join(","))
}

const INVARIANCE_QUERIES: [&str; 14] = [
    "SELECT COUNT(*), COUNT(n), SUM(n), MIN(f), MAX(f), COUNT(s), COUNT(b), SUM(g) FROM logs",
    "SELECT raw, i, n, f, s, b, k, g, m, severity, severity_text, message, timestamp \
     FROM logs ORDER BY raw",
    "SELECT s, COUNT(*), SUM(f) FROM logs GROUP BY s ORDER BY s",
    "SELECT k, COUNT(*) FROM logs GROUP BY k ORDER BY k",
    "SELECT m, COUNT(*) FROM logs GROUP BY m ORDER BY m",
    "SELECT raw FROM logs WHERE n > 50 ORDER BY raw",
    "SELECT raw, f FROM logs WHERE f >= 10 ORDER BY raw DESC LIMIT 7",
    "SELECT b, COUNT(*) FROM logs WHERE s IS NULL GROUP BY b ORDER BY b",
    "SELECT severity, MAX(n) FROM logs GROUP BY severity ORDER BY severity",
    // #633: a distinct value is counted once whichever file, page or row
    // group its copies landed in.
    "SELECT COUNT(DISTINCT n), COUNT(*) FROM logs",
    "SELECT s, COUNT(DISTINCT k), SUM(n) FROM logs GROUP BY s ORDER BY s",
    // #589: equality on sort keys skips files and row groups.
    "SELECT raw FROM logs WHERE s = 'v2' ORDER BY raw",
    "SELECT raw FROM logs WHERE n = 42 ORDER BY raw",
    "SELECT raw FROM logs WHERE n = 7 OR s = 'v0' OR s = 'v4' ORDER BY raw",
];

/// #679: in a record table, a TIMESTAMP column compared with a quoted
/// timestamp compares as a timestamp -- from pages and, after a flush,
/// from Parquet -- and a quoted value that is no timestamp is an error.
#[test]
fn a_timestamp_column_compares_with_a_quoted_timestamp() {
    let t = TempTable::new();
    let mut w = t.writer_with(TableOptions {
        schema: Some(vec![
            ColumnSpec::new("id", ColumnType::Int64, false),
            ColumnSpec::new("ts", ColumnType::TimestampMicros, true),
        ]),
        ..TableOptions::default()
    });
    let day = 1_767_225_600_000_000; // 2026-01-01T00:00:00Z in microseconds
    for (id, ts) in [(1, Some(day)), (2, Some(day + 86_400_000_000)), (3, None)] {
        let ts = ts.map_or(RecordValue::Null, RecordValue::Int);
        w.append_row(vec![RecordValue::Int(id), ts], Ack::Memory)
            .unwrap();
    }
    let ids = |w: &TableWriter| {
        rows(
            w,
            "SELECT id FROM logs WHERE ts >= '2026-01-01 12:00:00' ORDER BY id",
        )
    };
    assert_eq!(ids(&w), vec![vec![Cell::Int(2)]]);
    w.flush().unwrap();
    assert_eq!(ids(&w), vec![vec![Cell::Int(2)]]);
    assert!(answer(&w, "SELECT id FROM logs WHERE ts < 'soon'").is_err());
}

/// A record table's (#594) schema for the flush-timing test: unsigned,
/// decimal, binary and timestamp columns, which a flush writes as their
/// Parquet types and the engine reads back from both pages and files.
fn record_schema() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("id", ColumnType::UInt64, false),
        ColumnSpec::new("kind", ColumnType::Utf8, true),
        ColumnSpec::new("n", ColumnType::Int32, true),
        ColumnSpec::new(
            "amount",
            ColumnType::Decimal {
                precision: 10,
                scale: 2,
            },
            true,
        ),
        ColumnSpec::new("u", ColumnType::UInt32, true),
        ColumnSpec::new("blob", ColumnType::Binary, true),
        ColumnSpec::new("ts", ColumnType::TimestampNanos, false),
        ColumnSpec::new("f", ColumnType::Float, true),
        ColumnSpec::new("h", ColumnType::MapUtf8, true),
    ]
}

/// A random row of [`record_schema`]: `id` is `i`, the others random or
/// NULL. Amounts step by 0.25 and floats by 0.5, so sums are exact in any
/// order.
fn random_row(rng: &mut Rng, i: u64) -> Vec<RecordValue> {
    let maybe = |rng: &mut Rng, v: RecordValue| {
        if rng.below(4) == 0 {
            RecordValue::Null
        } else {
            v
        }
    };
    let kind = RecordValue::Str(format!("k{}", rng.below(4)));
    let n = RecordValue::Int(rng.below(100) as i64 - 50);
    let amount = RecordValue::Decimal(i128::from(rng.below(400) * 25));
    // Above i32::MAX half the time: read unsigned or it would sort wrong.
    let u = RecordValue::UInt(rng.below(2) * 4_000_000_000 + rng.below(9));
    let blob = RecordValue::Bytes(vec![0x80 | rng.below(4) as u8, 0xff]);
    let f = RecordValue::Float(rng.below(40) as f64 * 0.5);
    vec![
        RecordValue::UInt(i),
        maybe(rng, kind),
        maybe(rng, n),
        maybe(rng, amount),
        maybe(rng, u),
        maybe(rng, blob),
        RecordValue::Int(1_760_000_000_000_000_000 + rng.below(1_000) as i64),
        maybe(rng, f),
        {
            let h = random_map(rng);
            maybe(rng, h)
        },
    ]
}

/// A random map (#621): up to three of five keys, some values NULL, a
/// repeated key now and then (joined at append).
fn random_map(rng: &mut Rng) -> RecordValue {
    let keys = ["a", "B", "c", "Host", "ä"];
    let entries = (0..rng.below(4))
        .map(|_| {
            let key = keys[rng.below(5) as usize].to_string();
            let value = (rng.below(4) > 0).then(|| format!("v{}", rng.below(3)));
            (key, value)
        })
        .collect();
    RecordValue::Map(entries)
}

const RECORD_INVARIANCE_QUERIES: [&str; 13] = [
    "SELECT COUNT(*), COUNT(n), SUM(n), MIN(amount), MAX(amount), SUM(u), COUNT(blob), SUM(f) \
     FROM logs",
    "SELECT id, kind, n, amount, u, blob, ts, f FROM logs ORDER BY id",
    "SELECT kind, COUNT(*), SUM(amount) FROM logs GROUP BY kind ORDER BY kind",
    "SELECT id FROM logs WHERE u > 3000000000 ORDER BY id",
    "SELECT id, amount FROM logs WHERE amount >= 5 ORDER BY id DESC LIMIT 7",
    "SELECT blob, COUNT(*) FROM logs GROUP BY blob ORDER BY blob",
    "SELECT u FROM logs WHERE u IS NOT NULL ORDER BY u DESC LIMIT 3",
    "SELECT id, h, h['a'], map_get_ci(h, 'HOST'), cardinality(h), map_keys(h) FROM logs \
     ORDER BY id",
    "SELECT h['a'] AS a, count(*) FROM logs GROUP BY a ORDER BY a",
    "SELECT id FROM logs WHERE h['B'] = 'v1' ORDER BY id",
    "SELECT kind, COUNT(DISTINCT amount), COUNT(n) FROM logs GROUP BY kind ORDER BY kind",
    "SELECT id FROM logs WHERE kind = 'k1' ORDER BY id",
    "SELECT id FROM logs WHERE u = 3 OR u = 4000000005 OR n = -7 ORDER BY id",
];

/// What the flush-timing test appends: a lines table's lines or a record
/// table's rows.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Lines,
    Records,
}

enum Record {
    Line(String),
    Row(Vec<RecordValue>),
}

impl Record {
    fn append(&self, w: &mut TableWriter) {
        match self {
            Record::Line(l) => w.append(l.as_bytes(), Ack::Memory),
            Record::Row(r) => w.append_row(r.clone(), Ack::Memory),
        }
        .unwrap();
    }
}

/// The same records give the same answers however they were split into
/// pages and files, whether read in or out of process, also for fields
/// whose type changes inside a page (#584), and for tiered tables, whose
/// hours and days close as the clock moves and are compacted (#573).
#[test]
fn query_results_do_not_depend_on_flush_timing() {
    flush_timing_invariance(Kind::Lines);
}

/// The same for a record table (#594): its typed rows read the same from
/// pages, flushed hours and merged days.
#[test]
fn record_table_query_results_do_not_depend_on_flush_timing() {
    flush_timing_invariance(Kind::Records);
}

fn flush_timing_invariance(kind: Kind) {
    let cases = std::env::var("DB_CORE_FLUSH_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12u64);
    let (queries, sort_keys): (&[&str], [&[&str]; 3]) = match kind {
        Kind::Lines => (&INVARIANCE_QUERIES, [&[], &["s"], &["k", "n"]]),
        Kind::Records => (&RECORD_INVARIANCE_QUERIES, [&[], &["kind"], &["u", "n"]]),
    };
    let schema = (kind == Kind::Records).then(record_schema);
    for seed in 0..cases {
        let mut rng = Rng::new(seed);
        let count = 10 + rng.below(150);
        let switch = (rng.below(count), rng.below(count));
        let lines: Vec<Record> = (0..count)
            .map(|i| match kind {
                Kind::Lines => Record::Line(random_line(&mut rng, i, switch)),
                Kind::Records => Record::Row(random_row(&mut rng, i)),
            })
            .collect();
        let reference = TempTable::new();
        let mut never = reference.writer_with(TableOptions {
            clock,
            schema: schema.clone(),
            ..TableOptions::default()
        });
        // Half the seeds tiered: hours close as the clock moves, days
        // merge into the archive, row groups sorted by a random key.
        let tiered = rng.below(2) == 0;
        let layout = if tiered {
            Layout::Tiered(TierOptions {
                sort_key: sort_keys[rng.below(3) as usize]
                    .iter()
                    .map(|k| k.to_string())
                    .collect(),
                row_group_max_rows: 1 + rng.below(40) as usize,
                max_file_size: if rng.below(2) == 0 { 1 } else { 1 << 30 },
                delete_grace_ns: rng.below(2) as i64 * HOUR_NS,
                // Spill the open hour now and then, or never.
                spill_rows: if rng.below(2) == 0 {
                    rng.below(30) as usize
                } else {
                    usize::MAX
                },
                // Retention drops rows the reference keeps.
                retention_days: None,
                session_key: None,
                stat_columns: Vec::new(),
            })
        } else {
            Layout::Flat
        };
        let t = TempTable::new();
        let mut w = t.writer_with(TableOptions {
            layout,
            clock,
            schema: schema.clone(),
            ..TableOptions::default()
        });
        // Seal and flush at random points, and query at random points too.
        let (seal_odds, flush_odds) = (2 + rng.below(20), 2 + rng.below(30));
        for (k, l) in lines.iter().enumerate() {
            if rng.below(8) == 0 {
                advance_clock(rng.below(3 * HOUR_NS as u64) as i64);
            }
            l.append(&mut never);
            l.append(&mut w);
            if rng.below(seal_odds) == 0 {
                w.seal_head().unwrap();
            }
            if rng.below(flush_odds) == 0 {
                w.flush().unwrap();
            }
            if rng.below(flush_odds) == 0 {
                w.compact().unwrap();
            }
            let last = k + 1 == lines.len();
            if [rng.below(40) == 0, last].contains(&true) {
                for &sql in queries {
                    assert_eq!(
                        answer(&w, sql),
                        answer(&never, sql),
                        "seed {seed}, after line {k}: {sql}"
                    );
                }
            }
        }
        // ...and from another process's point of view, after all flushes
        // (for a tiered table: two days on, so everything is archived).
        if tiered {
            advance_clock(2 * DAY_NS);
        }
        w.flush_all().unwrap();
        w.compact().unwrap();
        if tiered {
            assert!(
                w.manifest()
                    .files
                    .iter()
                    .all(|f| f.area == crate::storage::ingest::Area::Archive),
                "seed {seed}"
            );
        }
        let mut engine = TableEngine::open(&t.0).unwrap();
        for &sql in queries.iter().filter(|q| q.contains("DISTINCT")) {
            assert!(answer(&never, sql).is_ok(), "seed {seed}: {sql}");
        }
        for &sql in queries {
            assert_eq!(
                engine
                    .run_query(sql)
                    .map(|r| r.rows)
                    .map_err(|e| e.to_string()),
                answer(&never, sql),
                "seed {seed}, all flushed: {sql}"
            );
        }
    }
}

#[test]
fn the_engine_sees_new_records_on_every_query() {
    let t = TempTable::new();
    let mut w = t.writer();
    append(&mut w, &[r#"{"n":1}"#]);
    w.flush_all().unwrap();
    let mut engine = TableEngine::open(&t.0).unwrap();
    assert_eq!(engine.table_name(), "logs");
    assert_eq!(engine.path(), t.0.as_path());
    assert_eq!(engine.mode(), Mode::Batch);
    append(&mut w, &[r#"{"n":2,"user":"ann"}"#]);
    let count = "SELECT COUNT(*), MAX(n) FROM logs";
    assert_eq!(
        engine.run_query(count).unwrap().rows,
        vec![vec![Cell::Int(2), Cell::Int(2)]]
    );
    assert_eq!(engine.snapshot().last_lsn(), 2);
    assert_eq!(
        engine.stats(),
        FileStats::Batch {
            row_groups: 2,
            rows: 2
        }
    );
    let tables = engine.tables().unwrap();
    let columns: Vec<(&str, &str)> = tables[0]
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.type_name.as_str()))
        .collect();
    assert_eq!(columns[0], ("timestamp", "INT64"));
    assert_eq!(columns[2], ("severity", "INT32"));
    assert_eq!(&columns[7..], &[("n", "INT64"), ("user", "BYTE_ARRAY")]);
    assert!(!engine.explain_plan(count).unwrap().is_empty());
    assert!(!engine.explain_opcodes(count).unwrap().is_empty());
    let pred = engine.compile_predicate("user = 'ann'").unwrap();
    drop(pred);
}

#[test]
fn errors() {
    let t = TempTable::new();
    let mut w = t.writer();
    append(&mut w, &[r#"{"n":1}"#]);
    let snapshot = w.snapshot().unwrap();
    let err = |sql: &str| query_snapshot(&snapshot, "logs", sql).unwrap_err();
    assert!(err("SELECT * FROM other")
        .to_string()
        .contains("this directory is table `logs`"));
    assert!(err("SELECT nope FROM logs").to_string().contains("nope"));
    assert_eq!(
        err("SELECT n FROM logs JOIN x ON n = x.n").kind,
        ErrorKind::Unsupported
    );
    assert_eq!(
        err("SELECT n FROM logs; SELECT n FROM logs").kind,
        ErrorKind::Unsupported
    );
    let mut engine = TableEngine::open(&t.0).unwrap();
    assert!(engine.explain_plan("SELECT * FROM other").is_err());
    // The directory stops being a table.
    drop(w);
    fs::remove_file(t.0.join("manifest.json")).unwrap();
    assert_eq!(
        engine.run_query("SELECT n FROM logs").unwrap_err().kind,
        ErrorKind::Open
    );
    assert_eq!(TableEngine::open(&t.0).unwrap_err().kind, ErrorKind::Open);
    assert!(format!("{engine:?}").contains("TableEngine"));
}

#[test]
fn a_corrupt_file_is_an_open_error() {
    let t = TempTable::new();
    let mut w = t.writer();
    append(&mut w, &[r#"{"n":1}"#]);
    let entry = w.flush_all().unwrap().remove(0);
    let mut engine = TableEngine::open(&t.0).unwrap();
    fs::write(t.0.join(&entry.path), b"PAR1 not really").unwrap();
    let snapshot = w.snapshot().unwrap();
    assert_eq!(
        query_snapshot(&snapshot, "logs", "SELECT n FROM logs")
            .unwrap_err()
            .kind,
        ErrorKind::Open
    );
    assert_eq!(
        engine.run_query("SELECT n FROM logs").unwrap_err().kind,
        ErrorKind::Open
    );
    assert_eq!(
        engine.stats(),
        FileStats::Batch {
            row_groups: 0,
            rows: 1
        },
        "an unreadable file has no row groups to count"
    );
}

#[test]
fn take_values_reads_either_column_map() {
    let batch = Batch::new(2).with_column("a", vec![Value::Int(1), Value::Null]);
    let mut b = batch.clone();
    assert_eq!(take_values(&mut b, "a"), vec![Value::Int(1), Value::Null]);
    assert_eq!(take_values(&mut b, "a"), vec![Value::Null, Value::Null]);
    let mut typed = Batch::new(2).with_typed_column(
        "t",
        crate::vm::column::Column::from(vec![Value::Int(4), Value::Int(5)]),
    );
    assert_eq!(
        take_values(&mut typed, "t"),
        vec![Value::Int(4), Value::Int(5)]
    );
}

/// #573 acceptance: with a `sort_key`, each row group of a closed hour
/// holds a range of the key, so footer statistics skip the row groups a
/// non-time predicate cannot match; in arrival order they skip none.
#[test]
fn a_sort_key_lets_statistics_skip_row_groups() {
    let pruned = |sort_key: &[&str]| {
        crate::test_clock::set(20_737 * DAY_NS);
        let t = TempTable::new();
        let mut w = t.writer_with(TableOptions {
            layout: Layout::Tiered(TierOptions {
                sort_key: sort_key.iter().map(|k| k.to_string()).collect(),
                row_group_max_rows: 30,
                ..TierOptions::default()
            }),
            clock,
            ..TableOptions::default()
        });
        for i in 0..90 {
            let kind = ["login", "logout", "error"][i % 3];
            w.append(
                format!(r#"{{"i":{i},"kind":"{kind}"}}"#).as_bytes(),
                Ack::Memory,
            )
            .unwrap();
        }
        advance_clock(HOUR_NS);
        let entry = w.flush().unwrap().remove(0);
        let bytes = fs::read(t.0.join(&entry.path)).unwrap();
        let file = ParquetFile::open(&bytes).unwrap();
        let select = crate::parser::parse("SELECT i FROM logs WHERE kind = 'logout'").unwrap();
        let predicate = select.where_clause.unwrap();
        let leaves = leaf_columns(&file);
        let by_name = leaves
            .iter()
            .map(|(n, i, t)| (n.as_str(), (*i, *t)))
            .collect();
        let skipped = file
            .row_groups()
            .filter(|rg| super::super::prune::definitely_empty(&predicate, rg, &by_name))
            .count();
        // ...and the query still finds every row.
        assert_eq!(
            rows(&w, "SELECT COUNT(*) FROM logs WHERE kind = 'logout'"),
            vec![vec![Cell::Int(30)]]
        );
        (file.num_row_groups(), skipped)
    };
    assert_eq!(pruned(&["kind"]), (3, 2));
    assert_eq!(pruned(&[]), (3, 0));
}

#[test]
fn logfmt_and_clf_strings_read_the_same_before_and_after_a_flush() {
    // #616: quoted logfmt values and CLF fields decode their escapes on
    // a page and in the file it flushes to; a bare logfmt value does not.
    let t = TempTable::new();
    let mut w = t.writer_with(TableOptions {
        format: Format::Logfmt,
        ..TableOptions::default()
    });
    append(&mut w, &[r#"level=info msg="a \"q\"" path=C:\new"#]);
    let sql = "SELECT message, path FROM logs";
    let before = rows(&w, sql);
    assert_eq!(before, vec![vec![text(r#"a "q""#), text(r"C:\new")]]);
    w.flush().unwrap();
    assert_eq!(rows(&w, sql), before);

    let t = TempTable::new();
    let mut w = t.writer_with(TableOptions {
        format: Format::Clf,
        ..TableOptions::default()
    });
    append(
        &mut w,
        &[r#"10.0.0.1 - - [10/Sep/2026:08:00:05 +0000] "GET /a\"b HTTP/1.1" 404 12 "-" "x\x22y""#],
    );
    let sql = "SELECT path, status, bytes, ua FROM logs";
    let before = rows(&w, sql);
    assert_eq!(
        before,
        vec![vec![
            text(r#"/a"b"#),
            Cell::Int(404),
            Cell::Int(12),
            text(r#"x"y"#)
        ]]
    );
    w.flush().unwrap();
    assert_eq!(rows(&w, sql), before);
}

#[test]
fn json_strings_read_the_same_before_and_after_a_flush() {
    // #583: a page and the file it flushes to both decode JSON escapes,
    // so flush timing never changes a query's answer.
    let t = TempTable::new();
    let mut w = t.writer();
    append(
        &mut w,
        &[
            r#"{"n":1,"msg":"a\"b\n","user":"caf\u00e9"}"#,
            r"plain C:\new line",
        ],
    );
    let sql = "SELECT message, user FROM logs ORDER BY observed_ts";
    let before = rows(&w, sql);
    assert_eq!(
        before,
        vec![
            vec![text("a\"b\n"), text("café")],
            vec![text(r"plain C:\new line"), Cell::Null],
        ]
    );
    w.flush().unwrap();
    assert_eq!(rows(&w, sql), before);
    let filtered = rows(&w, "SELECT n FROM logs WHERE user = 'café'");
    assert_eq!(filtered, vec![vec![Cell::Int(1)]]);
}

#[test]
fn the_table_handle_answers_the_same_before_and_after_a_flush() {
    use crate::storage::ingest::Table;
    let t = TempTable::new();
    let table = Table::open(&t.0, TableOptions::default()).unwrap();
    table
        .append_batch(
            &[
                r#"{"k":"a","n":1}"#,
                r#"{"k":"b","n":2}"#,
                r#"{"k":"a","n":3}"#,
            ],
            Ack::Durable,
        )
        .unwrap();
    // The table is named after its directory.
    let sql = "SELECT k, sum(n) FROM logs GROUP BY k ORDER BY k";
    let before = table.query(sql).unwrap();
    assert_eq!(
        before.rows,
        [
            vec![Cell::Text("a".into()), Cell::Int(4)],
            vec![Cell::Text("b".into()), Cell::Int(2)]
        ]
    );
    assert_eq!(table.flush().unwrap().len(), 1);
    assert_eq!(table.query(sql).unwrap().rows, before.rows);
    let err = table.query("SELECT nope FROM logs").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile, "{err:?}");
    table.close().unwrap();
    let err = table.query(sql).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Open, "a closed table: {err:?}");
}

#[test]
fn read_snapshot_gives_the_whole_table_with_the_tables_types() {
    let t = TempTable::new();
    let mut w = t.writer();
    // A first file without `m` and `n` an integer, a second where `n` is a
    // float (so the table's `n` is DOUBLE) and `m` appears, then a page.
    append(&mut w, &[r#"{"k":"a","n":1}"#, r#"{"k":"b","n":2}"#]);
    w.flush_all().unwrap();
    append(&mut w, &[r#"{"k":"c","n":2.5,"m":"x"}"#]);
    w.flush_all().unwrap();
    append(&mut w, &[r#"{"k":"d","n":4}"#]);
    let snapshot = w.snapshot().unwrap();
    assert_eq!(snapshot.files().count(), 2);
    assert_eq!(snapshot.pages().len(), 1);

    let columns = ["logs.k".to_string(), "n".to_string(), "m".to_string()];
    let batch = read_snapshot(&snapshot, &columns).unwrap();
    assert_eq!(batch.num_rows, 4);
    let column = |key: &str| -> Vec<Value> {
        batch
            .columns
            .get(key)
            .map(|c| c.to_vec())
            .unwrap_or_default()
    };
    let s = |x: &str| Value::Str(x.to_string().into());
    assert_eq!(
        column("logs.k"),
        [s("a"), s("b"), s("c"), s("d")],
        "keyed as asked"
    );
    assert_eq!(
        column("n"),
        [
            Value::Float(1.0),
            Value::Float(2.0),
            Value::Float(2.5),
            Value::Float(4.0)
        ]
    );
    assert_eq!(column("m"), [Value::Null, Value::Null, s("x"), Value::Null]);

    let err = read_snapshot(&snapshot, &["nope".to_string()]).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
}

#[test]
fn snapshot_segments_drive_a_join_as_a_caller_runs_one() {
    use crate::codegen::batch::{self as planner, BuildSourceKind};
    use crate::engine::column::{snapshot_files, snapshot_segments};
    use crate::vm::batch::ScanSource;
    use crate::vm::engine::{self, NoResolver};

    let events = TempTable::new();
    let mut w = events.writer();
    append(&mut w, &[r#"{"k":"a","n":1}"#, r#"{"k":"b","n":2}"#]);
    w.flush_all().unwrap();
    append(&mut w, &[r#"{"k":"a","n":3}"#]);
    let left = w.snapshot().unwrap();

    let names = TempTable::new();
    let mut lookup = names.writer();
    append(
        &mut lookup,
        &[
            r#"{"k":"a","label":"alpha"}"#,
            r#"{"k":"b","label":"beta"}"#,
        ],
    );
    lookup.flush_all().unwrap();
    let right = lookup.snapshot().unwrap();

    let select = crate::parser::column::parse(
        "SELECT r.label, SUM(l.n) FROM logs l JOIN names r ON l.k = r.k GROUP BY r.label ORDER BY r.label",
    )
    .unwrap();
    let plan = planner::compile_join(&select, BuildSourceKind::InMemory).unwrap();
    let files = snapshot_files(&left).unwrap();
    let eager = Program::from_opcodes([]);
    let probe = snapshot_segments(&left, &files, &plan.left_columns, &eager).unwrap();
    assert_eq!(probe.len(), 2, "one row group and one page");
    let build = read_snapshot(&right, &plan.right_columns).unwrap();
    let output =
        engine::run_join_segments(probe, ScanSource::InMemory(build), &plan, &NoResolver).unwrap();
    let rows: Vec<Vec<Value>> = output.rows().collect();
    let s = |x: &str| Value::Str(x.to_string().into());
    assert_eq!(
        rows,
        [
            vec![s("alpha"), Value::Int(4)],
            vec![s("beta"), Value::Int(2)]
        ]
    );

    // #633: COUNT(DISTINCT ...) through the fused probe-and-group, over a
    // build-side column and a probe-side one.
    for (sql, expected) in [
        (
            "SELECT COUNT(DISTINCT r.label), COUNT(*) FROM logs l JOIN names r ON l.k = r.k",
            vec![vec![Value::Int(2), Value::Int(3)]],
        ),
        (
            "SELECT r.label, COUNT(DISTINCT l.n) FROM logs l JOIN names r ON l.k = r.k \
             GROUP BY r.label ORDER BY r.label",
            vec![
                vec![s("alpha"), Value::Int(2)],
                vec![s("beta"), Value::Int(1)],
            ],
        ),
    ] {
        let select = crate::parser::column::parse(sql).unwrap();
        let plan = planner::compile_join(&select, BuildSourceKind::InMemory).unwrap();
        let probe = snapshot_segments(&left, &files, &plan.left_columns, &eager).unwrap();
        let build = read_snapshot(&right, &plan.right_columns).unwrap();
        let output =
            engine::run_join_segments(probe, ScanSource::InMemory(build), &plan, &NoResolver)
                .unwrap();
        assert_eq!(output.rows().collect::<Vec<_>>(), expected, "{sql}");
    }
}

/// A tiered table with `sort_key = ["kind"]`, one closed hour per entry of
/// `hours`, each hour the kinds listed.
fn hours_of_kinds(t: &TempTable, hours: &[&[&str]], parquet: WriterOptions) -> TableWriter {
    crate::test_clock::set(20_737 * DAY_NS);
    let mut w = t.writer_with(TableOptions {
        layout: Layout::Tiered(TierOptions {
            sort_key: vec!["kind".to_string()],
            ..TierOptions::default()
        }),
        parquet,
        clock,
        ..TableOptions::default()
    });
    let mut i = 0;
    for kinds in hours {
        for kind in *kinds {
            w.append(
                format!(r#"{{"i":{i},"kind":"{kind}"}}"#).as_bytes(),
                Ack::Memory,
            )
            .unwrap();
            i += 1;
        }
        advance_clock(HOUR_NS);
        w.flush().unwrap();
    }
    w
}

/// #589 acceptance: a filter on the sort key opens only the files whose
/// manifest range can hold a match.
#[test]
fn a_sort_key_filter_opens_only_the_files_whose_range_matches() {
    let t = TempTable::new();
    let hours: &[&[&str]] = &[&["a", "b"], &["c", "d"], &["a"], &["x", "y"]];
    let w = hours_of_kinds(&t, hours, WriterOptions::default());
    let snapshot = w.snapshot().unwrap();
    assert_eq!(snapshot.files().count(), 4);
    let schema = snapshot.schema().unwrap();
    let opened = |sql: &str| {
        let select = crate::parser::parse(sql).unwrap();
        snapshot
            .files()
            .filter(|(entry, _)| may_match(entry, &schema, None, select.where_clause.as_ref()))
            .count()
    };
    assert_eq!(opened("SELECT i FROM logs WHERE kind = 'c'"), 1);
    assert_eq!(
        opened("SELECT i FROM logs WHERE kind = 'a' OR kind = 'x'"),
        3
    );
    assert_eq!(opened("SELECT i FROM logs WHERE kind = 'zz'"), 0);
    assert_eq!(
        opened("SELECT i FROM logs WHERE kind > 'c' AND kind < 'x'"),
        1
    );
    // #737: an `IN` list, lowered by the column dialect to the `OR` chain
    // above, prunes the same way.
    let opened_in = |sql: &str| {
        let select = crate::parser::column::parse(sql).unwrap();
        snapshot
            .files()
            .filter(|(entry, _)| may_match(entry, &schema, None, select.where_clause.as_ref()))
            .count()
    };
    assert_eq!(opened_in("SELECT i FROM logs WHERE kind IN ('a', 'x')"), 3);
    assert_eq!(opened_in("SELECT i FROM logs WHERE kind IN ('zz', 'e')"), 0);
    assert_eq!(opened("SELECT i FROM logs WHERE kind = 'c' OR i = 1"), 4);
    assert_eq!(opened("SELECT i FROM logs WHERE kind <> 'a'"), 4);
    assert_eq!(opened("SELECT i FROM logs"), 4);
    // ...and every answer is the unpruned one.
    assert_eq!(
        rows(
            &w,
            "SELECT i FROM logs WHERE kind = 'a' OR kind = 'x' ORDER BY i"
        ),
        vec![vec![Cell::Int(0)], vec![Cell::Int(4)], vec![Cell::Int(5)]]
    );
    assert_eq!(
        rows(&w, "SELECT COUNT(*) FROM logs WHERE kind = 'zz'"),
        vec![vec![Cell::Int(0)]]
    );
}

/// #589 acceptance: a row group whose min/max covers a literal is still
/// skipped when its dictionary lacks it, and only when every data page
/// is dictionary-encoded.
#[test]
fn a_dictionary_skips_a_row_group_its_range_does_not() {
    let skipped = |parquet: WriterOptions, sql: &str| {
        let t = TempTable::new();
        let w = hours_of_kinds(&t, &[&["a", "c", "a", "c"]], parquet);
        let entry = w.manifest().files[0].clone();
        let bytes = fs::read(t.0.join(&entry.path)).unwrap();
        let file = ParquetFile::open(&bytes).unwrap();
        let predicate = crate::parser::parse(sql).unwrap().where_clause.unwrap();
        let leaves = leaf_columns(&file);
        let by_name = leaves
            .iter()
            .map(|(n, i, t)| (n.as_str(), (*i, *t)))
            .collect();
        let rg = file.row_group(0).unwrap();
        super::super::prune::definitely_empty(&predicate, &rg, &by_name)
    };
    let dictionary = WriterOptions::default();
    assert!(skipped(
        dictionary.clone(),
        "SELECT i FROM logs WHERE kind = 'b'"
    ));
    assert!(skipped(
        dictionary.clone(),
        "SELECT i FROM logs WHERE kind = 'b' OR kind = 'bb'"
    ));
    assert!(!skipped(
        dictionary.clone(),
        "SELECT i FROM logs WHERE kind = 'b' OR kind = 'c'"
    ));
    assert!(!skipped(
        dictionary.clone(),
        "SELECT i FROM logs WHERE kind = 'a'"
    ));
    // A comparison other than = is the range's alone.
    assert!(!skipped(dictionary, "SELECT i FROM logs WHERE kind <> 'b'"));
    let plain = WriterOptions {
        dictionary: false,
        ..WriterOptions::default()
    };
    assert!(!skipped(plain, "SELECT i FROM logs WHERE kind = 'b'"));
}

/// A file flushed while a column was an integer keeps an integer range;
/// once the column is a string it bounds nothing, as its statistics.
#[test]
fn a_key_range_of_a_widened_column_never_skips_a_file() {
    crate::test_clock::set(20_737 * DAY_NS);
    let t = TempTable::new();
    let mut w = t.writer_with(TableOptions {
        layout: Layout::Tiered(TierOptions {
            sort_key: vec!["x".to_string()],
            ..TierOptions::default()
        }),
        clock,
        ..TableOptions::default()
    });
    append(&mut w, &[r#"{"x":5}"#, r#"{"x":7}"#]);
    advance_clock(HOUR_NS);
    w.flush().unwrap();
    append(&mut w, &[r#"{"x":"high"}"#]);
    assert_eq!(
        w.manifest().files[0].keys[0].min,
        crate::storage::ingest::KeyBound::Int(5)
    );
    // As strings, '5' and '7' are > '10': the integer range 5..=7 must
    // not skip the file.
    assert_eq!(
        rows(&w, "SELECT COUNT(*) FROM logs WHERE x > '10'"),
        vec![vec![Cell::Int(3)]]
    );
    assert_eq!(
        rows(&w, "SELECT COUNT(*) FROM logs WHERE x = '5'"),
        vec![vec![Cell::Int(1)]]
    );
}

/// #740: a table that gained a TEXT and a MAP column answers over the old
/// files (which lack both) and the new ones alike: NULL for the old rows,
/// and a sort key on the new column never skips a file that has no range
/// for it. Before and after a day merge.
#[test]
fn queries_see_null_for_columns_added_after_a_file_was_written() {
    crate::test_clock::set(20_737 * DAY_NS);
    let tiered = |key: &str| {
        Layout::Tiered(TierOptions {
            sort_key: vec![key.to_string()],
            row_group_max_rows: 10,
            delete_grace_ns: 0,
            ..TierOptions::default()
        })
    };
    let old = vec![
        ColumnSpec::new("id", ColumnType::UInt64, false),
        ColumnSpec::new("kind", ColumnType::Utf8, true),
    ];
    let mut evolved = old.clone();
    evolved.push(ColumnSpec::new("note", ColumnType::Utf8, true));
    evolved.push(ColumnSpec::new("h", ColumnType::MapUtf8, true));

    let t = TempTable::new();
    let mut w = t.writer_with(TableOptions {
        layout: tiered("kind"),
        schema: Some(old),
        clock,
        ..TableOptions::default()
    });
    for i in 0..30u64 {
        let kind = RecordValue::Str(["a", "b", "c"][(i % 3) as usize].to_string());
        w.append_row(vec![RecordValue::UInt(i), kind], Ack::Memory)
            .unwrap();
    }
    advance_clock(HOUR_NS);
    w.flush().unwrap();
    w.close().unwrap();

    let mut w = t.writer_with(TableOptions {
        layout: tiered("note"),
        schema: Some(evolved),
        clock,
        ..TableOptions::default()
    });
    for i in 30..60u64 {
        let map = RecordValue::Map(vec![("k".to_string(), Some(format!("v{}", i % 2)))]);
        w.append_row(
            vec![
                RecordValue::UInt(i),
                RecordValue::Str("a".to_string()),
                RecordValue::Str("x".to_string()),
                map,
            ],
            Ack::Memory,
        )
        .unwrap();
    }
    advance_clock(HOUR_NS);
    w.flush().unwrap();

    let count = |w: &TableWriter, filter: &str| {
        rows(w, &format!("SELECT COUNT(*) FROM logs WHERE {filter}"))
    };
    let check = |w: &TableWriter| {
        assert_eq!(count(w, "note IS NULL"), vec![vec![Cell::Int(30)]]);
        assert_eq!(count(w, "note = 'x'"), vec![vec![Cell::Int(30)]]);
        assert_eq!(count(w, "note = 'zzz'"), vec![vec![Cell::Int(0)]]);
        assert_eq!(count(w, "note < 'zzz'"), vec![vec![Cell::Int(30)]]);
        assert_eq!(count(w, "h['k'] = 'v1'"), vec![vec![Cell::Int(15)]]);
        assert_eq!(count(w, "h['k'] IS NULL"), vec![vec![Cell::Int(30)]]);
        assert_eq!(
            rows(w, "SELECT COUNT(*) FROM logs"),
            vec![vec![Cell::Int(60)]]
        );
    };
    check(&w);
    // The two hours' day is merged into the archive under the new schema.
    advance_clock(2 * DAY_NS);
    w.flush().unwrap();
    w.compact().unwrap();
    assert!(w
        .manifest()
        .files
        .iter()
        .any(|f| f.area == crate::storage::ingest::Area::Archive));
    check(&w);
}

/// Start of the day the session-key tests run in, in seconds.
const SESSION_BASE: i64 = 20_737 * 86_400;

/// The key of a session that started `offset` seconds into hour `hour`
/// (negative: before it, as one that arrives in the hour has).
fn session_id(hour: i64, offset: i64) -> u64 {
    u64::try_from((SESSION_BASE + hour * 3_600 + offset) << 32).unwrap()
}

/// A tiered record table with `session_key` on `sid`, four closed hours
/// (two rows of one session each) and one session still in the open hour.
fn sessions_by_hour(t: &TempTable, session_key: Option<SessionKey>) -> TableWriter {
    crate::test_clock::set(20_737 * DAY_NS);
    let mut w = t.writer_with(TableOptions {
        layout: Layout::Tiered(TierOptions {
            session_key,
            ..TierOptions::default()
        }),
        schema: Some(vec![
            ColumnSpec::new("sid", ColumnType::UInt64, false),
            ColumnSpec::new("n", ColumnType::UInt64, false),
        ]),
        clock,
        ..TableOptions::default()
    });
    for hour in 0..5 {
        for n in 0..2u64 {
            w.append_row(
                vec![
                    RecordValue::UInt(session_id(hour, -10)),
                    RecordValue::UInt(n),
                ],
                Ack::Memory,
            )
            .unwrap();
        }
        if hour < 4 {
            advance_clock(HOUR_NS);
            w.flush().unwrap();
        }
    }
    w
}

/// #599 acceptance: a filter on a session key opens only the files that
/// can hold the session's rows, for `=`, `IN` and ranges, and answers as
/// without the option.
#[test]
fn a_session_key_filter_opens_only_the_files_its_arrival_window_can_hold() {
    let t = TempTable::new();
    let w = sessions_by_hour(
        &t,
        Some(SessionKey {
            column: "sid".to_string(),
            max_session_secs: 1_800,
        }),
    );
    let snapshot = w.snapshot().unwrap();
    assert_eq!(snapshot.files().count(), 4);
    let schema = snapshot.schema().unwrap();
    let key = snapshot.manifest().session_key.clone();
    assert!(key.is_some(), "readers get the option from the manifest");
    let opened = |select: Select| {
        snapshot
            .files()
            .filter(|(entry, _)| {
                may_match(entry, &schema, key.as_ref(), select.where_clause.as_ref())
            })
            .count()
    };
    let sql =
        |filter: &str| crate::parser::parse(&format!("SELECT n FROM t WHERE {filter}")).unwrap();
    let column = |filter: &str| {
        crate::parser::column::parse(&format!("SELECT n FROM t WHERE {filter}")).unwrap()
    };
    let (s1, s2) = (session_id(1, -10), session_id(2, -10));
    assert_eq!(opened(sql(&format!("sid = {s2}"))), 1);
    assert_eq!(opened(column(&format!("sid IN ({s1}, {s2})"))), 2);
    assert_eq!(
        opened(sql(&format!("sid >= {s1} AND sid <= {s2}"))),
        2,
        "a range maps to a window"
    );
    assert_eq!(opened(sql(&format!("sid = {s1} OR sid = {s2}"))), 2);
    // A key outside every window reads no file.
    assert_eq!(opened(sql(&format!("sid = {}", session_id(30, 0)))), 0);
    assert_eq!(opened(sql(&format!("sid = {}", session_id(-30, 0)))), 0);
    // Shapes it cannot bound keep every file.
    assert_eq!(opened(sql(&format!("sid <> {s2}"))), 4);
    assert_eq!(opened(sql(&format!("sid = {s2} OR n = 0"))), 4);
    assert_eq!(opened(sql("n = 0")), 4);
    // ...and the answers are the unpruned ones, the open hour included.
    let count = |filter: &str| rows(&w, &format!("SELECT COUNT(*) FROM logs WHERE {filter}"));
    assert_eq!(count(&format!("sid = {s2}")), vec![vec![Cell::Int(2)]]);
    assert_eq!(
        count(&format!("sid = {}", session_id(4, -10))),
        vec![vec![Cell::Int(2)]]
    );
    assert_eq!(
        count(&format!("sid = {}", session_id(30, 0))),
        vec![vec![Cell::Int(0)]]
    );
}

/// #599 acceptance: without the option, the same filter opens every file.
#[test]
fn without_a_session_key_option_no_file_is_skipped_on_the_key() {
    let t = TempTable::new();
    let w = sessions_by_hour(&t, None);
    let snapshot = w.snapshot().unwrap();
    assert!(snapshot.manifest().session_key.is_none());
    let schema = snapshot.schema().unwrap();
    let select = crate::parser::parse(&format!(
        "SELECT n FROM t WHERE sid = {}",
        session_id(2, -10)
    ))
    .unwrap();
    let opened = snapshot
        .files()
        .filter(|(entry, _)| may_match(entry, &schema, None, select.where_clause.as_ref()))
        .count();
    assert_eq!(opened, 4);
}

/// A tiered record table with four closed hours of two rows each, hour `h`
/// holding `sid = h * 100`, a late row of session 0 in hour 3, and two rows
/// of session 400 in the open hour.
fn stats_by_hour(t: &TempTable, stat_columns: &[&str]) -> TableWriter {
    crate::test_clock::set(20_737 * DAY_NS);
    let mut w = t.writer_with(TableOptions {
        layout: Layout::Tiered(TierOptions {
            stat_columns: stat_columns.iter().map(|c| c.to_string()).collect(),
            ..TierOptions::default()
        }),
        schema: Some(vec![
            ColumnSpec::new("sid", ColumnType::Int64, false),
            ColumnSpec::new("n", ColumnType::Int64, false),
        ]),
        clock,
        ..TableOptions::default()
    });
    for hour in 0..5i64 {
        let mut sids = vec![hour * 100; 2];
        if hour == 3 {
            sids.push(0);
        }
        for (n, sid) in (0i64..).zip(sids) {
            w.append_row(
                vec![RecordValue::Int(sid), RecordValue::Int(n)],
                Ack::Memory,
            )
            .unwrap();
        }
        if hour < 4 {
            advance_clock(HOUR_NS);
            w.flush().unwrap();
        }
    }
    w
}

/// #777 acceptance: a column listed in `stat_columns` prunes files by its
/// exact range, with no contract, and never hides a late row.
#[test]
fn a_stat_column_prunes_files_by_its_exact_range() {
    let t = TempTable::new();
    let w = stats_by_hour(&t, &["sid"]);
    let snapshot = w.snapshot().unwrap();
    assert_eq!(snapshot.files().count(), 4);
    let schema = snapshot.schema().unwrap();
    let opened = |filter: &str| {
        let select = crate::parser::parse(&format!("SELECT n FROM t WHERE {filter}")).unwrap();
        snapshot
            .files()
            .filter(|(entry, _)| may_match(entry, &schema, None, select.where_clause.as_ref()))
            .count()
    };
    // Session 0 has a late row in hour 3's file, whose range spans 0..=300,
    // so that file is read for every key in it.
    assert_eq!(opened("sid = 200"), 2);
    assert_eq!(opened("sid = 0"), 2);
    assert_eq!(opened("sid IN (100, 200)"), 3);
    assert_eq!(opened("sid >= 100 AND sid < 200"), 2);
    assert_eq!(opened("sid = 999"), 0);
    assert_eq!(opened("sid <> 200"), 4);
    assert_eq!(opened("n = 0"), 4);
    let count = |filter: &str| rows(&w, &format!("SELECT COUNT(*) FROM logs WHERE {filter}"));
    assert_eq!(count("sid = 0"), vec![vec![Cell::Int(3)]], "late row");
    assert_eq!(count("sid = 400"), vec![vec![Cell::Int(2)]], "open hour");
}

#[test]
fn without_stat_columns_a_non_sort_column_prunes_nothing() {
    let t = TempTable::new();
    let w = stats_by_hour(&t, &[]);
    let snapshot = w.snapshot().unwrap();
    let schema = snapshot.schema().unwrap();
    let select = crate::parser::parse("SELECT n FROM t WHERE sid = 200").unwrap();
    let opened = snapshot
        .files()
        .filter(|(entry, _)| may_match(entry, &schema, None, select.where_clause.as_ref()))
        .count();
    assert_eq!(opened, 4);
}

/// #777 acceptance: a build side or window input read with a filter skips
/// the files and record pages the predicate excludes.
#[test]
fn read_snapshot_where_skips_excluded_files_and_pages() {
    let t = TempTable::new();
    let w = stats_by_hour(&t, &["sid"]);
    let snapshot = w.snapshot().unwrap();
    assert_eq!(snapshot.pages().len(), 1);
    let columns = ["sid".to_string(), "n".to_string()];
    let read = |predicate: &str| {
        let filter = SnapshotFilter::parse(&snapshot, "logs", predicate).unwrap();
        read_snapshot_where(&snapshot, &columns, &filter)
            .unwrap()
            .num_rows
    };
    assert_eq!(read_snapshot(&snapshot, &columns).unwrap().num_rows, 11);
    // Hours 0 and 3 (the late row), not the page.
    assert_eq!(read("sid = 0"), 5);
    // Only the open hour's page.
    assert_eq!(read("sid = 400"), 2);
    assert_eq!(read("sid = 999"), 0);
    // A shape it cannot bound reads everything.
    assert_eq!(read("sid <> 200"), 11);
    assert!(SnapshotFilter::parse(&snapshot, "logs", "sid = ").is_err());
}

/// #777: a day merge keeps a stat column's range, so the archive file is
/// still skipped on it, and answers do not change.
#[test]
fn a_day_merge_keeps_the_ranges_of_stat_columns() {
    let t = TempTable::new();
    let mut w = stats_by_hour(&t, &["sid"]);
    let count = |w: &TableWriter, filter: &str| {
        rows(w, &format!("SELECT COUNT(*) FROM logs WHERE {filter}"))
    };
    let before = (count(&w, "sid = 0"), count(&w, "sid = 999"));
    advance_clock(2 * DAY_NS);
    w.flush().unwrap();
    w.compact().unwrap();
    let snapshot = w.snapshot().unwrap();
    let archived: Vec<_> = snapshot
        .manifest()
        .files
        .iter()
        .filter(|f| f.area == crate::storage::ingest::Area::Archive)
        .collect();
    assert_eq!(archived.len(), 1, "the five hours are one day");
    let sid = archived[0].keys.iter().find(|k| k.column == "sid").unwrap();
    assert_eq!(
        (&sid.min, &sid.max),
        (&KeyBound::Int(0), &KeyBound::Int(400))
    );
    let files_for = |filter: &str| {
        query_snapshot_files(
            &snapshot,
            "logs",
            &format!("SELECT n FROM logs WHERE {filter}"),
        )
        .unwrap()
    };
    assert_eq!(files_for("sid = 999"), 0);
    assert_eq!(files_for("sid = 200"), 1);
    assert_eq!((count(&w, "sid = 0"), count(&w, "sid = 999")), before);
}

/// #786: a join key `UINT_64` above `i64::MAX` reads as its signed bit
/// pattern, in a flushed row group and in a record page alike, when the
/// caller names it; without that it is still an error (#595).
#[test]
fn snapshot_segments_read_a_bit_pattern_join_key_in_files_and_pages() {
    use crate::engine::column::{
        snapshot_files, snapshot_segments, snapshot_segments_with_bit_patterns,
    };
    use crate::vm::engine;

    let t = TempTable::new();
    let mut w = t.writer_with(TableOptions {
        schema: Some(vec![
            ColumnSpec::new("id", ColumnType::UInt64, false),
            ColumnSpec::new("n", ColumnType::Int64, false),
        ]),
        clock,
        ..TableOptions::default()
    });
    for (id, n) in [(u64::MAX, 1), (5, 2)] {
        w.append_row(
            vec![RecordValue::UInt(id), RecordValue::Int(n)],
            Ack::Durable,
        )
        .unwrap();
    }
    w.flush_all().unwrap();
    w.append_row(
        vec![RecordValue::UInt(1 << 63), RecordValue::Int(3)],
        Ack::Durable,
    )
    .unwrap();
    let snapshot = w.snapshot().unwrap();

    let select = crate::parser::column::parse("SELECT logs.id FROM logs ORDER BY logs.n").unwrap();
    let program: Program = planner::compile(&select).unwrap();
    let columns = program.columns_to_load();
    let files = snapshot_files(&snapshot).unwrap();

    let plain = snapshot_segments(&snapshot, &files, &columns, &program).unwrap();
    assert_eq!(plain.len(), 2, "one row group and one page");
    assert!(engine::run(&plain, &program).is_err());

    let bits = snapshot_segments_with_bit_patterns(
        &snapshot,
        &files,
        &columns,
        &program,
        &["logs.id".to_string()],
    )
    .unwrap();
    let rows: Vec<Vec<Value>> = engine::run(&bits, &program).unwrap().rows().collect();
    assert_eq!(
        rows,
        [
            vec![Value::Int(-1)],
            vec![Value::Int(5)],
            vec![Value::Int(i64::MIN)]
        ]
    );

    // The whole table, as a join build side reads it.
    let names = ["logs.id".to_string(), "logs.n".to_string()];
    assert!(read_snapshot(&snapshot, &names).is_err());
    let whole =
        read_snapshot_with_bit_patterns(&snapshot, &names, &["logs.id".to_string()]).unwrap();
    assert_eq!(whole.num_rows, 3);
}

/// #772: `snapshot_segments_where` leaves out the row groups of a file whose
/// footer statistics prove they hold no row the filter accepts, and keeps
/// every record page: a plain `snapshot_segments` loads them all.
#[test]
fn snapshot_segments_where_skips_row_groups_by_footer_statistics() {
    use crate::engine::column::{snapshot_files_where, snapshot_segments, snapshot_segments_where};

    crate::test_clock::set(20_737 * DAY_NS);
    let t = TempTable::new();
    let mut w = t.writer_with(TableOptions {
        layout: Layout::Tiered(TierOptions {
            sort_key: vec!["id".to_string()],
            row_group_max_rows: 5,
            delete_grace_ns: 0,
            ..TierOptions::default()
        }),
        schema: Some(vec![ColumnSpec::new("id", ColumnType::Int64, false)]),
        clock,
        ..TableOptions::default()
    });
    for id in 0..20i64 {
        w.append_row(vec![RecordValue::Int(id)], Ack::Memory)
            .unwrap();
    }
    advance_clock(HOUR_NS);
    w.flush().unwrap();
    w.append_row(vec![RecordValue::Int(100)], Ack::Durable)
        .unwrap();
    let snapshot = w.snapshot().unwrap();

    let filter = SnapshotFilter::parse(&snapshot, "logs", "id >= 7 AND id <= 8").unwrap();
    let files = snapshot_files_where(&snapshot, &filter).unwrap();
    let program = Program::from_opcodes([]);
    let columns = ["id".to_string()];
    let all = snapshot_segments(&snapshot, &files, &columns, &program).unwrap();
    let kept = snapshot_segments_where(&snapshot, &files, &columns, &program, &filter).unwrap();
    // Four row groups of five ids and one live page; only ids 5..=9 can match.
    assert_eq!(all.iter().filter(|s| s.is_row_group()).count(), 4);
    assert_eq!(kept.iter().filter(|s| s.is_row_group()).count(), 1);
}
