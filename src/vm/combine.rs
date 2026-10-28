// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `Combine`'s cross-segment merge of partial `GROUP BY` aggregates, done
//! column-major over the per-segment [`Chunk`]s `run_parallel` produces
//! (#478 phase 1) instead of over rows.
//!
//! [`super::engine::finalize`] is the row-based reference implementation
//! (public, called directly by AOT-emitted binaries and tests); it
//! transposes the whole output to `Vec<Vec<Value>>` first -- one heap
//! allocation per partial row -- and then dispatches on [`AggPart`] per
//! row. At 100K groups x 82 segments that is 8.2M rows and 8.2M
//! allocations before any merging happens. [`combine_chunks`] never
//! materializes a row: keys are hashed column-wise
//! ([`hash_columns_by_row`], #440), each row is mapped to a group id
//! against a hash-then-verify table (the shape `GroupReduce` itself uses,
//! #439), and every aggregate column of the chunk is then merged into its
//! accumulator column with **one** [`SlotOp`] dispatch per column-chunk,
//! not per row. Same shape as DataFusion's `GroupValuesPrimitive` /
//! `FinalPartitioned` merge.
//!
//! The merge semantics -- `Null` as the additive identity, `COUNT` kept as
//! a checked `i64`, `MIN`/`MAX` compared as `f64`, `AVG`'s `(sum, count)`
//! slot pair (#404) -- live here once, as [`merge_slot`]; `finalize`'s
//! `merge_rows` is built on the same function so the two paths cannot
//! drift (a differential test below pins them to each other).
//!
//! Groups are emitted in first-seen order across chunks in chunk order,
//! exactly as `finalize` does -- `GROUP BY` without `ORDER BY` has no
//! guaranteed order, but the segment-split-invariance suite compares
//! whole results, so the order is observable and must not change here.

use std::collections::HashMap;
use std::sync::Arc;

use super::acc_column::AccColumn;
use super::batch::{
    agg_cmp, apply_map_op, chunk_len, hash_columns_by_row, mix_hash, AggOperand, AggPart, Chunk,
    HiddenPart, MapOp, QueryOutput, Result, SlotState, Value, VmError, HASH_SEED,
};
use super::hll::Hll;
use super::int_key_table::IntKeyTable;

/// What `Combine` does to one emitted slot (column) when two partial rows
/// of the same group meet -- [`AggPart`] resolved onto the emitted row's
/// slot positions, so an `Avg` (one part, two slots: #404) is two ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotOp {
    /// A `GROUP BY` key, or a slot no part claims: never merged, the
    /// first-seen row's value stands.
    Key,
    /// Partial `SUM`: added; `Null` is the identity, `Null`+`Null` stays `Null`.
    Sum,
    /// Partial `COUNT`: added as `i64`, overflow is an error, stays `Int`.
    Count,
    /// Partial `MIN`: the smaller as `f64`; a `Null` side yields the other.
    Min,
    /// Partial `MAX`: the larger as `f64`; a `Null` side yields the other.
    Max,
    /// `approx_count_distinct`'s sketch (#603): merged register-wise.
    Sketch,
    /// An exact `COUNT(DISTINCT <integer>)`'s sorted distinct values: unioned
    /// (#817).
    IntSet,
    /// [`SlotOp::IntSet`] for strings, in no order (#820).
    StrSet,
    /// `AVG`'s sum slot: merged like [`SlotOp::Sum`].
    AvgSum,
    /// `AVG`'s count slot: added as `f64` (only ever divided into by the
    /// finalizer, never surfaced, so it skips `Count`'s overflow check).
    AvgCount,
}

/// Resolves `parts` onto the `num_slots` slots of an emitted row, walking
/// the same row cursor `finalize`'s `merge_rows` does: every part takes one
/// slot except `Avg(sum_i, count_i)`, which names its two slots directly
/// and leaves the cursor just past `count_i` (#404). A slot no part claims
/// is [`SlotOp::Key`] (untouched), which is what the cursor walk always did
/// for it. An `Avg` index outside the row is a planner bug, reported rather
/// than indexed.
pub(crate) fn slot_ops(parts: &[AggPart], num_slots: usize) -> Result<Vec<SlotOp>> {
    let mut ops = vec![SlotOp::Key; num_slots];
    let mut cursor = 0usize;
    for part in parts {
        match part {
            AggPart::Avg(sum_i, count_i) => {
                for (i, op) in [(*sum_i, SlotOp::AvgSum), (*count_i, SlotOp::AvgCount)] {
                    let slot = ops.get_mut(i).ok_or_else(|| VmError::MalformedProgram {
                        opcode: "Combine",
                        reason: format!(
                            "AVG slot {i} is outside the {num_slots}-column emitted row"
                        ),
                    })?;
                    *slot = op;
                }
                cursor = count_i.saturating_add(1);
            }
            // No slot of its own (#496): both operands name slots another
            // part already claimed, so the cursor doesn't move.
            AggPart::Expr(..) => {}
            other => {
                if let Some(slot) = ops.get_mut(cursor) {
                    *slot = match other {
                        AggPart::GroupKey | AggPart::DistinctKey => SlotOp::Key,
                        AggPart::Sum | AggPart::Hidden(HiddenPart::Sum) => SlotOp::Sum,
                        AggPart::Count | AggPart::Hidden(HiddenPart::Count) => SlotOp::Count,
                        AggPart::Min | AggPart::Hidden(HiddenPart::Min) => SlotOp::Min,
                        AggPart::Max | AggPart::Hidden(HiddenPart::Max) => SlotOp::Max,
                        AggPart::ApproxCountDistinct => SlotOp::Sketch,
                        AggPart::ExactCountDistinct => SlotOp::IntSet,
                        AggPart::ExactCountDistinctStr => SlotOp::StrSet,
                        AggPart::Avg(..) | AggPart::Expr(..) => SlotOp::Key,
                    };
                }
                cursor = cursor.saturating_add(1);
            }
        }
    }
    Ok(ops)
}

/// How many slots an emitted row of `parts` has: one per part, an `Avg`
/// up to its count slot, an `Expr` none -- the walk [`slot_ops`] makes.
fn slot_count(parts: &[AggPart]) -> usize {
    parts.iter().fold(0usize, |cursor, part| match part {
        AggPart::Avg(_, count_i) => count_i.saturating_add(1),
        AggPart::Expr(..) => cursor,
        _ => cursor.saturating_add(1),
    })
}

/// Resolves one [`AggOperand`] against a slot accessor -- shared by
/// [`output_columns`] (`slot` indexes a merged accumulator column) and
/// `engine::finalize_row` (`slot` indexes a merged row) so the two
/// `AggPart::Expr` evaluations (#496) cannot drift, the same discipline
/// [`merge_slot`] already keeps for the rest of `Combine`.
pub(crate) fn eval_agg_operand(
    op: &AggOperand,
    slot: impl Fn(usize) -> Result<Value>,
) -> Result<Value> {
    Ok(match op {
        AggOperand::Slot(i) => slot(*i)?,
        AggOperand::Avg(sum_i, count_i) => finish_avg(&slot(*sum_i)?, &slot(*count_i)?)?,
        AggOperand::Literal(f) => Value::Float(*f),
    })
}

/// Resolves one [`AggPart::Expr`] against a slot accessor, per
/// [`eval_agg_operand`].
pub(crate) fn eval_agg_expr(
    map_op: MapOp,
    lhs: &AggOperand,
    rhs: &AggOperand,
    slot: impl Fn(usize) -> Result<Value>,
) -> Result<Value> {
    let a = eval_agg_operand(lhs, &slot)?;
    let b = eval_agg_operand(rhs, &slot)?;
    apply_map_op(map_op, &a, &b)
}

/// Merges one partial slot `from` into the accumulated slot `into` per
/// `op` -- the single definition of `Combine`'s merge semantics.
pub(crate) fn merge_slot(op: SlotOp, into: &mut Value, from: &Value) -> Result<()> {
    match op {
        SlotOp::Key => {}
        SlotOp::Sum => *into = merge_sum_partials(into, from)?,
        // `AVG`'s sum is a float (#631): it never fails on overflow.
        SlotOp::AvgSum => {
            if !(matches!(into, Value::Null) && matches!(from, Value::Null)) {
                *into = Value::Float(partial_f64(into)? + partial_f64(from)?);
            }
        }
        // A merged COUNT stays an integer, as a single segment's does
        // (#272): before, it came back as `Float`, so the *type* of
        // `COUNT(*)` depended on how many segments the scan had.
        SlotOp::Count => {
            let total = partial_i64(into)?
                .checked_add(partial_i64(from)?)
                .ok_or_else(|| VmError::MalformedProgram {
                    opcode: "Combine",
                    reason: "partial COUNT overflowed i64".to_string(),
                })?;
            *into = Value::Int(total);
        }
        // MIN/MAX keep their type and compare as one segment does (#631):
        // a segment with no rows contributes `Null`, which never wins.
        SlotOp::Min => keep_partial_extreme(into, from, std::cmp::Ordering::Less),
        SlotOp::Max => keep_partial_extreme(into, from, std::cmp::Ordering::Greater),
        // A register-wise `max`: the same sketch whatever the merge order
        // (#603). `Null` is a segment that saw no rows.
        SlotOp::Sketch => {
            if let Some(from) = decode_sketch(from)? {
                let mut merged = decode_sketch(into)?.unwrap_or_default();
                merged.merge(&from);
                *into = Value::Bytes(merged.encode().into());
            }
        }
        // The union of two sorted sets, still sorted (#817). `Null` is a
        // segment that saw no rows.
        SlotOp::IntSet => {
            if !matches!(from, Value::Null) {
                *into = Value::Bytes(union_int_sets(set_bytes(into)?, set_bytes(from)?)?.into());
            }
        }
        // The same union for strings (#820).
        SlotOp::StrSet => {
            if !matches!(from, Value::Null) {
                *into = Value::Bytes(union_str_sets(str_set(into)?, str_set(from)?).into());
            }
        }
        // Previously a no-op (db-core#404): with more than one segment,
        // `AVG` silently returned the first segment's local average. The
        // per-segment count is a `Value::Int` in real execution but is
        // only ever divided into by the finalizer, so it is not routed
        // through `Count`'s overflow-checked path.
        SlotOp::AvgCount => *into = Value::Float(partial_f64(into)? + partial_f64(from)?),
    }
    Ok(())
}

/// An `approx_count_distinct` partial slot as a sketch; `None` for `Null`
/// (a segment with no rows). Anything else is a planner bug.
fn decode_sketch(v: &Value) -> Result<Option<Hll>> {
    match v {
        Value::Null => Ok(None),
        Value::Bytes(bytes) => {
            Hll::decode(bytes)
                .map(Some)
                .ok_or_else(|| VmError::MalformedProgram {
                    opcode: "Combine",
                    reason: "a partial approx_count_distinct is not a valid sketch".to_string(),
                })
        }
        other => Err(VmError::MalformedProgram {
            opcode: "Combine",
            reason: format!("a partial approx_count_distinct is not a sketch: {other}"),
        }),
    }
}

/// An exact distinct count's partial slot as its bytes; `Null` (a segment
/// with no rows) is the empty set, anything else but a whole number of
/// 8-byte values is a planner bug.
fn set_bytes(v: &Value) -> Result<&[u8]> {
    match v {
        Value::Null => Ok(&[]),
        Value::Bytes(bytes) if bytes.len().is_multiple_of(8) => Ok(bytes),
        other => Err(VmError::MalformedProgram {
            opcode: "Combine",
            reason: format!("a partial exact COUNT(DISTINCT) is not a set of integers: {other}"),
        }),
    }
}

