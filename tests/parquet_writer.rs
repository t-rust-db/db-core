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

use db_core::storage::parquet::footer::{Codec, Statistics};
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

/// One row group's column, read back as the variant it was written as.
fn read_column(
    file: &ParquetFile,
    group: usize,
    column: usize,
    column_type: ColumnType,
) -> ColumnValues {
    let rg = file.row_group(group).unwrap();
    match column_type {
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
    }
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
    ]
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
            r.map(|i| Some(1_700_000_000_000_000 + i64::from(i)))
                .collect(),
        ),
    ]
}

#[test]
fn every_type_round_trips_under_each_codec_and_encoding() {
    for compression in [Compression::Uncompressed, Compression::Snappy] {
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
        check_stats(values, rg.column_statistics(i).unwrap().unwrap());
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
    let cases = [
        (Vec::new(), WriterOptions::default()),
        (vec![col("")], WriterOptions::default()),
        (vec![col("a"), col("a")], WriterOptions::default()),
        (vec![col("a")], options(Compression::Snappy, true, 0)),
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
        prop_oneof![Just(Compression::Uncompressed), Just(Compression::Snappy)],
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
                check_stats(&read_column(&file, group, i, spec.column_type), rg.column_statistics(i).unwrap().unwrap());
            }
        }
    }
}
