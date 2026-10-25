// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! Writes the Parquet writer's interoperability cases (#566) for
//! `tools/check_parquet_write_oracle.py`, which reads them back with DuckDB
//! and pyarrow: per case a `<name>.parquet` and a `<name>.json` holding the
//! schema and the exact values written, so the checker derives the expected
//! values and statistics from the input, not from our own reader.
//!
//!     cargo run --example parquet_write_oracle -- <out_dir>

#![allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "fixture generator for the oracle check (test tooling, like tests/): fixed, in-range case data"
)]

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use db_core::storage::ingest::Ack;
use db_core::storage::ingest::{Layout, RecordValue, TableOptions, TableWriter, TierOptions};
use db_core::storage::parquet::footer::PhysicalType;
use db_core::storage::parquet::writer::{
    ColumnSpec, ColumnType, ColumnValues, Compression, MapEntries, ParquetWriter, WriterOptions,
};
use db_core::storage::parquet::ParquetFile;

struct Case {
    name: &'static str,
    schema: Vec<ColumnSpec>,
    batches: Vec<Vec<ColumnValues>>,
    options: WriterOptions,
}

fn options(compression: Compression, dictionary: bool, row_group_rows: usize) -> WriterOptions {
    WriterOptions {
        compression,
        dictionary,
        row_group_rows,
        ..WriterOptions::default()
    }
}

fn all_types_schema() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("flag", ColumnType::Boolean, true),
        ColumnSpec::new("code", ColumnType::Int32, false),
        ColumnSpec::new("bytes", ColumnType::Int64, true),
        ColumnSpec::new("ratio", ColumnType::Float, true),
        ColumnSpec::new("latency", ColumnType::Double, false),
        ColumnSpec::new("level", ColumnType::Utf8, true),
        ColumnSpec::new("ts", ColumnType::TimestampMicros, false),
    ]
}

/// Log-like rows: repeating levels and codes (dictionary-friendly),
/// increasing timestamps, a sprinkling of nulls.
fn log_batch(start: i64, n: i64) -> Vec<ColumnValues> {
    let rows = start..start + n;
    let levels = ["INFO", "WARN", "ERROR", "DEBUG"];
    vec![
        ColumnValues::Boolean(
            rows.clone()
                .map(|i| (i % 11 != 0).then_some(i % 3 == 0))
                .collect(),
        ),
        ColumnValues::Int32(
            rows.clone()
                .map(|i| Some([200, 404, 500][(i % 3) as usize]))
                .collect(),
        ),
        ColumnValues::Int64(
            rows.clone()
                .map(|i| (i % 7 != 0).then_some(i * 1013 % 100_000))
                .collect(),
        ),
        ColumnValues::Float(
            rows.clone()
                .map(|i| (i % 5 != 0).then_some((i % 100) as f32 / 8.0))
                .collect(),
        ),
        ColumnValues::Double(
            rows.clone()
                .map(|i| Some((i % 997) as f64 * 0.125 - 3.0))
                .collect(),
        ),
        ColumnValues::Utf8(
            rows.clone()
                .map(|i| (i % 13 != 0).then(|| levels[(i % 4) as usize].to_string()))
                .collect(),
        ),
        ColumnValues::TimestampMicros(
            rows.map(|i| Some(1_760_000_000_000_000 + i * 1_000))
                .collect(),
        ),
    ]
}

fn extended_types_schema() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("blob", ColumnType::Binary, true),
        ColumnSpec::new("u8", ColumnType::UInt8, true),
        ColumnSpec::new("u16", ColumnType::UInt16, true),
        ColumnSpec::new("u32", ColumnType::UInt32, true),
        ColumnSpec::new("u64", ColumnType::UInt64, false),
        ColumnSpec::new(
            "dec_9_2",
            ColumnType::Decimal {
                precision: 9,
                scale: 2,
            },
            true,
        ),
        ColumnSpec::new(
            "dec_18_4",
            ColumnType::Decimal {
                precision: 18,
                scale: 4,
            },
            true,
        ),
        ColumnSpec::new(
            "dec_38_10",
            ColumnType::Decimal {
                precision: 38,
                scale: 10,
            },
            true,
        ),
        ColumnSpec::new("ip", ColumnType::FixedLenByteArray(16), true),
    ]
}