/// A string distinct count's partial slot as its strings (see
/// [`encode_str_set`](crate::vm::batch::encode_str_set)); `Null` is the
/// empty set, anything torn is a planner bug.
fn str_set(v: &Value) -> Result<Vec<&[u8]>> {
    let torn = || VmError::MalformedProgram {
        opcode: "Combine",
        reason: format!("a partial string COUNT(DISTINCT) is not a set of strings: {v}"),
    };
    let mut rest: &[u8] = match v {
        Value::Null => return Ok(Vec::new()),
        Value::Bytes(bytes) => bytes,
        _ => return Err(torn()),
    };
    let mut out = Vec::new();
    while !rest.is_empty() {
        let (len, tail) = rest.split_first_chunk::<4>().ok_or_else(torn)?;
        let len = u32::from_le_bytes(*len) as usize;
        let (item, tail) = tail.split_at_checked(len).ok_or_else(torn)?;
        out.push(item);
        rest = tail;
    }
    Ok(out)
}

/// The union of two repeat-free string sets, in the same form: all of `a`,
/// then what `b` adds.
fn union_str_sets(a: Vec<&[u8]>, b: Vec<&[u8]>) -> Vec<u8> {
    let seen: std::collections::HashSet<&[u8]> = a.iter().copied().collect();
    let mut out = Vec::new();
    for item in a
        .iter()
        .copied()
        .chain(b.into_iter().filter(|i| !seen.contains(i)))
    {
        out.extend_from_slice(&u32::try_from(item.len()).unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(item);
    }
    out
}

/// The union of two sorted, repeat-free sets of 8-byte little-endian
/// integers, in the same form.
fn union_int_sets(a: &[u8], b: &[u8]) -> Result<Vec<u8>> {
    let read = |bytes: &[u8]| -> Vec<i64> {
        bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| i64::from_le_bytes(*c))
            .collect()
    };
    let (a, b) = (read(a), read(b));
    let mut out = Vec::with_capacity(a.len().saturating_add(b.len()).saturating_mul(8));
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() || j < b.len() {
        let next = match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) if x < y => {
                i = i.saturating_add(1);
                *x
            }
            (Some(x), Some(y)) if x > y => {
                j = j.saturating_add(1);
                *y
            }
            (Some(x), Some(_)) => {
                i = i.saturating_add(1);
                j = j.saturating_add(1);
                *x
            }
            (Some(x), None) => {
                i = i.saturating_add(1);
                *x
            }
            (None, Some(y)) => {
                j = j.saturating_add(1);
                *y
            }
            (None, None) => break,
        };
        out.extend_from_slice(&next.to_le_bytes());
    }
    Ok(out)
}

/// A merged exact `COUNT(DISTINCT <integer>)` slot as its count; no rows is
/// `0`.
pub(crate) fn finish_int_set(v: &Value) -> Result<Value> {
    let bytes = set_bytes(v)?;
    Ok(Value::Int(
        i64::try_from(bytes.len() / 8).unwrap_or(i64::MAX),
    ))
}

/// A merged exact `COUNT(DISTINCT <string>)` slot as its count (#820).
pub(crate) fn finish_str_set(v: &Value) -> Result<Value> {
    Ok(Value::Int(
        i64::try_from(str_set(v)?.len()).unwrap_or(i64::MAX),
    ))
}

/// A merged `approx_count_distinct` slot as its estimate; no rows is `0`.
pub(crate) fn finish_sketch(v: &Value) -> Result<Value> {
    Ok(Value::Int(
        decode_sketch(v)?.map_or(0, |sketch| sketch.estimate()),
    ))
}

/// Replaces `into` with `from` when `from` is further in direction `want`
/// ([`agg_cmp`]); a `Null` partial (a segment with no rows) never wins.
fn keep_partial_extreme(into: &mut Value, from: &Value, want: std::cmp::Ordering) {
    if matches!(from, Value::Null) {
        return;
    }
    if matches!(into, Value::Null) || agg_cmp(from, into) == want {
        *into = from.clone();
    }
}

/// A partial COUNT slot as an integer. NULL is the additive identity (a
/// segment that saw no rows); anything else non-integer is a planner bug.
pub(crate) fn partial_i64(v: &Value) -> Result<i64> {
    match v {
        Value::Null => Ok(0),
        Value::Int(n) => Ok(*n),
        other => Err(VmError::MalformedProgram {
            opcode: "Combine",
            reason: format!("partial COUNT slot holds {other:?}, not an integer"),
        }),
    }
}

/// A partial SUM/COUNT/AVG slot as a number. NULL is the additive identity
/// (a segment that saw no rows); anything else non-numeric is a planner
/// bug -- before, it silently merged as `0.0` into a plausible wrong total
/// (db-core#232).
pub(crate) fn partial_f64(v: &Value) -> Result<f64> {
    match v {
        Value::Null => Ok(0.0),
        other => other.as_f64().ok_or_else(|| VmError::MalformedProgram {
            opcode: "Combine",
            reason: format!("partial aggregate slot holds {other:?}, not a number"),
        }),
    }
}

/// Merges two partial `SUM`s. A segment with no surviving rows emits
/// `Null` (#452: `Reduce` always emits one row), and `Null` must be the
/// identity here -- `Null` with `Null` stays `Null`, so a `SUM` over zero
/// rows across every segment is `NULL` as SQL requires, not `0`; `Null`
/// with a number is that number. Two integers add exactly, an error past
/// 64 bits as in one segment (#631); a float on either side makes a float.
pub(crate) fn merge_sum_partials(into: &Value, from: &Value) -> Result<Value> {
    match (into, from) {
        (Value::Null, Value::Null) => Ok(Value::Null),
        (Value::Int(a), Value::Int(b)) => a
            .checked_add(*b)
            .map(Value::Int)
            .ok_or(VmError::IntegerOverflow { opcode: "Combine" }),
        (Value::Null, Value::Int(n)) | (Value::Int(n), Value::Null) => Ok(Value::Int(*n)),
        // Decimals (and an integer at scale 0) add exactly (#632).
        (Value::Null, d @ Value::Decimal { .. }) | (d @ Value::Decimal { .. }, Value::Null) => {
            Ok(d.clone())
        }
        (Value::Decimal { .. }, Value::Decimal { .. } | Value::Int(_))
        | (Value::Int(_), Value::Decimal { .. }) => add_decimals(into, from),
        _ => Ok(Value::Float(partial_f64(into)? + partial_f64(from)?)),
    }
}

/// Two exact partials, a decimal and a decimal or an integer, added at the
/// larger scale; an error past `i128` or DECIMAL(38) (#632).
fn add_decimals(a: &Value, b: &Value) -> Result<Value> {
    let parts = |v: &Value| match v {
        Value::Decimal { unscaled, scale } => (*unscaled, *scale),
        Value::Int(i) => (i128::from(*i), 0),
        _ => (0, 0),
    };
    let ((x, sx), (y, sy)) = (parts(a), parts(b));
    let scale = sx.max(sy);
    let overflow = || VmError::IntegerOverflow { opcode: "Combine" };
    let at = |v: i128, s: u8| {
        crate::vm::batch::pow10(u32::from(scale.abs_diff(s)))
            .and_then(|p| v.checked_mul(p))
            .ok_or_else(overflow)
    };
    let unscaled = at(x, sx)?.checked_add(at(y, sy)?).ok_or_else(overflow)?;
    if unscaled.unsigned_abs() >= 10_u128.pow(38) {
        return Err(overflow());
    }
    Ok(Value::Decimal { unscaled, scale })
}

/// `AVG`'s final value from its merged `(sum, count)` slots: `NULL` over
/// zero rows, `sum / count` otherwise.
pub(crate) fn finish_avg(sum: &Value, count: &Value) -> Result<Value> {
    let (sum, count) = (partial_f64(sum)?, partial_f64(count)?);
    Ok(if count == 0.0 {
        Value::Null
    } else {
        Value::Float(sum / count)
    })
}

/// Marks a row that *created* its group in the current chunk: its values
/// were copied in as the group's initial state, so the merge pass must
/// skip it rather than merge the row into itself (which would, e.g., turn
/// a lone `Int(5)` into `Float(10.0)`).
const NEW_GROUP: usize = usize::MAX;

/// The position a partial row was first seen at, as one sortable key:
/// `(segment index, row within the segment)`. Comparing these
/// lexicographically is comparing global row order, since segments are
/// contiguous -- so groups sorted by their smallest key come out in the
/// same first-seen order `finalize` produces, whatever order the worker
/// pool actually processed the segments in (#488).
pub(crate) fn order_key(segment: usize, row: usize) -> u64 {
    let segment = u64::try_from(segment).unwrap_or(u64::MAX);
    let row = u64::try_from(row).unwrap_or(u64::MAX);
    (segment << 32) | (row & 0xFFFF_FFFF)
}

/// Merges every chunk's partial rows into one row per group, column by
/// column, and finalizes (`AVG` = sum / count). Output columns follow
/// `parts` in order, exactly as `finalize` emits them; groups follow
/// first-seen order across `chunks`. Chunks must all have the same width
/// and every column of a chunk the same length -- a ragged chunk or a
/// `num_group_keys` wider than the row is a planner bug, reported as
/// [`VmError::MalformedProgram`] rather than indexed.
///
/// One [`Combiner`] fed every chunk in order; see there for the key-type
/// specialization. `run()` uses this direct form when there are too few
/// segments per worker for pre-aggregation to pay (#488, see
/// `PREAGGREGATE_MIN_SEGMENTS_PER_THREAD`), and the tests use it as the
/// reference [`combine_partials`] must agree with.
pub(crate) fn combine_chunks(
    parts: &[AggPart],
    num_group_keys: usize,
    chunks: &[Chunk],
) -> Result<QueryOutput> {
    let mut combiner = Combiner::new(parts, num_group_keys);
    for (segment, chunk) in chunks.iter().enumerate() {
        combiner.push_chunk(chunk, |row| order_key(segment, row))?;
    }
    combiner.finish()
}

/// Merges the per-worker partials of [`Combiner::finish_partial`] (#488)
/// into the final result: the same merge again -- partials carry the raw
/// accumulator slots, so `SUM`s add, `COUNT`s add, `AVG` pairs add -- with
/// each group's smallest first-seen key carried through, so the output is
/// in global first-seen order however the pool interleaved the segments.
pub(crate) fn combine_partials(
    parts: &[AggPart],
    num_group_keys: usize,
    partials: Vec<Partial>,
) -> Result<QueryOutput> {
    let mut combiner = Combiner::new(parts, num_group_keys);
    // Sized once, so the accumulators rarely double-and-copy while the
    // partials are still live (#643). The groups are at least the largest
    // partial and at most the sum of them; workers sharing keys make the
    // sum an overestimate up to `threads` times, and a key table sized
    // for it is written in full, so take the middle.
    let sizes = partials.iter().map(|partial| partial.first_seen.len());
    let largest = sizes.clone().max().unwrap_or(0);
    let total: usize = sizes.sum();
    combiner.expect_groups(largest.max(total / 2));
    // By value: each worker's partial is dropped as soon as it is merged,
    // not held until the last one is (#643).
    for partial in partials {
        combiner.push_partial(&partial)?;
    }
    combiner.finish()
}

/// A [`Combiner`]'s still-mergeable state, [`Combiner::finish_partial`]:
/// one accumulator column per slot, typed keys kept typed (#643), and
/// each group's first-seen key for [`combine_partials`] to order by. No
/// columns when no chunk was ever pushed.
#[derive(Debug, Default)]
pub(crate) struct Partial {
    columns: Vec<AccColumn>,
    first_seen: Vec<u64>,
}

impl Partial {
    /// The groups, as one chunk of the input's width and slot layout.
    pub(crate) fn into_chunk(self) -> Chunk {
        self.columns
            .into_iter()
            .map(|column| Arc::new(column.into_values()))
            .collect()
    }

    /// How many groups it holds.
    pub(crate) fn len(&self) -> usize {
        self.columns.first().map_or(0, AccColumn::len)
    }
}

/// The key hash of group `g` -- what [`hash_columns_by_row`] gives the
/// same key as a row of `Value`s.
fn acc_key_hash(key_columns: &[AccColumn], g: usize) -> u64 {
    key_columns
        .iter()
        .fold(HASH_SEED, |acc, column| mix_hash(acc, column.hash(g)))
}

