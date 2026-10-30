// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! Parquet writer (#566): every file written is read back with the in-tree
//! reader ([`ParquetFile::open`] over the bytes), values and statistics
//! compared, including a property test over random flat schemas and data.
//! Here rather than under `src/` so the `proptest` macros stay outside the
//! qualified-subset gate's scan.

#![cfg(feature = "storage-column-write")]
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

use std::io::{self, Write};

use db_core::storage::parquet::footer::{Codec, LogicalType, Statistics};
use db_core::storage::parquet::page::Encoding;
use db_core::storage::parquet::writer::*;
use db_core::storage::parquet::ParquetFile;
use proptest::prelude::*;

/// `true` where the value is not null.
fn validity(values: &ColumnValues) -> Vec<bool> {
    match values {
        ColumnValues::Boolean(v) => v.iter().map(Option::is_some).collect(),
        ColumnValues::Int32(v) => v.iter().map(Option::is_some).collect(),
        ColumnValues::Int64(v)
        | ColumnValues::TimestampMicros(v)
        | ColumnValues::TimestampNanos(v) => v.iter().map(Option::is_some).collect(),
        ColumnValues::Float(v) => v.iter().map(Option::is_some).collect(),
        ColumnValues::Double(v) => v.iter().map(Option::is_some).collect(),
        ColumnValues::Utf8(v) => v.iter().map(Option::is_some).collect(),
        ColumnValues::Binary(v) | ColumnValues::FixedLenByteArray(v) => {
            v.iter().map(Option::is_some).collect()
        }
        ColumnValues::UInt8(v) => v.iter().map(Option::is_some).collect(),
        ColumnValues::UInt16(v) => v.iter().map(Option::is_some).collect(),
        ColumnValues::UInt32(v) => v.iter().map(Option::is_some).collect(),
        ColumnValues::UInt64(v) => v.iter().map(Option::is_some).collect(),
        ColumnValues::Decimal(v) => v.iter().map(Option::is_some).collect(),
        ColumnValues::Map(v) => v.iter().map(Option::is_some).collect(),
        ColumnValues::List(v) => v.iter().map(Option::is_some).collect(),
    }
}

fn append(into: &mut ColumnValues, from: ColumnValues) {
    match (into, from) {
        (ColumnValues::Boolean(a), ColumnValues::Boolean(b)) => a.extend(b),
        (ColumnValues::Int32(a), ColumnValues::Int32(b)) => a.extend(b),
        (ColumnValues::Int64(a), ColumnValues::Int64(b)) => a.extend(b),
        (ColumnValues::Float(a), ColumnValues::Float(b)) => a.extend(b),
        (ColumnValues::Double(a), ColumnValues::Double(b)) => a.extend(b),
        (ColumnValues::Utf8(a), ColumnValues::Utf8(b)) => a.extend(b),
        (ColumnValues::TimestampMicros(a), ColumnValues::TimestampMicros(b)) => a.extend(b),
        (ColumnValues::TimestampNanos(a), ColumnValues::TimestampNanos(b)) => a.extend(b),
        (ColumnValues::Binary(a), ColumnValues::Binary(b)) => a.extend(b),
        (ColumnValues::UInt8(a), ColumnValues::UInt8(b)) => a.extend(b),
        (ColumnValues::UInt16(a), ColumnValues::UInt16(b)) => a.extend(b),
        (ColumnValues::UInt32(a), ColumnValues::UInt32(b)) => a.extend(b),
        (ColumnValues::UInt64(a), ColumnValues::UInt64(b)) => a.extend(b),
        (ColumnValues::Decimal(a), ColumnValues::Decimal(b)) => a.extend(b),
        (ColumnValues::Map(a), ColumnValues::Map(b)) => a.extend(b),
        (ColumnValues::List(a), ColumnValues::List(b)) => a.extend(b),
        (ColumnValues::FixedLenByteArray(a), ColumnValues::FixedLenByteArray(b)) => a.extend(b),
        (a, b) => panic!("append {b:?} to {a:?}"),
    }
}

fn write(
    schema: Vec<ColumnSpec>,
    options: WriterOptions,
    batches: Vec<Vec<ColumnValues>>,
) -> Vec<u8> {
    let mut w = ParquetWriter::new(Vec::new(), schema, options).unwrap();
    for batch in batches {
        w.write_columns(batch).unwrap();
    }
    w.finish().unwrap().0
}

fn options(compression: Compression, dictionary: bool, row_group_rows: usize) -> WriterOptions {
    WriterOptions {
        compression,
        row_group_rows,
        dictionary,
        ..WriterOptions::default()
    }
}

/// The footer reads back the `LogicalType` union and each chunk's
/// encodings the writer recorded, for tools that describe a file.
#[test]
fn the_footer_reads_back_logical_types_and_encodings() {
    let schema = vec![
        ColumnSpec::new("s", ColumnType::Utf8, true),
        ColumnSpec::new("us", ColumnType::TimestampMicros, false),
        ColumnSpec::new("ns", ColumnType::TimestampNanos, false),
        ColumnSpec::new("u8", ColumnType::UInt8, true),
        ColumnSpec::new("d", decimal(9, 2), true),
        ColumnSpec::new("labels", ColumnType::ListUtf8, true),
        ColumnSpec::new("i", ColumnType::Int32, false),
    ];
    let batch = vec![
        ColumnValues::Utf8(vec![Some("a".into()), Some("a".into())]),
        ColumnValues::TimestampMicros(vec![Some(1), Some(2)]),
        ColumnValues::TimestampNanos(vec![Some(1), Some(2)]),
        ColumnValues::UInt8(vec![Some(1), None]),
        ColumnValues::Decimal(vec![Some(100), None]),
        ColumnValues::List(vec![list(&[Some("x")]), None]),
        ColumnValues::Int32(vec![Some(1), Some(2)]),
    ];
    let bytes = write(
        schema,
        options(Compression::Uncompressed, true, 100),
        vec![batch],
    );
    let file = ParquetFile::open(&bytes).unwrap();
    let meta = file.metadata();
    let logical: Vec<_> = meta
        .schema
        .iter()
        .map(|e| (e.name.as_str(), e.logical_type))
        .collect();
    assert_eq!(
        logical,
        [
            ("schema", None),
            ("s", Some(LogicalType::String)),
            (
                "us",
                Some(LogicalType::Timestamp {
                    units: Some(1_000_000),
                    utc: true
                })
            ),
            (
                "ns",
                Some(LogicalType::Timestamp {
                    units: Some(1_000_000_000),
                    utc: false
                })
            ),
            (
                "u8",
                Some(LogicalType::Integer {
                    bit_width: 8,
                    signed: false
                })
            ),
            (
                "d",
                Some(LogicalType::Decimal {
                    scale: 2,
                    precision: 9
                })
            ),
            ("labels", Some(LogicalType::List)),
            ("list", None),
            ("element", Some(LogicalType::String)),
            ("i", None),
        ]
    );
    let encodings = |leaf: usize| {
        let chunk = meta.row_groups[0].columns[leaf].meta_data.as_ref().unwrap();
        chunk.encodings.clone()
    };
    assert_eq!(
        encodings(0),
        [Encoding::Plain, Encoding::Rle, Encoding::RleDictionary]
    );
    assert_eq!(encodings(6)[..2], [Encoding::Plain, Encoding::Rle]);
}