/// The new types' edges (#595): unsigned values at and above the signed
/// limits, decimals at their precision's limits, non-UTF-8 and empty
/// binary values.
fn extended_types_batch() -> Vec<ColumnValues> {
    let mut mapped = [0u8; 16];
    mapped[10] = 0xff;
    mapped[11] = 0xff;
    mapped[12..].copy_from_slice(&[192, 168, 1, 7]);
    let max_digits = |p: u32| 10i128.pow(p) - 1;
    vec![
        ColumnValues::Binary(vec![
            Some(Vec::new()),
            Some(vec![0x00]),
            Some(vec![0xff, 0x00]),
            Some(vec![0x80, 0x81, 0xc3]),
            None,
            Some(b"abc".to_vec()),
        ]),
        ColumnValues::UInt8(vec![
            Some(0),
            Some(u8::MAX),
            None,
            Some(1),
            Some(128),
            Some(7),
        ]),
        ColumnValues::UInt16(vec![
            Some(0),
            Some(u16::MAX),
            Some(32_768),
            None,
            Some(1),
            Some(7),
        ]),
        ColumnValues::UInt32(vec![
            Some(0),
            Some(u32::MAX),
            Some(1 << 31),
            None,
            Some(1),
            Some(7),
        ]),
        ColumnValues::UInt64(vec![
            Some(0),
            Some(u64::MAX),
            Some(1 << 63),
            Some(i64::MAX as u64),
            Some(1),
            Some(7),
        ]),
        ColumnValues::Decimal(vec![
            Some(-max_digits(9)),
            Some(max_digits(9)),
            Some(0),
            None,
            Some(-1),
            Some(1234),
        ]),
        ColumnValues::Decimal(vec![
            Some(-max_digits(18)),
            Some(max_digits(18)),
            None,
            Some(0),
            Some(-1),
            Some(1234),
        ]),
        ColumnValues::Decimal(vec![
            Some(-max_digits(38)),
            Some(max_digits(38)),
            Some(0),
            Some(-1),
            None,
            Some(12_345_678_901_234_567_890_123),
        ]),
        ColumnValues::FixedLenByteArray(vec![
            Some(vec![0; 16]),
            Some(vec![0xff; 16]),
            Some(mapped.to_vec()),
            None,
            Some((1..=16).collect()),
            Some(vec![0x80; 16]),
        ]),
    ]
}

/// #621: maps between flat columns (leaf indices shift): two entries, a
/// NULL map, an empty map, a NULL value, non-ASCII keys, 100 entries, and
/// entries in non-sorted order; a required map after them.
fn map_batch() -> Vec<ColumnValues> {
    let entries = |e: &[(&str, Option<&str>)]| {
        Some(
            e.iter()
                .map(|(k, v)| ((*k).into(), v.map(Into::into)))
                .collect::<MapEntries>(),
        )
    };
    let hundred: MapEntries = (0..100)
        .map(|i| {
            (
                format!("key{i:03}").into(),
                (i % 7 != 0).then(|| format!("v{}", i % 5).into()),
            )
        })
        .collect();
    vec![
        ColumnValues::Int32((0..7).map(Some).collect()),
        ColumnValues::Map(vec![
            entries(&[("User-Agent", Some("curl/8")), ("Accept", Some("*/*"))]),
            None,
            entries(&[]),
            entries(&[("k", None)]),
            entries(&[("héader", Some("välue")), ("ключ", Some("значение"))]),
            Some(hundred),
            entries(&[("z", Some("1")), ("a", Some("2"))]),
        ]),
        ColumnValues::Int64((0..7).map(|i| (i != 3).then_some(i * 10)).collect()),
        ColumnValues::Map(
            (0..7)
                .map(|i| entries(&[("n", Some("v"))][..(i % 2)]))
                .collect(),
        ),
    ]
}