/// Every chunk must have `num_slots` columns of one common length.
fn validate_chunk(chunk: &Chunk, num_slots: usize) -> Result<usize> {
    if chunk.len() != num_slots {
        return Err(VmError::MalformedProgram {
            opcode: "Combine",
            reason: format!("chunk has {} columns, expected {num_slots}", chunk.len()),
        });
    }
    let n = chunk_len(chunk);
    if let Some(ragged) = chunk.iter().find(|column| column.len() != n) {
        return Err(VmError::MalformedProgram {
            opcode: "Combine",
            reason: format!(
                "ragged chunk: a column has {} rows, expected {n}",
                ragged.len()
            ),
        });
    }
    Ok(n)
}

/// Whether a key column is all `Int` (a `Null` key is fine: it is its own
/// group, [`IntKeyTable::null_group`]), so the integer table applies.
fn is_int_key_column(column: &[Value]) -> bool {
    column
        .iter()
        .all(|v| matches!(v, Value::Int(_) | Value::Null))
}

/// Phase B of a chunk: merge each aggregate column into its accumulator,
/// one op dispatch per column-chunk. Rows marked [`NEW_GROUP`] were copied
/// in as their group's initial state.
#[allow(
    clippy::indexing_slicing,
    reason = "`chunk[c]`/`acc[c]` index `< num_slots` (validated), `column[r]` indexes a column of exactly `row_group.len()` rows, and every non-sentinel `g` was pushed into `acc` before being recorded"
)]
fn merge_columns(
    ops: &[SlotOp],
    chunk: &Chunk,
    acc: &mut [AccColumn],
    row_group: &[usize],
) -> Result<()> {
    for (c, op) in ops.iter().enumerate() {
        if *op == SlotOp::Key {
            continue;
        }
        let column = &chunk[c];
        let acc_column = &mut acc[c];
        for (r, &g) in row_group.iter().enumerate() {
            if g != NEW_GROUP {
                merge_entry(*op, acc_column, g, &column[r])?;
            }
        }
    }
    Ok(())
}

/// [`merge_slot`] of `from` into group `g` of `column`; a typed `COUNT`
/// column adds in place, with `merge_slot`'s semantics and error.
fn merge_entry(op: SlotOp, column: &mut AccColumn, g: usize, from: &Value) -> Result<()> {
    if op == SlotOp::Count && matches!(column, AccColumn::Ints { .. }) {
        return column
            .add_int(g, partial_i64(from)?)
            .ok_or_else(|| VmError::MalformedProgram {
                opcode: "Combine",
                reason: "partial COUNT overflowed i64".to_string(),
            });
    }
    merge_slot(op, aggregate_slot(column, g)?, from)
}

/// Group `g`'s entry in an aggregate column, which is never a typed key
/// column.
fn aggregate_slot(column: &mut AccColumn, g: usize) -> Result<&mut Value> {
    column
        .value_mut(g)
        .ok_or_else(|| VmError::MalformedProgram {
            opcode: "Combine",
            reason: format!("aggregate slot of group {g} is missing or a typed key column"),
        })
}

/// Copies row `r` of `chunk` in as a new group's initial state.
#[allow(
    clippy::indexing_slicing,
    reason = "`column[r]`: `r < chunk_len(chunk)` and every column has that many rows (validated)"
)]
fn push_new_group(acc: &mut [AccColumn], chunk: &Chunk, r: usize) {
    for (slot, column) in acc.iter_mut().zip(chunk) {
        slot.push_value(&column[r]);
    }
}

/// How a [`Combiner`] maps a key to its group: decided from the first
/// chunk it sees, the way ClickHouse's `chooseMethod` and DataFusion's
/// `GroupValuesPrimitive` pick a specialization from the schema -- a
/// column is one type. A later chunk that contradicts that (only
/// possible with hand-built in-memory batches) demotes the table to the
/// generic index in place, keeping every group already seen.
enum KeyIndex {
    /// No chunk pushed yet.
    Undecided,
    /// A single `Int`/`Null` key: [`IntKeyTable`].
    Int(IntKeyTable),
    /// Any key shape: hash-then-verify buckets of group ids, exact
    /// `Value` equality against the key columns of `acc` on a hit (#439).
    Generic(GenericIndex),
}

/// Group ids by key hash: the first group with a hash inline, any others
/// (a collision, rare) in a side table -- one map entry per group instead
/// of a `Vec` allocated per group (#643). Slotted by [`SlotState`]'s
/// Murmur mix of the already-mixed key hash, not SipHash (#751, as #514).
#[derive(Default)]
struct GenericIndex {
    first: HashMap<u64, usize, SlotState>,
    more: HashMap<u64, Vec<usize>, SlotState>,
}

impl GenericIndex {
    fn with_capacity(groups: usize) -> Self {
        GenericIndex {
            first: HashMap::with_capacity_and_hasher(groups, SlotState),
            more: HashMap::default(),
        }
    }

    /// The groups whose key hashes to `hash`.
    fn candidates(&self, hash: u64) -> impl Iterator<Item = usize> + '_ {
        self.first
            .get(&hash)
            .copied()
            .into_iter()
            .chain(self.more.get(&hash).into_iter().flatten().copied())
    }

    fn insert(&mut self, hash: u64, group: usize) {
        match self.first.entry(hash) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(group);
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                self.more.entry(hash).or_default().push(group);
            }
        }
    }
}

/// An incremental `Combine`: chunks of partial `GROUP BY` rows pushed one
/// at a time, one row per group accumulated column-major (#478), then
/// either finalized ([`Self::finish`]) or handed on as a still-mergeable
/// partial ([`Self::finish_partial`]) -- which is how each worker of the
/// pool pre-aggregates every segment it processes before the final merge
/// sees anything (#488: `threads x groups` partial rows instead of
/// `segments x groups`).
pub(crate) struct Combiner {
    parts: Vec<AggPart>,
    num_group_keys: usize,
    /// Resolved from `parts` at the first chunk (needs the row width).
    ops: Vec<SlotOp>,
    /// Set by the first chunk; `None` until then.
    num_slots: Option<usize>,
    /// Column-major accumulators, one entry per group, in first-seen
    /// order; key columns typed where they can be (#643).
    acc: Vec<AccColumn>,
    index: KeyIndex,
    /// Each group's smallest [`order_key`] -- see there.
    first_seen: Vec<u64>,
    /// Scratch: this chunk's row -> group id (or [`NEW_GROUP`]).
    row_group: Vec<usize>,
    /// How many groups to size for at the first chunk, when known.
    expected_groups: Option<usize>,
}

impl Combiner {
    pub(crate) fn new(parts: &[AggPart], num_group_keys: usize) -> Self {
        Combiner {
            parts: parts.to_vec(),
            num_group_keys,
            ops: Vec::new(),
            num_slots: None,
            acc: Vec::new(),
            index: KeyIndex::Undecided,
            first_seen: Vec::new(),
            row_group: Vec::new(),
            expected_groups: None,
        }
    }

    /// Merges one chunk of partial rows in; `order` gives each row's
    /// first-seen key ([`order_key`] for a segment's own output, the
    /// carried key for a partial).
    pub(crate) fn push_chunk(&mut self, chunk: &Chunk, order: impl Fn(usize) -> u64) -> Result<()> {
        let num_slots = match self.num_slots {
            Some(n) => n,
            None => self.start(chunk)?,
        };
        let n = validate_chunk(chunk, num_slots)?;
        self.row_group.clear();
        self.row_group.reserve(n);
        let resume_at = match &mut self.index {
            KeyIndex::Int(_) => self.map_rows_int(chunk, n, &order)?,
            KeyIndex::Generic(_) | KeyIndex::Undecided => Some(0),
        };
        if let Some(start) = resume_at {
            if !matches!(self.index, KeyIndex::Generic(_)) {
                self.demote_to_generic();
            }
            self.map_rows_generic(chunk, start, n, &order)?;
        }
        merge_columns(&self.ops, chunk, &mut self.acc, &self.row_group)
    }

    /// Sizes the accumulators for `groups` groups up front, for a merge
    /// that knows its bound (`combine_partials`).
    fn expect_groups(&mut self, groups: usize) {
        self.expected_groups = Some(groups);
    }

    /// First chunk: fixes the row width, resolves the slot ops, sizes the
    /// accumulators from its row count (one partial row per group per
    /// segment makes that the group count, or close) and picks the key
    /// index from its key column.
    fn start(&mut self, chunk: &Chunk) -> Result<usize> {
        let num_slots = chunk.len();
        if self.num_group_keys > num_slots {
            return Err(VmError::MalformedProgram {
                opcode: "Combine",
                reason: format!(
                    "{} group keys but the emitted row has {num_slots} columns",
                    self.num_group_keys
                ),
            });
        }
        self.ops = slot_ops(&self.parts, num_slots)?;
        let expected_groups = self.expected_groups.unwrap_or_else(|| chunk_len(chunk));
        self.acc = chunk
            .iter()
            .enumerate()
            .map(|(c, column)| {
                let count = self.ops.get(c) == Some(&SlotOp::Count);
                AccColumn::for_values(c < self.num_group_keys, count, column, expected_groups)
            })
            .collect();
        let int_key =
            self.num_group_keys == 1 && chunk.first().is_some_and(|keys| is_int_key_column(keys));
        self.start_index(int_key, expected_groups);
        self.num_slots = Some(num_slots);
        Ok(num_slots)
    }

    /// [`Self::start`] for a first input that is a partial: its columns'
    /// kinds carry over.
    fn start_partial(&mut self, partial: &Partial) -> Result<usize> {
        let num_slots = partial.columns.len();
        if self.num_group_keys > num_slots {
            return Err(VmError::MalformedProgram {
                opcode: "Combine",
                reason: format!(
                    "{} group keys but the partial has {num_slots} columns",
                    self.num_group_keys
                ),
            });
        }
        self.ops = slot_ops(&self.parts, num_slots)?;
        let expected_groups = self.expected_groups.unwrap_or_else(|| partial.len());
        self.acc = partial
            .columns
            .iter()
            .map(|column| AccColumn::like(column, expected_groups))
            .collect();
        let int_key = self.num_group_keys == 1
            && matches!(partial.columns.first(), Some(AccColumn::Ints { .. }));
        self.start_index(int_key, expected_groups);
        self.num_slots = Some(num_slots);
        Ok(num_slots)
    }

    /// The key index and first-seen keys, sized for `expected_groups`.
    fn start_index(&mut self, int_key: bool, expected_groups: usize) {
        self.first_seen = Vec::with_capacity(expected_groups);
        self.index = if int_key {
            KeyIndex::Int(IntKeyTable::with_capacity(expected_groups))
        } else {
            KeyIndex::Generic(GenericIndex::with_capacity(expected_groups))
        };
    }

