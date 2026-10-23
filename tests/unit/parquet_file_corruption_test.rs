// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Corrupt Parquet files never panic the reader (#657): every
//! `ParquetFile` / `RowGroupReader` call on a malformed file returns `Ok`
//! or `Err`. File bytes are untrusted -- offsets, lengths, page headers,
//! row counts and column-chunk ranges all come from the file.
//!
//! Each fixture in `tests/fixtures/parquet` is mutated deterministically:
//!
//! - truncated at evenly spaced lengths and at every length in its last
//!   32 bytes (the footer and its length/magic trailer);
//! - with a range cut out of the middle (the footer intact, every offset
//!   after the cut pointing at the wrong bytes);
//! - with each byte of the first page headers of every column chunk set
//!   to two of `0x00`, `0xFF`, high bit flipped and `+1` (alternating);
//! - with single-byte flips at pseudo-random offsets across the data pages
//!   and across the footer.
//!
//! Each mutated file is opened, and every column of every row group is read
//! with the readers for its type plus one rotating other reader (a reader
//! of the wrong type must fail cleanly too), then every top-level field as
//! nested values. Panics are caught and collected, so a failure lists every
//! mutation that panicked and where, not just the first.
//!
//! Run it in a debug build (the default for `cargo test`): arithmetic
//! overflow panics only with overflow checks on, so a release build
//! would let a wrapping bug through.

#![cfg(feature = "storage-column")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "test code fails fast (db-core#230)"
)]

use db_core::storage::parquet::footer::PhysicalType;
use db_core::storage::parquet::page::{self, PageType};
use db_core::storage::parquet::{ParquetFile, RowGroupReader};
use std::cell::{Cell, RefCell};
use std::panic::{self, AssertUnwindSafe};
use std::sync::Once;

const FIXTURES: &[&str] = &[
    "production.parquet",
    "production_snappy.parquet",
    "production_uncompressed.parquet",
    "nested_map.parquet",
    "nested_shapes.parquet",
];

/// Pages per column chunk whose headers get every-byte mutations.
const HEADER_PAGES_PER_CHUNK: usize = 1;
/// Pseudo-random single-byte flips in the data region, per file, at full
/// size.
const DATA_FLIPS: usize = 200;
/// Pseudo-random single-byte flips in the footer, per file, at full size.
const FOOTER_FLIPS: usize = 60;
/// Evenly spaced truncation lengths, per file, at full size.
const TRUNCATIONS: usize = 50;

