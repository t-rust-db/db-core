//! Record tables (#594): typed rows through the WAL, pages, flushes,
//! hours, days and crashes, read back with exactly the declared schema.

use super::*;
use crate::storage::ingest::layout::{TierOptions, DAY_NS, HOUR_NS};
use crate::storage::ingest::record::{join_repeated_keys, read_column, RecordValue};
use crate::storage::ingest::PageBody;
use crate::storage::parquet::footer::ConvertedType;
use crate::storage::parquet::writer::{ColumnType, ColumnValues};
use crate::storage::parquet::ParquetFile;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

static NOW: AtomicI64 = AtomicI64::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn clock() -> i64 {
    NOW.load(Ordering::SeqCst)
}

/// `day` days and `hour` hours past 2026-10-11.
fn set_clock(day: i64, hour: i64) {
    NOW.store((20_737 + day) * DAY_NS + hour * HOUR_NS, Ordering::SeqCst);
}

struct TempTable(PathBuf);

impl TempTable {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("db-core-records-{}-{n}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        TempTable(dir)
    }
}

impl Drop for TempTable {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

/// An event schema: the types a relational dump needs (#592).
fn schema() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("session_id", ColumnType::UInt64, false),
        ColumnSpec::new("ts", ColumnType::TimestampNanos, false),
        ColumnSpec::new("event_id", ColumnType::UInt16, false),
        ColumnSpec::new("kind", ColumnType::Utf8, true),
        ColumnSpec::new("value", ColumnType::Binary, true),
        ColumnSpec::new(
            "amount",
            ColumnType::Decimal {
                precision: 18,
                scale: 2,
            },
            true,
        ),
        ColumnSpec::new("ip", ColumnType::FixedLenByteArray(16), true),
        ColumnSpec::new("headers", ColumnType::MapUtf8, true),
    ]
}

fn flat() -> TableOptions {
    TableOptions {
        schema: Some(schema()),
        clock,
        ..TableOptions::default()
    }
}

fn tiered() -> TableOptions {
    TableOptions {
        layout: Layout::Tiered(TierOptions {
            sort_key: vec!["kind".to_string(), "session_id".to_string()],
            row_group_max_rows: 3,
            max_file_size: 1 << 30,
            delete_grace_ns: 0,
            spill_rows: usize::MAX,
            retention_days: None,
        }),
        ..flat()
    }
}

/// Row `i`: a session id above `i64::MAX`, `kind` cycling c, a, b so a
/// sort moves rows, NULLs in the nullable columns now and then.
fn row(i: u64) -> Vec<RecordValue> {
    let kind = ["c", "a", "b"][usize::try_from(i % 3).unwrap()];
    vec![
        RecordValue::UInt(u64::MAX - i),
        RecordValue::Int(1_760_000_000_000_000_000 + i64::try_from(i).unwrap()),
        RecordValue::Int(i64::try_from(i % 7).unwrap()),
        RecordValue::Str(kind.to_string()),
        if i.is_multiple_of(4) {
            RecordValue::Null
        } else {
            RecordValue::Bytes(vec![0x80, u8::try_from(i % 256).unwrap()])
        },
        RecordValue::Decimal(-1_000_000 + i128::from(i) * 125),
        if i.is_multiple_of(5) {
            RecordValue::Null
        } else {
            RecordValue::Bytes(
                (0..16)
                    .map(|b| u8::try_from((b + i) % 256).unwrap())
                    .collect(),
            )
        },
        match i % 4 {
            0 => RecordValue::Null,
            1 => RecordValue::Map(Vec::new()),
            // A repeated key, joined at append.
            2 => RecordValue::Map(vec![
                ("Via".to_string(), Some(format!("p{i}"))),
                ("Accept".to_string(), None),
                ("Via".to_string(), Some("q".to_string())),
            ]),
            _ => RecordValue::Map(vec![("héader".to_string(), Some(i.to_string()))]),
        },
    ]
}