    /// Rows -> groups through the integer table. `Ok(None)` when every
    /// row was mapped; `Ok(Some(r))` when row `r`'s key is not an
    /// `Int`/`Null` after all, with rows `..r` already mapped -- the
    /// caller demotes to the generic index and continues from `r`.
    #[allow(
        clippy::indexing_slicing,
        reason = "`chunk[0]`: `num_slots >= 1` since `num_group_keys == 1 <= num_slots`; `[r]`: `r < n`, every column has `n` rows (validated); `first_seen[g]`: `g` was pushed before being recorded"
    )]
    fn map_rows_int(
        &mut self,
        chunk: &Chunk,
        n: usize,
        order: &impl Fn(usize) -> u64,
    ) -> Result<Option<usize>> {
        let KeyIndex::Int(table) = &mut self.index else {
            return Ok(Some(0));
        };
        let keys = &chunk[0];
        for r in 0..n {
            let (g, inserted) = match &keys[r] {
                Value::Int(key) => table.get_or_insert(*key, self.first_seen.len())?,
                Value::Null => table.null_group_or_insert(self.first_seen.len()),
                _ => return Ok(Some(r)),
            };
            if inserted {
                push_new_group(&mut self.acc, chunk, r);
                self.first_seen.push(order(r));
                self.row_group.push(NEW_GROUP);
            } else {
                self.first_seen[g] = self.first_seen[g].min(order(r));
                self.row_group.push(g);
            }
        }
        Ok(None)
    }

    /// Rebuilds the key index as the generic hash-then-verify map over
    /// the groups already accumulated (their keys are `acc`'s key
    /// columns), so a chunk with an unexpected key type continues
    /// without losing anything.
    fn demote_to_generic(&mut self) {
        let num_groups = self.first_seen.len();
        let key_columns = self.acc.get(..self.num_group_keys).unwrap_or(&[]);
        let mut map = GenericIndex::with_capacity(num_groups);
        for g in 0..num_groups {
            map.insert(acc_key_hash(key_columns, g), g);
        }
        self.index = KeyIndex::Generic(map);
    }

    /// Rows `start..n` -> groups through the generic index.
    #[allow(
        clippy::indexing_slicing,
        reason = "`chunk[..num_group_keys]`: `num_group_keys <= num_slots` (checked in `start`); `acc[c][g]`: `c < num_group_keys` and `g` was pushed before being recorded; `column[r]`/`hashes[r]`: `r < n` (validated)"
    )]
    fn map_rows_generic(
        &mut self,
        chunk: &Chunk,
        start: usize,
        n: usize,
        order: &impl Fn(usize) -> u64,
    ) -> Result<()> {
        let KeyIndex::Generic(map) = &mut self.index else {
            return Err(VmError::MalformedProgram {
                opcode: "Combine",
                reason: "key index not initialized".to_string(),
            });
        };
        let key_columns: Vec<&[Value]> = chunk[..self.num_group_keys]
            .iter()
            .map(|column| column.as_slice())
            .collect();
        let hashes = hash_columns_by_row(&key_columns, n, |row| row);
        for r in start..n {
            let existing = map.candidates(hashes[r]).find(|&g| {
                key_columns
                    .iter()
                    .enumerate()
                    .all(|(c, column)| self.acc[c].eq_value(g, &column[r]))
            });
            match existing {
                Some(g) => {
                    self.first_seen[g] = self.first_seen[g].min(order(r));
                    self.row_group.push(g);
                }
                None => {
                    map.insert(hashes[r], self.first_seen.len());
                    push_new_group(&mut self.acc, chunk, r);
                    self.first_seen.push(order(r));
                    self.row_group.push(NEW_GROUP);
                }
            }
        }
        Ok(())
    }

    /// Several `COUNT(DISTINCT x_i)` (#807). Each `(keys, x_1 .. x_n)` group
    /// is merged, so grouping by `(keys, x_i)` alone and counting its
    /// non-`NULL` `x_i` gives distinct `x_i` per key -- one regrouping per
    /// distinct, each by [`Self::finish_distinct`]. A pass moves its `x_i`
    /// next to the keys and leaves the other `x_j` as `MAX` slots, which
    /// keeps every other slot where the parts expect it; its output is
    /// `[keys, d_i, (n - 1 unused), aggregates]`. All passes merge the same
    /// rows, so their groups come out in the same order; the result takes
    /// `d_i` from pass `i` and everything else from the first.
    fn finish_distinct_many(self, distinct: usize) -> Result<QueryOutput> {
        let malformed = |reason: &str| VmError::MalformedProgram {
            opcode: "Combine",
            reason: reason.to_string(),
        };
        let keys = self.num_group_keys.saturating_sub(distinct);
        let block = keys..keys.saturating_add(distinct);
        let Combiner {
            parts,
            acc,
            first_seen,
            num_slots,
            ..
        } = self;
        if parts
            .get(block.clone())
            .is_none_or(|p| p.iter().any(|p| *p != AggPart::DistinctKey))
        {
            return Err(malformed(
                "COUNT(DISTINCT) keys do not follow the group keys",
            ));
        }
        let mut passes: Vec<Vec<Vec<Value>>> = Vec::with_capacity(distinct);
        for i in 0..distinct {
            let mut pass_parts: Vec<AggPart> = parts.get(..keys).unwrap_or_default().to_vec();
            pass_parts.push(AggPart::DistinctKey);
            pass_parts.extend((1..distinct).map(|_| AggPart::Max));
            pass_parts.extend(parts.get(block.end..).unwrap_or_default());
            let mut merge = Combiner::new(&pass_parts, keys.saturating_add(1));
            if num_slots.is_some() {
                let own = keys.saturating_add(i);
                let others = block.clone().filter(|c| *c != own);
                let order = (0..keys)
                    .chain(std::iter::once(own))
                    .chain(others)
                    .chain(block.end..acc.len());
                let columns = order
                    .enumerate()
                    .map(|(at, c)| {
                        let column = acc.get(c).cloned().unwrap_or(AccColumn::Values(Vec::new()));
                        // The unused slots merge as `MAX`, never as keys.
                        if at > keys {
                            AccColumn::Values(column.into_values())
                        } else {
                            column
                        }
                    })
                    .collect();
                merge.push_partial(&Partial {
                    columns,
                    first_seen: first_seen.clone(),
                })?;
            }
            passes.push(merge.finish_distinct()?.into_columns());
        }
        let Some(first) = passes.first() else {
            return Ok(QueryOutput::default());
        };
        let rows = first.first().map_or(0, Vec::len);
        let width = first.len();
        if passes
            .iter()
            .any(|p| p.len() != width || p.iter().any(|c| c.len() != rows))
        {
            return Err(malformed("COUNT(DISTINCT) passes disagree on the groups"));
        }
        let mut out: Vec<Vec<Value>> = Vec::with_capacity(width);
        for (c, column) in first.iter().enumerate() {
            if c < keys || c >= block.end {
                out.push(column.clone());
            } else if c == keys {
                out.extend(passes.iter().filter_map(|p| p.get(keys).cloned()));
            }
        }
        Ok(QueryOutput::new(out))
    }

    /// The finalized result (`AVG` divided, `parts` order), groups in
    /// first-seen order.
    pub(crate) fn finish(self) -> Result<QueryOutput> {
        if self.parts.contains(&AggPart::DistinctKey) {
            return self.finish_distinct();
        }
        if self.num_slots.is_none() {
            return Ok(QueryOutput::default());
        }
        let acc = reorder_by_first_seen(self.acc, &self.first_seen);
        let acc = acc.into_iter().map(AccColumn::into_values).collect();
        Ok(QueryOutput::new(output_columns(&self.parts, acc)?))
    }

    /// `COUNT(DISTINCT x)`'s second grouping (#633). Every partial is
    /// merged, so each `(keys, x)` group is one distinct `x`: its slot
    /// becomes a partial count (1, or 0 for `x` NULL), and a second
    /// `Combiner` groups by the real keys alone -- the `DistinctKey` part
    /// a plain `Count` there, every other part merged as before. Without
    /// `GROUP BY` the result is one row even over no rows, as for any
    /// other aggregate.
    fn finish_distinct(self) -> Result<QueryOutput> {
        let distinct = self
            .parts
            .iter()
            .filter(|p| **p == AggPart::DistinctKey)
            .count();
        if distinct > 1 {
            return self.finish_distinct_many(distinct);
        }
        let keys = self.num_group_keys.saturating_sub(1);
        let parts: Vec<AggPart> = self
            .parts
            .iter()
            .map(|p| match p {
                AggPart::DistinctKey => AggPart::Count,
                other => *other,
            })
            .collect();
        let mut regroup = Combiner::new(&parts, keys);
        // Only the accumulators and first-seen keys go on: the pair
        // index is freed before the regroup builds its own (#643).
        let Combiner {
            acc: mut columns,
            first_seen,
            num_slots,
            ..
        } = self;
        if num_slots.is_some() {
            if let Some(column) = columns.get_mut(keys) {
                let n = column.len();
                let data = (0..n)
                    .map(|g| i64::from(!column.eq_value(g, &Value::Null)))
                    .collect();
                *column = AccColumn::Ints {
                    data,
                    nulls: vec![false; n],
                };
            }
            regroup.push_partial(&Partial {
                columns,
                first_seen,
            })?;
        }
        if keys > 0 || regroup.num_slots.is_some() {
            return regroup.finish();
        }
        let ops = slot_ops(&parts, slot_count(&parts))?;
        let empty_row: Vec<Vec<Value>> = ops
            .iter()
            .map(|op| {
                vec![match op {
                    SlotOp::Count | SlotOp::AvgCount => Value::Int(0),
                    _ => Value::Null,
                }]
            })
            .collect();
        Ok(QueryOutput::new(output_columns(&parts, empty_row)?))
    }

    /// The still-mergeable state: the raw accumulator slots as one chunk
    /// (same width and slot layout as the input, so a later [`Combiner`]
    /// merges it like any other partial) plus each group's first-seen key
    /// for [`combine_partials`] to order by. Empty (no columns) when no
    /// chunk was ever pushed.
    pub(crate) fn finish_partial(self) -> Partial {
        Partial {
            columns: self.acc,
            first_seen: self.first_seen,
        }
    }

    /// Merges a partial in ([`combine_partials`], `COUNT(DISTINCT)`'s
    /// regrouping): as [`Self::push_chunk`], but key to key -- typed
    /// columns compare and copy typed, so no `Value` is built per group.
    #[allow(
        clippy::indexing_slicing,
        reason = "`columns[c]`/`acc[c]`: `c < num_slots`, both have exactly `num_slots` columns (checked); `first_seen[g]`: `g` was pushed before being recorded"
    )]
    fn push_partial(&mut self, partial: &Partial) -> Result<()> {
        if partial.columns.is_empty() {
            return Ok(());
        }
        let num_slots = match self.num_slots {
            Some(n) => n,
            None => self.start_partial(partial)?,
        };
        let n = partial.len();
        if partial.columns.len() != num_slots
            || partial.columns.iter().any(|column| column.len() != n)
            || partial.first_seen.len() != n
        {
            return Err(VmError::MalformedProgram {
                opcode: "Combine",
                reason: format!(
                    "partial of {} columns and {} first-seen keys does not fit {num_slots} slots of {n} rows",
                    partial.columns.len(),
                    partial.first_seen.len()
                ),
            });
        }
        let keys = &partial.columns[..self.num_group_keys];
        for r in 0..n {
            let order = partial.first_seen[r];
            let found = match (&mut self.index, keys.first()) {
                (KeyIndex::Int(table), Some(AccColumn::Ints { data, nulls })) => {
                    Some(match (data.get(r), nulls.get(r)) {
                        (Some(&key), Some(false)) => {
                            table.get_or_insert(key, self.first_seen.len())?
                        }
                        _ => table.null_group_or_insert(self.first_seen.len()),
                    })
                }
                _ => None,
            };
            let (g, inserted) = match found {
                Some(found) => found,
                None => {
                    if !matches!(self.index, KeyIndex::Generic(_)) {
                        self.demote_to_generic();
                    }
                    let KeyIndex::Generic(map) = &mut self.index else {
                        return Err(VmError::MalformedProgram {
                            opcode: "Combine",
                            reason: "key index not initialized".to_string(),
                        });
                    };
                    let hash = acc_key_hash(keys, r);
                    let acc = &self.acc;
                    let existing = map.candidates(hash).find(|&g| {
                        keys.iter()
                            .enumerate()
                            .all(|(c, column)| acc[c].eq_entry(g, column, r))
                    });
                    match existing {
                        Some(g) => (g, false),
                        None => {
                            let g = self.first_seen.len();
                            map.insert(hash, g);
                            (g, true)
                        }
                    }
                }
            };
            if inserted {
                for (slot, column) in self.acc.iter_mut().zip(&partial.columns) {
                    slot.push_entry(column, r);
                }
                self.first_seen.push(order);
                continue;
            }
            self.first_seen[g] = self.first_seen[g].min(order);
            for (c, op) in self.ops.iter().enumerate() {
                if *op != SlotOp::Key {
                    merge_entry(*op, &mut self.acc[c], g, &partial.columns[c].value(r))?;
                }
            }
        }
        Ok(())
    }
}

