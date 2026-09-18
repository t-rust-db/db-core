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

use super::batch::{
    apply_map_op, chunk_len, hash_columns_by_row, AggOperand, AggPart, Chunk, HiddenPart, MapOp,
    QueryOutput, Result, Value, VmError,
};

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
                        AggPart::GroupKey => SlotOp::Key,
                        AggPart::Sum | AggPart::Hidden(HiddenPart::Sum) => SlotOp::Sum,
                        AggPart::Count | AggPart::Hidden(HiddenPart::Count) => SlotOp::Count,
                        AggPart::Min | AggPart::Hidden(HiddenPart::Min) => SlotOp::Min,
                        AggPart::Max | AggPart::Hidden(HiddenPart::Max) => SlotOp::Max,
                        AggPart::Avg(..) | AggPart::Expr(..) => SlotOp::Key,
                    };
                }
                cursor = cursor.saturating_add(1);
            }
        }
    }
    Ok(ops)
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
    Ok(apply_map_op(map_op, &a, &b))
}

/// Merges one partial slot `from` into the accumulated slot `into` per
/// `op` -- the single definition of `Combine`'s merge semantics.
pub(crate) fn merge_slot(op: SlotOp, into: &mut Value, from: &Value) -> Result<()> {
    match op {
        SlotOp::Key => {}
        SlotOp::Sum | SlotOp::AvgSum => *into = merge_sum_partials(into, from)?,
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
        SlotOp::Min => {
            if let (Some(a), Some(b)) = (into.as_f64(), from.as_f64()) {
                *into = Value::Float(a.min(b));
            } else if matches!(into, Value::Null) {
                *into = from.clone();
            }
        }
        SlotOp::Max => {
            if let (Some(a), Some(b)) = (into.as_f64(), from.as_f64()) {
                *into = Value::Float(a.max(b));
            } else if matches!(into, Value::Null) {
                *into = from.clone();
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

/// Merges two partial `SUM`s (also `AVG`'s sum slot). A segment with no
/// surviving rows emits `Null` (#452: `Reduce` always emits one row), and
/// `Null` must be the identity here -- `Null` with `Null` stays `Null`, so
/// a `SUM` over zero rows across every segment is `NULL` as SQL requires,
/// not `0.0`; `Null` with a number is that number.
pub(crate) fn merge_sum_partials(into: &Value, from: &Value) -> Result<Value> {
    match (into, from) {
        (Value::Null, Value::Null) => Ok(Value::Null),
        _ => Ok(Value::Float(partial_f64(into)? + partial_f64(from)?)),
    }
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
    partials: &[(Chunk, Vec<u64>)],
) -> Result<QueryOutput> {
    let mut combiner = Combiner::new(parts, num_group_keys);
    for (chunk, first_seen) in partials {
        if chunk.is_empty() || chunk_len(chunk) == 0 {
            continue;
        }
        if first_seen.len() != chunk_len(chunk) {
            return Err(VmError::MalformedProgram {
                opcode: "Combine",
                reason: format!(
                    "partial carries {} first-seen keys for {} rows",
                    first_seen.len(),
                    chunk_len(chunk)
                ),
            });
        }
        combiner.push_chunk(chunk, |row| {
            first_seen.get(row).copied().unwrap_or(u64::MAX)
        })?;
    }
    combiner.finish()
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
    acc: &mut [Vec<Value>],
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
                merge_slot(*op, &mut acc_column[g], &column[r])?;
            }
        }
    }
    Ok(())
}

/// Copies row `r` of `chunk` in as a new group's initial state.
#[allow(
    clippy::indexing_slicing,
    reason = "`column[r]`: `r < chunk_len(chunk)` and every column has that many rows (validated)"
)]
fn push_new_group(acc: &mut [Vec<Value>], chunk: &Chunk, r: usize) {
    for (slot, column) in acc.iter_mut().zip(chunk) {
        slot.push(column[r].clone());
    }
}

/// MurmurHash3's 64-bit finalizer -- what DuckDB and ClickHouse hash
/// integer keys with. Not a bare multiplicative hash: real ids carry
/// their entropy in the high bits (timestamps, shifted keys), which a
/// multiply alone never brings down to the low bits an open-addressing
/// table indexes by; the xor-shifts do.
const fn murmur_finalize(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x = x.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    x ^ (x >> 33)
}

/// An empty slot in [`IntKeyTable::slots`].
const EMPTY: u32 = u32::MAX;

