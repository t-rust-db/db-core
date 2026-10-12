use super::*;
use crate::engine::Cell;
use crate::storage::ingest::layout::{DAY_NS, HOUR_NS};
use crate::storage::ingest::{Ack, Layout, TableOptions, TableWriter, TierOptions};
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

/// The tests' clock: shared, so tests that move it hold [`serial`].
static NOW: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(20_737 * DAY_NS);
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn clock() -> i64 {
    NOW.load(Ordering::SeqCst)
}

fn advance_clock(ns: i64) {
    NOW.fetch_add(ns, Ordering::SeqCst);
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
            Cell::Real(10.0),
            Cell::Real(1.0),
            Cell::Real(4.0)
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

const INVARIANCE_QUERIES: [&str; 9] = [
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
];

/// The same records give the same answers however they were split into
/// pages and files, whether read in or out of process, also for fields
/// whose type changes inside a page (#584), and for tiered tables, whose
/// hours and days close as the clock moves and are compacted (#573).
#[test]
fn query_results_do_not_depend_on_flush_timing() {
    let cases = std::env::var("DB_CORE_FLUSH_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12u64);
    let _serial = serial();
    for seed in 0..cases {
        let mut rng = Rng::new(seed);
        let count = 10 + rng.below(150);
        let switch = (rng.below(count), rng.below(count));
        let lines: Vec<String> = (0..count)
            .map(|i| random_line(&mut rng, i, switch))
            .collect();
        let reference = TempTable::new();
        let mut never = reference.writer_with(TableOptions {
            clock,
            ..TableOptions::default()
        });
        // Half the seeds tiered: hours close as the clock moves, days
        // merge into the archive, row groups sorted by a random key.
        let tiered = rng.below(2) == 0;
        let sort_keys: [&[&str]; 3] = [&[], &["s"], &["k", "n"]];
        let layout = if tiered {
            Layout::Tiered(TierOptions {
                sort_key: sort_keys[rng.below(3) as usize]
                    .iter()
                    .map(|k| k.to_string())
                    .collect(),
                row_group_max_rows: 1 + rng.below(40) as usize,
                max_file_size: if rng.below(2) == 0 { 1 } else { 1 << 30 },
                delete_grace_ns: rng.below(2) as i64 * HOUR_NS,
            })
        } else {
            Layout::Flat
        };
        let t = TempTable::new();
        let mut w = t.writer_with(TableOptions {
            layout,
            clock,
            ..TableOptions::default()
        });
        // Seal and flush at random points, and query at random points too.
        let (seal_odds, flush_odds) = (2 + rng.below(20), 2 + rng.below(30));
        for (k, l) in lines.iter().enumerate() {
            if rng.below(8) == 0 {
                advance_clock(rng.below(3 * HOUR_NS as u64) as i64);
            }
            never.append(l.as_bytes(), Ack::Memory).unwrap();
            w.append(l.as_bytes(), Ack::Memory).unwrap();
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
                for sql in INVARIANCE_QUERIES {
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
        for sql in INVARIANCE_QUERIES {
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
        vec![vec![Cell::Int(2), Cell::Real(2.0)]]
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
    let _serial = serial();
    let pruned = |sort_key: &[&str]| {
        NOW.store(20_737 * DAY_NS, Ordering::SeqCst);
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