fn map_schema() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("id", ColumnType::Int32, false),
        ColumnSpec::new("h", ColumnType::MapUtf8, true),
        ColumnSpec::new("x", ColumnType::Int64, true),
        ColumnSpec::new("r", ColumnType::MapUtf8, false),
    ]
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "maps_dictionary",
            schema: map_schema(),
            batches: (0..3).map(|_| map_batch()).collect(),
            options: options(Compression::Snappy, true, 100),
        },
        Case {
            name: "maps_plain",
            schema: map_schema(),
            batches: vec![map_batch()],
            options: options(Compression::Uncompressed, false, 3),
        },
        Case {
            name: "extended_types_dictionary",
            schema: extended_types_schema(),
            // Repeated, so the dictionary pays off.
            batches: (0..4).map(|_| extended_types_batch()).collect(),
            options: options(Compression::Snappy, true, 100),
        },
        Case {
            name: "extended_types_plain",
            schema: extended_types_schema(),
            batches: vec![extended_types_batch()],
            options: options(Compression::Uncompressed, false, 100),
        },
        Case {
            name: "logs_snappy_dictionary",
            schema: all_types_schema(),
            batches: (0..5).map(|b| log_batch(b * 4_000, 4_000)).collect(),
            options: options(Compression::Snappy, true, 6_000),
        },
        Case {
            name: "logs_uncompressed_plain",
            schema: all_types_schema(),
            batches: (0..3).map(|b| log_batch(b * 1_000, 1_000)).collect(),
            options: options(Compression::Uncompressed, false, 1_000),
        },
        // #622: zstd pages for every writer type, dictionary and PLAIN.
        Case {
            name: "maps_zstd_dictionary",
            schema: map_schema(),
            batches: (0..3).map(|_| map_batch()).collect(),
            options: options(Compression::Zstd, true, 100),
        },
        Case {
            name: "extended_types_zstd_plain",
            schema: extended_types_schema(),
            batches: vec![extended_types_batch()],
            options: options(Compression::Zstd, false, 100),
        },
        Case {
            name: "logs_zstd_dictionary",
            schema: all_types_schema(),
            batches: (0..5).map(|b| log_batch(b * 4_000, 4_000)).collect(),
            options: options(Compression::Zstd, true, 6_000),
        },
        Case {
            name: "logs_zstd_plain",
            schema: all_types_schema(),
            batches: (0..3).map(|b| log_batch(b * 1_000, 1_000)).collect(),
            options: options(Compression::Zstd, false, 1_000),
        },
        Case {
            name: "edge_values",
            schema: vec![
                ColumnSpec::new("i32", ColumnType::Int32, true),
                ColumnSpec::new("i64", ColumnType::Int64, true),
                ColumnSpec::new("f32", ColumnType::Float, true),
                ColumnSpec::new("f64", ColumnType::Double, true),
                ColumnSpec::new("s", ColumnType::Utf8, true),
                ColumnSpec::new("ts", ColumnType::TimestampMicros, true),
                ColumnSpec::new("ts_ns", ColumnType::TimestampNanos, true),
            ],
            batches: vec![vec![
                ColumnValues::Int32(vec![
                    Some(i32::MIN),
                    Some(i32::MAX),
                    Some(0),
                    None,
                    Some(-1),
                    Some(1),
                ]),
                ColumnValues::Int64(vec![
                    Some(i64::MIN),
                    Some(i64::MAX),
                    None,
                    Some(0),
                    Some(-1),
                    Some(1),
                ]),
                ColumnValues::Float(vec![
                    Some(f32::NAN),
                    Some(-0.0),
                    Some(0.0),
                    Some(f32::MIN_POSITIVE),
                    None,
                    Some(f32::MAX),
                ]),
                ColumnValues::Double(vec![
                    Some(f64::NEG_INFINITY),
                    Some(f64::INFINITY),
                    Some(f64::NAN),
                    Some(-0.0),
                    Some(5e-324),
                    None,
                ]),
                ColumnValues::Utf8(vec![
                    Some(String::new()),
                    Some("é\u{1F980}\u{0}".to_string()),
                    Some("quote \" backslash \\ newline \n tab \t".to_string()),
                    Some("z".repeat(10_000)),
                    None,
                    Some("A".to_string()),
                ]),
                ColumnValues::TimestampMicros(vec![
                    Some(0),
                    Some(-1),
                    Some(253_402_300_799_999_999),
                    None,
                    Some(1_760_000_000_000_000),
                    Some(-62_135_596_800_000_000),
                ]),
                ColumnValues::TimestampNanos(vec![
                    Some(0),
                    Some(-1),
                    // DuckDB's TIMESTAMP_NS uses i64::MAX and i64::MIN + 1 as
                    // +/-infinity, and cannot convert instants at the very
                    // bottom of the range (late 1677) even in files it wrote
                    // itself: the near-max value and 1678-06-18 are the
                    // edges every reader agrees on.
                    Some(i64::MAX - 1),
                    None,
                    Some(1_760_000_000_123_456_789),
                    Some(-9_200_000_000_000_000_000),
                ]),
            ]],
            options: options(Compression::Snappy, true, 100),
        },
        Case {
            name: "all_null_and_single_value",
            schema: vec![
                ColumnSpec::new("nothing", ColumnType::Utf8, true),
                ColumnSpec::new("only", ColumnType::Int64, true),
                ColumnSpec::new("nan", ColumnType::Double, true),
            ],
            batches: vec![vec![
                ColumnValues::Utf8(vec![None; 50]),
                ColumnValues::Int64((0..50).map(|i| (i % 2 == 0).then_some(42)).collect()),
                ColumnValues::Double(vec![Some(f64::NAN); 50]),
            ]],
            options: options(Compression::Snappy, true, 100),
        },
        Case {
            name: "empty_file",
            schema: all_types_schema(),
            batches: Vec::new(),
            options: WriterOptions::default(),
        },
    ]
}

fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python's `json` reads `NaN`/`Infinity`/`-Infinity`; finite values print
/// in Rust's shortest round-trip form.
fn json_float(out: &mut String, x: f64) {
    if x.is_nan() {
        out.push_str("NaN");
    } else if x.is_infinite() {
        out.push_str(if x > 0.0 { "Infinity" } else { "-Infinity" });
    } else {
        out.push_str(&format!("{x:?}"));
    }
}