/// Open-addressing table from an `i64` group key to its group id -- the
/// `GROUP BY <int column>` specialization every engine has (ClickHouse
/// `key64`, DataFusion `GroupValuesPrimitive`, Velox's array mode).
/// Linear probing over a power-of-two `slots` array of indices (4 bytes
/// each, so 100K groups at load <= 1/2 is ~800 KB: L2-resident); the key
/// and its group id live in the dense `keys` vector the slots index into,
/// and the probe compares that `i64` directly -- no cached hash needed
/// when the key is 8 bytes. `Null` keys form one group of their own,
/// outside the table (`GROUP BY` semantics: `Null` groups with `Null`) --
/// which is why a key's position in `keys` and its group id differ.
struct IntKeyTable {
    /// Index into `keys`, or [`EMPTY`].
    slots: Vec<u32>,
    mask: usize,
    /// `(key, group id)` in insertion order.
    keys: Vec<(i64, u32)>,
    null_group: Option<usize>,
}

impl IntKeyTable {
    /// Sized for `expected_groups` at load factor <= 1/2 (all engines
    /// pay for the resize; the first chunk's row count is a good guess
    /// at the group count when every segment sees every group).
    fn with_capacity(expected_groups: usize) -> Self {
        let capacity = expected_groups
            .saturating_mul(2)
            .max(16)
            .next_power_of_two();
        IntKeyTable {
            slots: vec![EMPTY; capacity],
            mask: capacity.wrapping_sub(1),
            keys: Vec::with_capacity(expected_groups),
            null_group: None,
        }
    }

    /// The home slot of `key`: its bit pattern (not a sign-converted
    /// magnitude) through the finalizer, masked into `slots`.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "masked to `< slots.len()`, which is a `usize`, before use"
    )]
    fn slot_of(&self, key: i64) -> usize {
        (murmur_finalize(u64::from_ne_bytes(key.to_ne_bytes())) as usize) & self.mask
    }

    /// The group id for `key`, inserting it as group `next_id` when unseen
    /// -- `(id, inserted)`.
    #[allow(
        clippy::indexing_slicing,
        reason = "`slots[i]`: `i` is masked into `0..slots.len()`; `keys[k]`: every non-EMPTY slot holds a `k < keys.len()` by construction"
    )]
    fn get_or_insert(&mut self, key: i64, next_id: usize) -> Result<(usize, bool)> {
        if self.keys.len().saturating_mul(2) >= self.slots.len() {
            self.grow();
        }
        let mut i = self.slot_of(key);
        loop {
            let slot = self.slots[i];
            if slot == EMPTY {
                let too_many = || VmError::MalformedProgram {
                    opcode: "Combine",
                    reason: format!("more than {} groups", u32::MAX),
                };
                let id = u32::try_from(next_id).map_err(|_| too_many())?;
                let k = u32::try_from(self.keys.len()).map_err(|_| too_many())?;
                self.slots[i] = k;
                self.keys.push((key, id));
                return Ok((next_id, true));
            }
            let (candidate, id) = self.keys[slot as usize];
            if candidate == key {
                return Ok((id as usize, false));
            }
            i = i.wrapping_add(1) & self.mask;
        }
    }

    /// Doubles `slots` and re-places every group by its key's hash.
    #[allow(
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
        reason = "`slots[i]`: `i` is masked into `0..slots.len()`; `k as u32`: `k < keys.len() <= u32::MAX`, every entry was admitted by `get_or_insert`'s `u32::try_from`"
    )]
    fn grow(&mut self) {
        let capacity = self.slots.len().saturating_mul(2);
        self.slots = vec![EMPTY; capacity];
        self.mask = capacity.wrapping_sub(1);
        for k in 0..self.keys.len() {
            let mut i = self.slot_of(self.keys[k].0);
            while self.slots[i] != EMPTY {
                i = i.wrapping_add(1) & self.mask;
            }
            self.slots[i] = k as u32;
        }
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
    Generic(HashMap<u64, Vec<usize>>),
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
    /// Column-major accumulators, one entry per group, in first-seen order.
    acc: Vec<Vec<Value>>,
    index: KeyIndex,
    /// Each group's smallest [`order_key`] -- see there.
    first_seen: Vec<u64>,
    /// Scratch: this chunk's row -> group id (or [`NEW_GROUP`]).
    row_group: Vec<usize>,
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
        let expected_groups = chunk_len(chunk);
        self.acc = (0..num_slots)
            .map(|_| Vec::with_capacity(expected_groups))
            .collect();
        self.first_seen = Vec::with_capacity(expected_groups);
        let int_key =
            self.num_group_keys == 1 && chunk.first().is_some_and(|keys| is_int_key_column(keys));
        self.index = if int_key {
            KeyIndex::Int(IntKeyTable::with_capacity(expected_groups))
        } else {
            KeyIndex::Generic(HashMap::with_capacity(expected_groups))
        };
        self.num_slots = Some(num_slots);
        Ok(num_slots)
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
                Value::Null => match table.null_group {
                    Some(g) => (g, false),
                    None => {
                        let g = self.first_seen.len();
                        table.null_group = Some(g);
                        (g, true)
                    }
                },
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
        let key_columns: Vec<&[Value]> = self
            .acc
            .iter()
            .take(self.num_group_keys)
            .map(|column| column.as_slice())
            .collect();
        let hashes = hash_columns_by_row(&key_columns, num_groups, |g| g);
        let mut map: HashMap<u64, Vec<usize>> = HashMap::with_capacity(num_groups);
        for (g, hash) in hashes.into_iter().enumerate() {
            map.entry(hash).or_default().push(g);
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
            let bucket = map.entry(hashes[r]).or_default();
            let existing = bucket.iter().copied().find(|&g| {
                key_columns
                    .iter()
                    .enumerate()
                    .all(|(c, column)| self.acc[c][g] == column[r])
            });
            match existing {
                Some(g) => {
                    self.first_seen[g] = self.first_seen[g].min(order(r));
                    self.row_group.push(g);
                }
                None => {
                    bucket.push(self.first_seen.len());
                    push_new_group(&mut self.acc, chunk, r);
                    self.first_seen.push(order(r));
                    self.row_group.push(NEW_GROUP);
                }
            }
        }
        Ok(())
    }

    /// The finalized result (`AVG` divided, `parts` order), groups in
    /// first-seen order.
    pub(crate) fn finish(self) -> Result<QueryOutput> {
        if self.num_slots.is_none() {
            return Ok(QueryOutput::default());
        }
        let acc = reorder_by_first_seen(self.acc, &self.first_seen);
        Ok(QueryOutput::new(output_columns(&self.parts, acc)?))
    }

    /// The still-mergeable state: the raw accumulator slots as one chunk
    /// (same width and slot layout as the input, so a later [`Combiner`]
    /// merges it like any other partial) plus each group's first-seen key
    /// for [`combine_partials`] to order by. Empty (no columns) when no
    /// chunk was ever pushed.
    pub(crate) fn finish_partial(self) -> (Chunk, Vec<u64>) {
        (
            self.acc.into_iter().map(Arc::new).collect(),
            self.first_seen,
        )
    }
}