/// How much of the mutation space a run applies, in percent:
/// `DB_CORE_CORRUPTION_PERCENT`, 25 by default so that `cargo test` stays
/// quick. The full run sets 100 (#726). It scales the evenly spaced
/// truncations, the random flips and the page-header bytes (every fourth
/// at 25); the footer's last 32 bytes and the cuts always run in full.
fn percent() -> usize {
    std::env::var("DB_CORE_CORRUPTION_PERCENT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25)
}

/// Every `stride()`-th page-header byte is mutated: 1 at 100%, 4 at 25%.
fn stride() -> usize {
    (100 / percent().clamp(1, 100)).max(1)
}

/// `full` at [`percent`], at least 1.
fn scaled(full: usize) -> usize {
    (full * percent() / 100).max(1)
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/parquet/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// xorshift64: deterministic, so a failing mutation reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Byte ranges of the page headers in each column chunk of `bytes` (the
/// unmutated fixture), up to `HEADER_PAGES_PER_CHUNK` per chunk.
fn page_header_ranges(bytes: &[u8]) -> Vec<std::ops::Range<usize>> {
    let file = ParquetFile::open(bytes).unwrap();
    let mut ranges = Vec::new();
    for row_group in &file.metadata().row_groups {
        for chunk in &row_group.columns {
            let meta = chunk.meta_data.as_ref().unwrap();
            let start = meta.dictionary_page_offset.unwrap_or(meta.data_page_offset) as usize;
            let end = start + meta.total_compressed_size as usize;
            let mut pos = start;
            for _ in 0..HEADER_PAGES_PER_CHUNK {
                if pos >= end {
                    break;
                }
                let (header, consumed) = page::decode_page_header(&bytes[pos..end]).unwrap();
                ranges.push(pos..pos + consumed);
                pos += consumed + header.compressed_page_size as usize;
            }
        }
    }
    ranges
}

/// Where the footer starts: the 4-byte length before the trailing `PAR1`.
fn footer_start(bytes: &[u8]) -> usize {
    let n = bytes.len();
    let len = u32::from_le_bytes(bytes[n - 8..n - 4].try_into().unwrap()) as usize;
    n - 8 - len
}

/// Every mutation of `bytes`, labelled for the failure report.
fn mutations(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let n = bytes.len();

    let truncations = scaled(TRUNCATIONS);
    for i in 0..truncations {
        let len = n * i / truncations;
        out.push((format!("truncate to {len}"), bytes[..len].to_vec()));
    }
    for len in n.saturating_sub(32)..n {
        out.push((format!("truncate to {len}"), bytes[..len].to_vec()));
    }

    let footer = footer_start(bytes);
    for (cut_at, cut_len) in [(4, 1), (4, 7), (footer / 3, 13), (footer / 2, 100)] {
        if cut_at + cut_len < footer {
            let mut m = bytes[..cut_at].to_vec();
            m.extend_from_slice(&bytes[cut_at + cut_len..]);
            out.push((format!("cut {cut_len} bytes at {cut_at}"), m));
        }
    }

    for range in page_header_ranges(bytes) {
        for at in range.filter(|at| at % stride() == 0) {
            let original = bytes[at];
            // Two of four values per byte, alternating, to keep the count
            // (and the debug-build run time) down.
            let values = [0x00, 0xFF, original ^ 0x80, original.wrapping_add(1)];
            for value in values.into_iter().skip(at % 2 * 2).take(2) {
                if value != original {
                    let mut m = bytes.to_vec();
                    m[at] = value;
                    out.push((format!("page header byte {at} = {value:#04x}"), m));
                }
            }
        }
    }

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ n as u64);
    for (count, region) in [
        (scaled(DATA_FLIPS), 4..footer),
        (scaled(FOOTER_FLIPS), footer..n - 8),
    ] {
        for _ in 0..count {
            let at = region.start + rng.below(region.len());
            let mask = 1u8 << rng.below(8);
            let mut m = bytes.to_vec();
            m[at] ^= mask;
            out.push((format!("flip byte {at} ^ {mask:#04x}"), m));
        }
    }
    out
}

/// One reader call on column `c` of a row group, result ignored.
type Reader = fn(&RowGroupReader<'_, '_>, usize, &[u32]);

/// Every column reader, in a fixed order the per-type lists below index.
const READERS: &[Reader] = &[
    |rg, c, _| drop(rg.read_int64_column(c)),                // 0
    |rg, c, _| drop(rg.read_int32_column(c)),                // 1
    |rg, c, _| drop(rg.read_double_column(c)),               // 2
    |rg, c, _| drop(rg.read_float_column(c)),                // 3
    |rg, c, _| drop(rg.read_boolean_column(c)),              // 4
    |rg, c, _| drop(rg.read_string_column(c)),               // 5
    |rg, c, _| drop(rg.read_binary_column(c)),               // 6
    |rg, c, _| drop(rg.read_fixed_len_byte_array_column(c)), // 7
    |rg, c, _| drop(rg.read_int96_column(c)),                // 8
    |rg, c, _| drop(rg.read_timestamp_column(c)),            // 9
    |rg, c, _| drop(rg.read_decimal_column(c)),              // 10
    |rg, c, _| drop(rg.read_int64_column_dictionary_indices(c)), // 11
    |rg, c, _| drop(rg.read_double_column_dictionary_indices(c)), // 12
    |rg, c, _| drop(rg.read_boolean_column_dictionary_indices(c)), // 13
    |rg, c, _| drop(rg.read_string_column_dictionary_indices(c)), // 14
    |rg, c, p| drop(rg.read_int64_column_at(c, p)),          // 15
    |rg, c, p| drop(rg.read_int32_column_at(c, p)),          // 16
    |rg, c, p| drop(rg.read_double_column_at(c, p)),         // 17
    |rg, c, p| drop(rg.read_float_column_at(c, p)),          // 18
    |rg, c, p| drop(rg.read_boolean_column_at(c, p)),        // 19
    |rg, c, p| drop(rg.read_string_column_at(c, p)),         // 20
    |rg, c, _| drop(rg.read_plain_fixed_column::<i64, 8>(c, i64::from_le_bytes)), // 21
    |rg, c, _| drop(rg.read_plain_fixed_column::<i32, 4>(c, i32::from_le_bytes)), // 22
    |rg, c, p| drop(rg.read_plain_fixed_column_at::<i64, 8>(c, i64::from_le_bytes, p)), // 23
    |rg, c, p| drop(rg.read_plain_fixed_column_at::<f32, 4>(c, f32::from_le_bytes, p)), // 24
    |rg, c, _| drop(rg.read_plain_string_column(c)),         // 25
];

/// The readers a caller would use for a column of `physical_type`
/// (indices into [`READERS`]).
fn readers_for(physical_type: PhysicalType) -> &'static [usize] {
    match physical_type {
        PhysicalType::Int64 => &[0, 9, 10, 11, 15, 21, 23],
        PhysicalType::Int32 => &[1, 10, 16, 22, 24],
        PhysicalType::Double => &[2, 12, 17, 21, 23],
        PhysicalType::Float => &[3, 18, 22, 24],
        PhysicalType::Boolean => &[4, 13, 19],
        PhysicalType::ByteArray => &[5, 6, 14, 20, 25],
        PhysicalType::FixedLenByteArray => &[7, 10],
        PhysicalType::Int96 => &[8, 9],
        PhysicalType::Unknown(_) => &[],
    }
}