fn json_values(out: &mut String, values: &ColumnValues) {
    fn list<T>(out: &mut String, v: &[Option<T>], mut item: impl FnMut(&mut String, &T)) {
        out.push('[');
        for (i, x) in v.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            match x {
                Some(x) => item(out, x),
                None => out.push_str("null"),
            }
        }
        out.push(']');
    }
    match values {
        ColumnValues::Boolean(v) => {
            list(out, v, |o, b| o.push_str(if *b { "true" } else { "false" }))
        }
        ColumnValues::Int32(v) => list(out, v, |o, x| {
            o.push_str(&format!("{x}"));
        }),
        ColumnValues::Int64(v)
        | ColumnValues::TimestampMicros(v)
        | ColumnValues::TimestampNanos(v) => list(out, v, |o, x| {
            o.push_str(&format!("{x}"));
        }),
        // An f32 widens to f64 exactly; the checker rounds back to f32.
        ColumnValues::Float(v) => list(out, v, |o, x| json_float(o, f64::from(*x))),
        ColumnValues::Double(v) => list(out, v, |o, x| json_float(o, *x)),
        ColumnValues::Utf8(v) => list(out, v, |o, s| json_string(o, s)),
        // Bytes as lowercase hex.
        ColumnValues::Binary(v) | ColumnValues::FixedLenByteArray(v) => list(out, v, |o, b| {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            json_string(o, &hex);
        }),
        ColumnValues::UInt8(v) => list(out, v, |o, x| o.push_str(&format!("{x}"))),
        ColumnValues::UInt16(v) => list(out, v, |o, x| o.push_str(&format!("{x}"))),
        ColumnValues::UInt32(v) => list(out, v, |o, x| o.push_str(&format!("{x}"))),
        ColumnValues::UInt64(v) => list(out, v, |o, x| o.push_str(&format!("{x}"))),
        // The unscaled value; the scale is in the type name.
        ColumnValues::Decimal(v) => list(out, v, |o, x| o.push_str(&format!("{x}"))),
        // A map as its [key, value|null] pairs, in order.
        ColumnValues::Map(v) => list(out, v, |o, entries| {
            o.push('[');
            for (i, (k, val)) in entries.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                o.push('[');
                json_string(o, k);
                o.push(',');
                match val {
                    Some(val) => json_string(o, val),
                    None => o.push_str("null"),
                }
                o.push(']');
            }
            o.push(']');
        }),
    }
}

fn type_name(t: ColumnType) -> String {
    match t {
        ColumnType::Boolean => "boolean".into(),
        ColumnType::Int32 => "int32".into(),
        ColumnType::Int64 => "int64".into(),
        ColumnType::Float => "float".into(),
        ColumnType::Double => "double".into(),
        ColumnType::Utf8 => "utf8".into(),
        ColumnType::TimestampMicros => "timestamp_micros".into(),
        ColumnType::TimestampNanos => "timestamp_nanos".into(),
        ColumnType::Binary => "binary".into(),
        ColumnType::UInt8 => "uint8".into(),
        ColumnType::UInt16 => "uint16".into(),
        ColumnType::UInt32 => "uint32".into(),
        ColumnType::UInt64 => "uint64".into(),
        ColumnType::Decimal { precision, scale } => format!("decimal({precision},{scale})"),
        ColumnType::FixedLenByteArray(len) => format!("fixed({len})"),
        ColumnType::MapUtf8 => "map(utf8,utf8)".into(),
    }
}

fn case_json(case: &Case) -> String {
    let mut out = String::from("{\"columns\":[");
    for (i, spec) in case.schema.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        json_string(&mut out, &spec.name);
        out.push_str(&format!(
            ",\"type\":\"{}\",\"nullable\":{},\"values\":",
            type_name(spec.column_type),
            spec.nullable
        ));
        let mut all = ColumnValues::empty(spec.column_type);
        for batch in &case.batches {
            all = concat(all, &batch[i]);
        }
        json_values(&mut out, &all);
        out.push('}');
    }
    out.push_str("]}");
    out
}