fn append(w: &mut TableWriter, range: std::ops::Range<u64>) -> Vec<String> {
    range
        .map(|i| {
            w.append_row(row(i), Ack::Durable).unwrap();
            render_row(&join_repeated_keys(row(i)))
        })
        .collect()
}

/// A row as text, the same whether it comes from a page or a file.
fn render_row(row: &[RecordValue]) -> String {
    format!("{row:?}")
}

/// The `RecordValue` at row `i` of a column read back as written.
fn value_at(values: &ColumnValues, i: usize) -> RecordValue {
    fn at<T: Clone>(v: &[Option<T>], i: usize) -> Option<T> {
        v.get(i).cloned().flatten()
    }
    let v = match values {
        ColumnValues::UInt64(v) => at(v, i).map(RecordValue::UInt),
        ColumnValues::TimestampNanos(v) => at(v, i).map(RecordValue::Int),
        ColumnValues::UInt16(v) => at(v, i).map(|x| RecordValue::Int(i64::from(x))),
        ColumnValues::Utf8(v) => at(v, i).map(RecordValue::Str),
        ColumnValues::Binary(v) | ColumnValues::FixedLenByteArray(v) => {
            at(v, i).map(RecordValue::Bytes)
        }
        ColumnValues::Decimal(v) => at(v, i).map(RecordValue::Decimal),
        ColumnValues::Map(v) => at(v, i).map(|m| {
            RecordValue::Map(
                m.into_iter()
                    .map(|(k, v)| (k.to_string(), v.map(|v| v.to_string())))
                    .collect(),
            )
        }),
        other => panic!("not in the test schema: {other:?}"),
    };
    v.unwrap_or(RecordValue::Null)
}

/// Every row the table holds, files then pages, sorted (tiered files sort
/// rows), each rendered from the typed columns.
fn all_rows(w: &TableWriter) -> Vec<String> {
    let s = w.snapshot().unwrap();
    let schema = schema();
    let mut rows = Vec::new();
    let mut add = |columns: &[ColumnValues], n: usize| {
        for i in 0..n {
            let row: Vec<RecordValue> = columns.iter().map(|c| value_at(c, i)).collect();
            rows.push(render_row(&row));
        }
    };
    for (_, bytes) in s.files() {
        let file = ParquetFile::open(bytes).unwrap();
        for rg in file.row_groups() {
            let columns: Vec<ColumnValues> = schema
                .iter()
                .enumerate()
                .map(|(i, spec)| read_column(&rg, &file.fields()[i], spec.column_type).unwrap())
                .collect();
            add(&columns, usize::try_from(rg.num_rows()).unwrap());
        }
    }
    for page in s.pages() {
        let PageBody::Rows(rows) = &page.body else {
            panic!("a lines page in a record table");
        };
        add(rows.columns(), rows.len());
    }
    rows.sort();
    rows
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// Each file's schema is the table's: names, order, physical and logical
/// types, repetition.
fn assert_files_have_the_schema(w: &TableWriter) {
    for file in &w.manifest().files {
        let bytes = fs::read(w.dir().join(&file.path)).unwrap();
        let parquet = ParquetFile::open(&bytes).unwrap();
        let leaves = &parquet.metadata().schema[1..];
        let names: Vec<&str> = leaves.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "session_id",
                "ts",
                "event_id",
                "kind",
                "value",
                "amount",
                "ip",
                "headers",
                "key_value",
                "key",
                "value"
            ],
            "{}",
            file.path
        );
        assert_eq!(leaves[0].converted_type, Some(ConvertedType::Uint64));
        assert_eq!(leaves[2].converted_type, Some(ConvertedType::Uint16));
        assert_eq!(leaves[4].converted_type, None, "BINARY: no annotation");
        assert_eq!(leaves[5].converted_type, Some(ConvertedType::Decimal));
        assert_eq!((leaves[5].precision, leaves[5].scale), (Some(18), Some(2)));
        assert_eq!(leaves[6].type_length, Some(16));
        // The map is its group, key_value group and two leaves.
        assert_eq!(leaves[7].converted_type, Some(ConvertedType::Other(1)));
    }
}

