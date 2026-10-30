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
//!
//! ## Arrow-shape conversion contract (#461, #472)
//!
//! Every source that decodes straight into a `Column` (`storage::column`'s
//! Parquet reader, both its whole-row-group and positional paths) follows
//! the same conversion from that source's native `Vec<Option<T>>` (one
//! nullable value per row) to a `Column` variant:
//!
//! - **Nullability moves out of the per-row `Option` and into one
//!   [`Bitmap`] bit per row** (`true` = not NULL), rather than staying an
//!   `Option<T>` per element.
//! - **A NULL row's data slot still gets a real, cheap default** for its
//!   type (`Int`/`Float`/`Bool`: the type's zero value; `Str`: an empty
//!   string) instead of leaving a hole -- the [`Bitmap`], not the
//!   defaulted value, is what a reader must consult to tell a NULL row
//!   from a genuine zero/empty value.
//! - **`Dict` is the one variant built from two parallel inputs** instead
//!   of one: the source's dictionary strings and its per-row `Option<u32>`
//!   codes split the same way -- `None` becomes a defaulted `0` code plus
//!   an invalid bitmap bit, `Some(code)` passes the code through
//!   unchanged.
//!
//! This is a *decode-time* contract only: it says nothing about how a
//! `Column` is produced (a Parquet page, a sealed stream segment, or a
//! `Vec<Value>` fallback via [`Column::from`]) or consumed (VM opcode
//! dispatch) -- only how a source's raw nullable values become one of
//! this module's variants. Any new decode path that builds a `Column`
//! directly (rather than through the `Vec<Value>` fallback) should follow
//! it, so every source's NULL handling reads back identically regardless
//! of which one produced the batch.

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

    /// Bits `start..end` (clamped) as a bitmap of their own: whole words
    /// when `start` is word-aligned, as every `SLICE_ROWS` slice is
    /// (#643), bit by bit otherwise.
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "`start / 64` and `start % 64` divide/mod by a fixed non-zero power of two"
    )]
    pub(crate) fn slice(&self, start: usize, end: usize) -> Bitmap {
        let end = end.min(self.len);
        let start = start.min(end);
        let len = end.saturating_sub(start);
        if start.is_multiple_of(64) {
            let first = start / 64;
            let last = first.saturating_add(len.div_ceil(64)).min(self.bits.len());
            return Bitmap {
                bits: self.bits.get(first..last).unwrap_or_default().to_vec(),
                len,
            };
        }
        Bitmap::from_bools((start..end).map(|i| self.get(i)))
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
    /// Binary blobs in the same offsets+data layout as [`Column::Str`], but
    /// raw bytes (#868): compared bytewise, never as text.
    Blob {
        /// Row `i`'s byte range into `data` is `offsets[i]..offsets[i + 1]`.
        offsets: Vec<u32>,
        /// Concatenated bytes for every non-NULL row.
        data: Vec<u8>,
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
    /// A list of strings in a flat layout (#809): row `i`'s elements are
    /// `elements[offsets[i]..offsets[i + 1]]`, with one `Vec` for the whole
    /// column instead of one per row. `offsets` has `len + 1` entries; a NULL
    /// element is a NULL row of `elements`, a NULL list a cleared bit in `valid`.
    List {
        /// Row `i`'s element range is `offsets[i]..offsets[i + 1]`.
        offsets: Vec<u32>,
        /// Every row's elements, back to back (a [`Column::Dict`]).
        elements: Box<Column>,
        /// Per-row validity (`true` = not a NULL list).
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
            | Column::Blob { valid, .. }
            | Column::Dict { valid, .. }
            | Column::List { valid, .. } => valid.len(),
        }
    }

    /// `true` if this column has zero rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Rows `start..start + len` (clamped to the column) as a column of
    /// their own (#643: a segment runs in slices). A dictionary column
    /// keeps its dictionary while it is small next to the slice; a large
    /// one is decoded to `Str` instead, so slicing never copies a big
    /// dictionary once per slice.
    pub(crate) fn slice(&self, start: usize, len: usize) -> Column {
        let end = start.saturating_add(len).min(self.len());
        let start = start.min(end);
        let range = start..end;
        match self {
            Column::Int { data, valid } => Column::Int {
                data: data.get(range).unwrap_or_default().to_vec(),
                valid: valid.slice(start, end),
            },
            Column::Float { data, valid } => Column::Float {
                data: data.get(range).unwrap_or_default().to_vec(),
                valid: valid.slice(start, end),
            },
            Column::Bool { data, valid } => Column::Bool {
                data: data.get(range).unwrap_or_default().to_vec(),
                valid: valid.slice(start, end),
            },
            // One copy of the slice's bytes, offsets rebased to it.
            Column::Str {
                offsets,
                data,
                valid,
            } => {
                let at = |i: usize| {
                    offsets
                        .get(i)
                        .and_then(|o| usize::try_from(*o).ok())
                        .unwrap_or(0)
                };
                let (lo, hi) = (at(start), at(end));
                let base = offsets.get(start).copied().unwrap_or(0);
                Column::Str {
                    offsets: offsets
                        .get(start..=end)
                        .unwrap_or_default()
                        .iter()
                        .map(|o| o.saturating_sub(base))
                        .collect(),
                    data: data.get(lo..hi).unwrap_or_default().to_string(),
                    valid: valid.slice(start, end),
                }
            }
            Column::Blob {
                offsets,
                data,
                valid,
            } => {
                let at = |i: usize| {
                    offsets
                        .get(i)
                        .and_then(|o| usize::try_from(*o).ok())
                        .unwrap_or(0)
                };
                let (lo, hi) = (at(start), at(end));
                let base = offsets.get(start).copied().unwrap_or(0);
                Column::Blob {
                    offsets: offsets
                        .get(start..=end)
                        .unwrap_or_default()
                        .iter()
                        .map(|o| o.saturating_sub(base))
                        .collect(),
                    data: data.get(lo..hi).unwrap_or_default().to_vec(),
                    valid: valid.slice(start, end),
                }
            }
            Column::List {
                offsets,
                elements,
                valid,
            } => {
                let at = |i: usize| {
                    offsets
                        .get(i)
                        .and_then(|o| usize::try_from(*o).ok())
                        .unwrap_or(0)
                };
                let (lo, hi) = (at(start), at(end));
                let base = offsets.get(start).copied().unwrap_or(0);
                Column::List {
                    offsets: offsets
                        .get(start..=end)
                        .unwrap_or_default()
                        .iter()
                        .map(|o| o.saturating_sub(base))
                        .collect(),
                    elements: Box::new(elements.slice(lo, hi.saturating_sub(lo))),
                    valid: valid.slice(start, end),
                }
            }
            Column::Dict {
                dict,
                indices,
                valid,
            } if dict.len() <= end.saturating_sub(start).saturating_mul(4) => Column::Dict {
                dict: dict.clone(),
                indices: indices.get(range).unwrap_or_default().to_vec(),
                valid: valid.slice(start, end),
            },
            // A dictionary large next to the slice: its strings, copied
            // once each, instead of the whole dictionary per slice.
            Column::Dict {
                dict,
                indices,
                valid,
            } => {
                let mut offsets = Vec::with_capacity(range.len().saturating_add(1));
                let mut data = String::new();
                offsets.push(0);
                for i in range {
                    if valid.get(i) {
                        let code = indices.get(i).and_then(|c| usize::try_from(*c).ok());
                        if let Some(s) = code.and_then(|c| dict.get(c)) {
                            data.push_str(s);
                        }
                    }
                    offsets.push(u32::try_from(data.len()).unwrap_or(u32::MAX));
                }
                Column::Str {
                    offsets,
                    data,
                    valid: valid.slice(start, end),
                }
            }
        }
    }

    /// The rows at `rows`, in that order and repeats allowed, as a column of
    /// their own (#815: a selection or an `UNNEST` reshapes typed columns
    /// without a `Value` per row). A dictionary column keeps its dictionary
    /// while it is small next to `rows`, else its strings are copied into a
    /// `Str`. `None` when a row is out of range.
    pub(crate) fn take(&self, rows: &[usize]) -> Option<Column> {
        if rows.iter().any(|&r| r >= self.len()) {
            return None;
        }
        let valid = Bitmap::from_bools(rows.iter().map(|&r| self.valid_at(r)));
        Some(match self {
            Column::Int { data, .. } => Column::Int {
                data: rows
                    .iter()
                    .map(|&r| data.get(r).copied().unwrap_or(0))
                    .collect(),
                valid,
            },
            Column::Float { data, .. } => Column::Float {
                data: rows
                    .iter()
                    .map(|&r| data.get(r).copied().unwrap_or(0.0))
                    .collect(),
                valid,
            },
            Column::Bool { data, .. } => Column::Bool {
                data: rows
                    .iter()
                    .map(|&r| data.get(r).copied().unwrap_or(false))
                    .collect(),
                valid,
            },
            Column::Dict { dict, indices, .. } if dict.len() <= rows.len().saturating_mul(4) => {
                Column::Dict {
                    dict: dict.clone(),
                    indices: rows
                        .iter()
                        .map(|&r| indices.get(r).copied().unwrap_or(0))
                        .collect(),
                    valid,
                }
            }
            Column::List { .. } => return self.take_list(rows),
            Column::Blob { offsets, data, .. } => {
                let mut out_offsets = Vec::with_capacity(rows.len().saturating_add(1));
                let mut out = Vec::new();
                out_offsets.push(0);
                for &r in rows {
                    let lo = offsets.get(r).and_then(|x| usize::try_from(*x).ok());
                    let hi = offsets
                        .get(r.saturating_add(1))
                        .and_then(|x| usize::try_from(*x).ok());
                    if let Some(b) = lo.zip(hi).and_then(|(lo, hi)| data.get(lo..hi)) {
                        out.extend_from_slice(b);
                    }
                    out_offsets.push(u32::try_from(out.len()).unwrap_or(u32::MAX));
                }
                Column::Blob {
                    offsets: out_offsets,
                    data: out,
                    valid,
                }
            }
            Column::Str { .. } | Column::Dict { .. } => {
                let mut offsets = Vec::with_capacity(rows.len().saturating_add(1));
                let mut data = String::new();
                offsets.push(0);
                for &r in rows {
                    if self.valid_at(r) {
                        match self {
                            Column::Str {
                                offsets: o,
                                data: d,
                                ..
                            } => {
                                let lo = o.get(r).and_then(|x| usize::try_from(*x).ok());
                                let hi = o
                                    .get(r.saturating_add(1))
                                    .and_then(|x| usize::try_from(*x).ok());
                                if let Some(s) = lo.zip(hi).and_then(|(lo, hi)| d.get(lo..hi)) {
                                    data.push_str(s);
                                }
                            }
                            Column::Dict { dict, indices, .. } => {
                                let code = indices.get(r).and_then(|c| usize::try_from(*c).ok());
                                if let Some(s) = code.and_then(|c| dict.get(c)) {
                                    data.push_str(s);
                                }
                            }
                            _ => {}
                        }
                    }
                    offsets.push(u32::try_from(data.len()).unwrap_or(u32::MAX));
                }
                Column::Str {
                    offsets,
                    data,
                    valid,
                }
            }
        })
    }

    fn valid_at(&self, i: usize) -> bool {
        !self.is_null(i)
    }

    /// Whether row `i` is NULL. `i >= len()` reads as NULL rather than
    /// panicking.
    pub fn is_null(&self, i: usize) -> bool {
        let valid = match self {
            Column::Int { valid, .. }
            | Column::Float { valid, .. }
            | Column::Bool { valid, .. }
            | Column::Str { valid, .. }
            | Column::Blob { valid, .. }
            | Column::Dict { valid, .. }
            | Column::List { valid, .. } => valid,
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
            Column::Blob { offsets, data, .. } => {
                let lo = offsets.get(i).and_then(|o| usize::try_from(*o).ok());
                let hi = offsets
                    .get(i.saturating_add(1))
                    .and_then(|o| usize::try_from(*o).ok());
                lo.zip(hi)
                    .and_then(|(lo, hi)| data.get(lo..hi))
                    .map_or(Value::Null, |b| Value::Blob(b.into()))
            }
            Column::Dict { dict, indices, .. } => indices
                .get(i)
                .and_then(|&code| dict.get(usize::try_from(code).unwrap_or(usize::MAX)))
                .map_or(Value::Null, |s| Value::Str(s.to_string().into())),
            Column::List {
                offsets, elements, ..
            } => {
                let at = |i: usize| {
                    offsets
                        .get(i)
                        .and_then(|o| usize::try_from(*o).ok())
                        .unwrap_or(0)
                };
                let items = (at(i)..at(i.saturating_add(1)))
                    .map(|j| elements.shared_str(j))
                    .collect();
                Value::List(Arc::new(items))
            }
        }
    }

    /// [`Self::take`] of a [`Column::List`] (#809): the listed rows'
    /// elements, back to back, so a selection or a repeat (`UNNEST`) keeps
    /// the list flat.
    fn take_list(&self, rows: &[usize]) -> Option<Column> {
        let Column::List {
            offsets,
            elements,
            valid,
        } = self
        else {
            return None;
        };
        let at = |i: usize| {
            offsets
                .get(i)
                .and_then(|o| usize::try_from(*o).ok())
                .unwrap_or(0)
        };
        let mut taken_offsets = Vec::with_capacity(rows.len().saturating_add(1));
        taken_offsets.push(0u32);
        let mut positions: Vec<usize> = Vec::new();
        let mut taken_valid: Vec<bool> = Vec::with_capacity(rows.len());
        for &row in rows {
            positions.extend(at(row)..at(row.saturating_add(1)));
            taken_offsets.push(u32::try_from(positions.len()).ok()?);
            taken_valid.push(valid.get(row));
        }
        Some(Column::List {
            offsets: taken_offsets,
            elements: Box::new(elements.take(&positions)?),
            valid: Bitmap::from_bools(taken_valid.into_iter()),
        })
    }

    /// Row `i` of a [`Column::Dict`] or [`Column::Str`] as a shared string
    /// (`None` for NULL, out of range, or any other variant).
    pub(crate) fn shared_str(&self, i: usize) -> Option<Arc<str>> {
        match self {
            Column::Dict {
                dict,
                indices,
                valid,
            } if valid.get(i) => indices
                .get(i)
                .and_then(|&code| dict.get(usize::try_from(code).unwrap_or(usize::MAX)))
                .cloned(),
            Column::Str {
                offsets,
                data,
                valid,
            } if valid.get(i) => str_at(offsets, data, i).map(Arc::from),
            _ => None,
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
pub(crate) fn build_str_column(
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

        let all_blob = values
            .iter()
            .all(|v| matches!(v, Value::Blob(_) | Value::Null));

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
        } else if all_blob {
            let mut offsets = Vec::with_capacity(values.len().saturating_add(1));
            let mut data = Vec::new();
            offsets.push(0);
            for v in &values {
                if let Value::Blob(b) = v {
                    data.extend_from_slice(b);
                }
                offsets.push(u32::try_from(data.len()).unwrap_or(u32::MAX));
            }
            Column::Blob {
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

    /// #643: a slice of each column shape keeps its rows and NULLs; a
    /// dictionary large next to the slice becomes `Str`.
    #[test]
    fn slice_keeps_rows_nulls_and_small_dictionaries() {
        let ints = Column::from(vec![
            Value::Int(1),
            Value::Null,
            Value::Int(3),
            Value::Int(4),
        ]);
        let s = ints.slice(1, 2);
        assert_eq!(
            (s.len(), s.get(0), s.get(1)),
            (2, Value::Null, Value::Int(3))
        );
        let floats = Column::Float {
            data: vec![0.5, 1.5, 2.5],
            valid: Bitmap::new(3, true),
        };
        assert_eq!(floats.slice(2, 10).get(0), Value::Float(2.5));
        let bools = Column::Bool {
            data: vec![true, false],
            valid: Bitmap::new(2, true),
        };
        assert_eq!(bools.slice(1, 1).get(0), Value::Bool(false));
        let strs = Column::Str {
            offsets: vec![0, 1, 3, 3],
            data: "abc".to_string(),
            valid: Bitmap::from_bools([true, true, false].into_iter()),
        };
        let s = strs.slice(1, 2);
        assert_eq!((s.get(0), s.get(1)), (Value::Str("bc".into()), Value::Null));
        let dict = |n: usize| Column::Dict {
            dict: (0..n)
                .map(|i| Arc::from(format!("v{i}").as_str()))
                .collect(),
            indices: vec![0, 1, 0, 1],
            valid: Bitmap::new(4, true),
        };
        assert!(matches!(dict(2).slice(0, 2), Column::Dict { .. }));
        let big = dict(100).slice(1, 2);
        assert!(matches!(big, Column::Str { .. }));
        assert_eq!(
            (big.get(0), big.get(1)),
            (Value::Str("v1".into()), Value::Str("v0".into()))
        );
        assert!(ints.slice(9, 3).is_empty());
    }

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