/// One row group's column, read back as the variant it was written as.
fn read_column(
    file: &ParquetFile,
    group: usize,
    field: usize,
    column_type: ColumnType,
) -> ColumnValues {
    let rg = file.row_group(group).unwrap();
    // Leaf (column chunk) indices: a map before a column shifts it.
    let field = &file.fields()[field];
    let column = field.leaves.start;
    match column_type {
        ColumnType::MapUtf8 => ColumnValues::Map(rg.read_map_utf8_column(field).unwrap()),
        ColumnType::ListUtf8 => ColumnValues::List(rg.read_list_utf8_column(field).unwrap()),
        ColumnType::Boolean => ColumnValues::Boolean(rg.read_boolean_column(column).unwrap()),
        ColumnType::Int32 => ColumnValues::Int32(rg.read_int32_column(column).unwrap()),
        ColumnType::Int64 => ColumnValues::Int64(rg.read_int64_column(column).unwrap()),
        ColumnType::Float => ColumnValues::Float(rg.read_float_column(column).unwrap()),
        ColumnType::Double => ColumnValues::Double(rg.read_double_column(column).unwrap()),
        ColumnType::Utf8 => ColumnValues::Utf8(rg.read_string_column(column).unwrap()),
        ColumnType::TimestampMicros => {
            ColumnValues::TimestampMicros(rg.read_timestamp_column(column).unwrap())
        }
        // The reader has no nanosecond unit; the raw INT64 is the value.
        ColumnType::TimestampNanos => {
            ColumnValues::TimestampNanos(rg.read_int64_column(column).unwrap())
        }
        ColumnType::Binary => ColumnValues::Binary(rg.read_binary_column(column).unwrap()),
        // Unsigned values are stored as their bit patterns.
        ColumnType::UInt8 => ColumnValues::UInt8(unsigned32(rg.read_int32_column(column).unwrap())),
        ColumnType::UInt16 => {
            ColumnValues::UInt16(unsigned32(rg.read_int32_column(column).unwrap()))
        }
        ColumnType::UInt32 => {
            ColumnValues::UInt32(unsigned32(rg.read_int32_column(column).unwrap()))
        }
        ColumnType::UInt64 => ColumnValues::UInt64(
            rg.read_int64_column(column)
                .unwrap()
                .into_iter()
                .map(|v| v.map(|x| x as u64))
                .collect(),
        ),
        ColumnType::Decimal { scale, .. } => ColumnValues::Decimal(
            rg.read_decimal_column(column)
                .unwrap()
                .into_iter()
                .map(|v| {
                    v.map(|d| {
                        assert_eq!(d.scale, i32::from(scale));
                        d.unscaled
                    })
                })
                .collect(),
        ),
        ColumnType::FixedLenByteArray(_) => {
            ColumnValues::FixedLenByteArray(rg.read_fixed_len_byte_array_column(column).unwrap())
        }
    }
}

/// INT32 bit patterns read back as the unsigned type they were written from.
fn unsigned32<T: TryFrom<u32>>(values: Vec<Option<i32>>) -> Vec<Option<T>>
where
    T::Error: std::fmt::Debug,
{
    values
        .into_iter()
        .map(|v| v.map(|x| T::try_from(x as u32).unwrap()))
        .collect()
}

/// Values compare bit-for-bit, so NaN and -0.0 count.
fn bits(values: &ColumnValues) -> Vec<Option<u64>> {
    match values {
        ColumnValues::Float(v) => v
            .iter()
            .map(|x| x.map(|x| u64::from(x.to_bits())))
            .collect(),
        ColumnValues::Double(v) => v.iter().map(|x| x.map(f64::to_bits)).collect(),
        _ => Vec::new(),
    }
}

fn same(a: &ColumnValues, b: &ColumnValues) -> bool {
    match (a, b) {
        (ColumnValues::Float(_), ColumnValues::Float(_))
        | (ColumnValues::Double(_), ColumnValues::Double(_)) => bits(a) == bits(b),
        _ => a == b,
    }
}

/// Reads every row group back, concatenated per column.
fn read_all(bytes: &[u8], schema: &[ColumnSpec]) -> Vec<ColumnValues> {
    let file = ParquetFile::open(bytes).unwrap();
    let mut out: Vec<ColumnValues> = schema
        .iter()
        .map(|c| ColumnValues::empty(c.column_type))
        .collect();
    for group in 0..file.num_row_groups() {
        for (i, spec) in schema.iter().enumerate() {
            append(&mut out[i], read_column(&file, group, i, spec.column_type));
        }
    }
    out
}

/// [`check_stats`] for every leaf of field `field`: a map's key and value
/// leaves and a list's element leaf are UTF8 columns with one slot per
/// level entry.
fn check_field_stats(file: &ParquetFile, group: usize, field: usize, values: &ColumnValues) {
    let rg = file.row_group(group).unwrap();
    let leaves = file.fields()[field].leaves.clone();
    if let ColumnValues::List(rows) = values {
        let elements = rows
            .iter()
            .flat_map(|row| match row.as_deref() {
                None | Some([]) => vec![None],
                Some(items) => items
                    .iter()
                    .map(|v| v.as_ref().map(|v| v.to_string()))
                    .collect(),
            })
            .collect();
        check_stats(
            &ColumnValues::Utf8(elements),
            rg.column_statistics(leaves.start).unwrap().unwrap(),
        );
        return;
    }
    let ColumnValues::Map(rows) = values else {
        check_stats(values, rg.column_statistics(leaves.start).unwrap().unwrap());
        return;
    };
    let (mut keys, mut vals) = (Vec::new(), Vec::new());
    for row in rows {
        match row.as_deref() {
            None | Some([]) => {
                keys.push(None);
                vals.push(None);
            }
            Some(entries) => {
                for (k, v) in entries {
                    keys.push(Some(k.to_string()));
                    vals.push(v.as_ref().map(|v| v.to_string()));
                }
            }
        }
    }
    check_stats(
        &ColumnValues::Utf8(keys),
        rg.column_statistics(leaves.start).unwrap().unwrap(),
    );
    check_stats(
        &ColumnValues::Utf8(vals),
        rg.column_statistics(leaves.start + 1).unwrap().unwrap(),
    );
}

/// Pruning safety for one chunk: every non-null, non-NaN value lies within
/// [min, max], both bounds are attained (up to the zero widening), and the
/// null count is exact.
fn check_stats(values: &ColumnValues, stats: &Statistics) {
    let nulls = validity(values).iter().filter(|v| !**v).count();
    assert_eq!(stats.null_count, Some(i64::try_from(nulls).unwrap()));
    match values {
        ColumnValues::Boolean(v) => bounds(v.iter().flatten().copied(), stats, |b| b == [1]),
        ColumnValues::Int32(v) => bounds(v.iter().flatten().copied(), stats, |b| {
            i32::from_le_bytes(b.try_into().unwrap())
        }),
        ColumnValues::Int64(v)
        | ColumnValues::TimestampMicros(v)
        | ColumnValues::TimestampNanos(v) => bounds(v.iter().flatten().copied(), stats, |b| {
            i64::from_le_bytes(b.try_into().unwrap())
        }),
        ColumnValues::Utf8(v) => bounds(
            v.iter().flatten().map(|s| s.as_bytes().to_vec()),
            stats,
            <[u8]>::to_vec,
        ),
        ColumnValues::Float(v) => {
            float_bounds(v.iter().flatten().map(|x| f64::from(*x)), stats, |b| {
                f64::from(f32::from_le_bytes(b.try_into().unwrap()))
            })
        }
        ColumnValues::Double(v) => float_bounds(v.iter().flatten().copied(), stats, |b| {
            f64::from_le_bytes(b.try_into().unwrap())
        }),
        ColumnValues::Binary(v) | ColumnValues::FixedLenByteArray(v) => {
            bounds(v.iter().flatten().cloned(), stats, <[u8]>::to_vec)
        }
        // Unsigned order: a signed decode of u32::MAX would be -1.
        ColumnValues::UInt8(v) => bounds(v.iter().flatten().map(|x| u32::from(*x)), stats, |b| {
            u32::from_le_bytes(b.try_into().unwrap())
        }),
        ColumnValues::UInt16(v) => bounds(v.iter().flatten().map(|x| u32::from(*x)), stats, |b| {
            u32::from_le_bytes(b.try_into().unwrap())
        }),
        ColumnValues::UInt32(v) => bounds(v.iter().flatten().copied(), stats, |b| {
            u32::from_le_bytes(b.try_into().unwrap())
        }),
        ColumnValues::UInt64(v) => bounds(v.iter().flatten().copied(), stats, |b| {
            u64::from_le_bytes(b.try_into().unwrap())
        }),
        // The bound's width says where the decimal is stored.
        ColumnValues::Map(_) | ColumnValues::List(_) => {
            panic!("maps and lists are checked per leaf")
        }
        ColumnValues::Decimal(v) => bounds(v.iter().flatten().copied(), stats, |b| match b.len() {
            4 => i128::from(i32::from_le_bytes(b.try_into().unwrap())),
            8 => i128::from(i64::from_le_bytes(b.try_into().unwrap())),
            _ => db_core::storage::parquet::decimal::from_be_bytes(b),
        }),
    }
}