/// Opens `bytes` and reads every column of every row group with the
/// readers for its physical type, plus one more reader rotated by `salt`
/// (a wrong-type read must fail cleanly too; across mutations every reader
/// meets every column), then every top-level field as nested values.
/// Results are ignored: only a panic is a failure.
fn read_everything(bytes: &[u8], salt: usize) {
    let Ok(file) = ParquetFile::open(bytes) else {
        return;
    };
    let _ = file.is_flat();
    let _ = file.num_rows();
    let fields = file.fields();
    for row_group_index in 0..file.num_row_groups() {
        let Some(rg) = file.row_group(row_group_index) else {
            continue;
        };
        // A few positions in and just past the row group.
        let rows = u32::try_from(rg.num_rows().clamp(0, 1 << 20)).unwrap_or(0);
        let positions = [0, rows / 2, rows.saturating_sub(1), rows, 3, 1];
        let chunks = &file.metadata().row_groups[row_group_index].columns;
        for (c, chunk) in chunks.iter().enumerate() {
            let _ = rg.schema_element(c);
            let _ = rg.nested_field_at(c);
            drop(rg.column_statistics(c));
            let typed = chunk
                .meta_data
                .as_ref()
                .map_or(&[][..], |m| readers_for(m.physical_type));
            let rotated = (c + salt) % READERS.len();
            for &r in typed.iter().chain([rotated].iter()) {
                READERS[r](&rg, c, &positions);
            }
        }
        for field in &fields {
            drop(file.read_nested_column(row_group_index, &field.name));
            drop(rg.read_map_utf8_column(field));
        }
    }
}

thread_local! {
    /// Set while this thread runs mutations: its panics are expected
    /// (to be caught), so the hook records them instead of printing.
    static CATCHING: Cell<bool> = const { Cell::new(false) };
    /// Where this thread's last caught panic happened.
    static PANIC_AT: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Installs, once per process, a panic hook that is quiet on threads
/// running mutations and the default hook everywhere else.
fn install_quiet_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if CATCHING.with(Cell::get) {
                let at = info.location().map(ToString::to_string).unwrap_or_default();
                PANIC_AT.with(|p| *p.borrow_mut() = at);
            } else {
                default(info);
            }
        }));
    });
}