fn concat(a: ColumnValues, b: &ColumnValues) -> ColumnValues {
    match (a, b) {
        (ColumnValues::Boolean(mut a), ColumnValues::Boolean(b)) => {
            a.extend_from_slice(b);
            ColumnValues::Boolean(a)
        }
        (ColumnValues::Int32(mut a), ColumnValues::Int32(b)) => {
            a.extend_from_slice(b);
            ColumnValues::Int32(a)
        }
        (ColumnValues::Int64(mut a), ColumnValues::Int64(b)) => {
            a.extend_from_slice(b);
            ColumnValues::Int64(a)
        }
        (ColumnValues::Float(mut a), ColumnValues::Float(b)) => {
            a.extend_from_slice(b);
            ColumnValues::Float(a)
        }
        (ColumnValues::Double(mut a), ColumnValues::Double(b)) => {
            a.extend_from_slice(b);
            ColumnValues::Double(a)
        }
        (ColumnValues::Utf8(mut a), ColumnValues::Utf8(b)) => {
            a.extend_from_slice(b);
            ColumnValues::Utf8(a)
        }
        (ColumnValues::TimestampMicros(mut a), ColumnValues::TimestampMicros(b)) => {
            a.extend_from_slice(b);
            ColumnValues::TimestampMicros(a)
        }
        (ColumnValues::TimestampNanos(mut a), ColumnValues::TimestampNanos(b)) => {
            a.extend_from_slice(b);
            ColumnValues::TimestampNanos(a)
        }
        (ColumnValues::Binary(mut a), ColumnValues::Binary(b)) => {
            a.extend_from_slice(b);
            ColumnValues::Binary(a)
        }
        (ColumnValues::UInt8(mut a), ColumnValues::UInt8(b)) => {
            a.extend_from_slice(b);
            ColumnValues::UInt8(a)
        }
        (ColumnValues::UInt16(mut a), ColumnValues::UInt16(b)) => {
            a.extend_from_slice(b);
            ColumnValues::UInt16(a)
        }
        (ColumnValues::UInt32(mut a), ColumnValues::UInt32(b)) => {
            a.extend_from_slice(b);
            ColumnValues::UInt32(a)
        }
        (ColumnValues::UInt64(mut a), ColumnValues::UInt64(b)) => {
            a.extend_from_slice(b);
            ColumnValues::UInt64(a)
        }
        (ColumnValues::Decimal(mut a), ColumnValues::Decimal(b)) => {
            a.extend_from_slice(b);
            ColumnValues::Decimal(a)
        }
        (ColumnValues::FixedLenByteArray(mut a), ColumnValues::FixedLenByteArray(b)) => {
            a.extend_from_slice(b);
            ColumnValues::FixedLenByteArray(a)
        }
        (ColumnValues::Map(mut a), ColumnValues::Map(b)) => {
            a.extend_from_slice(b);
            ColumnValues::Map(a)
        }
        (a, _) => a,
    }
}

fn write_case(dir: &Path, case: Case) -> Result<(), String> {
    let json = case_json(&case);
    let mut writer = ParquetWriter::new(Vec::new(), case.schema, case.options)
        .map_err(|e| format!("{}: {e}", case.name))?;
    for batch in case.batches {
        writer
            .write_columns(batch)
            .map_err(|e| format!("{}: {e}", case.name))?;
    }
    let (bytes, summary) = writer.finish().map_err(|e| format!("{}: {e}", case.name))?;
    fs::write(dir.join(format!("{}.parquet", case.name)), bytes).map_err(|e| e.to_string())?;
    fs::write(dir.join(format!("{}.json", case.name)), json).map_err(|e| e.to_string())?;
    println!(
        "{}: {} rows, {} row groups, {} bytes",
        case.name, summary.num_rows, summary.row_groups, summary.bytes
    );
    Ok(())
}

