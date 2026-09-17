// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The Parquet row-group -> [`vm::batch`](crate::vm::batch) decode boundary
//! (#467): the one place that turns [`RowGroupReader`] columns into typed
//! [`Column`]s. `engine::column` is the only caller today; column-rs's own
//! `RowGroupSegment` (`query.rs`) keeps a second, divergent implementation
//! of this same dispatch and should call these functions instead so
//! storage-layer changes (e.g. #457, #461, #472) reach both loaders. Note
//! one behavioral choice a column-rs caller must account for: a decode
//! failure here is returned as an `Err`, never substituted with a batch of
//! NULLs (column-rs#27 wants the latter for its own loader).

use super::parquet::{footer::PhysicalType, FileError, RowGroupReader};
use crate::vm::batch::{Bitmap, Column};
use crate::vm::column::build_str_column;

/// One column's decoded shape out of a whole row group: every
/// [`PhysicalType`] arm builds a typed [`Column`] directly (#461) rather
/// than rebuilding a per-row [`Value`](crate::vm::batch::Value).
pub enum Decoded {
    Column(Column),
}

/// Decodes one column for every row of `rg` -- the whole-row-group path
/// shared by the eager load and the predicate phase of the two-phase load
/// (ADR-0026). Every physical type decodes straight into a typed [`Column`]
/// buffer -- no per-row [`Value`](crate::vm::batch::Value) rebuild pass --
/// mirroring #457's dictionary-string path below for the numeric/bool
/// primitives (#461).
pub fn decode_column_full(
    rg: &RowGroupReader<'_, '_>,
    index: usize,
    physical_type: PhysicalType,
) -> Result<Decoded, FileError> {
    match physical_type {
        PhysicalType::Int64 => rg
            .read_int64_column(index)
            .map(|col| Decoded::Column(int_column(col))),
        PhysicalType::Int32 => rg.read_int32_column(index).map(|col| {
            Decoded::Column(int_column(
                col.into_iter().map(|v| v.map(i64::from)).collect(),
            ))
        }),
        PhysicalType::Double => rg
            .read_double_column(index)
            .map(|col| Decoded::Column(float_column(col))),
        PhysicalType::Float => rg.read_float_column(index).map(|col| {
            Decoded::Column(float_column(
                col.into_iter().map(|v| v.map(f64::from)).collect(),
            ))
        }),
        PhysicalType::Boolean => rg
            .read_boolean_column(index)
            .map(|col| Decoded::Column(bool_column(col))),
        // #457: a `PLAIN_DICTIONARY`-encoded string column materializes
        // as `Column::Dict` (one dict entry per distinct value, one
        // `u32` code per row) instead of decoding every row to its own
        // owned `String` -- the dictionary case column-rs's
        // `GroupReduce`/`Map` dict fast paths were already built to
        // consume. A column with no dictionary page, or one that falls
        // back to `PLAIN` partway through (`Ok(None)`), decodes into
        // `Column::Str` directly instead (#461).
        _ => rg.read_string_column_dictionary_indices(index).and_then(
            |maybe_dict| match maybe_dict {
                Some((dict, codes)) => Ok(Decoded::Column(dict_column(dict, codes))),
                None => rg
                    .read_string_column(index)
                    .map(|col| Decoded::Column(str_column(col))),
            },
        ),
    }
}