fn bounds<T: Ord + Clone + std::fmt::Debug>(
    values: impl Iterator<Item = T>,
    stats: &Statistics,
    decode: impl Fn(&[u8]) -> T,
) {
    let values: Vec<T> = values.collect();
    let (Some(min), Some(max)) = (&stats.min, &stats.max) else {
        assert!(values.is_empty(), "no bounds over {} values", values.len());
        return;
    };
    let (min, max) = (decode(min), decode(max));
    assert!(values.iter().all(|v| *v >= min && *v <= max));
    assert!(values.contains(&min) && values.contains(&max));
}

fn float_bounds(
    values: impl Iterator<Item = f64>,
    stats: &Statistics,
    decode: impl Fn(&[u8]) -> f64,
) {
    let values: Vec<f64> = values.filter(|v| !v.is_nan()).collect();
    let (Some(min), Some(max)) = (&stats.min, &stats.max) else {
        assert!(values.is_empty());
        return;
    };
    let (min, max) = (decode(min), decode(max));
    assert!(values.iter().all(|v| *v >= min && *v <= max));
    assert!(values.contains(&min) && values.contains(&max));
    if min == 0.0 {
        assert!(min.is_sign_negative(), "a zero min is -0.0");
    }
    if max == 0.0 {
        assert!(max.is_sign_positive(), "a zero max is +0.0");
    }
}

fn all_types_schema() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("b", ColumnType::Boolean, true),
        ColumnSpec::new("i32", ColumnType::Int32, false),
        ColumnSpec::new("i64", ColumnType::Int64, true),
        ColumnSpec::new("f32", ColumnType::Float, true),
        ColumnSpec::new("f64", ColumnType::Double, false),
        ColumnSpec::new("s", ColumnType::Utf8, true),
        ColumnSpec::new("ts", ColumnType::TimestampMicros, false),
        ColumnSpec::new("blob", ColumnType::Binary, true),
        ColumnSpec::new("u8", ColumnType::UInt8, true),
        ColumnSpec::new("u16", ColumnType::UInt16, false),
        ColumnSpec::new("u32", ColumnType::UInt32, true),
        ColumnSpec::new("u64", ColumnType::UInt64, false),
        ColumnSpec::new("d9", decimal(9, 2), true),
        ColumnSpec::new("d18", decimal(18, 4), false),
        ColumnSpec::new("d38", decimal(38, 6), true),
        ColumnSpec::new("ip", ColumnType::FixedLenByteArray(16), true),
    ]
}

fn decimal(precision: u8, scale: u8) -> ColumnType {
    ColumnType::Decimal { precision, scale }
}

fn all_types_batch(n: i32) -> Vec<ColumnValues> {
    let r = 0..n;
    vec![
        ColumnValues::Boolean(
            r.clone()
                .map(|i| (i % 3 != 0).then_some(i % 2 == 0))
                .collect(),
        ),
        ColumnValues::Int32(r.clone().map(|i| Some(i % 5 - 2)).collect()),
        ColumnValues::Int64(
            r.clone()
                .map(|i| (i % 4 != 0).then_some(i64::from(i) * 1_000_000_007))
                .collect(),
        ),
        ColumnValues::Float(
            r.clone()
                .map(|i| (i % 7 != 0).then_some(i as f32 / 3.0))
                .collect(),
        ),
        ColumnValues::Double(r.clone().map(|i| Some(f64::from(i) - 0.5)).collect()),
        ColumnValues::Utf8(
            r.clone()
                .map(|i| {
                    (i % 6 != 0).then(|| ["INFO", "WARN", "ERROR"][(i % 3) as usize].to_string())
                })
                .collect(),
        ),
        ColumnValues::TimestampMicros(
            r.clone()
                .map(|i| Some(1_700_000_000_000_000 + i64::from(i)))
                .collect(),
        ),
        ColumnValues::Binary(
            r.clone()
                .map(|i| (i % 5 != 0).then(|| vec![0x80 | (i % 3) as u8, 0xff, (i % 2) as u8]))
                .collect(),
        ),
        ColumnValues::UInt8(
            r.clone()
                .map(|i| (i % 4 != 0).then_some((i * 37) as u8))
                .collect(),
        ),
        ColumnValues::UInt16(r.clone().map(|i| Some(u16::MAX - (i % 9) as u16)).collect()),
        ColumnValues::UInt32(
            r.clone()
                .map(|i| (i % 3 != 0).then_some(u32::MAX - (i % 11) as u32))
                .collect(),
        ),
        ColumnValues::UInt64(r.clone().map(|i| Some((i as u64) << 58)).collect()),
        ColumnValues::Decimal(
            r.clone()
                .map(|i| (i % 6 != 0).then_some(i128::from(i % 21 - 10) * 99_999_999))
                .collect(),
        ),
        ColumnValues::Decimal(
            r.clone()
                .map(|i| Some(i128::from(i) * -1_000_000_000_007))
                .collect(),
        ),
        ColumnValues::Decimal(
            r.clone()
                .map(|i| (i % 2 == 0).then_some(i128::from(i % 199 - 99) * 10i128.pow(36)))
                .collect(),
        ),
        ColumnValues::FixedLenByteArray(
            r.map(|i| (i % 7 != 0).then(|| (0..16).map(|b| (b * i) as u8).collect()))
                .collect(),
        ),
    ]
}

#[test]
fn every_type_round_trips_under_each_codec_and_encoding() {
    for compression in [
        Compression::Uncompressed,
        Compression::Snappy,
        Compression::Zstd,
    ] {
        for dictionary in [false, true] {
            let schema = all_types_schema();
            let batch = all_types_batch(500);
            let bytes = write(
                schema.clone(),
                options(compression, dictionary, 1000),
                vec![batch.clone()],
            );
            let back = read_all(&bytes, &schema);
            for (written, read) in batch.iter().zip(&back) {
                assert!(
                    same(written, read),
                    "{compression:?} dict={dictionary}: {written:?} != {read:?}"
                );
            }
        }
    }
}

#[test]
fn metadata_describes_the_file() {
    let schema = all_types_schema();
    let bytes = write(
        schema.clone(),
        options(Compression::Snappy, true, 300),
        vec![
            all_types_batch(200),
            all_types_batch(200),
            all_types_batch(50),
        ],
    );
    let file = ParquetFile::open(&bytes).unwrap();
    // 200 + 200 reaches 300: one group of 400; the last 50 flush on finish.
    assert_eq!(file.num_row_groups(), 2);
    assert_eq!(file.num_rows(), 450);
    let meta = file.metadata();
    assert_eq!(meta.version, 1);
    assert_eq!(
        meta.created_by.as_deref(),
        Some(WriterOptions::default().created_by.as_str())
    );
    assert_eq!(meta.schema.len(), schema.len() + 1);
    let chunk = meta.row_groups[0].columns[5].meta_data.as_ref().unwrap();
    assert_eq!(chunk.codec, Codec::Snappy);
    assert_eq!(chunk.num_values, 400);
    assert!(
        chunk.dictionary_page_offset.is_some(),
        "a 3-value string column is dictionary-encoded"
    );
    assert_eq!(meta.row_groups[0].num_rows, 400);
}

#[test]
fn statistics_bound_every_chunk() {
    let schema = all_types_schema();
    let batch = all_types_batch(700);
    let bytes = write(
        schema.clone(),
        options(Compression::Uncompressed, true, 10_000),
        vec![batch.clone()],
    );
    let file = ParquetFile::open(&bytes).unwrap();
    let rg = file.row_group(0).unwrap();
    for (i, values) in batch.iter().enumerate() {
        let _ = &rg;
        check_field_stats(&file, 0, i, values);
    }
}