#[test]
fn rows_round_trip_through_replay_and_flush_with_exactly_the_schema() {
    let _serial = serial();
    set_clock(0, 10);
    let t = TempTable::new();
    let mut w = TableWriter::open(&t.0, flat()).unwrap();
    let rows = append(&mut w, 0..10);
    assert_eq!(all_rows(&w), sorted(rows.clone()));
    w.close().unwrap();

    // Replayed from the WAL...
    let mut w = TableWriter::open(&t.0, flat()).unwrap();
    assert_eq!(all_rows(&w), sorted(rows.clone()));
    // ...also by a reader in another process.
    let outside = Snapshot::open(&t.0).unwrap();
    assert_eq!(outside.rows(), 10);
    assert_eq!(outside.schema().unwrap(), schema());

    // Flushed: exactly the schema's columns, nothing else.
    w.flush_all().unwrap();
    assert_eq!(w.manifest().files.len(), 1);
    assert_files_have_the_schema(&w);
    assert_eq!(all_rows(&w), sorted(rows));
    assert_eq!(w.manifest().schema.as_deref(), Some(schema().as_slice()));
    let text = fs::read_to_string(t.0.join("manifest.json")).unwrap();
    assert!(text.starts_with(r#"{"version":3,"#), "{text}");
    assert_eq!(Manifest::parse(&text).unwrap(), *w.manifest());
}

#[test]
fn a_crash_at_every_flush_step_recovers_every_row_once() {
    let _serial = serial();
    set_clock(0, 10);
    for step in [1u8, 2, 3, 5] {
        let t = TempTable::new();
        let mut w = TableWriter::open(&t.0, flat()).unwrap();
        let mut rows = append(&mut w, 0..6);
        w.flush_all().unwrap();
        rows.extend(append(&mut w, 6..20));
        w.seal_head().unwrap();
        w.crash_after = Some(step);
        let err = w.flush().unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("after flush step {step}")),
            "{err}"
        );
        drop(w);

        let mut w = TableWriter::open(&t.0, flat()).unwrap();
        assert_eq!(
            all_rows(&w),
            sorted(rows.clone()),
            "after a crash at step {step}"
        );
        assert_eq!(w.manifest().flushed_lsn, if step >= 3 { 20 } else { 6 });
        rows.extend(append(&mut w, 20..25));
        w.flush_all().unwrap();
        assert_eq!(all_rows(&w), sorted(rows), "step {step}");
        assert_files_have_the_schema(&w);
    }
}

#[test]
fn a_crash_at_every_flush_and_compaction_step_of_a_tiered_table_recovers_every_row_once() {
    let _serial = serial();
    for step in [1u8, 2, 3, 5, 11, 12, 13] {
        let t = TempTable::new();
        set_clock(0, 10);
        let mut w = TableWriter::open(&t.0, tiered()).unwrap();
        let mut rows = append(&mut w, 0..4);
        set_clock(0, 11);
        rows.extend(append(&mut w, 4..6));
        if step >= 11 {
            set_clock(1, 0);
            rows.extend(append(&mut w, 6..7));
            w.flush().unwrap();
        }
        w.crash_after = Some(step);
        let err = if step >= 11 {
            w.compact().unwrap_err()
        } else {
            w.flush().unwrap_err()
        };
        assert!(
            err.to_string()
                .contains(&format!("after flush step {step}")),
            "{err}"
        );
        drop(w);

        let mut w = TableWriter::open(&t.0, tiered()).unwrap();
        assert_eq!(all_rows(&w), sorted(rows.clone()), "step {step}");
        w.manifest().check().unwrap();
        set_clock(2, 0);
        rows.extend(append(&mut w, 10..11));
        w.flush().unwrap();
        w.compact().unwrap();
        assert_eq!(all_rows(&w), sorted(rows), "step {step}");
        assert!(w.manifest().files.iter().any(|f| f.area == Area::Archive));
        // A day merge reads each column as declared: types survive it.
        assert_files_have_the_schema(&w);
    }
}