/// Decodes one column only at `positions` (ADR-0026, phase 2b) -- the
/// projection-only path. Builds a typed [`Column`] directly, reusing the
/// same `int_column`/`float_column`/`bool_column`/`str_column` conversion
/// helpers as [`decode_column_full`] (#461, #472): the positional readers
/// already return the identical `Vec<Option<T>>` shape those helpers
/// expect, so no separate positional `Value`-rebuild path is needed.
/// Dictionary string columns deliberately fall through to the plain
/// positional string reader here rather than a positional
/// dictionary-codes reader (#457's dict fast path stays eager-decode-only
/// for v1; see ADR-0026's consequences).
pub fn decode_column_at(
    rg: &RowGroupReader<'_, '_>,
    index: usize,
    physical_type: PhysicalType,
    positions: &[u32],
) -> Result<Decoded, FileError> {
    match physical_type {
        PhysicalType::Int64 => rg
            .read_int64_column_at(index, positions)
            .map(|col| Decoded::Column(int_column(col))),
        PhysicalType::Int32 => rg.read_int32_column_at(index, positions).map(|col| {
            Decoded::Column(int_column(
                col.into_iter().map(|v| v.map(i64::from)).collect(),
            ))
        }),
        PhysicalType::Double => rg
            .read_double_column_at(index, positions)
            .map(|col| Decoded::Column(float_column(col))),
        PhysicalType::Float => rg.read_float_column_at(index, positions).map(|col| {
            Decoded::Column(float_column(
                col.into_iter().map(|v| v.map(f64::from)).collect(),
            ))
        }),
        PhysicalType::Boolean => rg
            .read_boolean_column_at(index, positions)
            .map(|col| Decoded::Column(bool_column(col))),
        _ => rg
            .read_string_column_at(index, positions)
            .map(|col| Decoded::Column(str_column(col))),
    }
}

/// Converts a Parquet dictionary chunk's `(dict, codes)` (`codes[i]` is
/// `None` for a NULL row) into the VM's [`Column::Dict`] representation,
/// which splits nullability out into a [`Bitmap`] and defaults a NULL row's
/// index to `0` rather than carrying an `Option` per row (#457).
fn dict_column(dict: Vec<String>, codes: Vec<Option<u32>>) -> Column {
    let valid = Bitmap::from_bools(codes.iter().map(Option::is_some));
    let indices = codes.iter().map(|c| c.unwrap_or(0)).collect();
    Column::Dict {
        dict: dict.into_iter().map(Into::into).collect(),
        indices,
        valid,
    }
}

/// Converts a Parquet `INT64`/`INT32`-widened column's `Vec<Option<i64>>`
/// (`None` for a NULL row) into [`Column::Int`], splitting nullability into
/// a [`Bitmap`] and defaulting a NULL row's slot to `0` (#461).
fn int_column(values: Vec<Option<i64>>) -> Column {
    let valid = Bitmap::from_bools(values.iter().map(Option::is_some));
    let data = values.into_iter().map(|v| v.unwrap_or(0)).collect();
    Column::Int { data, valid }
}

/// Converts a Parquet `DOUBLE`/`FLOAT`-widened column's `Vec<Option<f64>>`
/// into [`Column::Float`], splitting nullability into a [`Bitmap`] and
/// defaulting a NULL row's slot to `0.0` (#461).
fn float_column(values: Vec<Option<f64>>) -> Column {
    let valid = Bitmap::from_bools(values.iter().map(Option::is_some));
    let data = values.into_iter().map(|v| v.unwrap_or(0.0)).collect();
    Column::Float { data, valid }
}

/// Converts a Parquet `BOOLEAN` column's `Vec<Option<bool>>` into
/// [`Column::Bool`], splitting nullability into a [`Bitmap`] and defaulting
/// a NULL row's slot to `false` (#461).
fn bool_column(values: Vec<Option<bool>>) -> Column {
    let valid = Bitmap::from_bools(values.iter().map(Option::is_some));
    let data = values.into_iter().map(|v| v.unwrap_or(false)).collect();
    Column::Bool { data, valid }
}