#[test]
fn empty_file_has_no_row_groups() {
    let bytes = write(all_types_schema(), WriterOptions::default(), Vec::new());
    let file = ParquetFile::open(&bytes).unwrap();
    assert_eq!(file.num_row_groups(), 0);
    assert_eq!(file.num_rows(), 0);
}

#[test]
fn an_all_null_column_round_trips_without_bounds() {
    let schema = vec![ColumnSpec::new("s", ColumnType::Utf8, true)];
    let batch = vec![ColumnValues::Utf8(vec![None; 20])];
    let bytes = write(
        schema.clone(),
        WriterOptions::default(),
        vec![batch.clone()],
    );
    assert_eq!(read_all(&bytes, &schema), batch);
    let file = ParquetFile::open(&bytes).unwrap();
    let stats = file
        .row_group(0)
        .unwrap()
        .column_statistics(0)
        .unwrap()
        .cloned()
        .unwrap();
    assert_eq!(
        (stats.min, stats.max, stats.null_count),
        (None, None, Some(20))
    );
}

#[test]
fn dictionary_falls_back_to_plain() {
    let schema = vec![ColumnSpec::new("s", ColumnType::Utf8, false)];
    // All distinct: more than half distinct, so PLAIN.
    let distinct = vec![ColumnValues::Utf8(
        (0..100).map(|i| Some(format!("v{i}"))).collect(),
    )];
    // Repeating, but the dictionary is over the byte limit.
    let big = vec![ColumnValues::Utf8(
        (0..100)
            .map(|i| Some("x".repeat(100) + &(i % 10).to_string()))
            .collect(),
    )];
    for batch in [distinct, big] {
        let opts = WriterOptions {
            dictionary_max_bytes: 500,
            ..WriterOptions::default()
        };
        let bytes = write(schema.clone(), opts, vec![batch.clone()]);
        let file = ParquetFile::open(&bytes).unwrap();
        let chunk = file.metadata().row_groups[0].columns[0]
            .meta_data
            .clone()
            .unwrap();
        assert_eq!(chunk.dictionary_page_offset, None);
        assert_eq!(read_all(&bytes, &schema), batch);
    }
}

#[test]
fn single_entry_dictionary_round_trips() {
    let schema = vec![ColumnSpec::new("i", ColumnType::Int64, true)];
    let batch = vec![ColumnValues::Int64(
        (0..50).map(|i| (i % 2 == 0).then_some(42)).collect(),
    )];
    let bytes = write(
        schema.clone(),
        WriterOptions::default(),
        vec![batch.clone()],
    );
    assert_eq!(read_all(&bytes, &schema), batch);
}

#[test]
fn flush_row_group_cuts_a_group_early() {
    let schema = vec![ColumnSpec::new("i", ColumnType::Int32, false)];
    let mut w = ParquetWriter::new(Vec::new(), schema.clone(), WriterOptions::default()).unwrap();
    assert_eq!(w.schema(), schema.as_slice());
    w.write_columns(vec![ColumnValues::Int32(vec![Some(1)])])
        .unwrap();
    w.flush_row_group().unwrap();
    w.flush_row_group().unwrap(); // nothing buffered: no empty group
    w.write_columns(vec![ColumnValues::Int32(vec![Some(2)])])
        .unwrap();
    let (bytes, summary) = w.finish().unwrap();
    assert_eq!(
        summary,
        FileSummary {
            num_rows: 2,
            row_groups: 2,
            bytes: u64::try_from(bytes.len()).unwrap()
        }
    );
}

#[test]
fn invalid_schemas_and_options_are_refused() {
    let col = |name: &str| ColumnSpec::new(name, ColumnType::Int32, false);
    let typed = |t: ColumnType| vec![ColumnSpec::new("a", t, true)];
    let cases = [
        (Vec::new(), WriterOptions::default()),
        (vec![col("")], WriterOptions::default()),
        (vec![col("a"), col("a")], WriterOptions::default()),
        (vec![col("a")], options(Compression::Snappy, true, 0)),
        (typed(decimal(0, 0)), WriterOptions::default()),
        (typed(decimal(39, 2)), WriterOptions::default()),
        (typed(decimal(5, 6)), WriterOptions::default()),
        (
            typed(ColumnType::FixedLenByteArray(0)),
            WriterOptions::default(),
        ),
    ];
    for (schema, opts) in cases {
        let err = ParquetWriter::new(Vec::new(), schema, opts).err().unwrap();
        assert!(
            matches!(
                err,
                WriteError::InvalidSchema(_) | WriteError::InvalidOptions(_)
            ),
            "{err}"
        );
    }
}

#[test]
fn bad_batches_are_refused_and_leave_the_writer_usable() {
    let schema = vec![
        ColumnSpec::new("a", ColumnType::Int32, false),
        ColumnSpec::new("b", ColumnType::Utf8, true),
    ];
    let mut w = ParquetWriter::new(Vec::new(), schema.clone(), WriterOptions::default()).unwrap();
    let int = |v: Vec<Option<i32>>| ColumnValues::Int32(v);
    let s = |v: Vec<Option<&str>>| {
        ColumnValues::Utf8(v.into_iter().map(|x| x.map(str::to_string)).collect())
    };
    let bad = [
        vec![int(vec![Some(1)])],
        vec![int(vec![Some(1)]), ColumnValues::Int64(vec![Some(1)])],
        vec![int(vec![Some(1)]), s(vec![Some("x"), None])],
        vec![int(vec![None]), s(vec![Some("x")])],
    ];
    let expected = [
        "batch has 1 columns, schema has 2",
        "column 'b': values do not match the column type",
        "column 'b': 2 values, expected 1",
        "column 'a' is required but got a null",
    ];
    for (batch, message) in bad.into_iter().zip(expected) {
        assert_eq!(w.write_columns(batch).unwrap_err().to_string(), message);
    }
    w.write_columns(vec![int(vec![Some(7)]), s(vec![None])])
        .unwrap();
    let (bytes, _) = w.finish().unwrap();
    assert_eq!(
        read_all(&bytes, &schema),
        vec![int(vec![Some(7)]), s(vec![None])]
    );
}

#[test]
fn values_that_do_not_fit_their_column_are_refused() {
    let schema = vec![
        ColumnSpec::new("d", decimal(4, 2), true),
        ColumnSpec::new("ip", ColumnType::FixedLenByteArray(4), true),
    ];
    let mut w = ParquetWriter::new(Vec::new(), schema.clone(), WriterOptions::default()).unwrap();
    let bad = [
        vec![
            ColumnValues::Decimal(vec![Some(-10_000)]),
            ColumnValues::FixedLenByteArray(vec![None]),
        ],
        vec![
            ColumnValues::Decimal(vec![Some(9_999)]),
            ColumnValues::FixedLenByteArray(vec![Some(vec![1, 2, 3])]),
        ],
    ];
    let expected = [
        "column 'd': unscaled value -10000 has more than 4 digits",
        "column 'ip': 3-byte value in a 4-byte column",
    ];
    for (batch, message) in bad.into_iter().zip(expected) {
        assert_eq!(w.write_columns(batch).unwrap_err().to_string(), message);
    }
    let good = vec![
        ColumnValues::Decimal(vec![Some(-9_999)]),
        ColumnValues::FixedLenByteArray(vec![Some(vec![1, 2, 3, 4])]),
    ];
    w.write_columns(good.clone()).unwrap();
    let (bytes, _) = w.finish().unwrap();
    assert_eq!(read_all(&bytes, &schema), good);
}

#[test]
fn error_messages() {
    let io = WriteError::from(io::Error::other("disk full"));
    assert_eq!(io.to_string(), "parquet write: disk full");
    assert_eq!(
        WriteError::from(EncodeError::MixedCollection).to_string(),
        "parquet metadata: list or map elements of more than one thrift type"
    );
    assert_eq!(
        WriteError::TooLarge("page size".into()).to_string(),
        "too large for parquet: page size"
    );
    assert_eq!(
        WriteError::InvalidSchema("no columns".into()).to_string(),
        "invalid parquet schema: no columns"
    );
    assert_eq!(
        WriteError::InvalidOptions("x".into()).to_string(),
        "invalid parquet writer options: x"
    );
}