/// The first file a real ingested table flushes (#572): JSON lines with
/// fields `n` (int), `lat` (float), `ok` (bool) and `user` (string), some
/// fields missing, some lines not JSON at all. Some `user` and `msg`
/// strings carry JSON escapes (#583); `json_fields` asks the checker to
/// compare those columns with DuckDB's own decoding of each `raw` line. Values are given for `raw`,
/// `n` and `user`, which follow from the input alone; for the columns the
/// parser derives (severity, message...) and the arrival-time
/// `observed_ts`, the JSON says `null` and the checker requires DuckDB and
/// pyarrow to agree with each other and with the file's statistics.
fn ingested_table_case(dir: &Path) -> Result<(), String> {
    let table = dir.join("ingested_table.dir");
    fs::remove_dir_all(&table).ok();
    let mut writer =
        TableWriter::open(&table, TableOptions::default()).map_err(|e| e.to_string())?;
    let mut lines = Vec::new();
    for i in 0..3_000i64 {
        let line = match i % 5 {
            0 => format!("plain text line {i}"),
            1 => format!(
                r#"{{"n":{i},"user":"{}","msg":"line {i}: \"quoted\"\n\tcaf\u00e9 \ud83d\ude00 C:\\tmp"}}"#,
                user_json(i)
            ),
            _ => format!(
                r#"{{"n":{i},"lat":{i}.25,"ok":{},"user":"{}"}}"#,
                i % 2 == 0,
                user_json(i)
            ),
        };
        writer
            .append(line.as_bytes(), Ack::Memory)
            .map_err(|e| e.to_string())?;
        lines.push((i, line));
        if i == 1_999 {
            writer.flush_all().map_err(|e| e.to_string())?;
        }
    }
    writer.flush_all().map_err(|e| e.to_string())?;
    let first = writer
        .manifest()
        .files
        .first()
        .ok_or("no file flushed")?
        .clone();
    writer.close().map_err(|e| e.to_string())?;
    let bytes = fs::read(table.join(&first.path)).map_err(|e| e.to_string())?;
    let parquet = ParquetFile::open(&bytes).map_err(|e| e.to_string())?;
    let rows = &lines[..first.rows as usize];
    let mut out = String::from("{\"columns\":[");
    for (i, element) in parquet.metadata().schema[1..].iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let type_name = match (element.name.as_str(), element.physical_type) {
            ("timestamp" | "observed_ts", _) => "timestamp_nanos",
            (_, Some(PhysicalType::Int32)) => "int32",
            (_, Some(PhysicalType::Int64)) => "int64",
            (_, Some(PhysicalType::Double)) => "double",
            (_, Some(PhysicalType::Boolean)) => "boolean",
            _ => "utf8",
        };
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"type\":\"{type_name}\",\"nullable\":true,\"values\":",
            element.name
        ));
        let values: Option<ColumnValues> = match element.name.as_str() {
            "raw" => Some(ColumnValues::Utf8(
                rows.iter().map(|(_, l)| Some(l.clone())).collect(),
            )),
            "n" => Some(ColumnValues::Int64(
                rows.iter()
                    .map(|(i, _)| (i % 5 != 0).then_some(*i))
                    .collect(),
            )),
            "user" => Some(ColumnValues::Utf8(
                rows.iter()
                    .map(|(i, _)| (i % 5 != 0).then(|| user_value(*i)))
                    .collect(),
            )),
            _ => None,
        };
        match values {
            Some(v) => json_values(&mut out, &v),
            None => out.push_str("null"),
        }
        out.push('}');
    }
    out.push_str("],\"json_fields\":{\"user\":\"user\",\"message\":\"msg\"}}");
    fs::write(dir.join("ingested_table.parquet"), &bytes).map_err(|e| e.to_string())?;
    fs::write(dir.join("ingested_table.json"), out).map_err(|e| e.to_string())?;
    fs::remove_dir_all(&table).ok();
    println!("ingested_table: {} rows from a flushed table", first.rows);
    Ok(())
}

/// `user` as written in line `i`'s JSON: every seventh carries escapes.
fn user_json(i: i64) -> String {
    match i % 7 {
        3 => r#"q\"3\\\u00e9"#.to_string(),
        k => format!("u{k}"),
    }
}

/// The string [`user_json`] stands for.
fn user_value(i: i64) -> String {
    match i % 7 {
        3 => "q\"3\\é".to_string(),
        k => format!("u{k}"),
    }
}

/// Arrival time for the tiered table: set by `tiered_table_case`.
static NOW: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

fn now() -> i64 {
    NOW.load(std::sync::atomic::Ordering::SeqCst)
}