/// Permutes `acc`'s groups into ascending `first_seen` order -- a no-op
/// (no copy) when they already are, which is every single-combiner case,
/// since chunks arrive in segment order.
#[allow(
    clippy::indexing_slicing,
    reason = "`perm` is a permutation of `0..num_groups` and every column of `acc` has exactly `num_groups` entries"
)]
fn reorder_by_first_seen(mut acc: Vec<AccColumn>, first_seen: &[u64]) -> Vec<AccColumn> {
    if first_seen.windows(2).all(|w| w[0] <= w[1]) {
        return acc;
    }
    // `perm[i]` is the group that lands at `i`. Applied in place by
    // following its cycles, so no column is copied (#643).
    let mut perm: Vec<usize> = (0..first_seen.len()).collect();
    perm.sort_unstable_by_key(|&g| first_seen[g]);
    let mut done = vec![false; perm.len()];
    for start in 0..perm.len() {
        if done[start] {
            continue;
        }
        let mut i = start;
        loop {
            done[i] = true;
            let next = perm[i];
            if next == start {
                break;
            }
            for column in &mut acc {
                column.swap(i, next);
            }
            i = next;
        }
    }
    acc
}

/// Lays `acc`'s merged slots out as the final output columns, following
/// `parts` in order with `finalize_row`'s cursor (#404): an `Avg` yields one
/// column computed from its two slots, everything else its own slot.
#[allow(
    clippy::indexing_slicing,
    reason = "`slot_ops` already verified every `Avg` index is `< num_slots`; the cursor stays in range for the same reason `finalize_row`'s does"
)]
fn output_columns(parts: &[AggPart], mut acc: Vec<Vec<Value>>) -> Result<Vec<Vec<Value>>> {
    let mut out = Vec::with_capacity(parts.len());
    let mut cursor = 0usize;
    // Taken up front: the key arm below moves its column out of `acc`, so
    // `acc[0]` is empty by the time an `Expr` part runs under GROUP BY.
    let len = acc.first().map_or(0, Vec::len);
    for part in parts {
        match part {
            AggPart::Avg(sum_i, count_i) => {
                let column = acc[*sum_i]
                    .iter()
                    .zip(acc[*count_i].iter())
                    .map(|(sum, count)| finish_avg(sum, count))
                    .collect::<Result<Vec<_>>>()?;
                out.push(column);
                cursor = count_i.saturating_add(1);
            }
            // Merged, but not itself an output column (#496) -- its acc
            // column is consumed (the cursor still advances past it) but
            // never pushed to `out`; only an `Expr` part reads it.
            AggPart::Hidden(_) => {
                cursor = cursor.saturating_add(1);
            }
            // The merged sketch becomes its estimate (#603).
            AggPart::ApproxCountDistinct => {
                if let Some(column) = acc.get_mut(cursor) {
                    out.push(
                        std::mem::take(column)
                            .iter()
                            .map(finish_sketch)
                            .collect::<Result<Vec<_>>>()?,
                    );
                }
                cursor = cursor.saturating_add(1);
            }
            // The merged set becomes its size (#817).
            AggPart::ExactCountDistinct => {
                if let Some(column) = acc.get_mut(cursor) {
                    out.push(
                        std::mem::take(column)
                            .iter()
                            .map(finish_int_set)
                            .collect::<Result<Vec<_>>>()?,
                    );
                }
                cursor = cursor.saturating_add(1);
            }
            // The same for strings (#820).
            AggPart::ExactCountDistinctStr => {
                if let Some(column) = acc.get_mut(cursor) {
                    out.push(
                        std::mem::take(column)
                            .iter()
                            .map(finish_str_set)
                            .collect::<Result<Vec<_>>>()?,
                    );
                }
                cursor = cursor.saturating_add(1);
            }
            // No acc column of its own -- both operands name a slot
            // another part already claimed above, so the cursor doesn't
            // move (#496).
            AggPart::Expr(map_op, lhs, rhs) => {
                let column = (0..len)
                    .map(|r| {
                        eval_agg_expr(*map_op, lhs, rhs, |i| {
                            acc.get(i).and_then(|c| c.get(r)).cloned().ok_or_else(|| {
                                VmError::MalformedProgram {
                                    opcode: "Combine",
                                    reason: format!(
                                        "Expr operand slot {i} is outside the emitted row"
                                    ),
                                }
                            })
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                out.push(column);
            }
            _ => {
                if let Some(column) = acc.get_mut(cursor) {
                    out.push(std::mem::take(column));
                }
                cursor = cursor.saturating_add(1);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::engine::finalize;
    use super::*;

    fn set(values: &[i64]) -> Value {
        Value::Bytes(crate::vm::batch::encode_int_set(values).into())
    }

    /// #817: sets of integers merge as a union, still sorted, with a
    /// segment that saw no rows (`Null`) adding nothing, and the merged set
    /// is its size.
    #[test]
    fn exact_distinct_partials_union_and_count() {
        let mut into = Value::Null;
        for part in [
            set(&[1, 5, 9]),
            Value::Null,
            set(&[i64::MIN, 5, 7]),
            set(&[]),
            set(&[9, i64::MAX]),
        ] {
            merge_slot(SlotOp::IntSet, &mut into, &part).unwrap();
        }
        assert_eq!(into, set(&[i64::MIN, 1, 5, 7, 9, i64::MAX]));
        assert_eq!(finish_int_set(&into).unwrap(), Value::Int(6));
        assert_eq!(finish_int_set(&Value::Null).unwrap(), Value::Int(0));
    }

    // vm_combine_union_int_sets_d36c0e8a (`union_int_sets`): `i < a.len() || j < b.len()`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_union_int_sets_d36c0e8a__v1_the_first_set_has_values_left() {
        let a = crate::vm::batch::encode_int_set(&[1, 2]);
        let b = crate::vm::batch::encode_int_set(&[2]);
        assert_eq!(
            union_int_sets(&a, &b).unwrap(),
            crate::vm::batch::encode_int_set(&[1, 2])
        );
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_union_int_sets_d36c0e8a__v2_only_the_second_set_has_values_left() {
        let b = crate::vm::batch::encode_int_set(&[3]);
        assert_eq!(union_int_sets(&[], &b).unwrap(), b);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_union_int_sets_d36c0e8a__v3_both_sets_are_exhausted() {
        assert_eq!(union_int_sets(&[], &[]).unwrap(), Vec::<u8>::new());
    }

    /// A partial that is not a whole number of 8-byte values, or not bytes
    /// at all, is a planner bug and reported.
    #[test]
    fn exact_distinct_partials_that_are_not_sets_are_errors() {
        let torn = Value::Bytes(vec![1u8, 2, 3].into());
        let mut into = set(&[1]);
        assert!(merge_slot(SlotOp::IntSet, &mut into, &torn).is_err());
        assert!(merge_slot(SlotOp::IntSet, &mut into, &Value::Int(3)).is_err());
        assert!(finish_int_set(&torn).is_err());
    }

    fn strs(values: &[&str]) -> Value {
        Value::Bytes(crate::vm::batch::encode_str_set(values.iter().copied()).into())
    }

    /// The strings of a merged string set, sorted.
    fn strs_of(v: &Value) -> Vec<String> {
        let mut out: Vec<String> = str_set(v)
            .unwrap()
            .into_iter()
            .map(|s| String::from_utf8(s.to_vec()).unwrap())
            .collect();
        out.sort();
        out
    }

    /// #820: sets of strings merge as a union (in no particular order), a
    /// segment that saw no rows (`Null`) adds nothing, the empty string is a
    /// member like any other, and the merged set is its size.
    #[test]
    fn string_distinct_partials_union_and_count() {
        let mut into = Value::Null;
        for part in [
            strs(&["b", "", "a"]),
            Value::Null,
            strs(&["ab", "a"]),
            strs(&[]),
            strs(&["é", "b"]),
        ] {
            merge_slot(SlotOp::StrSet, &mut into, &part).unwrap();
        }
        assert_eq!(strs_of(&into), ["", "a", "ab", "b", "é"]);
        assert_eq!(finish_str_set(&into).unwrap(), Value::Int(5));
        assert_eq!(finish_str_set(&Value::Null).unwrap(), Value::Int(0));
    }

    /// A string partial cut short, or not bytes at all, is a planner bug and
    /// reported.
    #[test]
    fn string_distinct_partials_that_are_not_sets_are_errors() {
        let mut into = strs(&["a"]);
        for torn in [vec![1u8, 0], vec![5, 0, 0, 0, b'a'], vec![1, 0, 0, 0]] {
            let torn = Value::Bytes(torn.into());
            assert!(merge_slot(SlotOp::StrSet, &mut into, &torn).is_err());
            assert!(finish_str_set(&torn).is_err());
        }
        assert!(merge_slot(SlotOp::StrSet, &mut into, &Value::Int(3)).is_err());
    }

    /// #643 review: the in-place reorder handles a fixed point, a
    /// 2-cycle and a longer cycle, moving every column together.
    #[test]
    fn reorder_by_first_seen_permutes_every_column_in_place() {
        // Groups 0..6 in creation order; their first-seen keys put them in
        // the order 1, 0, 2, 5, 3, 4: a 2-cycle (0 1), a fixed point (2),
        // and a 3-cycle (3 4 5).
        let first_seen = [10, 5, 20, 40, 50, 30];
        let col = |tag: &str| -> Vec<Value> {
            (0..6)
                .map(|g| Value::Str(format!("{tag}{g}").into()))
                .collect()
        };
        // One plain column and one typed (interned) key column.
        let typed = |values: Vec<Value>| {
            let mut column = AccColumn::for_values(true, false, &values, 0);
            for v in &values {
                column.push_value(v);
            }
            column
        };
        let values = |acc: Vec<AccColumn>| -> Vec<Vec<Value>> {
            acc.into_iter().map(AccColumn::into_values).collect()
        };
        let out = reorder_by_first_seen(
            vec![AccColumn::Values(col("a")), typed(col("b"))],
            &first_seen,
        );
        let want = |tag: &str| -> Vec<Value> {
            [1, 0, 2, 5, 3, 4]
                .iter()
                .map(|g| Value::Str(format!("{tag}{g}").into()))
                .collect()
        };
        assert_eq!(values(out), vec![want("a"), want("b")]);
        let sorted = reorder_by_first_seen(vec![typed(col("a"))], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(values(sorted), vec![col("a")]);
    }

    /// A partial of `chunk`'s rows, its first column a typed key.
    fn partial(chunk: Chunk, first_seen: Vec<u64>) -> Partial {
        let columns = chunk
            .iter()
            .enumerate()
            .map(|(c, column)| {
                let mut acc = AccColumn::for_values(c == 0, false, column, 0);
                for v in column.iter() {
                    acc.push_value(v);
                }
                acc
            })
            .collect();
        Partial {
            columns,
            first_seen,
        }
    }

    fn chunk(columns: Vec<Vec<Value>>) -> Chunk {
        columns.into_iter().map(Arc::new).collect()
    }

    fn rows_of(chunks: &[Chunk]) -> Vec<Vec<Value>> {
        let mut rows = Vec::new();
        for c in chunks {
            for r in 0..chunk_len(c) {
                rows.push(c.iter().map(|col| col[r].clone()).collect());
            }
        }
        rows
    }

    fn s(v: &str) -> Value {
        Value::Str(v.to_string().into())
    }

    fn int_acc(value: i64) -> AccColumn {
        let values = [Value::Int(value)];
        let mut column = AccColumn::for_values(true, false, &values, 1);
        column.push_value(&values[0]);
        column
    }

    // vm_combine_merge_entry_f3bef4a0 (`merge_entry`):
    // `op == SlotOp::Count && matches!(column, AccColumn::Ints { .. })`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_merge_entry_f3bef4a0__v1_a_typed_count_column_adds_in_place() {
        let mut column = int_acc(1);
        merge_entry(SlotOp::Count, &mut column, 0, &Value::Int(2)).unwrap();
        assert_eq!(column.into_values(), vec![Value::Int(3)]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_merge_entry_f3bef4a0__v2_another_op_on_a_typed_column_is_malformed() {
        let mut column = int_acc(1);
        let err = merge_entry(SlotOp::Sum, &mut column, 0, &Value::Int(2)).unwrap_err();
        assert!(err.to_string().contains("typed key column"), "{err}");
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_merge_entry_f3bef4a0__v3_a_count_over_a_values_column_goes_through_merge_slot(
    ) {
        let mut column = AccColumn::Values(vec![Value::Int(1)]);
        merge_entry(SlotOp::Count, &mut column, 0, &Value::Int(2)).unwrap();
        assert_eq!(column.into_values(), vec![Value::Int(3)]);
    }

    fn pushed_partials(partials: Vec<Partial>) -> Result<QueryOutput> {
        let parts = [AggPart::GroupKey, AggPart::Count];
        combine_partials(&parts, 1, partials)
    }

    fn two_col_partial(rows: usize, first_seen: Vec<u64>) -> Partial {
        let keys: Vec<Value> = (0..rows).map(|i| Value::Int(i as i64)).collect();
        let counts: Vec<Value> = (0..rows).map(|_| Value::Int(1)).collect();
        partial(chunk(vec![keys, counts]), first_seen)
    }

    // vm_combine_push_partial_cab084bc (`push_partial`):
    // `partial.columns.len() != num_slots
    //  || partial.columns.iter().any(|column| column.len() != n)
    //  || partial.first_seen.len() != n`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_push_partial_cab084bc__v1_a_well_formed_partial_merges() {
        let out = pushed_partials(vec![
            two_col_partial(2, vec![0, 1]),
            two_col_partial(1, vec![5]),
        ])
        .unwrap();
        assert_eq!(
            out.into_rows(),
            vec![
                vec![Value::Int(0), Value::Int(2)],
                vec![Value::Int(1), Value::Int(1)]
            ]
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_push_partial_cab084bc__v2_a_wider_partial_is_malformed() {
        let wide = partial(
            chunk(vec![
                vec![Value::Int(0)],
                vec![Value::Int(1)],
                vec![Value::Int(1)],
            ]),
            vec![0],
        );
        let err = pushed_partials(vec![two_col_partial(1, vec![0]), wide]).unwrap_err();
        assert!(err.to_string().contains("does not fit"), "{err}");
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_push_partial_cab084bc__v3_a_ragged_column_is_malformed() {
        let ragged = partial(
            chunk(vec![
                vec![Value::Int(0), Value::Int(1)],
                vec![Value::Int(1)],
            ]),
            vec![0, 1],
        );
        let err = pushed_partials(vec![ragged]).unwrap_err();
        assert!(err.to_string().contains("does not fit"), "{err}");
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_push_partial_cab084bc__v4_a_first_seen_length_mismatch_is_malformed() {
        let err = pushed_partials(vec![two_col_partial(1, vec![0, 1])]).unwrap_err();
        assert!(err.to_string().contains("does not fit"), "{err}");
    }

    // vm_combine_finish_distinct_f71d60b5 (`finish_distinct`):
    // `keys > 0 || regroup.num_slots.is_some()`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_finish_distinct_f71d60b5__v1_grouped_over_nothing_is_empty() {
        let parts = [AggPart::GroupKey, AggPart::DistinctKey];
        assert!(combine_chunks(&parts, 2, &[]).unwrap().is_empty());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_finish_distinct_f71d60b5__v2_ungrouped_rows_are_counted() {
        let x = chunk(vec![vec![Value::Int(1), Value::Int(1), Value::Int(2)]]);
        let out = combine_chunks(&[AggPart::DistinctKey], 1, &[x]).unwrap();
        assert_eq!(out.into_rows(), vec![vec![Value::Int(2)]]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_finish_distinct_f71d60b5__v3_ungrouped_nothing_is_one_zero_row() {
        let out = combine_chunks(&[AggPart::DistinctKey], 1, &[]).unwrap();
        assert_eq!(out.into_rows(), vec![vec![Value::Int(0)]]);
    }

    /// #643: typed accumulator columns are an internal form only. Worker
    /// partials with an integer key, a string `COUNT(DISTINCT)` key (both
    /// with NULLs) and a SUM, merged through `combine_partials`, answer
    /// exactly as one untyped `combine_chunks` over the same chunks --
    /// values and group order -- also when a later chunk's key column
    /// turns out not to be all integers (the typed column demotes).
    #[test]
    fn typed_partials_merge_as_untyped_chunks_do() {
        let parts = [AggPart::GroupKey, AggPart::DistinctKey, AggPart::Sum];
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        let mut chunks = Vec::new();
        for segment in 0..12 {
            let n = 40;
            let keys: Vec<Value> = (0..n)
                .map(|_| match next(25) {
                    0 => Value::Null,
                    // One late chunk has a float key: the column demotes.
                    1 if segment == 9 => Value::Float(0.5),
                    k => Value::Int(i64::try_from(k).unwrap_or(0)),
                })
                .collect();
            let names: Vec<Value> = (0..n)
                .map(|_| match next(6) {
                    0 => Value::Null,
                    k => s(&format!("name{k}")),
                })
                .collect();
            let sums: Vec<Value> = (0..n)
                .map(|_| Value::Int(i64::try_from(next(100)).unwrap_or(0)))
                .collect();
            chunks.push(chunk(vec![keys, names, sums]));
        }
        let direct = combine_chunks(&parts, 2, &chunks).unwrap();
        // Three workers, each claiming every third segment.
        let partials: Vec<Partial> = (0..3)
            .map(|worker| {
                let mut combiner = Combiner::new(&parts, 2);
                for (segment, c) in chunks.iter().enumerate().skip(worker).step_by(3) {
                    combiner
                        .push_chunk(c, |row| order_key(segment, row))
                        .unwrap();
                }
                combiner.finish_partial()
            })
            .collect();
        assert!(partials
            .iter()
            .any(|p| matches!(p.columns.get(1), Some(AccColumn::Strs { .. }))));
        let merged = combine_partials(&parts, 2, partials).unwrap();
        assert_eq!(merged, direct);
        assert!(!direct.is_empty());
    }

    /// #633: `[k, x, SUM, AVG sum, AVG count]` partials keyed on `(k, x)`
    /// from two workers. The same `(k, x)` from both counts once, a NULL
    /// `x` never, and SUM/AVG still add every partial -- the same through
    /// `combine_partials`, `combine_chunks` and the row form `finalize`.
    /// #807: several `COUNT(DISTINCT)` merged together and with `SUM`/`AVG`,
    /// across segments, through workers' partials and in the row form.
    #[test]
    fn several_count_distincts_count_each_column_on_its_own() {
        let parts = [
            AggPart::GroupKey,
            AggPart::DistinctKey,
            AggPart::DistinctKey,
            AggPart::Sum,
            AggPart::Avg(4, 5),
        ];
        let rows = |r: &[(&str, Option<i64>, Option<i64>, i64)]| {
            chunk(vec![
                r.iter().map(|t| s(t.0)).collect(),
                r.iter()
                    .map(|t| t.1.map_or(Value::Null, Value::Int))
                    .collect(),
                r.iter()
                    .map(|t| t.2.map_or(Value::Null, Value::Int))
                    .collect(),
                r.iter().map(|t| Value::Int(t.3)).collect(),
                r.iter().map(|t| Value::Int(t.3)).collect(),
                r.iter().map(|_| Value::Int(1)).collect(),
            ])
        };
        let a = rows(&[
            ("a", Some(1), Some(10), 10),
            ("b", Some(7), None, 1),
            ("a", None, Some(10), 5),
        ]);
        let b = rows(&[
            ("a", Some(1), Some(20), 20),
            ("a", Some(2), Some(20), 30),
            ("b", Some(7), Some(3), 2),
        ]);
        let expected = vec![
            vec![
                s("a"),
                Value::Int(2),
                Value::Int(2),
                Value::Int(65),
                Value::Float(65.0 / 4.0),
            ],
            vec![
                s("b"),
                Value::Int(1),
                Value::Int(1),
                Value::Int(3),
                Value::Float(1.5),
            ],
        ];
        let direct = combine_chunks(&parts, 3, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(direct.into_rows(), expected);
        let worker = |c: &Chunk, segment| {
            let mut w = Combiner::new(&parts, 3);
            w.push_chunk(c, |row| order_key(segment, row)).unwrap();
            w.finish_partial()
        };
        let merged = combine_partials(&parts, 3, vec![worker(&b, 1), worker(&a, 0)]).unwrap();
        assert_eq!(merged.into_rows(), expected, "first-seen order is kept");
        let all = rows_of(&[a, b]);
        let out = finalize(&parts, 3, false, &[], None, all).unwrap();
        assert_eq!(out.into_rows(), expected);
    }

    // vm_combine_finish_distinct_many_65ed9eb1 (`finish_distinct_many`):
    // `c < keys || c >= block.end` (#807).
    fn two_distinct_columns() -> (Vec<Value>, Vec<Value>) {
        (
            vec![Value::Int(1), Value::Int(1), Value::Int(2)],
            vec![Value::Int(5), Value::Null, Value::Int(5)],
        )
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_finish_distinct_many_65ed9eb1__v1_only_the_distinct_columns() {
        let (x, y) = two_distinct_columns();
        let parts = [AggPart::DistinctKey, AggPart::DistinctKey];
        let out = combine_chunks(&parts, 2, &[chunk(vec![x, y])]).unwrap();
        assert_eq!(out.into_rows(), vec![vec![Value::Int(2), Value::Int(1)]]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_finish_distinct_many_65ed9eb1__v2_a_key_column_comes_first() {
        let (x, y) = two_distinct_columns();
        let key = vec![s("k"), s("k"), s("k")];
        let parts = [
            AggPart::GroupKey,
            AggPart::DistinctKey,
            AggPart::DistinctKey,
        ];
        let out = combine_chunks(&parts, 3, &[chunk(vec![key, x, y])]).unwrap();
        assert_eq!(
            out.into_rows(),
            vec![vec![s("k"), Value::Int(2), Value::Int(1)]]
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_finish_distinct_many_65ed9eb1__v3_an_aggregate_comes_after() {
        let (x, y) = two_distinct_columns();
        let sum = vec![Value::Int(1), Value::Int(2), Value::Int(3)];
        let parts = [AggPart::DistinctKey, AggPart::DistinctKey, AggPart::Sum];
        let out = combine_chunks(&parts, 2, &[chunk(vec![x, y, sum])]).unwrap();
        assert_eq!(
            out.into_rows(),
            vec![vec![Value::Int(2), Value::Int(1), Value::Int(6)]]
        );
    }

    #[test]
    fn several_count_distincts_without_group_by_give_one_row() {
        let parts = [
            AggPart::DistinctKey,
            AggPart::DistinctKey,
            AggPart::DistinctKey,
            AggPart::Count,
        ];
        let x = chunk(vec![
            vec![Value::Int(1), Value::Int(1), Value::Null],
            vec![Value::Int(5), Value::Int(6), Value::Int(5)],
            vec![Value::Null, Value::Null, Value::Null],
            vec![Value::Int(1), Value::Int(1), Value::Int(1)],
        ]);
        let out = combine_chunks(&parts, 3, &[x]).unwrap();
        assert_eq!(
            out.into_rows(),
            vec![vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(0),
                Value::Int(3)
            ]]
        );
        let none = combine_chunks(&parts, 3, &[]).unwrap();
        assert_eq!(
            none.into_rows(),
            vec![vec![
                Value::Int(0),
                Value::Int(0),
                Value::Int(0),
                Value::Int(0)
            ]]
        );
    }

    #[test]
    fn several_count_distincts_must_follow_the_group_keys() {
        let parts = [AggPart::DistinctKey, AggPart::Sum, AggPart::DistinctKey];
        let x = chunk(vec![
            vec![Value::Int(1)],
            vec![Value::Int(1)],
            vec![Value::Int(1)],
        ]);
        assert!(combine_chunks(&parts, 2, &[x]).is_err());
    }

    #[test]
    fn count_distinct_regroups_merged_partials_by_the_real_keys() {
        let parts = [
            AggPart::GroupKey,
            AggPart::DistinctKey,
            AggPart::Sum,
            AggPart::Avg(3, 4),
        ];
        let rows = |r: &[(&str, Option<i64>, i64)]| {
            chunk(vec![
                r.iter().map(|t| s(t.0)).collect(),
                r.iter()
                    .map(|t| t.1.map_or(Value::Null, Value::Int))
                    .collect(),
                r.iter().map(|t| Value::Int(t.2)).collect(),
                r.iter().map(|t| Value::Int(t.2)).collect(),
                r.iter().map(|_| Value::Int(1)).collect(),
            ])
        };
        let a = rows(&[("a", Some(1), 10), ("b", Some(7), 1), ("a", None, 5)]);
        let b = rows(&[("a", Some(1), 20), ("a", Some(2), 30), ("b", Some(7), 2)]);
        let expected = vec![
            vec![
                s("a"),
                Value::Int(2),
                Value::Int(65),
                Value::Float(65.0 / 4.0),
            ],
            vec![s("b"), Value::Int(1), Value::Int(3), Value::Float(1.5)],
        ];
        let direct = combine_chunks(&parts, 2, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(direct.into_rows(), expected);
        let worker = |c: &Chunk, segment| {
            let mut w = Combiner::new(&parts, 2);
            w.push_chunk(c, |row| order_key(segment, row)).unwrap();
            w.finish_partial()
        };
        let merged = combine_partials(&parts, 2, vec![worker(&b, 1), worker(&a, 0)]).unwrap();
        assert_eq!(merged.into_rows(), expected, "first-seen order is kept");
        let mut all = rows_of(&[a, b]);
        all.rotate_left(1);
        let reference = finalize(&parts, 2, false, &[(0, false)], None, all).unwrap();
        assert_eq!(reference.into_rows(), expected);
    }

    /// Without `GROUP BY`, `COUNT(DISTINCT x)` is one row even over no
    /// rows: 0, with `COUNT` 0 and everything else NULL.
    #[test]
    fn count_distinct_without_group_by_is_one_row_over_nothing() {
        let parts = [
            AggPart::DistinctKey,
            AggPart::Count,
            AggPart::Sum,
            AggPart::Avg(3, 4),
            AggPart::Hidden(HiddenPart::Count),
            AggPart::Expr(MapOp::Add, AggOperand::Slot(5), AggOperand::Literal(1.0)),
        ];
        assert_eq!(slot_count(&parts), 6);
        let out = combine_chunks(&parts, 1, &[]).unwrap();
        assert_eq!(
            out.into_rows(),
            vec![vec![
                Value::Int(0),
                Value::Int(0),
                Value::Null,
                Value::Null,
                Value::Float(1.0)
            ]]
        );
        let reference = finalize(&parts, 1, false, &[], None, Vec::new()).unwrap();
        assert_eq!(reference.num_rows(), 1);
        let grouped = [AggPart::GroupKey, AggPart::DistinctKey];
        assert!(combine_chunks(&grouped, 2, &[]).unwrap().is_empty());
    }

    /// #632: decimal SUM partials merge exactly at the larger scale, an
    /// integer partial joining at scale 0, and a total past DECIMAL(38)
    /// is an error.
    #[test]
    fn merge_sum_partials_adds_decimals_exactly() {
        let dec = |unscaled, scale| Value::Decimal { unscaled, scale };
        let merge = |a: &Value, b: &Value| merge_sum_partials(a, b).unwrap();
        assert_eq!(merge(&dec(10, 2), &dec(2, 1)), dec(30, 2));
        assert_eq!(merge(&dec(10, 2), &Value::Int(1)), dec(110, 2));
        assert_eq!(merge(&Value::Int(1), &dec(5, 1)), dec(15, 1));
        assert_eq!(merge(&Value::Null, &dec(5, 1)), dec(5, 1));
        assert_eq!(merge(&dec(5, 1), &Value::Null), dec(5, 1));
        assert!(matches!(
            merge(&dec(5, 1), &Value::Float(0.5)),
            Value::Float(_)
        ));
        let max38 = 10_i128.pow(38) - 1;
        assert!(merge_sum_partials(&dec(max38, 0), &dec(1, 0)).is_err());
        assert!(merge_sum_partials(&dec(i128::MAX, 0), &dec(1, 1)).is_err());
        assert!(merge_sum_partials(&dec(i128::MAX, 0), &dec(i128::MAX, 0)).is_err());
    }

    /// The differential obligation: the column-major merge must produce
    /// exactly what the row-based `finalize` does -- same groups, same
    /// order, same values and value *types* -- over every aggregate kind,
    /// `Null` partials (a segment with no surviving rows), keys that only
    /// collide when stringified, and an `AVG` that precedes another
    /// aggregate (#404's slot offset).
    #[test]
    fn combine_chunks_matches_finalize_over_every_aggregate_kind() {
        let parts = [
            AggPart::GroupKey,
            AggPart::GroupKey,
            AggPart::Avg(2, 3),
            AggPart::Sum,
            AggPart::Count,
            AggPart::Min,
            AggPart::Max,
        ];
        // slots: k1, k2, avg_sum, avg_count, sum, count, min, max
        let chunks = vec![
            chunk(vec![
                vec![Value::Int(1), Value::Int(2), s("1"), Value::Int(3)],
                vec![s("a"), s("b"), s("a"), s("a")],
                vec![
                    Value::Float(10.0),
                    Value::Null,
                    Value::Float(4.0),
                    Value::Float(1.0),
                ],
                vec![Value::Int(2), Value::Null, Value::Int(1), Value::Int(1)],
                vec![Value::Int(7), Value::Null, Value::Float(2.5), Value::Int(9)],
                vec![Value::Int(3), Value::Int(0), Value::Int(1), Value::Int(4)],
                vec![
                    Value::Int(5),
                    Value::Null,
                    Value::Float(-1.0),
                    Value::Int(8),
                ],
                vec![
                    Value::Int(5),
                    Value::Null,
                    Value::Float(-1.0),
                    Value::Int(8),
                ],
            ]),
            chunk(vec![
                vec![Value::Int(2), Value::Int(1), Value::Int(4)],
                vec![s("b"), s("a"), s("z")],
                vec![Value::Float(6.0), Value::Float(5.0), Value::Null],
                vec![Value::Int(1), Value::Int(1), Value::Null],
                vec![Value::Int(1), Value::Int(-2), Value::Null],
                vec![Value::Int(2), Value::Int(5), Value::Int(0)],
                vec![Value::Int(1), Value::Int(9), Value::Null],
                vec![Value::Int(1), Value::Int(9), Value::Null],
            ]),
            chunk(vec![
                vec![Value::Int(1)],
                vec![s("a")],
                vec![Value::Float(3.0)],
                vec![Value::Int(1)],
                vec![Value::Int(100)],
                vec![Value::Int(1)],
                vec![Value::Int(0)],
                vec![Value::Int(100)],
            ]),
        ];
        let expected = finalize(&parts, 2, false, &[], None, rows_of(&chunks)).unwrap();
        let got = combine_chunks(&parts, 2, &chunks).unwrap();
        assert_eq!(got, expected);
        // Sanity on the content itself: 5 groups, first-seen order.
        let rows = got.into_rows();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0][..2], [Value::Int(1), s("a")]);
        assert_eq!(rows[1][..2], [Value::Int(2), s("b")]);
        assert_eq!(rows[2][..2], [s("1"), s("a")], "Str(\"1\") is not Int(1)");
        assert_eq!(rows[3][..2], [Value::Int(3), s("a")]);
        assert_eq!(rows[4][..2], [Value::Int(4), s("z")]);
        // Group (1, "a"): avg (10+5+3)/(2+1+1) = 4.5; sum 7 + -2 + 100 = 105.0;
        // count 3+5+1 = 9 (stays Int); min 0.0; max 100.0.
        assert_eq!(
            rows[0][2..],
            [
                Value::Float(4.5),
                Value::Int(105),
                Value::Int(9),
                Value::Int(0),
                Value::Int(100)
            ]
        );
    }

    #[test]
    fn a_group_seen_once_keeps_its_original_value_types() {
        let chunks = vec![chunk(vec![
            vec![s("a"), s("b")],
            vec![Value::Int(1), Value::Int(5)],
        ])];
        let out = combine_chunks(&[AggPart::GroupKey, AggPart::Sum], 1, &chunks).unwrap();
        assert_eq!(
            out.into_rows(),
            vec![vec![s("a"), Value::Int(1)], vec![s("b"), Value::Int(5)]]
        );
    }

    #[test]
    fn null_partials_are_the_identity_and_all_null_stays_null() {
        // Two segments that saw no rows for the group, then one that did.
        let chunks = vec![
            chunk(vec![
                vec![Value::Int(1)],
                vec![Value::Null],
                vec![Value::Null],
            ]),
            chunk(vec![
                vec![Value::Int(1)],
                vec![Value::Null],
                vec![Value::Null],
            ]),
            chunk(vec![
                vec![Value::Int(1)],
                vec![Value::Int(3)],
                vec![Value::Int(2)],
            ]),
        ];
        let parts = [AggPart::GroupKey, AggPart::Sum, AggPart::Count];
        let out = combine_chunks(&parts, 1, &chunks).unwrap();
        assert_eq!(
            out.into_rows(),
            vec![vec![Value::Int(1), Value::Int(3), Value::Int(2)]]
        );
        let all_null = vec![
            chunk(vec![
                vec![Value::Int(1)],
                vec![Value::Null],
                vec![Value::Null],
            ]),
            chunk(vec![
                vec![Value::Int(1)],
                vec![Value::Null],
                vec![Value::Null],
            ]),
        ];
        let out = combine_chunks(&parts, 1, &all_null).unwrap();
        assert_eq!(
            out.into_rows(),
            vec![vec![Value::Int(1), Value::Null, Value::Int(0)]]
        );
    }

    #[test]
    fn no_chunks_is_an_empty_output() {
        let out = combine_chunks(&[AggPart::GroupKey, AggPart::Sum], 1, &[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn ragged_or_misshapen_chunks_are_errors_not_panics() {
        let parts = [AggPart::GroupKey, AggPart::Sum];
        let ragged = vec![chunk(vec![
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Int(1)],
        ])];
        assert!(matches!(
            combine_chunks(&parts, 1, &ragged),
            Err(VmError::MalformedProgram {
                opcode: "Combine",
                ..
            })
        ));
        let narrow = vec![chunk(vec![vec![Value::Int(1)]])];
        assert!(matches!(
            combine_chunks(&parts, 2, &narrow),
            Err(VmError::MalformedProgram {
                opcode: "Combine",
                ..
            })
        ));
        let mixed_width = vec![
            chunk(vec![vec![Value::Int(1)], vec![Value::Int(1)]]),
            chunk(vec![vec![Value::Int(1)]]),
        ];
        assert!(matches!(
            combine_chunks(&parts, 1, &mixed_width),
            Err(VmError::MalformedProgram {
                opcode: "Combine",
                ..
            })
        ));
        assert!(matches!(
            slot_ops(&[AggPart::GroupKey, AggPart::Avg(1, 7)], 3),
            Err(VmError::MalformedProgram {
                opcode: "Combine",
                ..
            })
        ));
    }

    #[test]
    fn slot_ops_offsets_every_part_after_an_avg_by_its_extra_slot() {
        // #404: `Avg` is one part but two slots; the `Sum` after it must
        // land on slot 3, not slot 2.
        let ops = slot_ops(&[AggPart::GroupKey, AggPart::Avg(1, 2), AggPart::Sum], 4).unwrap();
        assert_eq!(
            ops,
            vec![SlotOp::Key, SlotOp::AvgSum, SlotOp::AvgCount, SlotOp::Sum]
        );
    }

    #[test]
    fn count_overflow_is_an_error() {
        let mut into = Value::Int(i64::MAX);
        assert!(merge_slot(SlotOp::Count, &mut into, &Value::Int(1)).is_err());
        let mut into = Value::Int(1);
        assert!(merge_slot(SlotOp::Count, &mut into, &Value::Float(1.0)).is_err());
        let mut into = Value::Int(1);
        assert!(merge_slot(SlotOp::Sum, &mut into, &s("x")).is_err());
    }

    #[test]
    fn min_and_max_treat_a_null_side_as_absent() {
        let mut min = Value::Null;
        merge_slot(SlotOp::Min, &mut min, &Value::Int(3)).unwrap();
        assert_eq!(min, Value::Int(3));
        merge_slot(SlotOp::Min, &mut min, &Value::Null).unwrap();
        assert_eq!(min, Value::Int(3), "a Null partial leaves the min alone");
        merge_slot(SlotOp::Min, &mut min, &Value::Int(1)).unwrap();
        assert_eq!(min, Value::Int(1));
        let mut max = Value::Int(3);
        merge_slot(SlotOp::Max, &mut max, &Value::Float(9.5)).unwrap();
        assert_eq!(max, Value::Float(9.5));
    }

    fn int_chunk(keys: &[Option<i64>], vals: &[i64]) -> Chunk {
        chunk(vec![
            keys.iter()
                .map(|k| k.map_or(Value::Null, Value::Int))
                .collect(),
            vals.iter().copied().map(Value::Int).collect(),
        ])
    }

    /// #478 phase 2: the integer-key table must agree exactly with
    /// `finalize` -- same groups, order, values and value types -- over
    /// negative keys, keys whose entropy is in the high bits (the case a
    /// bare multiplicative hash mishandles), a `Null` key group, and enough
    /// new groups in later chunks to force the table to grow several times
    /// past its first-chunk sizing.
    #[test]
    fn int_key_path_matches_finalize_including_null_keys_and_growth() {
        let parts = [AggPart::GroupKey, AggPart::Sum];
        let mut chunks = vec![int_chunk(
            &[Some(7), None, Some(-3), Some(1 << 40)],
            &[1, 2, 3, 4],
        )];
        for seg in 0..3i64 {
            let keys: Vec<Option<i64>> = (0..5_000i64)
                .map(|g| Some((g - 2_500) << 32 | seg))
                .chain([None, Some(7), Some(-3)])
                .collect();
            let vals: Vec<i64> = (0..keys.len() as i64).collect();
            chunks.push(int_chunk(&keys, &vals));
        }
        let got = combine_chunks(&parts, 1, &chunks).unwrap();
        let reference = finalize(&parts, 1, false, &[], None, rows_of(&chunks)).unwrap();
        assert_eq!(got, reference);
        // 4 first-chunk groups + 3 x 5000 distinct, minus the one later key
        // that recurs: `(2756 - 2500) << 32 | 0 == 1 << 40`.
        assert_eq!(got.num_rows(), 4 + 3 * 5_000 - 1);
        let rows = got.into_rows();
        assert_eq!(rows[0], vec![Value::Int(7), Value::Int(1 + 3 * 5_001)]);
        assert_eq!(rows[1][0], Value::Null, "Null keys are one group");
        assert_eq!(rows[1][1], Value::Int(2 + 3 * 5_000));
    }

    /// A later chunk whose key is not an `Int` demotes the integer table
    /// to the generic index in place -- nothing already accumulated is
    /// lost, and the result still equals `finalize`'s.
    #[test]
    fn a_non_int_key_in_a_later_chunk_demotes_to_the_generic_index() {
        let parts = [AggPart::GroupKey, AggPart::Count];
        let chunks = vec![
            int_chunk(&[Some(1), Some(2)], &[1, 1]),
            chunk(vec![
                vec![Value::Int(1), s("x"), Value::Int(2)],
                vec![Value::Int(1), Value::Int(1), Value::Int(5)],
            ]),
            int_chunk(&[Some(2), Some(3)], &[1, 1]),
        ];
        let mut combiner = Combiner::new(&parts, 1);
        for (i, c) in chunks.iter().enumerate() {
            combiner.push_chunk(c, |row| order_key(i, row)).unwrap();
        }
        assert!(matches!(combiner.index, KeyIndex::Generic(_)));
        let out = combiner.finish().unwrap();
        assert_eq!(
            out,
            finalize(&parts, 1, false, &[], None, rows_of(&chunks)).unwrap()
        );
        assert_eq!(
            out.into_rows(),
            vec![
                vec![Value::Int(1), Value::Int(2)],
                vec![Value::Int(2), Value::Int(7)],
                vec![s("x"), Value::Int(1)],
                vec![Value::Int(3), Value::Int(1)],
            ]
        );
    }

    /// #488: per-worker partials merged by `combine_partials` must equal
    /// the direct merge of the same chunks, *including group order*, even
    /// when the "workers" processed the segments interleaved and out of
    /// order -- the first-seen keys restore global segment order.
    #[test]
    fn partials_merged_out_of_order_equal_the_direct_merge_including_order() {
        let parts = [
            AggPart::GroupKey,
            AggPart::Avg(1, 2),
            AggPart::Sum,
            AggPart::Count,
        ];
        // 6 "segments": segment i introduces group i and touches groups
        // 0..=i, so first-seen order is 0,1,2,3,4,5 in segment order.
        let chunks: Vec<Chunk> = (0..6i64)
            .map(|i| {
                let keys: Vec<Value> = (0..=i).map(|g| s(&format!("g{g}"))).collect();
                let n = keys.len();
                chunk(vec![
                    keys,
                    (0..n).map(|r| Value::Float(r as f64 + i as f64)).collect(),
                    vec![Value::Int(1); n],
                    (0..n).map(|r| Value::Int(r as i64 * 10)).collect(),
                    vec![Value::Int(2); n],
                ])
            })
            .collect();
        let direct = combine_chunks(&parts, 1, &chunks).unwrap();
        let reference = finalize(&parts, 1, false, &[], None, rows_of(&chunks)).unwrap();
        assert_eq!(direct, reference);

        // Worker A got segments 5, 2, 0 (in that order); worker B got 4, 1;
        // worker C got 3 -- dynamic claiming can produce any such split.
        let mut partials = Vec::new();
        for claimed in [vec![5usize, 2, 0], vec![4, 1], vec![3]] {
            let mut worker = Combiner::new(&parts, 1);
            for &seg in &claimed {
                worker
                    .push_chunk(&chunks[seg], |row| order_key(seg, row))
                    .unwrap();
            }
            partials.push(worker.finish_partial());
        }
        // A worker that claimed nothing contributes an empty partial.
        partials.push(Combiner::new(&parts, 1).finish_partial());
        let merged = combine_partials(&parts, 1, partials).unwrap();
        assert_eq!(merged, direct);
        assert_eq!(
            merged
                .into_rows()
                .iter()
                .map(|r| r[0].clone())
                .collect::<Vec<_>>(),
            (0..6).map(|g| s(&format!("g{g}"))).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_partial_with_mismatched_first_seen_keys_is_an_error() {
        let parts = [AggPart::GroupKey, AggPart::Sum];
        let partial = partial(int_chunk(&[Some(1), Some(2)], &[1, 2]), vec![0u64]);
        assert!(matches!(
            combine_partials(&parts, 1, vec![partial]),
            Err(VmError::MalformedProgram {
                opcode: "Combine",
                ..
            })
        ));
    }

    #[test]
    fn order_key_sorts_by_segment_then_row() {
        assert!(order_key(0, 5) < order_key(1, 0));
        assert!(order_key(3, 1) < order_key(3, 2));
        assert_eq!(order_key(2, 7), (2 << 32) | 7);
    }

    // An empty partial -- a worker that claimed nothing, or claimed only
    // filtered-out rows -- merges as nothing.
    #[test]
    fn column_less_partial_is_skipped() {
        let parts = [AggPart::GroupKey, AggPart::Sum];
        let real = partial(int_chunk(&[Some(1)], &[5]), vec![0u64]);
        // A worker that claimed nothing: no columns at all (first leaf
        // true), and its empty first-seen list must not trip the
        // length check.
        let empty: Chunk = Vec::new();
        let merged = combine_partials(&parts, 1, vec![partial(empty, Vec::new()), real]).unwrap();
        assert_eq!(merged.into_rows(), vec![vec![Value::Int(1), Value::Int(5)]]);
    }

    #[test]
    fn zero_row_partial_is_skipped() {
        let parts = [AggPart::GroupKey, AggPart::Sum];
        let real = partial(int_chunk(&[Some(1)], &[5]), vec![0u64]);
        // Columns present (first leaf false) but no rows (second leaf
        // true) -- again the length check must not fire.
        let zero_rows = partial(int_chunk(&[], &[]), Vec::new());
        let merged = combine_partials(&parts, 1, vec![zero_rows, real]).unwrap();
        assert_eq!(merged.into_rows(), vec![vec![Value::Int(1), Value::Int(5)]]);
    }

    #[test]
    fn populated_partials_are_merged() {
        let parts = [AggPart::GroupKey, AggPart::Sum];
        let a = partial(int_chunk(&[Some(1), Some(2)], &[5, 7]), vec![0u64, 1]);
        let b = partial(int_chunk(&[Some(1)], &[3]), vec![2u64]);
        let merged = combine_partials(&parts, 1, vec![a, b]).unwrap();
        assert_eq!(
            merged.into_rows(),
            vec![
                // A `SUM` merged across partials is finalized as a Float,
                // like `finalize`'s own cross-segment merge; a group seen
                // by one partial keeps its integer slot.
                vec![Value::Int(1), Value::Int(8)],
                vec![Value::Int(2), Value::Int(7)],
            ]
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_keep_partial_extreme_afe02b47__v1_an_empty_slot_takes_the_partial() {
        let mut min = Value::Null;
        merge_slot(SlotOp::Min, &mut min, &Value::Str("b".into())).unwrap();
        assert_eq!(min, Value::Str("b".into()));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_keep_partial_extreme_afe02b47__v2_a_further_partial_replaces_it() {
        let mut max = Value::Int(i64::MAX - 1);
        merge_slot(SlotOp::Max, &mut max, &Value::Int(i64::MAX)).unwrap();
        assert_eq!(max, Value::Int(i64::MAX), "exact, not through f64");
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_keep_partial_extreme_afe02b47__v3_a_nearer_partial_is_ignored() {
        let mut min = Value::Str("a".into());
        merge_slot(SlotOp::Min, &mut min, &Value::Str("b".into())).unwrap();
        assert_eq!(min, Value::Str("a".into()));
        // Numbers sort before text, as in SQLite.
        let mut max = Value::Str("a".into());
        merge_slot(SlotOp::Max, &mut max, &Value::Int(9)).unwrap();
        assert_eq!(max, Value::Str("a".into()));
    }

    #[test]
    fn integer_sum_partials_add_exactly_or_overflow() {
        let mut sum = Value::Int(i64::MAX - 1);
        merge_slot(SlotOp::Sum, &mut sum, &Value::Int(1)).unwrap();
        assert_eq!(sum, Value::Int(i64::MAX));
        let err = merge_slot(SlotOp::Sum, &mut sum, &Value::Int(1)).unwrap_err();
        assert_eq!(err, VmError::IntegerOverflow { opcode: "Combine" });
        let mut avg_sum = Value::Float(f64::from(i32::MAX));
        merge_slot(SlotOp::AvgSum, &mut avg_sum, &Value::Float(1.0)).unwrap();
        assert!(matches!(avg_sum, Value::Float(_)), "AVG's sum is a float");
        let mut mixed = Value::Int(2);
        merge_slot(SlotOp::Sum, &mut mixed, &Value::Float(0.5)).unwrap();
        assert_eq!(mixed, Value::Float(2.5));
    }
}