/// Accepts `limit` bytes, then fails every write.
struct Failing {
    limit: usize,
    written: usize,
}

impl Write for Failing {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() > self.limit {
            return Err(io::Error::other("disk full"));
        }
        self.written += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn io_errors_surface_at_every_stage() {
    let schema = vec![ColumnSpec::new("s", ColumnType::Utf8, true)];
    assert!(matches!(
        ParquetWriter::new(
            Failing {
                limit: 0,
                written: 0
            },
            schema.clone(),
            WriterOptions::default()
        ),
        Err(WriteError::Io(_))
    ));
    let batch = || {
        vec![ColumnValues::Utf8(
            (0..100).map(|i| Some((i % 3).to_string())).collect(),
        )]
    };
    // Fail inside the dictionary page, the data page, and the footer.
    for limit in [10, 60, 120] {
        let mut w = ParquetWriter::new(
            Failing { limit, written: 0 },
            schema.clone(),
            options(Compression::Uncompressed, true, 100_000),
        )
        .unwrap();
        w.write_columns(batch()).unwrap();
        assert!(
            matches!(w.finish(), Err(WriteError::Io(_))),
            "limit {limit}"
        );
    }
    let mut w = ParquetWriter::new(
        Failing {
            limit: 20,
            written: 0,
        },
        schema,
        options(Compression::Uncompressed, true, 1),
    )
    .unwrap();
    assert!(matches!(w.write_columns(batch()), Err(WriteError::Io(_))));
}

// --- property test ---------------------------------------------------------

fn column_type_strategy() -> impl Strategy<Value = ColumnType> {
    prop_oneof![
        Just(ColumnType::Boolean),
        Just(ColumnType::Int32),
        Just(ColumnType::Int64),
        Just(ColumnType::Float),
        Just(ColumnType::Double),
        Just(ColumnType::Utf8),
        Just(ColumnType::TimestampMicros),
        Just(ColumnType::TimestampNanos),
        Just(ColumnType::Binary),
        Just(ColumnType::UInt8),
        Just(ColumnType::UInt16),
        Just(ColumnType::UInt32),
        Just(ColumnType::UInt64),
        (1u8..=38)
            .prop_flat_map(|p| (Just(p), 0..=p))
            .prop_map(|(p, s)| decimal(p, s)),
        (1u16..20).prop_map(ColumnType::FixedLenByteArray),
        Just(ColumnType::MapUtf8),
        Just(ColumnType::ListUtf8),
    ]
}

/// `n` values of `t`, drawn from a small pool half the time so dictionaries
/// and RLE runs occur, with nulls only when `nullable`.
fn values_strategy(t: ColumnType, nullable: bool, n: usize) -> BoxedStrategy<ColumnValues> {
    fn col<T: std::fmt::Debug + Clone + 'static>(
        s: impl Strategy<Value = T> + 'static,
        pool: impl Strategy<Value = T> + 'static,
        nullable: bool,
        n: usize,
    ) -> BoxedStrategy<Vec<Option<T>>> {
        let value = prop_oneof![s, pool].boxed();
        let cell = if nullable {
            proptest::option::weighted(0.8, value).boxed()
        } else {
            value.prop_map(Some).boxed()
        };
        proptest::collection::vec(cell, n).boxed()
    }
    match t {
        ColumnType::Boolean => col(any::<bool>(), Just(true), nullable, n)
            .prop_map(ColumnValues::Boolean)
            .boxed(),
        ColumnType::Int32 => col(any::<i32>(), -2i32..3, nullable, n)
            .prop_map(ColumnValues::Int32)
            .boxed(),
        ColumnType::Int64 => col(
            any::<i64>(),
            prop_oneof![Just(i64::MIN), Just(i64::MAX), Just(0)],
            nullable,
            n,
        )
        .prop_map(ColumnValues::Int64)
        .boxed(),
        ColumnType::Float => col(
            any::<f32>(),
            prop_oneof![
                Just(0.0f32),
                Just(-0.0),
                Just(f32::NAN),
                Just(f32::INFINITY)
            ],
            nullable,
            n,
        )
        .prop_map(ColumnValues::Float)
        .boxed(),
        ColumnType::Double => col(
            any::<f64>(),
            prop_oneof![Just(0.0f64), Just(-0.0), Just(f64::NAN), Just(f64::MIN)],
            nullable,
            n,
        )
        .prop_map(ColumnValues::Double)
        .boxed(),
        ColumnType::Utf8 => col(
            ".{0,12}",
            prop_oneof![
                Just(String::new()),
                Just("é".to_string()),
                Just("INFO".to_string())
            ],
            nullable,
            n,
        )
        .prop_map(ColumnValues::Utf8)
        .boxed(),
        ColumnType::TimestampMicros => col(any::<i64>(), 0i64..4, nullable, n)
            .prop_map(ColumnValues::TimestampMicros)
            .boxed(),
        ColumnType::TimestampNanos => col(any::<i64>(), 0i64..4, nullable, n)
            .prop_map(ColumnValues::TimestampNanos)
            .boxed(),
        ColumnType::Binary => col(
            proptest::collection::vec(any::<u8>(), 0..10),
            prop_oneof![Just(Vec::new()), Just(vec![0xff])],
            nullable,
            n,
        )
        .prop_map(ColumnValues::Binary)
        .boxed(),
        ColumnType::UInt8 => col(any::<u8>(), Just(u8::MAX), nullable, n)
            .prop_map(ColumnValues::UInt8)
            .boxed(),
        ColumnType::UInt16 => col(any::<u16>(), Just(u16::MAX), nullable, n)
            .prop_map(ColumnValues::UInt16)
            .boxed(),
        ColumnType::UInt32 => col(
            any::<u32>(),
            prop_oneof![Just(0), Just(u32::MAX)],
            nullable,
            n,
        )
        .prop_map(ColumnValues::UInt32)
        .boxed(),
        ColumnType::UInt64 => col(
            any::<u64>(),
            prop_oneof![Just(0), Just(u64::MAX)],
            nullable,
            n,
        )
        .prop_map(ColumnValues::UInt64)
        .boxed(),
        ColumnType::Decimal { precision, .. } => {
            let limit = 10i128.pow(u32::from(precision)) - 1;
            col(
                -limit..=limit,
                prop_oneof![Just(-limit), Just(limit)],
                nullable,
                n,
            )
            .prop_map(ColumnValues::Decimal)
            .boxed()
        }
        // Unique keys (a writer requirement), any values, empty maps.
        ColumnType::MapUtf8 => col(
            proptest::collection::btree_map("[a-zé]{0,4}", proptest::option::of(".{0,5}"), 0..4)
                .prop_map(|m| {
                    m.into_iter()
                        .map(|(k, v)| (k.into(), v.map(Into::into)))
                        .collect::<MapEntries>()
                }),
            Just(vec![("k".into(), None)]),
            nullable,
            n,
        )
        .prop_map(ColumnValues::Map)
        .boxed(),
        // Any elements, NULL ones and empty lists included.
        ColumnType::ListUtf8 => col(
            proptest::collection::vec(proptest::option::of(".{0,5}"), 0..4).prop_map(|v| {
                v.into_iter()
                    .map(|e| e.map(Into::into))
                    .collect::<ListItems>()
            }),
            Just(vec![None]),
            nullable,
            n,
        )
        .prop_map(ColumnValues::List)
        .boxed(),
        ColumnType::FixedLenByteArray(len) => {
            let len = usize::from(len);
            col(
                proptest::collection::vec(any::<u8>(), len),
                Just(vec![0x80; len]),
                nullable,
                n,
            )
            .prop_map(ColumnValues::FixedLenByteArray)
            .boxed()
        }
    }
}

