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

use db_core::storage::parquet::writer::{
    ColumnSpec, ColumnType, ColumnValues, Compression, ParquetWriter, WriterOptions,
};

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

fn cases() -> Vec<Case> {
    vec![
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
        Case {
            name: "edge_values",
            schema: vec![
                ColumnSpec::new("i32", ColumnType::Int32, true),
                ColumnSpec::new("i64", ColumnType::Int64, true),
                ColumnSpec::new("f32", ColumnType::Float, true),
                ColumnSpec::new("f64", ColumnType::Double, true),
                ColumnSpec::new("s", ColumnType::Utf8, true),
                ColumnSpec::new("ts", ColumnType::TimestampMicros, true),
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
        ColumnValues::Int64(v) | ColumnValues::TimestampMicros(v) => list(out, v, |o, x| {
            o.push_str(&format!("{x}"));
        }),
        // An f32 widens to f64 exactly; the checker rounds back to f32.
        ColumnValues::Float(v) => list(out, v, |o, x| json_float(o, f64::from(*x))),
        ColumnValues::Double(v) => list(out, v, |o, x| json_float(o, *x)),
        ColumnValues::Utf8(v) => list(out, v, |o, s| json_string(o, s)),
    }
}

fn type_name(t: ColumnType) -> &'static str {
    match t {
        ColumnType::Boolean => "boolean",
        ColumnType::Int32 => "int32",
        ColumnType::Int64 => "int64",
        ColumnType::Float => "float",
        ColumnType::Double => "double",
        ColumnType::Utf8 => "utf8",
        ColumnType::TimestampMicros => "timestamp_micros",
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
    ExitCode::SUCCESS
}