/// A tiered table (ADR 0028, #573) over three closed days and an open
/// hour, its closed days merged into `archive/` and the first dropped by
/// retention. Writes the table under
/// `tiered_table/` and the `raw` of every record the archive must hold,
/// sorted, with its date, to `tiered_table.archive`.
fn tiered_table_case(dir: &Path) -> Result<(), String> {
    const HOUR: i64 = 3_600_000_000_000;
    const DAY: i64 = 24 * HOUR;
    let table = dir.join("tiered_table");
    fs::remove_dir_all(&table).ok();
    let start = 20_737 * DAY; // 2026-10-11
    NOW.store(start, std::sync::atomic::Ordering::SeqCst);
    let options = TableOptions {
        layout: Layout::Tiered(TierOptions {
            sort_key: vec!["user".to_string()],
            row_group_max_rows: 100,
            max_file_size: 64 * 1024,
            delete_grace_ns: 0,
            // Every 7th record is flushed: the open hour spills to
            // staging/, and its close merges the runs.
            spill_rows: 1_000_000,
            // Keeps two closed days: the first is dropped once the fourth
            // opens.
            retention_days: Some(2),
            session_key: None,
        }),
        clock: now,
        ..TableOptions::default()
    };
    let mut writer = TableWriter::open(&table, options).map_err(|e| e.to_string())?;
    let mut archived = Vec::new();
    for i in 0..4_000i64 {
        // Three days and a bit, a record every ~70 seconds.
        let at = start + i * 70_000_000_000;
        NOW.store(at, std::sync::atomic::Ordering::SeqCst);
        let line = match i % 4 {
            0 => format!(r#"{{"n":{i},"user":"u{}"}}"#, i % 7),
            1 => format!(r#"{{"n":{i},"lat":{i}.5,"user":"u{}"}}"#, i % 7),
            2 => format!(r#"{{"n":"x{i}","ok":true}}"#),
            _ => format!("plain text line {i}"),
        };
        writer
            .append(
                line.as_bytes(),
                if i % 7 == 0 {
                    Ack::Flushed
                } else {
                    Ack::Memory
                },
            )
            .map_err(|e| e.to_string())?;
        let day = at.div_euclid(DAY) - 20_737;
        if (1..3).contains(&day) {
            archived.push((format!("2026-10-{}", 11 + day), line));
        }
        if i % 500 == 0 {
            writer.compact().map_err(|e| e.to_string())?;
        }
    }
    NOW.store(
        start + 3 * DAY + 2 * HOUR,
        std::sync::atomic::Ordering::SeqCst,
    );
    writer.compact().map_err(|e| e.to_string())?;
    writer.compact().map_err(|e| e.to_string())?; // deletes the replaced hours
    let areas: Vec<&str> = writer
        .manifest()
        .files
        .iter()
        .map(|f| f.area.as_str())
        .collect();
    if !areas.contains(&"archive") || !areas.contains(&"live") {
        return Err(format!("expected archive and live files, got {areas:?}"));
    }
    writer.close().map_err(|e| e.to_string())?;
    archived.sort_by(|a, b| a.1.cmp(&b.1));
    let mut out = String::from("[");
    for (i, (date, raw)) in archived.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("[{:?},{:?}]", date, raw));
    }
    out.push(']');
    fs::write(dir.join("tiered_table.archive"), out).map_err(|e| e.to_string())?;
    println!(
        "tiered_table: {} archived rows over 2 days kept of 3",
        archived.len()
    );
    Ok(())
}

/// #594: a relational dump (transactions) imported into a tiered record
/// table over three days and compacted. Writes `record_table.records`: the
/// declared columns and every archived row, so the checker reads
/// `archive/` with DuckDB and pyarrow (no `union_by_name`: every file must
/// carry exactly the declared schema) and compares types and values.
fn record_table_case(dir: &Path) -> Result<(), String> {
    const HOUR: i64 = 3_600_000_000_000;
    const DAY: i64 = 24 * HOUR;
    let table = dir.join("record_table");
    fs::remove_dir_all(&table).ok();
    let schema = vec![
        ColumnSpec::new("txn_id", ColumnType::UInt64, false),
        ColumnSpec::new("account", ColumnType::Utf8, false),
        ColumnSpec::new(
            "amount",
            ColumnType::Decimal {
                precision: 18,
                scale: 2,
            },
            false,
        ),
        ColumnSpec::new("booked_at", ColumnType::TimestampMicros, false),
        ColumnSpec::new("channel", ColumnType::UInt8, false),
        ColumnSpec::new("ip", ColumnType::FixedLenByteArray(16), true),
        ColumnSpec::new("memo", ColumnType::Binary, true),
        ColumnSpec::new("score", ColumnType::Double, true),
        ColumnSpec::new("headers", ColumnType::MapUtf8, true),
    ];
    let start = 20_737 * DAY; // 2026-10-11
    NOW.store(start, std::sync::atomic::Ordering::SeqCst);
    let options = TableOptions {
        schema: Some(schema.clone()),
        layout: Layout::Tiered(TierOptions {
            sort_key: vec!["account".to_string(), "txn_id".to_string()],
            row_group_max_rows: 200,
            max_file_size: 32 * 1024,
            delete_grace_ns: 0,
            // Every 7th row is flushed, as in the tiered table case.
            spill_rows: 1_000_000,
            retention_days: None,
            session_key: None,
        }),
        clock: now,
        ..TableOptions::default()
    };
    let mut writer = TableWriter::open(&table, options).map_err(|e| e.to_string())?;
    let mut archived: Vec<Vec<String>> = Vec::new();
    for i in 0..3_000u64 {
        let at = start + i.cast_signed() * 100_000_000_000;
        NOW.store(at, std::sync::atomic::Ordering::SeqCst);
        let mut ip = [0u8; 16];
        ip[10] = 0xff;
        ip[11] = 0xff;
        ip[12..].copy_from_slice(&[10, 0, (i / 256) as u8, i as u8]);
        // Ids above i64::MAX, amounts both signs, some NULLs.
        let mut row = vec![
            RecordValue::UInt(u64::MAX - i * 7_919),
            RecordValue::Str(format!("NL{:02}BANK{:04}", i % 13, i % 97)),
            RecordValue::Decimal((i as i128 - 1_500) * 1_234_567),
            RecordValue::Int(1_760_000_000_000_000 + i.cast_signed() * 1_000_003),
            RecordValue::UInt(i % 256),
            if i.is_multiple_of(9) {
                RecordValue::Null
            } else {
                RecordValue::Bytes(ip.to_vec())
            },
            if i.is_multiple_of(4) {
                RecordValue::Null
            } else {
                RecordValue::Bytes(vec![0x80 | (i % 64) as u8, 0, 0xff])
            },
            if i.is_multiple_of(5) {
                RecordValue::Null
            } else {
                RecordValue::Float(i as f64 * 0.25 - 100.0)
            },
            RecordValue::Null,
        ];
        let (headers, stored) = record_headers(i);
        let mut json: Vec<String> = row.iter().map(record_json).collect();
        if let (Some(last), Some(slot)) = (row.last_mut(), json.last_mut()) {
            *last = headers;
            *slot = record_json(&stored);
        }
        writer
            .append_row(
                row,
                if i % 7 == 0 {
                    Ack::Flushed
                } else {
                    Ack::Memory
                },
            )
            .map_err(|e| e.to_string())?;
        if (at.div_euclid(DAY) - 20_737) < 3 {
            archived.push(json);
        }
        if i % 400 == 0 {
            writer.compact().map_err(|e| e.to_string())?;
        }
    }
    NOW.store(
        start + 3 * DAY + 2 * HOUR,
        std::sync::atomic::Ordering::SeqCst,
    );
    writer.compact().map_err(|e| e.to_string())?;
    writer.compact().map_err(|e| e.to_string())?;
    if !writer
        .manifest()
        .files
        .iter()
        .any(|f| f.area.as_str() == "archive")
    {
        return Err("expected archived days".to_string());
    }
    writer.close().map_err(|e| e.to_string())?;
    let mut out = String::from("{\"columns\":[");
    for (i, spec) in schema.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"type\":\"{}\",\"nullable\":{}}}",
            spec.name,
            type_name(spec.column_type),
            spec.nullable
        ));
    }
    out.push_str("],\"rows\":[");
    for (i, row) in archived.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("[{}]", row.join(",")));
    }
    out.push_str("]}");
    fs::write(dir.join("record_table.records"), out).map_err(|e| e.to_string())?;
    println!("record_table: {} archived rows over 3 days", archived.len());
    Ok(())
}