#[test]
fn a_tiered_hour_is_sorted_by_the_key_and_carries_arrival_times() {
    let _serial = serial();
    set_clock(0, 10);
    let t = TempTable::new();
    let mut w = TableWriter::open(&t.0, tiered()).unwrap();
    append(&mut w, 0..6);
    set_clock(0, 11);
    let entries = w.flush().unwrap();
    assert_eq!(entries.len(), 1);
    let ten = (20_737 * DAY_NS) + 10 * HOUR_NS;
    assert_eq!(entries[0].observed_ts, Some((ten, ten)));
    assert_eq!(entries[0].timestamp, None);
    let bytes = fs::read(t.0.join(&entries[0].path)).unwrap();
    let file = ParquetFile::open(&bytes).unwrap();
    let kinds: Vec<String> = file
        .row_groups()
        .flat_map(|rg| rg.read_string_column(3).unwrap())
        .map(Option::unwrap)
        .collect();
    assert_eq!(kinds, ["a", "a", "b", "b", "c", "c"]);
}

#[test]
fn rows_that_do_not_match_the_schema_are_refused() {
    let _serial = serial();
    let t = TempTable::new();
    let mut w = TableWriter::open(&t.0, flat()).unwrap();
    let with = |i: usize, v: RecordValue| {
        let mut r = row(1);
        r[i] = v;
        r
    };
    let bad = [
        (row(1)[..6].to_vec(), "6 values for 8 columns"),
        (with(7, RecordValue::Str("x".into())), "column 'headers'"),
        (
            with(0, RecordValue::Null),
            "column 'session_id' (uint64, required)",
        ),
        (with(1, RecordValue::Float(1.0)), "column 'ts'"),
        (
            with(2, RecordValue::Int(70_000)),
            "column 'event_id' (uint16, required)",
        ),
        (with(2, RecordValue::Int(-1)), "column 'event_id'"),
        (with(3, RecordValue::Int(1)), "column 'kind'"),
        (
            with(5, RecordValue::Decimal(10i128.pow(18))),
            "column 'amount'",
        ),
        (with(6, RecordValue::Bytes(vec![1; 15])), "column 'ip'"),
    ];
    for (row, why) in bad {
        let err = w.append_row(row, Ack::Durable).unwrap_err();
        assert!(matches!(err, IngestError::InvalidRecord(_)), "{err}");
        assert!(err.to_string().contains(why), "{err} lacks {why}");
    }
    let err = w.append(b"{\"a\":1}", Ack::Durable).unwrap_err();
    assert!(matches!(err, IngestError::Unsupported(_)), "{err}");
    // Nothing reached the WAL.
    assert_eq!(w.watermarks().unwrap().visible, 0);
    w.append_row(row(1), Ack::Durable).unwrap();
}

#[test]
fn a_table_opens_only_as_the_kind_and_schema_it_was_created_with() {
    let _serial = serial();
    let t = TempTable::new();
    TableWriter::open(&t.0, flat()).unwrap().close().unwrap();
    let lines = TableWriter::open(&t.0, TableOptions::default()).unwrap_err();
    assert!(matches!(lines, IngestError::SchemaMismatch), "{lines}");
    let mut other = schema();
    other[3].nullable = false;
    let err = TableWriter::open(
        &t.0,
        TableOptions {
            schema: Some(other),
            ..flat()
        },
    )
    .unwrap_err();
    assert!(matches!(err, IngestError::SchemaMismatch), "{err}");
    assert_eq!(
        err.to_string(),
        "the table's schema is not the one asked for"
    );

    let l = TempTable::new();
    let mut w = TableWriter::open(&l.0, TableOptions::default()).unwrap();
    let err = w.append_row(row(1), Ack::Durable).unwrap_err();
    assert!(matches!(err, IngestError::Unsupported(_)), "{err}");
    w.close().unwrap();
    let err = TableWriter::open(&l.0, flat()).unwrap_err();
    assert!(matches!(err, IngestError::SchemaMismatch), "{err}");
}