/// Converts a Parquet plain-encoded (non-dictionary) string column's
/// `Vec<Option<String>>` into [`Column::Str`]'s offsets+data layout, one
/// [`Bitmap`] bit per row rather than an `Option` wrapper per row (#461).
fn str_column(values: Vec<Option<String>>) -> Column {
    let valid = Bitmap::from_bools(values.iter().map(Option::is_some));
    let (offsets, data) = build_str_column(values.len(), |i| {
        values.get(i).and_then(Option::clone).unwrap_or_default()
    });
    Column::Str {
        offsets,
        data,
        valid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::batch::Value;

    #[test]
    fn dict_column_carries_codes_through_unchanged() {
        let dict = vec!["east".to_string(), "west".to_string()];
        let column = dict_column(dict, vec![Some(0), Some(1), Some(0)]);
        match column {
            Column::Dict {
                dict,
                indices,
                valid,
            } => {
                assert_eq!(
                    dict.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
                    ["east", "west"]
                );
                assert_eq!(indices, vec![0, 1, 0]);
                assert!(valid.all_valid());
            }
            other => panic!("expected Column::Dict, got {other:?}"),
        }
    }

    #[test]
    fn dict_column_defaults_a_null_rows_code_to_zero_but_marks_it_invalid() {
        // #457 acceptance criterion: NULL codes (`None` in the Parquet
        // reader's indices vector) must not read back as a spurious
        // dictionary entry -- the validity bitmap, not the defaulted `0`
        // index, is what marks the row NULL.
        let dict = vec!["east".to_string()];
        let column = dict_column(dict, vec![Some(0), None, Some(0)]);
        match &column {
            Column::Dict { indices, valid, .. } => {
                assert_eq!(*indices, vec![0, 0, 0]);
                assert!(valid.get(0));
                assert!(!valid.get(1));
                assert!(valid.get(2));
            }
            other => panic!("expected Column::Dict, got {other:?}"),
        }
        assert!(column.is_null(1));
        assert!(!column.is_null(0));
    }

    /// #461: an `INT64`/`INT32` row group decodes straight into
    /// `Column::Int`, and reading it back agrees row-for-row with the
    /// `Vec<Value>` fallback path (`Column::from`) over the same data --
    /// the differential obligation the issue's acceptance criteria ask for.
    #[test]
    fn int_column_matches_the_value_fallback_including_nulls() {
        let values = vec![Some(1), None, Some(-3)];
        let typed = int_column(values.clone());
        let fallback = Column::from(
            values
                .into_iter()
                .map(|v| v.map_or(Value::Null, Value::Int))
                .collect::<Vec<_>>(),
        );
        assert!(matches!(typed, Column::Int { .. }));
        for i in 0..3 {
            assert_eq!(typed.get(i), fallback.get(i));
            assert_eq!(typed.is_null(i), fallback.is_null(i));
        }
    }

    #[test]
    fn float_column_matches_the_value_fallback_including_nulls() {
        let values = vec![Some(1.5), None, Some(-2.25)];
        let typed = float_column(values.clone());
        let fallback = Column::from(
            values
                .into_iter()
                .map(|v| v.map_or(Value::Null, Value::Float))
                .collect::<Vec<_>>(),
        );
        assert!(matches!(typed, Column::Float { .. }));
        for i in 0..3 {
            assert_eq!(typed.get(i), fallback.get(i));
            assert_eq!(typed.is_null(i), fallback.is_null(i));
        }
    }

    #[test]
    fn bool_column_matches_the_value_fallback_including_nulls() {
        let values = vec![Some(true), None, Some(false)];
        let typed = bool_column(values.clone());
        let fallback = Column::from(
            values
                .into_iter()
                .map(|v| v.map_or(Value::Null, Value::Bool))
                .collect::<Vec<_>>(),
        );
        assert!(matches!(typed, Column::Bool { .. }));
        for i in 0..3 {
            assert_eq!(typed.get(i), fallback.get(i));
            assert_eq!(typed.is_null(i), fallback.is_null(i));
        }
    }

    #[test]
    fn str_column_matches_the_value_fallback_including_nulls() {
        let values = vec![Some("east".to_string()), None, Some("west".to_string())];
        let typed = str_column(values.clone());
        let fallback = Column::from(
            values
                .into_iter()
                .map(|v| v.map_or(Value::Null, |s| Value::Str(s.into())))
                .collect::<Vec<_>>(),
        );
        assert!(matches!(typed, Column::Str { .. }));
        for i in 0..3 {
            assert_eq!(typed.get(i), fallback.get(i));
            assert_eq!(typed.is_null(i), fallback.is_null(i));
        }
    }

    // Minimal single-column, single-row-group, REQUIRED/PLAIN Parquet file
    // builder so `decode_column_full`/`decode_column_at`'s Int32/Float/
    // Boolean/Double dispatch arms run against a real `RowGroupReader`, not
    // just their `_column` helpers in isolation. Mirrors `engine::column`'s
    // own `StructWriter`/`build_double_row_group` test helpers (each test
    // module keeps its own small copy rather than sharing a `pub(crate)`
    // surface just for tests).
    struct StructWriter {
        buf: Vec<u8>,
        last_field_id: i16,
    }

    impl StructWriter {
        fn new() -> Self {
            StructWriter {
                buf: Vec::new(),
                last_field_id: 0,
            }
        }
        fn write_varint(&mut self, mut v: u64) {
            loop {
                let mut b = (v & 0x7f) as u8;
                v >>= 7;
                if v != 0 {
                    b |= 0x80;
                }
                self.buf.push(b);
                if v == 0 {
                    break;
                }
            }
        }
        fn zigzag(v: i64) -> u64 {
            ((v << 1) ^ (v >> 63)) as u64
        }
        fn field_header(&mut self, field_id: i16, ctype: u8) {
            let delta = field_id - self.last_field_id;
            assert!((1..=15).contains(&delta));
            self.buf.push(((delta as u8) << 4) | ctype);
            self.last_field_id = field_id;
        }
        fn i32_field(&mut self, field_id: i16, v: i32) {
            self.field_header(field_id, 0x05);
            self.write_varint(Self::zigzag(i64::from(v)));
        }
        fn i64_field(&mut self, field_id: i16, v: i64) {
            self.field_header(field_id, 0x06);
            self.write_varint(Self::zigzag(v));
        }
        fn string_field(&mut self, field_id: i16, s: &str) {
            self.field_header(field_id, 0x08);
            self.write_varint(s.len() as u64);
            self.buf.extend_from_slice(s.as_bytes());
        }
        fn struct_field(&mut self, field_id: i16, inner: Vec<u8>) {
            self.field_header(field_id, 0x0c);
            self.buf.extend_from_slice(&inner);
        }
        fn list_of_structs_field(&mut self, field_id: i16, items: Vec<Vec<u8>>) {
            self.field_header(field_id, 0x09);
            let n = items.len();
            if n < 15 {
                self.buf.push(((n as u8) << 4) | 0x0c);
            } else {
                self.buf.push((15u8 << 4) | 0x0c);
                self.write_varint(n as u64);
            }
            for item in items {
                self.buf.extend_from_slice(&item);
            }
        }
        fn finish(mut self) -> Vec<u8> {
            self.buf.push(0x00);
            self.buf
        }
    }

    fn build_page_header(num_values: i32, page_size: i32) -> Vec<u8> {
        let mut dph = StructWriter::new();
        dph.i32_field(1, num_values);
        dph.i32_field(2, 0); // encoding = PLAIN
        dph.i32_field(3, 3);
        dph.i32_field(4, 3);
        let dph_bytes = dph.finish();

        let mut w = StructWriter::new();
        w.i32_field(1, 0); // DATA_PAGE
        w.i32_field(2, page_size);
        w.i32_field(3, page_size);
        w.struct_field(5, dph_bytes);
        w.finish()
    }

    /// A single-column (`v`, REQUIRED, `physical_type`), single-row-group
    /// file: `body` is that column's already-PLAIN-encoded page payload.
    fn build_single_column_file(physical_type: i32, body: &[u8], num_values: i32) -> Vec<u8> {
        let header = build_page_header(num_values, body.len() as i32);
        let mut page_bytes = header;
        page_bytes.extend_from_slice(body);

        // Page bytes are written right after the 4-byte "PAR1" magic, so
        // that's the data_page_offset both the chunk and its metadata must
        // agree on -- 0 here would point the reader at the magic bytes
        // instead of the page header.
        let base_offset = 4i64;

        let mut meta = StructWriter::new();
        meta.i32_field(1, physical_type);
        meta.field_header(3, 0x09); // path_in_schema list<string>
        meta.buf.push((1u8 << 4) | 0x08);
        meta.write_varint(1);
        meta.buf.push(b'v');
        meta.i64_field(5, i64::from(num_values));
        meta.i64_field(6, page_bytes.len() as i64);
        meta.i64_field(7, page_bytes.len() as i64);
        meta.i64_field(9, base_offset); // data_page_offset
        let meta_bytes = meta.finish();

        let mut file = Vec::new();
        file.extend_from_slice(b"PAR1");
        file.extend_from_slice(&page_bytes);

        let mut chunk = StructWriter::new();
        chunk.i64_field(2, base_offset);
        chunk.struct_field(3, meta_bytes);
        let chunk_bytes = chunk.finish();

        let mut rg = StructWriter::new();
        rg.list_of_structs_field(1, vec![chunk_bytes]);
        rg.i64_field(2, page_bytes.len() as i64);
        rg.i64_field(3, i64::from(num_values));
        let rg_bytes = rg.finish();

        let mut root = StructWriter::new();
        root.string_field(4, "schema");
        root.i32_field(5, 1);
        let root = root.finish();

        let mut col = StructWriter::new();
        col.i32_field(1, physical_type);
        col.i32_field(3, 0); // REQUIRED
        col.string_field(4, "v");
        let col = col.finish();

        let mut fmd = StructWriter::new();
        fmd.i32_field(1, 1);
        fmd.list_of_structs_field(2, vec![root, col]);
        fmd.i64_field(3, i64::from(num_values));
        fmd.list_of_structs_field(4, vec![rg_bytes]);
        fmd.string_field(6, "test");
        let metadata = fmd.finish();

        file.extend_from_slice(&metadata);
        file.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        file.extend_from_slice(b"PAR1");
        file
    }

    /// #461/#467: `decode_column_full`'s `Int32` arm widens straight to
    /// `Column::Int` through a real `RowGroupReader`, not just through
    /// `int_column`'s own unit test above.
    #[test]
    fn decode_column_full_widens_int32_to_column_int() {
        let values: [i32; 3] = [1, -2, 3];
        let mut body = Vec::new();
        for v in values {
            body.extend_from_slice(&v.to_le_bytes());
        }
        let file_bytes = build_single_column_file(1 /* INT32 */, &body, values.len() as i32);
        let file = super::super::parquet::ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let Decoded::Column(column) = decode_column_full(&rg, 0, PhysicalType::Int32).unwrap();
        match column {
            Column::Int { data, valid } => {
                assert_eq!(data, vec![1, -2, 3]);
                assert!(valid.all_valid());
            }
            other => panic!("expected Column::Int, got {other:?}"),
        }
    }

    /// `decode_column_full`'s `Float` arm widens straight to
    /// `Column::Float` through a real `RowGroupReader`.
    #[test]
    fn decode_column_full_widens_float_to_column_float() {
        let values: [f32; 2] = [1.5, -2.5];
        let mut body = Vec::new();
        for v in values {
            body.extend_from_slice(&v.to_le_bytes());
        }
        let file_bytes = build_single_column_file(4 /* FLOAT */, &body, values.len() as i32);
        let file = super::super::parquet::ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let Decoded::Column(column) = decode_column_full(&rg, 0, PhysicalType::Float).unwrap();
        match column {
            Column::Float { data, valid } => {
                assert_eq!(data, vec![1.5, -2.5]);
                assert!(valid.all_valid());
            }
            other => panic!("expected Column::Float, got {other:?}"),
        }
    }

    /// `decode_column_full`'s `Boolean` arm decodes the bit-packed PLAIN
    /// encoding straight to `Column::Bool` through a real `RowGroupReader`.
    #[test]
    fn decode_column_full_decodes_boolean_to_column_bool() {
        let values = [true, false, true, true, false];
        let mut byte = 0u8;
        for (i, v) in values.iter().enumerate() {
            if *v {
                byte |= 1 << i;
            }
        }
        let file_bytes =
            build_single_column_file(0 /* BOOLEAN */, &[byte], values.len() as i32);
        let file = super::super::parquet::ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let Decoded::Column(column) = decode_column_full(&rg, 0, PhysicalType::Boolean).unwrap();
        match column {
            Column::Bool { data, valid } => {
                assert_eq!(data, values.to_vec());
                assert!(valid.all_valid());
            }
            other => panic!("expected Column::Bool, got {other:?}"),
        }
    }

    /// #467: `decode_column_at`'s `Int32`/`Boolean` positional arms (the
    /// ADR-0026 projection-only path) agree with `decode_column_full`'s
    /// eager path at the same positions.
    #[test]
    fn decode_column_at_int32_and_boolean_match_the_eager_path_at_selected_positions() {
        let int_values: [i32; 4] = [10, 20, 30, 40];
        let mut int_body = Vec::new();
        for v in int_values {
            int_body.extend_from_slice(&v.to_le_bytes());
        }
        let int_file_bytes = build_single_column_file(1, &int_body, int_values.len() as i32);
        let int_file = super::super::parquet::ParquetFile::open(&int_file_bytes).unwrap();
        let int_rg = int_file.row_group(0).unwrap();
        let positions = [0u32, 2, 3];
        let Decoded::Column(int_at) =
            decode_column_at(&int_rg, 0, PhysicalType::Int32, &positions).unwrap();
        assert_eq!(int_at.get(0), Value::Int(10));
        assert_eq!(int_at.get(1), Value::Int(30));
        assert_eq!(int_at.get(2), Value::Int(40));

        let bool_values = [true, false, true, false, true];
        let mut byte = 0u8;
        for (i, v) in bool_values.iter().enumerate() {
            if *v {
                byte |= 1 << i;
            }
        }
        let bool_file_bytes = build_single_column_file(0, &[byte], bool_values.len() as i32);
        let bool_file = super::super::parquet::ParquetFile::open(&bool_file_bytes).unwrap();
        let bool_rg = bool_file.row_group(0).unwrap();
        let Decoded::Column(bool_at) =
            decode_column_at(&bool_rg, 0, PhysicalType::Boolean, &positions).unwrap();
        assert_eq!(bool_at.get(0), Value::Bool(true));
        assert_eq!(bool_at.get(1), Value::Bool(true));
        assert_eq!(bool_at.get(2), Value::Bool(false));
    }

    /// #472: `decode_column_at`'s typed positional decode must agree,
    /// row-for-row, with gathering the same positions out of
    /// `decode_column_full`'s whole-row-group decode -- the differential
    /// obligation the issue's acceptance criteria ask for. Both now share
    /// the same `float_column` conversion helper, so this pins the actual
    /// risk surface of the change: which physical type wires to which
    /// `read_*_column`/`read_*_column_at` pair, not the (already
    /// differentially tested, #461) `Vec<Option<T>>` -> `Column`
    /// conversion itself.
    #[test]
    fn decode_column_at_matches_decode_column_full_gathered_at_the_same_positions() {
        let values: [f64; 5] = [10.0, 20.0, 30.0, 40.0, 50.0];
        let mut body = Vec::new();
        for v in values {
            body.extend_from_slice(&v.to_le_bytes());
        }
        let file_bytes = build_single_column_file(5 /* DOUBLE */, &body, values.len() as i32);
        let file = super::super::parquet::ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();

        let Decoded::Column(full) = decode_column_full(&rg, 0, PhysicalType::Double).unwrap();

        let positions: Vec<u32> = vec![4, 1, 1, 0];
        let Decoded::Column(positional) =
            decode_column_at(&rg, 0, PhysicalType::Double, &positions).unwrap();

        assert_eq!(positional.len(), positions.len());
        for (i, &pos) in positions.iter().enumerate() {
            assert_eq!(
                positional.get(i),
                full.get(pos as usize),
                "position {i} (row {pos}) disagrees between decode_column_at and decode_column_full"
            );
            assert_eq!(positional.is_null(i), full.is_null(pos as usize));
        }
    }
}
