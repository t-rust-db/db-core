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

use super::batch::{
    chunk_len, hash_columns_by_row, AggPart, Chunk, QueryOutput, Result, Value, VmError,
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
            other => {
                if let Some(slot) = ops.get_mut(cursor) {
                    *slot = match other {
                        AggPart::GroupKey => SlotOp::Key,
                        AggPart::Sum => SlotOp::Sum,
                        AggPart::Count => SlotOp::Count,
                        AggPart::Min => SlotOp::Min,
                        AggPart::Max => SlotOp::Max,
                        AggPart::Avg(..) => SlotOp::Key,
                    };
                }
                cursor = cursor.saturating_add(1);
            }
        }
    }
    Ok(ops)
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

/// Merges every chunk's partial rows into one row per group, column by
/// column, and finalizes (`AVG` = sum / count). Output columns follow
/// `parts` in order, exactly as `finalize` emits them; groups follow
/// first-seen order across `chunks`. Chunks must all have the same width
/// and every column of a chunk the same length -- a ragged chunk or a
/// `num_group_keys` wider than the row is a planner bug, reported as
/// [`VmError::MalformedProgram`] rather than indexed.
#[allow(
    clippy::indexing_slicing,
    reason = "every `chunk[c]`/`acc[c]` index is `< num_slots` and every `[r]` is `< n`, both checked up front per chunk; every group id `g` was pushed into `acc` before it was recorded in `index`/`row_group`"
)]
pub(crate) fn combine_chunks(
    parts: &[AggPart],
    num_group_keys: usize,
    chunks: &[Chunk],
) -> Result<QueryOutput> {
    let Some(first) = chunks.first() else {
        return Ok(QueryOutput::default());
    };
    let num_slots = first.len();
    if num_group_keys > num_slots {
        return Err(VmError::MalformedProgram {
            opcode: "Combine",
            reason: format!(
                "{num_group_keys} group keys but the emitted row has {num_slots} columns"
            ),
        });
    }
    let ops = slot_ops(parts, num_slots)?;

    // Column-major accumulators, one entry per group, in first-seen order.
    // Pre-sized from the first chunk: with one partial row per group per
    // segment, the first chunk's row count is the group count (or close).
    let expected_groups = chunk_len(first);
    let mut acc: Vec<Vec<Value>> = (0..num_slots)
        .map(|_| Vec::with_capacity(expected_groups))
        .collect();
    // Hash-then-verify: buckets of group ids sharing a key hash, exact
    // key equality against `acc`'s key columns on collision.
    let mut index: HashMap<u64, Vec<usize>> = HashMap::with_capacity(expected_groups);
    let mut row_group: Vec<usize> = Vec::new();

    for chunk in chunks {
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

        // Phase A: row -> group id, hashing the key columns column-wise.
        let key_columns: Vec<&[Value]> = chunk[..num_group_keys]
            .iter()
            .map(|column| column.as_slice())
            .collect();
        let hashes = hash_columns_by_row(&key_columns, n, |row| row);
        row_group.clear();
        row_group.reserve(n);
        for (r, &hash) in hashes.iter().enumerate() {
            let bucket = index.entry(hash).or_default();
            let existing = bucket.iter().copied().find(|&g| {
                key_columns
                    .iter()
                    .enumerate()
                    .all(|(c, column)| acc[c][g] == column[r])
            });
            match existing {
                Some(g) => row_group.push(g),
                None => {
                    let g = acc[0].len();
                    for (c, column) in chunk.iter().enumerate() {
                        acc[c].push(column[r].clone());
                    }
                    bucket.push(g);
                    row_group.push(NEW_GROUP);
                }
            }
        }

        // Phase B: merge each aggregate column into its accumulator, one
        // op dispatch per column-chunk.
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
    }

    Ok(QueryOutput::new(output_columns(parts, acc)?))
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
}
