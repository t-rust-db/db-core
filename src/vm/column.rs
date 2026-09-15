//! Typed, contiguous column buffers for [`crate::vm::batch::Batch`] — the
//! `Column`/`Bitmap` core types for #130's typed-columnar-`Batch` epic
//! (child 3, db-core#425).
//!
//! Today [`Batch`](crate::vm::batch::Batch) stores each column as
//! `Arc<Vec<Value>>`: one 24-byte tagged [`Value`](crate::vm::batch::Value)
//! per row, with every `NULL` and every string boxed individually. `Column`
//! is the Arrow-shaped alternative — a packed buffer per primitive type plus
//! a separate validity [`Bitmap`], and an offsets+data layout for strings —
//! without adopting the `arrow` crate (ADR 0001: `db-core` stays
//! dependency-free) and without `unsafe` (this module is `forbid(unsafe_code)`
//! via `crate::vm`).
//!
//! This module is **additive only**: it does not change
//! [`Batch`](crate::vm::batch::Batch)'s field, any `Opcode` kernel, or any
//! existing call site. Wiring the batch VM's opcodes to dispatch over
//! `Column` instead of `Vec<Value>` is #130's child 4, not this issue.
//!
//! `Column` intentionally mirrors the shape of
//! `storage::stream::segment::OwnedColumn` (the sealed-segment encoding
//! `Column` will eventually accept without a per-row rebuild, db-core#399)
//! but replaces its `Vec<Option<T>>` nullability with a real bitmap.
//!
//! Per ADR-0025's constraint on this child: a column's type is a property
//! of one batch, not of a source. The same logical column can be `Int` in
//! one `Column`-shaped batch and `Str` in the next (e.g. a JSONL field that
//! seals differently across stream segments) — nothing here assumes a
//! static, source-level schema.

use crate::vm::batch::Value;
use std::sync::Arc;

/// A hand-rolled validity bitmap: one bit per row, `true` meaning "not
/// NULL". Backed by a `Vec<u64>` word buffer — no `unsafe`, no bitset
/// crate (ADR 0001: `db-core` stays dependency-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    bits: Vec<u64>,
    len: usize,
}

impl Bitmap {
    /// A bitmap of `len` bits, all set to `valid` (all-valid or all-null).
    pub fn new(len: usize, valid: bool) -> Self {
        let word = if valid { u64::MAX } else { 0 };
        let words = len.div_ceil(64);
        Bitmap {
            bits: vec![word; words],
            len,
        }
    }