/// Permutes `acc`'s groups into ascending `first_seen` order -- a no-op
/// (no copy) when they already are, which is every single-combiner case,
/// since chunks arrive in segment order.
#[allow(
    clippy::indexing_slicing,
    reason = "`perm` is a permutation of `0..num_groups` and every column of `acc` has exactly `num_groups` entries"
)]
fn reorder_by_first_seen(mut acc: Vec<Vec<Value>>, first_seen: &[u64]) -> Vec<Vec<Value>> {
    if first_seen.windows(2).all(|w| w[0] <= w[1]) {
        return acc;
    }
    let mut perm: Vec<usize> = (0..first_seen.len()).collect();
    perm.sort_unstable_by_key(|&g| first_seen[g]);
    for column in &mut acc {
        let reordered: Vec<Value> = perm
            .iter()
            .map(|&g| std::mem::replace(&mut column[g], Value::Null))
            .collect();
        *column = reordered;
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
            // No acc column of its own -- both operands name a slot
            // another part already claimed above, so the cursor doesn't
            // move (#496).
            AggPart::Expr(map_op, lhs, rhs) => {
                let len = acc.first().map_or(0, Vec::len);
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
        let expected = finalize(&parts, 2, false, None, None, rows_of(&chunks)).unwrap();
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
                Value::Float(105.0),
                Value::Int(9),
                Value::Float(0.0),
                Value::Float(100.0)
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
            vec![vec![Value::Int(1), Value::Float(3.0), Value::Int(2)]]
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
        assert_eq!(min, Value::Float(1.0));
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

    /// #478 phase 2: the integer-key table must agree exactly with the

    #[test]
    fn int_key_table_grows_and_keeps_every_key_findable() {
        let mut table = IntKeyTable::with_capacity(1);
        assert_eq!(table.slots.len(), 16);
        for k in 0..1_000i64 {
            let (g, inserted) = table.get_or_insert(k * 1_000_003, k as usize).unwrap();
            assert!(inserted);
            assert_eq!(g, k as usize);
        }
        assert!(table.slots.len() >= 2_000, "load factor stays <= 1/2");
        for k in 0..1_000i64 {
            // The `next_id` is ignored for a key already present.
            let (g, inserted) = table.get_or_insert(k * 1_000_003, usize::MAX).unwrap();
            assert!(!inserted);
            assert_eq!(g, k as usize);
        }
    }

    #[test]
    fn murmur_finalizer_spreads_high_bit_entropy_into_the_low_bits() {
        // Keys differing only above bit 32 must not all land in one slot
        // of a small table -- the failure mode of a bare multiply.
        let mask = 1023usize;
        let mut slots = std::collections::HashSet::new();
        for k in 0..1_000u64 {
            slots.insert((murmur_finalize(k << 40) as usize) & mask);
        }
        assert!(
            slots.len() > 600,
            "only {} distinct low-10-bit slots",
            slots.len()
        );
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
        let reference = finalize(&parts, 1, false, None, None, rows_of(&chunks)).unwrap();
        assert_eq!(got, reference);
        // 4 first-chunk groups + 3 x 5000 distinct, minus the one later key
        // that recurs: `(2756 - 2500) << 32 | 0 == 1 << 40`.
        assert_eq!(got.num_rows(), 4 + 3 * 5_000 - 1);
        let rows = got.into_rows();
        assert_eq!(
            rows[0],
            vec![Value::Int(7), Value::Float(1.0 + 3.0 * 5_001.0)]
        );
        assert_eq!(rows[1][0], Value::Null, "Null keys are one group");
        assert_eq!(rows[1][1], Value::Float(2.0 + 3.0 * 5_000.0));
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
            finalize(&parts, 1, false, None, None, rows_of(&chunks)).unwrap()
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
        let reference = finalize(&parts, 1, false, None, None, rows_of(&chunks)).unwrap();
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
        let merged = combine_partials(&parts, 1, &partials).unwrap();
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
        let partial = (int_chunk(&[Some(1), Some(2)], &[1, 2]), vec![0u64]);
        assert!(matches!(
            combine_partials(&parts, 1, &[partial]),
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

    // vm_combine_combine_partials_4f421cb8 (`combine_partials`):
    // `chunk.is_empty() || chunk_len(chunk) == 0`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_combine_partials_4f421cb8__v1_column_less_partial_is_skipped() {
        let parts = [AggPart::GroupKey, AggPart::Sum];
        let real = (int_chunk(&[Some(1)], &[5]), vec![0u64]);
        // A worker that claimed nothing: no columns at all (first leaf
        // true), and its empty first-seen list must not trip the
        // length check.
        let empty: Chunk = Vec::new();
        let merged = combine_partials(&parts, 1, &[(empty, Vec::new()), real]).unwrap();
        assert_eq!(merged.into_rows(), vec![vec![Value::Int(1), Value::Int(5)]]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_combine_partials_4f421cb8__v2_zero_row_partial_is_skipped() {
        let parts = [AggPart::GroupKey, AggPart::Sum];
        let real = (int_chunk(&[Some(1)], &[5]), vec![0u64]);
        // Columns present (first leaf false) but no rows (second leaf
        // true) -- again the length check must not fire.
        let zero_rows = (int_chunk(&[], &[]), Vec::new());
        let merged = combine_partials(&parts, 1, &[zero_rows, real]).unwrap();
        assert_eq!(merged.into_rows(), vec![vec![Value::Int(1), Value::Int(5)]]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_combine_combine_partials_4f421cb8__v3_populated_partials_are_merged() {
        let parts = [AggPart::GroupKey, AggPart::Sum];
        let a = (int_chunk(&[Some(1), Some(2)], &[5, 7]), vec![0u64, 1]);
        let b = (int_chunk(&[Some(1)], &[3]), vec![2u64]);
        let merged = combine_partials(&parts, 1, &[a, b]).unwrap();
        assert_eq!(
            merged.into_rows(),
            vec![
                // A `SUM` merged across partials is finalized as a Float,
                // like `finalize`'s own cross-segment merge; a group seen
                // by one partial keeps its integer slot.
                vec![Value::Int(1), Value::Float(8.0)],
                vec![Value::Int(2), Value::Int(7)],
            ]
        );
    }
}