/// Runs `read_everything` on every mutation of `name` and fails, listing
/// each one, if any panicked.
fn assert_no_panics(name: &str) {
    let bytes = fixture(name);
    // The unmutated file must read: otherwise the mutations test nothing.
    let file = ParquetFile::open(&bytes).unwrap();
    assert!(file.num_row_groups() > 0, "{name} has no row groups");
    drop(file);

    let mutations = mutations(&bytes);
    // The corpus must not quietly shrink: at least 300 mutations at full
    // size, in proportion at a smaller percentage.
    let floor = 300 * percent().min(100) / 100;
    assert!(
        mutations.len() >= floor,
        "{name}: only {} mutations, expected {floor}",
        mutations.len()
    );
    install_quiet_hook();
    CATCHING.with(|c| c.set(true));
    let mut panics = Vec::new();
    for (salt, (label, mutated)) in mutations.iter().enumerate() {
        if let Err(payload) =
            panic::catch_unwind(AssertUnwindSafe(|| read_everything(mutated, salt)))
        {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_default();
            let at = PANIC_AT.with(|p| p.borrow().clone());
            panics.push(format!("{name}: {label}: {message} at {at}"));
        }
    }
    CATCHING.with(|c| c.set(false));

    assert!(
        panics.is_empty(),
        "{} of {} mutation(s) panicked:\n{}",
        panics.len(),
        mutations.len(),
        panics.join("\n")
    );
}

#[test]
fn corrupt_production_never_panics() {
    assert_no_panics("production.parquet");
}

#[test]
fn corrupt_production_snappy_never_panics() {
    assert_no_panics("production_snappy.parquet");
}

#[test]
fn corrupt_production_uncompressed_never_panics() {
    assert_no_panics("production_uncompressed.parquet");
}

#[test]
fn corrupt_nested_map_never_panics() {
    assert_no_panics("nested_map.parquet");
}

#[test]
fn corrupt_nested_shapes_never_panics() {
    assert_no_panics("nested_shapes.parquet");
}

/// The unmutated fixtures still read in full: no reader the corruption
/// test exercises errors on a valid file of the matching type.
#[test]
fn unmutated_fixtures_read_without_error() {
    for name in FIXTURES {
        let bytes = fixture(name);
        let file = ParquetFile::open(&bytes).unwrap();
        for (i, rg) in file.row_groups().enumerate() {
            for c in 0..rg.num_columns() {
                let meta = &file.metadata().row_groups[i].columns[c];
                let Some(meta) = meta.meta_data.as_ref() else {
                    continue;
                };
                if !file.is_flat() {
                    continue;
                }
                let ok = match meta.physical_type {
                    PhysicalType::Int64 => rg.read_int64_column(c).is_ok(),
                    PhysicalType::Int32 => rg.read_int32_column(c).is_ok(),
                    PhysicalType::Double => rg.read_double_column(c).is_ok(),
                    PhysicalType::Float => rg.read_float_column(c).is_ok(),
                    PhysicalType::Boolean => rg.read_boolean_column(c).is_ok(),
                    PhysicalType::ByteArray => rg.read_binary_column(c).is_ok(),
                    PhysicalType::FixedLenByteArray => {
                        rg.read_fixed_len_byte_array_column(c).is_ok()
                    }
                    PhysicalType::Int96 => rg.read_int96_column(c).is_ok(),
                    PhysicalType::Unknown(t) => panic!("{name}: unknown physical type {t}"),
                };
                assert!(ok, "{name}: column {c} failed to read");
            }
        }
        for field in file.fields() {
            for i in 0..file.num_row_groups() {
                file.read_nested_column(i, &field.name)
                    .unwrap_or_else(|e| panic!("{name}: field {}: {e}", field.name));
            }
        }
    }
}

/// A page header whose type is neither data nor dictionary is never
/// produced by the fixtures; checked here so `page_header_ranges` walks
/// real page headers.
#[test]
fn page_header_ranges_cover_data_and_dictionary_pages() {
    let bytes = fixture("production.parquet");
    let ranges = page_header_ranges(&bytes);
    assert!(!ranges.is_empty());
    for range in ranges {
        let (header, consumed) = page::decode_page_header(&bytes[range.start..]).unwrap();
        assert_eq!(consumed, range.len());
        assert!(matches!(
            header.page_type,
            PageType::Data(_) | PageType::Dictionary(_)
        ));
    }
}