    /// Number of bits (rows) this bitmap covers.
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` if this bitmap covers zero rows.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether row `i` is valid (not NULL). `i >= len()` reads as invalid
    /// (`false`) rather than panicking.
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "`i / 64` and `i % 64` divide/mod by a fixed non-zero power of two; the shift amount `i % 64` is always in 0..64, so it cannot overflow a u64 shift"
    )]
    pub fn get(&self, i: usize) -> bool {
        let word = self.bits.get(i / 64).copied().unwrap_or(0);
        (word >> (i % 64)) & 1 == 1
    }

    /// Sets row `i`'s validity. A no-op if `i >= len()`.
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "`i / 64` and `i % 64` divide/mod by a fixed non-zero power of two; the shift amount `i % 64` is always in 0..64"
    )]
    pub fn set(&mut self, i: usize, valid: bool) {
        let Some(word) = self.bits.get_mut(i / 64) else {
            return;
        };
        if valid {
            *word |= 1 << (i % 64);
        } else {
            *word &= !(1 << (i % 64));
        }
    }

    /// Builds a bitmap from a validity sequence (`true` = not NULL).
    pub fn from_bools(valid: impl ExactSizeIterator<Item = bool>) -> Self {
        let mut bitmap = Bitmap::new(valid.len(), true);
        for (i, v) in valid.enumerate() {
            bitmap.set(i, v);
        }
        bitmap
    }

    /// `true` if every row is valid (a fast path for an all-non-null column).
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "`len / 64` and `len % 64` divide/mod by a fixed non-zero power of two; the shift amount `rem` is always in 1..64 once checked non-zero"
    )]
    pub fn all_valid(&self) -> bool {
        let full_words = self.len / 64;
        if self
            .bits
            .get(..full_words)
            .unwrap_or(&[])
            .iter()
            .any(|&w| w != u64::MAX)
        {
            return false;
        }
        let rem = self.len % 64;
        if rem == 0 {
            return true;
        }
        let mask = (1u64 << rem) - 1;
        self.bits.get(full_words).copied().unwrap_or(0) & mask == mask
    }
}

/// One column's worth of typed, contiguous data plus a validity bitmap —
/// the unit `Batch`'s eventual per-column storage is built from (#130
/// child 3). Every variant has exactly as many logical rows as its
/// [`Bitmap`] covers.
///
/// Mirrors `storage::stream::segment::OwnedColumn`'s variant set (`Int`,
/// `Float`, `Bool`, `Str`, `Dict`) so a sealed stream segment's encoding
/// maps onto this type without reshaping (db-core#399), but stores
/// nullability in one [`Bitmap`] per column rather than per-element
/// `Option<T>`.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    /// 64-bit signed integers, one `i64` per row (NULL rows carry `0`,
    /// ignored per the validity bitmap).
    Int {
        /// Per-row values; `0` for NULL rows.
        data: Vec<i64>,
        /// Per-row validity (`true` = not NULL).
        valid: Bitmap,
    },
    /// 64-bit IEEE floats, one `f64` per row.
    Float {
        /// Per-row values; `0.0` for NULL rows.
        data: Vec<f64>,
        /// Per-row validity (`true` = not NULL).
        valid: Bitmap,
    },
    /// Booleans, one `bool` per row.
    Bool {
        /// Per-row values; `false` for NULL rows.
        data: Vec<bool>,
        /// Per-row validity (`true` = not NULL).
        valid: Bitmap,
    },
    /// Strings in Arrow's offsets+data layout: row `i`'s bytes are
    /// `data[offsets[i]..offsets[i + 1]]`. `offsets` has `len + 1` entries.
    Str {
        /// Row `i`'s byte range into `data` is `offsets[i]..offsets[i + 1]`.
        offsets: Vec<u32>,
        /// Concatenated UTF-8 bytes for every non-NULL row.
        data: String,
        /// Per-row validity (`true` = not NULL).
        valid: Bitmap,
    },
    /// Dictionary-encoded strings: `indices[i]` is row `i`'s index into
    /// `dict` (meaningless, and left at `0`, for NULL rows). Preserves the
    /// dict-code representation so `= 'literal'` can compare codes instead
    /// of decoding every row (the encoding db-core#399's stream boundary
    /// exists to carry through).
    Dict {
        /// The distinct values, indexed by dictionary code.
        dict: Vec<Arc<str>>,
        /// Row `i`'s dictionary code is `indices[i]`.
        indices: Vec<u32>,
        /// Per-row validity (`true` = not NULL).
        valid: Bitmap,
    },
}

/// Row `i`'s owned copy out of a `Column::Str`'s offsets+data buffer, or
/// `None` if `i` is out of range. Kept free-standing (rather than a
/// `Column` method) since it is the one place `offsets`/`data` need
/// pairing. Returns an owned `String` (rather than `&str`) so the
/// signature needs no lifetime beyond per-parameter elision (the qualified
/// subset gate, `make check-mvl-limit`, forbids naming one).
fn str_at(offsets: &[u32], data: &str, i: usize) -> Option<String> {
    let start = usize::try_from(*offsets.get(i)?).ok()?;
    let end = usize::try_from(*offsets.get(i.checked_add(1)?)?).ok()?;
    data.get(start..end).map(ToString::to_string)
}

impl Column {
    /// Number of rows in this column.
    pub fn len(&self) -> usize {
        match self {
            Column::Int { valid, .. }
            | Column::Float { valid, .. }
            | Column::Bool { valid, .. }
            | Column::Str { valid, .. }
            | Column::Dict { valid, .. } => valid.len(),
        }
    }

    /// `true` if this column has zero rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether row `i` is NULL. `i >= len()` reads as NULL rather than
    /// panicking.
    pub fn is_null(&self, i: usize) -> bool {
        let valid = match self {
            Column::Int { valid, .. }
            | Column::Float { valid, .. }
            | Column::Bool { valid, .. }
            | Column::Str { valid, .. }
            | Column::Dict { valid, .. } => valid,
        };
        !valid.get(i)
    }

    /// Row `i` read back as a [`Value`] (`Value::Null` if `i` is NULL or
    /// `i >= len()`). A scalar-at-a-time escape hatch for callers that are
    /// not (yet) a per-column kernel — the fast path is reading the typed
    /// buffer directly.
    pub fn get(&self, i: usize) -> Value {
        if self.is_null(i) {
            return Value::Null;
        }
        match self {
            Column::Int { data, .. } => data.get(i).copied().map_or(Value::Null, Value::Int),
            Column::Float { data, .. } => data.get(i).copied().map_or(Value::Null, Value::Float),
            Column::Bool { data, .. } => data.get(i).copied().map_or(Value::Null, Value::Bool),
            Column::Str { offsets, data, .. } => {
                str_at(offsets, data, i).map_or(Value::Null, |s| Value::Str(s.into()))
            }
            Column::Dict { dict, indices, .. } => indices
                .get(i)
                .and_then(|&code| dict.get(usize::try_from(code).unwrap_or(usize::MAX)))
                .map_or(Value::Null, |s| Value::Str(s.to_string().into())),
        }
    }
}

/// Builds a [`Column`] from row-major `Vec<Value>` data — the migration
/// path for existing `Batch::with_column(name, Vec<Value>)` callers and
/// tests (#130 child 3's stated requirement) rather than a runtime hot
/// path.
///
/// Picks the tightest single-type variant that fits every non-`Null`
/// value. A column whose non-null values are not all the same `Value`
/// variant (SQL columns are single-typed; this can only happen from
/// hand-built/test data) falls back to `Str`, formatting each value with
/// its `Display` impl, since that is the only variant that can represent
/// any `Value`.
/// Builds a `Column::Str`'s offsets+data buffers from `len_bytes`, one
/// closure call per row producing that row's string contents (empty for a
/// row that should stay unrepresented, e.g. NULL). A byte length beyond
/// `u32::MAX` saturates rather than wrapping (`db-core`'s `cast_*` lints
/// forbid a silent truncating `as u32`); a single in-memory column
/// exceeding 4 GiB of string data is not a case this migration path (test
/// data / small `Batch`es) needs to handle exactly.
fn build_str_column(
    count: usize,
    mut row_bytes: impl FnMut(usize) -> String,
) -> (Vec<u32>, String) {
    let mut offsets = Vec::with_capacity(count.saturating_add(1));
    let mut data = String::new();
    offsets.push(0u32);
    for i in 0..count {
        data.push_str(&row_bytes(i));
        offsets.push(u32::try_from(data.len()).unwrap_or(u32::MAX));
    }
    (offsets, data)
}

impl From<Vec<Value>> for Column {
    fn from(values: Vec<Value>) -> Self {
        let valid = Bitmap::from_bools(values.iter().map(|v| !matches!(v, Value::Null)));

        let all_int = values
            .iter()
            .all(|v| matches!(v, Value::Int(_) | Value::Null));
        let all_float = values
            .iter()
            .all(|v| matches!(v, Value::Float(_) | Value::Int(_) | Value::Null));
        let all_bool = values
            .iter()
            .all(|v| matches!(v, Value::Bool(_) | Value::Null));
        let all_str = values
            .iter()
            .all(|v| matches!(v, Value::Str(_) | Value::Null));

        if all_int {
            let data = values
                .iter()
                .map(|v| match v {
                    Value::Int(n) => *n,
                    _ => 0,
                })
                .collect();
            Column::Int { data, valid }
        } else if all_float {
            let data = values
                .iter()
                .map(|v| match v {
                    Value::Float(f) => *f,
                    Value::Int(n) => *n as f64,
                    _ => 0.0,
                })
                .collect();
            Column::Float { data, valid }
        } else if all_bool {
            let data = values
                .iter()
                .map(|v| matches!(v, Value::Bool(true)))
                .collect();
            Column::Bool { data, valid }
        } else if all_str {
            let (offsets, data) = build_str_column(values.len(), |i| {
                values
                    .get(i)
                    .and_then(|v| match v {
                        Value::Str(s) => Some(s.to_string()),
                        _ => None,
                    })
                    .unwrap_or_default()
            });
            Column::Str {
                offsets,
                data,
                valid,
            }
        } else {
            // Mixed-type input: only reachable from hand-built data, not
            // from a real single-typed SQL column. `Str` is the only
            // variant that can hold any `Value`.
            let (offsets, data) = build_str_column(values.len(), |i| {
                values
                    .get(i)
                    .filter(|v| !matches!(v, Value::Null))
                    .map(ToString::to_string)
                    .unwrap_or_default()
            });
            Column::Str {
                offsets,
                data,
                valid,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_all_valid_default() {
        let bitmap = Bitmap::new(70, true);
        assert_eq!(bitmap.len(), 70);
        assert!(bitmap.all_valid());
        for i in 0..70 {
            assert!(bitmap.get(i));
        }
    }

    #[test]
    fn bitmap_set_clears_all_valid() {
        let mut bitmap = Bitmap::new(65, true);
        bitmap.set(64, false);
        assert!(!bitmap.get(64));
        assert!(bitmap.get(0));
        assert!(!bitmap.all_valid());
    }

    #[test]
    fn bitmap_from_bools_round_trips() {
        let bools = [true, false, true, true, false];
        let bitmap = Bitmap::from_bools(bools.iter().copied());
        for (i, &b) in bools.iter().enumerate() {
            assert_eq!(bitmap.get(i), b);
        }
    }

    #[test]
    fn column_from_int_values_with_nulls() {
        let column = Column::from(vec![Value::Int(1), Value::Null, Value::Int(3)]);
        assert_eq!(column.len(), 3);
        assert!(!column.is_null(0));
        assert!(column.is_null(1));
        assert_eq!(column.get(0), Value::Int(1));
        assert_eq!(column.get(1), Value::Null);
        assert_eq!(column.get(2), Value::Int(3));
        assert!(matches!(column, Column::Int { .. }));
    }

    #[test]
    fn column_from_int_and_float_promotes_to_float() {
        let column = Column::from(vec![Value::Int(1), Value::Float(2.5)]);
        assert!(matches!(column, Column::Float { .. }));
        assert_eq!(column.get(0), Value::Float(1.0));
        assert_eq!(column.get(1), Value::Float(2.5));
    }

    #[test]
    fn column_from_str_values() {
        let column = Column::from(vec![
            Value::Str("kern".into()),
            Value::Null,
            Value::Str("panic".into()),
        ]);
        assert_eq!(column.len(), 3);
        assert_eq!(column.get(0), Value::Str("kern".into()));
        assert_eq!(column.get(1), Value::Null);
        assert_eq!(column.get(2), Value::Str("panic".into()));
    }

    #[test]
    fn column_from_bool_values() {
        let column = Column::from(vec![Value::Bool(true), Value::Bool(false), Value::Null]);
        assert_eq!(column.get(0), Value::Bool(true));
        assert_eq!(column.get(1), Value::Bool(false));
        assert_eq!(column.get(2), Value::Null);
    }

    #[test]
    fn dict_column_preserves_codes() {
        let dict: Vec<Arc<str>> = vec!["kern".into(), "user".into()];
        let indices = vec![0u32, 1, 0];
        let valid = Bitmap::new(3, true);
        let column = Column::Dict {
            dict,
            indices,
            valid,
        };
        assert_eq!(column.len(), 3);
        assert_eq!(column.get(0), Value::Str("kern".into()));
        assert_eq!(column.get(1), Value::Str("user".into()));
        assert_eq!(column.get(2), Value::Str("kern".into()));
    }

    /// ADR-0025's binding constraint on this child: a column's type is a
    /// property of one batch/segment, not of a source. The same logical
    /// column name can seal as `Int` in one materialized `Column` and
    /// `Str` in the next (e.g. a JSONL field across two stream segments) --
    /// nothing in `Column`/`From<Vec<Value>>` bakes in a static schema, so
    /// building both from independent `Vec<Value>` inputs must just work.
    #[test]
    fn same_column_name_can_seal_as_different_types_across_batches() {
        let segment_a = Column::from(vec![Value::Int(1), Value::Int(2)]);
        let segment_b = Column::from(vec![Value::Str("oops".into()), Value::Null]);

        assert!(matches!(segment_a, Column::Int { .. }));
        assert!(matches!(segment_b, Column::Str { .. }));
        assert_eq!(segment_a.get(0), Value::Int(1));
        assert_eq!(segment_b.get(0), Value::Str("oops".into()));
    }
}