#[test]
fn invalid_schemas_and_sort_keys_are_refused() {
    let _serial = serial();
    let cases: [(Vec<ColumnSpec>, &str); 4] = [
        (Vec::new(), "at least one column"),
        (
            vec![
                ColumnSpec::new("a", ColumnType::Int64, true),
                ColumnSpec::new("a", ColumnType::Utf8, true),
            ],
            "duplicate column name 'a'",
        ),
        (
            vec![ColumnSpec::new("a\"b", ColumnType::Int64, true)],
            "without quotes",
        ),
        (
            vec![ColumnSpec::new(
                "d",
                ColumnType::Decimal {
                    precision: 40,
                    scale: 0,
                },
                true,
            )],
            "DECIMAL(40, 0)",
        ),
    ];
    for (schema, why) in cases {
        let t = TempTable::new();
        let err = TableWriter::open(
            &t.0,
            TableOptions {
                schema: Some(schema),
                ..flat()
            },
        )
        .unwrap_err();
        assert!(matches!(err, IngestError::InvalidSchema(_)), "{err}");
        assert!(err.to_string().contains(why), "{err} lacks {why}");
        assert!(!t.0.join("manifest.json").exists(), "nothing created");
    }
    for (key, why) in [
        (
            "nope",
            "sort key nope is not a column that sorts (a MAP does not)",
        ),
        (
            "headers",
            "sort key headers is not a column that sorts (a MAP does not)",
        ),
    ] {
        let t = TempTable::new();
        let mut options = tiered();
        if let Layout::Tiered(tier) = &mut options.layout {
            tier.sort_key = vec![key.to_string()];
        }
        let err = TableWriter::open(&t.0, options).unwrap_err();
        assert_eq!(err.to_string(), format!("invalid record schema: {why}"));
    }
}

#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors (`mcdc__<id>__vN`, joined to
    //! `tests/mcdc/obligations.json` by `make test-mcdc`).

    use super::*;
    use crate::storage::stream::detect::Format;

    fn reopen(t: &TempTable, options: TableOptions) -> Result<TableWriter, IngestError> {
        TableWriter::open(&t.0, options)
    }

    // storage_ingest_table_open_e26f89e4:
    // `manifest.schema.is_none() && manifest.format != options.format`
    #[test]
    fn mcdc__storage_ingest_table_open_e26f89e4__v1_a_lines_table_in_another_format_is_refused() {
        let t = TempTable::new();
        TableWriter::open(&t.0, TableOptions::default())
            .unwrap()
            .close()
            .unwrap();
        let logfmt = TableOptions {
            format: Format::Logfmt,
            ..TableOptions::default()
        };
        assert!(matches!(
            reopen(&t, logfmt),
            Err(IngestError::FormatMismatch { .. })
        ));
    }

    #[test]
    fn mcdc__storage_ingest_table_open_e26f89e4__v2_a_record_table_ignores_the_format() {
        let _serial = serial();
        let t = TempTable::new();
        TableWriter::open(&t.0, flat()).unwrap().close().unwrap();
        let logfmt = TableOptions {
            format: Format::Logfmt,
            ..flat()
        };
        assert!(reopen(&t, logfmt).is_ok());
    }

    #[test]
    fn mcdc__storage_ingest_table_open_e26f89e4__v3_a_lines_table_in_its_format_opens() {
        let t = TempTable::new();
        TableWriter::open(&t.0, TableOptions::default())
            .unwrap()
            .close()
            .unwrap();
        assert!(reopen(&t, TableOptions::default()).is_ok());
    }
}