/// Row `i`'s `headers` map as appended, and as the table stores it (a
/// repeated key's values joined, #621): NULL, empty, 1, 3 and 100 entries,
/// a NULL value, non-ASCII keys, a repeated `X-Forwarded-For`.
fn record_headers(i: u64) -> (RecordValue, RecordValue) {
    let map = |e: &[(&str, Option<&str>)]| {
        RecordValue::Map(
            e.iter()
                .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
                .collect(),
        )
    };
    let same = |v: RecordValue| (v.clone(), v);
    match i % 7 {
        0 => same(RecordValue::Null),
        1 => same(map(&[])),
        2 => same(map(&[("User-Agent", Some("curl/8"))])),
        3 => same(map(&[
            ("Accept", None),
            ("héader", Some("välue")),
            ("ключ", Some("значение")),
        ])),
        4 => same(RecordValue::Map(
            (0..100)
                .map(|k| (format!("h{k:03}"), Some(format!("{}", k * i))))
                .collect(),
        )),
        5 => (
            map(&[
                ("X-Forwarded-For", Some("10.0.0.1")),
                ("Host", Some("a")),
                ("X-Forwarded-For", Some("10.0.0.2")),
            ]),
            map(&[
                ("X-Forwarded-For", Some("10.0.0.1, 10.0.0.2")),
                ("Host", Some("a")),
            ]),
        ),
        _ => same(map(&[("Host", Some(&format!("h{i}")))])),
    }
}

/// A record value as the checker's JSON: integers and unscaled decimals
/// as numbers, bytes as lowercase hex, a map as `[key, value]` pairs.
fn record_json(v: &RecordValue) -> String {
    let mut out = String::new();
    match v {
        RecordValue::Null => out.push_str("null"),
        RecordValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        RecordValue::Int(i) => out.push_str(&i.to_string()),
        RecordValue::UInt(u) => out.push_str(&u.to_string()),
        RecordValue::Decimal(d) => out.push_str(&d.to_string()),
        RecordValue::Float(f) => json_float(&mut out, *f),
        RecordValue::Str(s) => json_string(&mut out, s),
        RecordValue::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            json_string(&mut out, &hex);
        }
        RecordValue::Map(entries) => {
            out.push('[');
            for (n, (k, v)) in entries.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                out.push('[');
                json_string(&mut out, k);
                out.push(',');
                match v {
                    Some(v) => json_string(&mut out, v),
                    None => out.push_str("null"),
                }
                out.push(']');
            }
            out.push(']');
        }
    }
    out
}

fn main() -> ExitCode {
    let Some(dir) = std::env::args().nth(1) else {
        eprintln!("usage: parquet_write_oracle <out_dir>");
        return ExitCode::from(2);
    };
    let dir = Path::new(&dir);
    if let Err(e) = fs::create_dir_all(dir) {
        eprintln!("{}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    for case in cases() {
        if let Err(e) = write_case(dir, case) {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    }
    if let Err(e) = ingested_table_case(dir) {
        eprintln!("ingested_table: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = tiered_table_case(dir) {
        eprintln!("tiered_table: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = record_table_case(dir) {
        eprintln!("record_table: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