fn case_strategy() -> impl Strategy<Value = (Vec<ColumnSpec>, Vec<Vec<ColumnValues>>, WriterOptions)>
{
    let schema =
        proptest::collection::vec((column_type_strategy(), any::<bool>()), 1..5).prop_map(|cols| {
            cols.into_iter()
                .enumerate()
                .map(|(i, (t, nullable))| ColumnSpec::new(format!("c{i}"), t, nullable))
                .collect::<Vec<_>>()
        });
    let opts = (
        prop_oneof![
            Just(Compression::Uncompressed),
            Just(Compression::Snappy),
            Just(Compression::Zstd)
        ],
        any::<bool>(),
        1usize..120,
    )
        .prop_map(|(c, d, rows)| options(c, d, rows));
    (schema, proptest::collection::vec(0usize..150, 1..4), opts).prop_flat_map(
        |(schema, sizes, opts)| {
            let batches: Vec<BoxedStrategy<Vec<ColumnValues>>> = sizes
                .into_iter()
                .map(|n| {
                    schema
                        .iter()
                        .map(|c| values_strategy(c.column_type, c.nullable, n))
                        .collect::<Vec<_>>()
                        .boxed()
                })
                .collect();
            (Just(schema), batches, Just(opts))
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// write -> read yields the same values, and every chunk's statistics
    /// bound its values.
    #[test]
    fn written_files_read_back_identically((schema, batches, opts) in case_strategy()) {
        let mut expected: Vec<ColumnValues> = schema.iter().map(|c| ColumnValues::empty(c.column_type)).collect();
        for batch in &batches {
            for (e, b) in expected.iter_mut().zip(batch) {
                append(e, b.clone());
            }
        }
        let bytes = write(schema.clone(), opts, batches);
        let back = read_all(&bytes, &schema);
        for (e, b) in expected.iter().zip(&back) {
            prop_assert!(same(e, b), "{:?} != {:?}", e, b);
        }
        let file = ParquetFile::open(&bytes).unwrap();
        for group in 0..file.num_row_groups() {
            let rg = file.row_group(group).unwrap();
            for (i, spec) in schema.iter().enumerate() {
                let _ = &rg;
                check_field_stats(&file, group, i, &read_column(&file, group, i, spec.column_type));
            }
        }
    }
}

// --- the column engine over the new types (#595) ---------------------------

/// Writes `batches` (one row group each) to a fresh file named `<table>.parquet`
/// and opens the column engine over it.
#[cfg(feature = "engine-column")]
fn engine_over(
    table: &str,
    schema: Vec<ColumnSpec>,
    batches: Vec<Vec<ColumnValues>>,
) -> (db_core::engine::column::BatchEngine, std::path::PathBuf) {
    let bytes = write(schema, options(Compression::Snappy, true, 1), batches);
    let dir = std::env::temp_dir().join(format!("db-core-595-{}-{table}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{table}.parquet"));
    std::fs::write(&path, bytes).unwrap();
    let engine =
        <db_core::engine::column::BatchEngine as db_core::engine::Engine>::open(&path).unwrap();
    (engine, dir)
}

#[cfg(feature = "engine-column")]
fn query(engine: &mut db_core::engine::column::BatchEngine, sql: &str) -> Vec<String> {
    use db_core::engine::Engine as _;
    engine
        .run_query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

#[cfg(feature = "engine-column")]
#[test]
fn the_column_engine_reads_bytes_as_blobs_decimals_exactly_and_unsigned_values_unsigned() {
    let schema = vec![
        ColumnSpec::new("blob", ColumnType::Binary, true),
        ColumnSpec::new("ip", ColumnType::FixedLenByteArray(4), true),
        ColumnSpec::new("price", decimal(9, 2), true),
        ColumnSpec::new("big", decimal(38, 3), true),
        ColumnSpec::new("u32", ColumnType::UInt32, true),
        ColumnSpec::new("u64", ColumnType::UInt64, true),
    ];
    let batch = vec![
        ColumnValues::Binary(vec![Some(vec![0x80, 0xff]), None]),
        ColumnValues::FixedLenByteArray(vec![Some(vec![192, 168, 1, 7]), None]),
        ColumnValues::Decimal(vec![Some(1234), Some(-5)]),
        ColumnValues::Decimal(vec![Some(-1_500), None]),
        ColumnValues::UInt32(vec![Some(u32::MAX), Some(1)]),
        ColumnValues::UInt64(vec![Some(i64::MAX as u64), Some(0)]),
    ];
    let (mut engine, dir) = engine_over("typed", schema, vec![batch]);
    assert_eq!(
        query(
            &mut engine,
            "SELECT blob, ip, price, big, u32, u64 FROM typed"
        ),
        [
            // A DECIMAL prints at its scale, as DuckDB prints it (#632).
            format!("X'80FF'|X'C0A80107'|12.34|-1.500|4294967295|{}", i64::MAX),
            "||-0.05||1|0".to_string(),
        ]
    );
    // #868: a blob literal compares by bytes; a blob never equals text,
    // including its own hex form.
    for (sql, expected) in [
        ("SELECT count(*) FROM typed WHERE blob = x'80ff'", "1"),
        ("SELECT count(*) FROM typed WHERE blob = X'80FF'", "1"),
        ("SELECT count(*) FROM typed WHERE blob <> x'80ff'", "0"),
        (
            "SELECT count(*) FROM typed WHERE blob IN (x'00', x'80ff')",
            "1",
        ),
        ("SELECT count(*) FROM typed WHERE blob > x'80fe'", "1"),
        ("SELECT count(*) FROM typed WHERE blob < x'80ff'", "0"),
        (
            "SELECT count(*) FROM typed WHERE blob BETWEEN x'80' AND x'81'",
            "1",
        ),
        ("SELECT count(*) FROM typed WHERE ip = x'c0a80107'", "1"),
        ("SELECT count(*) FROM typed WHERE blob = '80ff'", "0"),
        ("SELECT count(*) FROM typed WHERE blob = '80FF'", "0"),
        ("SELECT hex(blob) FROM typed WHERE blob IS NOT NULL", "80FF"),
        ("SELECT hex(unhex('80ff')) FROM typed LIMIT 1", "80FF"),
        ("SELECT count(*) FROM typed WHERE blob = unhex('80ff')", "1"),
        ("SELECT hex('ab') FROM typed LIMIT 1", "6162"),
        ("SELECT unhex('abc') IS NULL FROM typed LIMIT 1", "1"),
        ("SELECT unhex('zz') IS NULL FROM typed LIMIT 1", "1"),
        ("SELECT hex(unhex('')) FROM typed LIMIT 1", ""),
        (
            "SELECT hex(min(blob)), hex(max(blob)) FROM typed",
            "80FF|80FF",
        ),
    ] {
        assert_eq!(query(&mut engine, sql), [expected], "{sql}");
    }
    std::fs::remove_dir_all(dir).ok();
}

/// DECIMAL aggregates stay exact (#632): `0.10 + 0.20` sums to `0.30`
/// rather than f64's `0.30000000000000004`, MIN/MAX/ORDER BY rank by
/// value, GROUP BY keys on the decimal, and a SUM past 38 digits is an
/// error rather than a rounded DOUBLE.
#[cfg(feature = "engine-column")]
#[test]
fn the_column_engine_aggregates_decimals_exactly() {
    let schema = vec![
        ColumnSpec::new("k", ColumnType::Int64, false),
        ColumnSpec::new("price", decimal(9, 2), true),
        ColumnSpec::new("huge", decimal(38, 0), true),
    ];
    let max38 = 10_i128.pow(38) - 1;
    let batches = vec![
        vec![
            ColumnValues::Int64(vec![Some(1), Some(1)]),
            ColumnValues::Decimal(vec![Some(10), Some(20)]),
            ColumnValues::Decimal(vec![Some(max38), Some(max38)]),
        ],
        vec![
            ColumnValues::Int64(vec![Some(2), Some(2)]),
            ColumnValues::Decimal(vec![Some(-150), None]),
            ColumnValues::Decimal(vec![Some(1), None]),
        ],
    ];
    let (mut engine, dir) = engine_over("decimals", schema, batches);
    assert_eq!(
        query(&mut engine, "SELECT sum(price) FROM decimals WHERE k = 1"),
        ["0.30"]
    );
    assert_eq!(
        query(
            &mut engine,
            "SELECT min(price), max(price), sum(price) FROM decimals"
        ),
        ["-1.50|0.20|-1.20"]
    );
    assert_eq!(
        query(
            &mut engine,
            "SELECT k, sum(price) FROM decimals GROUP BY k ORDER BY k"
        ),
        ["1|0.30", "2|-1.50"]
    );
    assert_eq!(
        query(
            &mut engine,
            "SELECT price FROM decimals WHERE price IS NOT NULL ORDER BY price"
        ),
        ["-1.50", "0.10", "0.20"]
    );
    assert_eq!(
        query(
            &mut engine,
            "SELECT price, count(*) FROM decimals GROUP BY price ORDER BY price"
        ),
        // NULLS LAST, DuckDB's default.
        ["-1.50|1", "0.10|1", "0.20|1", "|1"]
    );
    {
        use db_core::engine::Engine as _;
        assert!(
            engine.run_query("SELECT sum(huge) FROM decimals").is_err(),
            "a SUM past 38 digits must error, not round"
        );
    }
    std::fs::remove_dir_all(dir).ok();
}

/// #633: `COUNT(DISTINCT x)` over a Parquet file, through the column
/// engine end to end: values repeat across row groups, NULLs are not
/// counted, and the other aggregates still see every row.
#[cfg(feature = "engine-column")]
#[test]
fn the_column_engine_counts_distinct_values_across_row_groups() {
    let schema = vec![
        ColumnSpec::new("k", ColumnType::Utf8, false),
        ColumnSpec::new("user", ColumnType::Int64, true),
    ];
    let batch = |k: [&str; 3], user: [Option<i64>; 3]| {
        vec![
            ColumnValues::Utf8(k.iter().map(|s| Some(s.to_string())).collect()),
            ColumnValues::Int64(user.to_vec()),
        ]
    };
    let batches = vec![
        batch(["a", "a", "b"], [Some(1), Some(2), Some(7)]),
        batch(["a", "b", "b"], [Some(1), None, Some(7)]),
        batch(["a", "b", "a"], [None, Some(8), Some(3)]),
    ];
    let (mut engine, dir) = engine_over("visits", schema, batches);
    assert_eq!(
        query(
            &mut engine,
            "SELECT k, count(DISTINCT user), count(user), count(*) FROM visits \
             GROUP BY k ORDER BY k"
        ),
        ["a|3|4|5", "b|2|3|4"]
    );
    assert_eq!(
        query(&mut engine, "SELECT count(DISTINCT user) FROM visits"),
        ["5"]
    );
    assert_eq!(
        query(
            &mut engine,
            "SELECT count(DISTINCT user) FROM visits WHERE k = 'zzz'"
        ),
        ["0"]
    );
    std::fs::remove_dir_all(dir).ok();
}

/// A footer bound above the signed range must not be read as a negative
/// number: before #595 the row group holding `u32::MAX` was pruned for
/// `u32 > 3000000000`.
#[cfg(feature = "engine-column")]
#[test]
fn unsigned_statistics_prune_in_unsigned_order() {
    let schema = vec![
        ColumnSpec::new("u32", ColumnType::UInt32, false),
        ColumnSpec::new("u64", ColumnType::UInt64, false),
    ];
    let group = |a: u32, b: u64| {
        vec![
            ColumnValues::UInt32(vec![Some(a)]),
            ColumnValues::UInt64(vec![Some(b)]),
        ]
    };
    let (mut engine, dir) = engine_over(
        "unsigned",
        schema,
        vec![
            group(1, 1),
            group(u32::MAX, i64::MAX as u64),
            group(2_147_483_648, 2),
        ],
    );
    let count = |engine: &mut _, sql: &str| query(engine, sql).join("");
    assert_eq!(
        count(
            &mut engine,
            "SELECT count(*) FROM unsigned WHERE u32 > 3000000000"
        ),
        "1"
    );
    assert_eq!(
        count(
            &mut engine,
            "SELECT count(*) FROM unsigned WHERE u32 >= 2147483648"
        ),
        "2"
    );
    assert_eq!(
        count(&mut engine, "SELECT count(*) FROM unsigned WHERE u32 < 5"),
        "1"
    );
    assert_eq!(
        count(&mut engine, "SELECT count(*) FROM unsigned WHERE u64 > 5"),
        "1"
    );
    assert_eq!(
        query(
            &mut engine,
            "SELECT u32 FROM unsigned ORDER BY u32 DESC LIMIT 1"
        ),
        ["4294967295"]
    );
    std::fs::remove_dir_all(dir).ok();
}

/// A `UINT_64` value the engine's integers cannot hold fails the query
/// rather than reading back as a wrapped negative.
#[cfg(feature = "engine-column")]
#[test]
fn an_unsigned_value_above_i64_max_is_an_error_not_a_negative() {
    use db_core::engine::Engine as _;
    let schema = vec![ColumnSpec::new("u64", ColumnType::UInt64, false)];
    let batch = vec![ColumnValues::UInt64(vec![Some(u64::MAX)])];
    let (mut engine, dir) = engine_over("too_big", schema, vec![batch]);
    let err = engine.run_query("SELECT u64 FROM too_big").unwrap_err();
    assert!(
        err.to_string()
            .contains("unsigned value 18446744073709551615 is above the largest supported integer"),
        "{err}"
    );
    std::fs::remove_dir_all(dir).ok();
}

/// An aggregate without GROUP BY is one row over no rows, also when footer
/// statistics prune every row group or the file has none (before, no row).
#[cfg(feature = "engine-column")]
#[test]
fn an_ungrouped_aggregate_over_no_row_groups_is_one_row() {
    let schema = vec![ColumnSpec::new("x", ColumnType::Int32, false)];
    let (mut engine, dir) = engine_over(
        "pruned",
        schema.clone(),
        vec![vec![ColumnValues::Int32(vec![Some(1), Some(2)])]],
    );
    let all_pruned = "SELECT count(*), sum(x), min(x) FROM pruned WHERE x < 0";
    assert_eq!(query(&mut engine, all_pruned), ["0||"]);
    assert!(query(
        &mut engine,
        "SELECT x, count(*) FROM pruned WHERE x < 0 GROUP BY x"
    )
    .is_empty());
    std::fs::remove_dir_all(dir).ok();

    let (mut engine, dir) = engine_over("no_groups", schema, Vec::new());
    assert_eq!(
        query(&mut engine, "SELECT count(*), max(x) FROM no_groups"),
        ["0|"]
    );
    std::fs::remove_dir_all(dir).ok();
}

// --- MAP<UTF8, UTF8> (#621) -------------------------------------------------

fn map(entries: &[(&str, Option<&str>)]) -> Option<MapEntries> {
    Some(
        entries
            .iter()
            .map(|(k, v)| ((*k).into(), v.map(Into::into)))
            .collect(),
    )
}

/// Maps between flat columns, so leaf indices shift: the empty, NULL and
/// 100-entry maps, NULL values, non-ASCII keys and entry order survive,
/// with and without dictionary encoding.
#[test]
fn map_columns_round_trip_between_flat_columns() {
    let schema = vec![
        ColumnSpec::new("id", ColumnType::Int32, false),
        ColumnSpec::new("h", ColumnType::MapUtf8, true),
        ColumnSpec::new("x", ColumnType::Int64, true),
        ColumnSpec::new("r", ColumnType::MapUtf8, false),
    ];
    let hundred: MapEntries = (0..100)
        .map(|i| {
            (
                format!("key{i:03}").into(),
                (i % 7 != 0).then(|| format!("v{}", i % 5).into()),
            )
        })
        .collect();
    let h = vec![
        map(&[("User-Agent", Some("curl/8")), ("Accept", Some("*/*"))]),
        None,
        map(&[]),
        map(&[("k", None)]),
        map(&[("héader", Some("välue")), ("ä", Some(""))]),
        Some(hundred),
        map(&[("z", Some("1")), ("a", Some("2"))]),
    ];
    let rows = h.len();
    let batch = vec![
        ColumnValues::Int32((0..rows as i32).map(Some).collect()),
        ColumnValues::Map(h),
        ColumnValues::Int64(
            (0..rows as i64)
                .map(|i| (i != 3).then_some(i * 10))
                .collect(),
        ),
        ColumnValues::Map(
            (0..rows)
                .map(|i| {
                    let n = i.to_string();
                    map(&[("n", Some(n.as_str()))][..(i % 2)])
                })
                .collect(),
        ),
    ];
    for dictionary in [false, true] {
        for row_group_rows in [1, 1000] {
            let bytes = write(
                schema.clone(),
                options(Compression::Snappy, dictionary, row_group_rows),
                vec![batch.clone()],
            );
            assert_eq!(
                read_all(&bytes, &schema),
                batch,
                "dict={dictionary} rg={row_group_rows}"
            );
            let file = ParquetFile::open(&bytes).unwrap();
            let paths: Vec<Vec<String>> = file.metadata().row_groups[0]
                .columns
                .iter()
                .map(|c| c.meta_data.as_ref().unwrap().path_in_schema.clone())
                .collect();
            assert_eq!(
                paths,
                [
                    vec!["id"],
                    vec!["h", "key_value", "key"],
                    vec!["h", "key_value", "value"],
                    vec!["x"],
                    vec!["r", "key_value", "key"],
                    vec!["r", "key_value", "value"]
                ]
            );
            for group in 0..file.num_row_groups() {
                for (i, spec) in schema.iter().enumerate() {
                    check_field_stats(
                        &file,
                        group,
                        i,
                        &read_column(&file, group, i, spec.column_type),
                    );
                }
            }
        }
    }
}

#[test]
fn a_map_with_a_repeated_key_is_refused() {
    let schema = vec![ColumnSpec::new("h", ColumnType::MapUtf8, true)];
    let mut w = ParquetWriter::new(Vec::new(), schema, WriterOptions::default()).unwrap();
    let err = w
        .write_columns(vec![ColumnValues::Map(vec![map(&[
            ("a", None),
            ("a", Some("1")),
        ])])])
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "column 'h': map key \"a\" repeated in one map"
    );
}

/// DuckDB's own MAP file (tools/gen_parquet_fixtures.sh) reads back.
#[test]
fn a_duckdb_map_reads_back() {
    let bytes = std::fs::read("tests/fixtures/parquet/nested_map.parquet").unwrap();
    let file = ParquetFile::open(&bytes).unwrap();
    let fields = file.fields();
    let rg = file.row_group(0).unwrap();
    assert_eq!(
        rg.read_map_utf8_column(&fields[1]).unwrap(),
        [
            map(&[("User-Agent", Some("curl/8")), ("Accept", Some("*/*"))]),
            None,
            map(&[]),
            map(&[("k", None)]),
            map(&[("héader", Some("välue")), ("k", Some("v"))]),
        ]
    );
    let err = rg.read_map_utf8_column(&fields[0]).unwrap_err();
    assert_eq!(err.to_string(), "field id is not a MAP<UTF8, UTF8>");
}

// --- LIST<UTF8> (#801) -----------------------------------------------------

fn list(items: &[Option<&str>]) -> Option<ListItems> {
    Some(items.iter().map(|v| v.map(Into::into)).collect())
}

/// Lists between flat columns, so leaf indices shift: the NULL, empty and
/// 100-element lists, NULL elements, non-ASCII elements and element order
/// survive, with and without dictionary encoding.
#[test]
fn list_columns_round_trip_between_flat_columns() {
    let schema = vec![
        ColumnSpec::new("id", ColumnType::Int32, false),
        ColumnSpec::new("labels", ColumnType::ListUtf8, true),
        ColumnSpec::new("x", ColumnType::Int64, true),
        ColumnSpec::new("r", ColumnType::ListUtf8, false),
    ];
    let hundred: ListItems = (0..100)
        .map(|i| (i % 7 != 0).then(|| format!("T{}", i % 5).into()))
        .collect();
    let labels = vec![
        list(&[Some("T1059"), Some("T1566")]),
        None,
        list(&[]),
        list(&[None]),
        list(&[Some("välue"), None, Some("")]),
        Some(hundred),
        list(&[Some("z"), Some("a"), Some("z")]),
    ];
    let rows = labels.len();
    let batch = vec![
        ColumnValues::Int32((0..rows as i32).map(Some).collect()),
        ColumnValues::List(labels),
        ColumnValues::Int64(
            (0..rows as i64)
                .map(|i| (i != 3).then_some(i * 10))
                .collect(),
        ),
        ColumnValues::List(
            (0..rows)
                .map(|i| {
                    let n = i.to_string();
                    list(&[Some(n.as_str())][..(i % 2)])
                })
                .collect(),
        ),
    ];
    for dictionary in [false, true] {
        for row_group_rows in [1, 1000] {
            let bytes = write(
                schema.clone(),
                options(Compression::Snappy, dictionary, row_group_rows),
                vec![batch.clone()],
            );
            assert_eq!(
                read_all(&bytes, &schema),
                batch,
                "dict={dictionary} rg={row_group_rows}"
            );
            let file = ParquetFile::open(&bytes).unwrap();
            let paths: Vec<Vec<String>> = file.metadata().row_groups[0]
                .columns
                .iter()
                .map(|c| c.meta_data.as_ref().unwrap().path_in_schema.clone())
                .collect();
            assert_eq!(
                paths,
                [
                    vec!["id"],
                    vec!["labels", "list", "element"],
                    vec!["x"],
                    vec!["r", "list", "element"],
                ]
            );
            for group in 0..file.num_row_groups() {
                for (i, spec) in schema.iter().enumerate() {
                    check_field_stats(
                        &file,
                        group,
                        i,
                        &read_column(&file, group, i, spec.column_type),
                    );
                }
            }
        }
    }
}

/// DuckDB's own `VARCHAR[]` file (tools/gen_parquet_fixtures.sh) reads
/// back, and a MAP is not a list.
#[test]
fn a_duckdb_list_reads_back() {
    let bytes = std::fs::read("tests/fixtures/parquet/nested_shapes.parquet").unwrap();
    let file = ParquetFile::open(&bytes).unwrap();
    let field = |name: &str| file.fields().into_iter().find(|f| f.name == name).unwrap();
    let rg = file.row_group(0).unwrap();
    assert_eq!(
        rg.read_list_utf8_column(&field("lst")).unwrap(),
        [list(&[Some("x")])]
    );
    let err = rg.read_list_utf8_column(&field("m")).unwrap_err();
    assert_eq!(err.to_string(), "field m is not a LIST<UTF8>");
}

/// #622: zstd pages of an empty batch, an all-NULL column and a
/// dictionary chunk read back, and the chunk records the ZSTD codec.
#[test]
fn zstd_pages_round_trip_empty_all_null_and_dictionary_chunks() {
    let schema = vec![
        ColumnSpec::new("s", ColumnType::Utf8, true),
        ColumnSpec::new("n", ColumnType::Int64, true),
    ];
    let cases = [
        vec![ColumnValues::Utf8(vec![]), ColumnValues::Int64(vec![])],
        vec![
            ColumnValues::Utf8(vec![None; 300]),
            ColumnValues::Int64(vec![None; 300]),
        ],
        vec![
            ColumnValues::Utf8((0..3000).map(|i| Some(format!("kind-{}", i % 7))).collect()),
            ColumnValues::Int64((0..3000).map(|i| Some(i % 5)).collect()),
        ],
    ];
    for batch in cases {
        let bytes = write(
            schema.clone(),
            options(Compression::Zstd, true, 10_000),
            vec![batch.clone()],
        );
        let file = ParquetFile::open(&bytes).unwrap();
        for rg in &file.metadata().row_groups {
            for chunk in &rg.columns {
                assert_eq!(chunk.meta_data.as_ref().unwrap().codec, Codec::Zstd);
            }
        }
        let back = read_all(&bytes, &schema);
        for (written, read) in batch.iter().zip(&back) {
            assert!(same(written, read), "{written:?} != {read:?}");
        }
    }
}
