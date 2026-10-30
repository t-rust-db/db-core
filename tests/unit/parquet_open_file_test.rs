// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `ParquetFile::open_file` reads the footer and then one column chunk at a
//! time through `VfsFile::read_at` (#841), where `ParquetFile::open` slices
//! a whole mapped file. The two must decode every column identically, and
//! a damaged file must fail the same way on both paths.

#![cfg(feature = "storage-column")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "test code fails fast (db-core#230)"
)]

use db_core::storage::parquet::footer::PhysicalType;
use db_core::storage::parquet::{ParquetFile, RowGroupReader};
use db_core::storage::{PosixVfs, Vfs};
use std::path::PathBuf;

const FIXTURES: &[&str] = &[
    "production.parquet",
    "production_snappy.parquet",
    "production_uncompressed.parquet",
    "nested_map.parquet",
    "nested_shapes.parquet",
    "typed_nulls.parquet",
    "list_dictionary.parquet",
];

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/parquet/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// `bytes` written to a file of its own, removed on drop.
struct TempFile(PathBuf);

impl TempFile {
    fn new(tag: &str, bytes: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "db-core-open-file-{}-{tag}.parquet",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        TempFile(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        drop(std::fs::remove_file(&self.0));
    }
}

/// What the typed readers return for column `c` of a row group, as text:
/// an `Err` compares as well as an `Ok`.
fn decode(rg: &RowGroupReader<'_, '_>, c: usize, physical: PhysicalType) -> String {
    match physical {
        PhysicalType::Int64 => format!("{:?}", rg.read_int64_column(c)),
        PhysicalType::Int32 => format!("{:?}", rg.read_int32_column(c)),
        PhysicalType::Double => format!("{:?}", rg.read_double_column(c)),
        PhysicalType::Float => format!("{:?}", rg.read_float_column(c)),
        PhysicalType::Boolean => format!("{:?}", rg.read_boolean_column(c)),
        PhysicalType::ByteArray => format!("{:?}", rg.read_string_column(c)),
        _ => String::new(),
    }
}

/// Every column of every row group, plus the nested fields, as text.
fn read_all(file: &ParquetFile<'_>) -> Vec<String> {
    let mut out = vec![format!("rows {}", file.num_rows())];
    for group in 0..file.num_row_groups() {
        let rg = file.row_group(group).unwrap();
        let chunks = &file.metadata().row_groups[group].columns;
        for (c, chunk) in chunks.iter().enumerate() {
            let physical = chunk
                .meta_data
                .as_ref()
                .map_or(PhysicalType::Unknown(-1), |m| m.physical_type);
            out.push(decode(&rg, c, physical));
        }
        for field in file.fields() {
            out.push(format!("{:?}", file.read_nested_column(group, &field.name)));
        }
    }
    out
}

fn open_file_result(path: &std::path::Path) -> Result<Vec<String>, String> {
    let handle = PosixVfs.open(path).unwrap();
    ParquetFile::open_file(&handle)
        .map(|file| read_all(&file))
        .map_err(|e| e.to_string())
}

#[test]
fn open_file_decodes_every_fixture_like_open() {
    for name in FIXTURES {
        let bytes = fixture(name);
        let temp = TempFile::new(name, &bytes);
        let mapped = read_all(&ParquetFile::open(&bytes).unwrap());
        assert_eq!(open_file_result(&temp.0).unwrap(), mapped, "{name}");
    }
}

#[test]
fn open_file_on_a_truncated_file_errors_where_open_does() {
    let bytes = fixture("production_snappy.parquet");
    let n = bytes.len();
    let lengths = (0..40)
        .map(|i| n * i / 40)
        .chain(n.saturating_sub(24)..n)
        .collect::<Vec<_>>();
    for len in lengths {
        let cut = &bytes[..len];
        let temp = TempFile::new(&format!("cut{len}"), cut);
        let mapped = ParquetFile::open(cut)
            .map(|file| read_all(&file))
            .map_err(|e| e.to_string());
        assert_eq!(open_file_result(&temp.0), mapped, "truncated to {len}");
    }
}

#[test]
fn open_file_refuses_a_footer_longer_than_the_file() {
    let mut bytes = fixture("production.parquet");
    let n = bytes.len();
    // The footer length field says four gigabytes.
    bytes[n - 8..n - 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let temp = TempFile::new("huge-footer", &bytes);
    let handle = PosixVfs.open(&temp.0).unwrap();
    assert!(ParquetFile::open_file(&handle).is_err());
}
