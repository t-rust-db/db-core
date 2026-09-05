//! Cross-segment orchestration over the batch executor (ADR 0007).
//!
//! [`super::batch::Vm`] executes a program against *one* segment's batch.
//! A planned program's trailing [`Opcode::Finalize`] is a barrier: it
//! merges per-segment partial aggregates, then applies the final `ORDER
//! BY`/`LIMIT` -- so it cannot run inside the per-segment loop (where the
//! VM treats it as a no-op control opcode). [`run`] is the entry point
//! that splits a [`Program`] at that trailing `Finalize`, runs the body per
//! segment via [`run_parallel`]/[`run_parallel_top_n`] (or a sequential
//! bounded scan when the plan is a bare `LIMIT`), and applies the
//! `Finalize` once over the concatenated output.
//!
//! The merge/sort/limit logic itself ([`finalize`]) is column-rs's former
//! `query::post_process`, moved here unchanged -- it never touched storage.
//! Likewise [`run_join`], the two-phase `HashBuild`/`HashProbe` driver
//! over two already-materialized tables, and [`semi_filter`].

use super::batch::{
    compare_for_order, run_parallel, run_parallel_top_n, AggPart, Batch, Opcode, Program, Result,
    Segment, TopN, Value, Vm, VmError,
};
use std::collections::{HashMap, HashSet};

/// A [`Segment`] over an already-materialized [`Batch`] -- for the join/
/// semi-join/window paths, which build one in-memory table and then run
/// the flat program over it as a single segment.
pub struct InMemorySegment(pub Batch);

impl Segment for InMemorySegment {
    fn load(&self) -> Batch {
        self.0.clone()
    }
}

/// Run `program` over `segments`: body per segment, then the trailing
/// [`Opcode::Finalize`] once (see module docs). A program with no
/// `Finalize` is a plain per-segment concatenation, exactly like
/// [`run_parallel`].
///
/// Two plan shapes short-circuit the general path, both decided from the
/// instruction stream alone:
/// - a bare `LIMIT` (no `Filter`, no aggregates, no `ORDER BY`) scans
///   segments sequentially in order and stops once `limit` rows are
///   collected, so later segments are never loaded (#108);
/// - `ORDER BY ... LIMIT ...` without aggregates runs as a bounded top-N
///   per segment and at the merge (#109) instead of materializing every
///   row before the final sort.
pub fn run<'s>(segments: &[Box<dyn Segment + 's>], program: &Program) -> Result<Vec<Vec<Value>>> {
    let (body, fin) = program.split_finalize();
    let Some(Opcode::Finalize {
        agg_parts,
        num_group_keys,
        distinct,
        order_by,
        limit,
    }) = fin
    else {
        return run_parallel(segments, &body);
    };

    if let Some(limit) = bounded_scan_limit(program) {
        return bounded_scan(segments, &body, limit);
    }
    let rows = match (agg_parts.is_empty() && !distinct, order_by, limit) {
        (true, Some((col, descending)), Some(limit)) => run_parallel_top_n(
            segments,
            &body,
            &TopN {
                col: *col,
                descending: *descending,
                limit: *limit,
            },
        )?,
        _ => run_parallel(segments, &body)?,
    };
    Ok(finalize(
        agg_parts,
        *num_group_keys,
        *distinct,
        *order_by,
        *limit,
        rows,
    ))
}

/// The `LIMIT` when `program` can be satisfied by a sequential prefix scan
/// (#108): a trailing [`Opcode::Finalize`] with no aggregates and no
/// `ORDER BY`, and no [`Opcode::Filter`] in the body -- i.e. the first
/// `limit` rows of the first however-many segments *are* the answer.
pub fn bounded_scan_limit(program: &Program) -> Option<usize> {
    let (body, fin) = program.split_finalize();
    let Some(Opcode::Finalize {
        agg_parts,
        distinct,
        order_by: None,
        limit: Some(limit),
        ..
    }) = fin
    else {
        return None;
    };
    if *distinct
        || !agg_parts.is_empty()
        || body.iter().any(|op| matches!(op, Opcode::Filter { .. }))
    {
        return None;
    }
    Some(*limit)
}

/// Sequentially scan `segments` in order, running `body` against each
/// one's freshly-loaded batch, stopping (and truncating to exactly `limit`
/// rows) as soon as enough have been collected -- segments past that
/// point are never loaded.
fn bounded_scan<'s>(
    segments: &[Box<dyn Segment + 's>],
    body: &[Opcode],
    limit: usize,
) -> Result<Vec<Vec<Value>>> {
    let mut rows = Vec::with_capacity(limit);
    for segment in segments {
        if rows.len() >= limit {
            break;
        }
        let batch = segment.load();
        let mut vm = Vm::new();
        vm.execute(&batch, body)?;
        rows.extend(vm.take_output());
    }
    rows.truncate(limit);
    Ok(rows)
}

/// Apply [`Opcode::Finalize`]'s semantics to a flat row list: merge rows
/// sharing a group key per `agg_parts`, then `ORDER BY`, then `LIMIT`.
/// Shared by every execution path, and callable directly with `const`
/// data by an AOT-emitted binary.
pub fn finalize(
    agg_parts: &[AggPart],
    num_group_keys: usize,
    distinct: bool,
    order_by: Option<(usize, bool)>,
    limit: Option<usize>,
    rows: Vec<Vec<Value>>,
) -> Vec<Vec<Value>> {
    let mut result_rows = if !agg_parts.is_empty() {
        let mut groups: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for row in rows {
            let key: Vec<Value> = row[..num_group_keys].to_vec();
            let key_str = key
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\u{0}");
            match index.get(&key_str) {
                Some(&i) => merge_rows(agg_parts, &mut groups[i].1, &row),
                None => {
                    index.insert(key_str, groups.len());
                    groups.push((key, row));
                }
            }
        }
        groups
            .into_iter()
            .map(|(_, row)| finalize_row(agg_parts, row))
            .collect()
    } else {
        rows
    };

    // `DISTINCT` dedups the fully-projected output rows: for a plain
    // `SELECT DISTINCT` this is the only dedup pass (agg_parts is empty
    // above); combined with `GROUP BY`, the group-key merge above already
    // collapsed rows to one per group, so this second pass only catches
    // coincidental duplicate output rows across distinct groups (e.g. a
    // SELECT list that omits some GROUP BY columns) -- matching DuckDB's
    // semantics of dedup applied after the hash-aggregate. Must run before
    // `ORDER BY`/`LIMIT` per standard SQL evaluation order.
    if distinct {
        let mut seen: HashSet<String> = HashSet::new();
        result_rows.retain(|row| {
            let key = row
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\u{0}");
            seen.insert(key)
        });
    }

    if let Some((pos, descending)) = order_by {
        result_rows.sort_by(|a, b| compare_for_order(&a[pos], &b[pos], descending));
    }

    if let Some(limit) = limit {
        result_rows.truncate(limit);
    }

    result_rows
}

/// Combine two emitted rows for the same group key, applying the
/// associative merge appropriate to each [`AggPart`].
fn merge_rows(parts: &[AggPart], into: &mut [Value], from: &[Value]) {
    for (i, part) in parts.iter().enumerate() {
        match part {
            AggPart::GroupKey => {}
            AggPart::Sum | AggPart::Count => {
                into[i] =
                    Value::Float(into[i].as_f64().unwrap_or(0.0) + from[i].as_f64().unwrap_or(0.0));
            }
            AggPart::Min => {
                if let (Some(a), Some(b)) = (into[i].as_f64(), from[i].as_f64()) {
                    into[i] = Value::Float(a.min(b));
                } else if matches!(into[i], Value::Null) {
                    into[i] = from[i].clone();
                }
            }
            AggPart::Max => {
                if let (Some(a), Some(b)) = (into[i].as_f64(), from[i].as_f64()) {
                    into[i] = Value::Float(a.max(b));
                } else if matches!(into[i], Value::Null) {
                    into[i] = from[i].clone();
                }
            }
            AggPart::Avg(_, _) => {}
        }
    }
}

fn finalize_row(parts: &[AggPart], row: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::with_capacity(parts.len());
    let mut skip: Option<usize> = None;
    for (i, part) in parts.iter().enumerate() {
        if skip == Some(i) {
            continue;
        }
        match part {
            AggPart::Avg(sum_i, count_i) => {
                let (sum, count) = (
                    row[*sum_i].as_f64().unwrap_or(0.0),
                    row[*count_i].as_f64().unwrap_or(0.0),
                );
                out.push(if count == 0.0 {
                    Value::Null
                } else {
                    Value::Float(sum / count)
                });
                skip = Some(*count_i);
            }
            _ => out.push(row[i].clone()),
        }
    }
    out
}

/// A planned two-table equi-join, as produced by
/// `crate::codegen::batch::compile_join` and driven by [`run_join`]: which
/// columns each side must materialize (in register order), the build and
/// probe programs, where the probe lands the build side's payload, and the
/// flat body (ending in [`Opcode::Finalize`]) to run over the joined batch.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinProgram {
    pub left_columns: Vec<String>,
    pub right_columns: Vec<String>,
    pub build: Program,
    pub probe: Program,
    pub payload_dst: Vec<usize>,
    pub body: Program,
}

/// Execute a [`JoinProgram`] over two fully materialized tables: run the
/// build program on `right`, the probe program on `left`, assemble the
/// joined batch (left columns then right payload), and run `body` over it
/// as a single in-memory segment via [`run`].
pub fn run_join(left: &Batch, right: &Batch, plan: &JoinProgram) -> Result<Vec<Vec<Value>>> {
    let build: Vec<Opcode> = plan.build.opcodes().cloned().collect();
    let probe: Vec<Opcode> = plan.probe.opcodes().cloned().collect();

    let mut vm = Vm::new();
    vm.execute(right, &build)?;
    vm.clear_registers();
    vm.execute(left, &probe)?;

    let num_rows = vm.register(0)?.len();
    let mut joined = Batch::new(num_rows);
    for (reg, name) in plan.left_columns.iter().enumerate() {
        joined
            .columns
            .insert(name.clone(), vm.register(reg)?.to_vec());
    }
    for (name, &reg) in plan.right_columns.iter().zip(&plan.payload_dst) {
        joined
            .columns
            .insert(name.clone(), vm.register(reg)?.to_vec());
    }

    let segments: Vec<Box<dyn Segment>> = vec![Box::new(InMemorySegment(joined))];
    run(&segments, &plan.body)
}

/// Keep only the rows of `batch` whose `key_column` value (stringified)
/// appears in `allowed` -- the `WHERE col IN (SELECT ...)` semi-join
/// filter, applied before the flat body runs over the survivors.
pub fn semi_filter(batch: &Batch, key_column: &str, allowed: &HashSet<String>) -> Result<Batch> {
    let key = batch
        .columns
        .get(key_column)
        .ok_or_else(|| VmError::UnknownColumn {
            opcode: "SemiFilter",
            column: key_column.to_string(),
        })?;
    let keep: Vec<usize> = (0..batch.num_rows)
        .filter(|&i| allowed.contains(&key[i].to_string()))
        .collect();

    let mut filtered = Batch::new(keep.len());
    for (name, column) in &batch.columns {
        filtered.columns.insert(
            name.clone(),
            keep.iter().map(|&i| column[i].clone()).collect(),
        );
    }
    Ok(filtered)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use crate::expr::AggFunc;
    use crate::vm::batch::Instruction;

    fn seg(rows: &[(i64, i64)]) -> Box<dyn Segment> {
        let batch = Batch::new(rows.len())
            .with_column("k", rows.iter().map(|(k, _)| Value::Int(*k)).collect())
            .with_column("v", rows.iter().map(|(_, v)| Value::Int(*v)).collect());
        Box::new(InMemorySegment(batch))
    }

    fn group_sum_program(fin: Opcode) -> Program {
        Program::new(vec![
            Instruction::new(Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            }),
            Instruction::new(Opcode::LoadColumn {
                reg: 1,
                column: "v".into(),
            }),
            Instruction::new(Opcode::GroupReduce {
                group_by: vec![0].into(),
                aggs: vec![(AggFunc::Sum, Some(1))].into(),
                agg_dst: vec![2].into(),
            }),
            Instruction::new(Opcode::Emit {
                registers: vec![0, 2].into(),
            }),
            Instruction::new(fin),
        ])
    }

    #[test]
    fn finalize_merges_partial_group_aggregates_across_segments() {
        let segments = vec![seg(&[(1, 10), (2, 5)]), seg(&[(1, 3)])];
        let program = group_sum_program(Opcode::Finalize {
            agg_parts: vec![AggPart::GroupKey, AggPart::Sum].into(),
            num_group_keys: 1,
            distinct: false,
            order_by: Some((0, false)),
            limit: None,
        });
        let rows = run(&segments, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Float(13.0)],
                vec![Value::Int(2), Value::Float(5.0)],
            ]
        );
    }

    #[test]
    fn program_without_finalize_is_plain_concatenation() {
        let segments = vec![seg(&[(1, 10), (2, 5)]), seg(&[(1, 3)])];
        let mut program = group_sum_program(Opcode::Halt);
        program.instructions.pop();
        let rows = run(&segments, &program).unwrap();
        // Per-segment partials, unmerged: (1,10),(2,5) then (1,3).
        assert_eq!(rows.len(), 3);
    }

    fn scan_program(fin: Opcode, with_filter: bool) -> Program {
        let mut instructions = vec![Instruction::new(Opcode::LoadColumn {
            reg: 0,
            column: "k".into(),
        })];
        if with_filter {
            instructions.push(Instruction::new(Opcode::Filter { predicate: 0 }));
        }
        instructions.push(Instruction::new(Opcode::Emit {
            registers: vec![0].into(),
        }));
        instructions.push(Instruction::new(fin));
        Program::new(instructions)
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_102__v1_distinct_disqualifies_bounded_scan() {
        let program = scan_program(
            Opcode::Finalize {
                agg_parts: vec![].into(),
                num_group_keys: 0,
                distinct: true,
                order_by: None,
                limit: Some(5),
            },
            false,
        );
        assert_eq!(bounded_scan_limit(&program), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_102__v2_non_empty_agg_parts_disqualifies_bounded_scan() {
        let program = scan_program(
            Opcode::Finalize {
                agg_parts: vec![AggPart::Sum].into(),
                num_group_keys: 0,
                distinct: false,
                order_by: None,
                limit: Some(5),
            },
            false,
        );
        assert_eq!(bounded_scan_limit(&program), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_102__v3_filter_in_body_disqualifies_bounded_scan() {
        let program = scan_program(
            Opcode::Finalize {
                agg_parts: vec![].into(),
                num_group_keys: 0,
                distinct: false,
                order_by: None,
                limit: Some(5),
            },
            true,
        );
        assert_eq!(bounded_scan_limit(&program), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_102__v4_no_distinct_no_aggs_no_filter_allows_bounded_scan() {
        let program = scan_program(
            Opcode::Finalize {
                agg_parts: vec![].into(),
                num_group_keys: 0,
                distinct: false,
                order_by: None,
                limit: Some(5),
            },
            false,
        );
        assert_eq!(bounded_scan_limit(&program), Some(5));
    }

    #[test]
    fn bare_limit_stops_before_loading_later_segments() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static LOADS: AtomicUsize = AtomicUsize::new(0);
        struct Counting(Batch);
        impl Segment for Counting {
            fn load(&self) -> Batch {
                LOADS.fetch_add(1, Ordering::SeqCst);
                self.0.clone()
            }
        }
        let mk = |n: i64| -> Box<dyn Segment> {
            Box::new(Counting(
                Batch::new(2).with_column("k", vec![Value::Int(n), Value::Int(n + 1)]),
            ))
        };
        let segments = vec![mk(0), mk(10), mk(20)];
        let program = Program::new(vec![
            Instruction::new(Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            }),
            Instruction::new(Opcode::Emit {
                registers: vec![0].into(),
            }),
            Instruction::new(Opcode::Finalize {
                agg_parts: vec![].into(),
                num_group_keys: 0,
                distinct: false,
                order_by: None,
                limit: Some(3),
            }),
        ]);
        let rows = run(&segments, &program).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            LOADS.load(Ordering::SeqCst),
            2,
            "third segment never loaded"
        );
    }

    #[test]
    fn distinct_dedups_rows_across_segments() {
        // Two segments each carry a duplicate (k=1,v=0) row; DISTINCT should
        // collapse them to one, keeping the other rows untouched.
        let segments = vec![seg(&[(1, 0), (2, 0)]), seg(&[(1, 0), (3, 0)])];
        let program = Program::new(vec![
            Instruction::new(Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            }),
            Instruction::new(Opcode::Emit {
                registers: vec![0].into(),
            }),
            Instruction::new(Opcode::Finalize {
                agg_parts: vec![].into(),
                num_group_keys: 0,
                distinct: true,
                order_by: Some((0, false)),
                limit: None,
            }),
        ]);
        let rows = run(&segments, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1)],
                vec![Value::Int(2)],
                vec![Value::Int(3)],
            ]
        );
    }

    #[test]
    fn distinct_dedups_before_order_by_and_limit() {
        // A naive "sort/limit first, dedup later" plan would keep both
        // copies of k=1 if limit truncated before dedup ran; the correct
        // order (dedup, then sort, then limit) collapses them first.
        let segments = vec![seg(&[(1, 0), (1, 0), (1, 0)]), seg(&[(2, 0)])];
        let program = Program::new(vec![
            Instruction::new(Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            }),
            Instruction::new(Opcode::Emit {
                registers: vec![0].into(),
            }),
            Instruction::new(Opcode::Finalize {
                agg_parts: vec![].into(),
                num_group_keys: 0,
                distinct: true,
                order_by: Some((0, false)),
                limit: Some(2),
            }),
        ]);
        let rows = run(&segments, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    }

    #[test]
    fn distinct_with_group_by_does_not_collapse_genuinely_distinct_groups() {
        // Same shape as `finalize_merges_partial_group_aggregates_across_segments`
        // but with `distinct: true` -- the GROUP BY merge already yields one
        // row per key (1 -> 13, 2 -> 5), and since those two output rows
        // aren't equal, DISTINCT's post-aggregate dedup pass must leave both.
        let segments = vec![seg(&[(1, 10), (2, 5)]), seg(&[(1, 3)])];
        let program = group_sum_program(Opcode::Finalize {
            agg_parts: vec![AggPart::GroupKey, AggPart::Sum].into(),
            num_group_keys: 1,
            distinct: true,
            order_by: Some((0, false)),
            limit: None,
        });
        let rows = run(&segments, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Float(13.0)],
                vec![Value::Int(2), Value::Float(5.0)],
            ]
        );
    }

    #[test]
    fn order_by_limit_without_aggregates_takes_top_n() {
        let segments = vec![seg(&[(3, 0), (1, 0)]), seg(&[(2, 0), (0, 0)])];
        let program = Program::new(vec![
            Instruction::new(Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            }),
            Instruction::new(Opcode::Emit {
                registers: vec![0].into(),
            }),
            Instruction::new(Opcode::Finalize {
                agg_parts: vec![].into(),
                num_group_keys: 0,
                distinct: false,
                order_by: Some((0, true)),
                limit: Some(2),
            }),
        ]);
        let rows = run(&segments, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Int(3)], vec![Value::Int(2)]]);
    }

    #[test]
    fn finalize_avg_divides_sum_by_count_and_handles_nulls() {
        let rows = vec![
            vec![Value::Int(1), Value::Float(10.0), Value::Float(2.0)],
            vec![Value::Int(1), Value::Float(5.0), Value::Float(1.0)],
        ];
        let out = finalize(
            &[AggPart::GroupKey, AggPart::Avg(1, 2)],
            1,
            false,
            None,
            None,
            rows,
        );
        assert_eq!(out, vec![vec![Value::Int(1), Value::Float(5.0)]]);
    }

    #[test]
    fn program_derives_columns_to_load_and_splits_trailing_finalize() {
        let program = group_sum_program(Opcode::Finalize {
            agg_parts: vec![AggPart::GroupKey, AggPart::Sum].into(),
            num_group_keys: 1,
            distinct: false,
            order_by: None,
            limit: None,
        });
        assert_eq!(program.columns_to_load(), vec!["k", "v"]);
        let (body, fin) = program.split_finalize();
        assert_eq!(body.len(), 4);
        assert!(matches!(fin, Some(Opcode::Finalize { .. })));
        assert!(matches!(body.last(), Some(Opcode::Emit { .. })));

        let plain = Program::from_opcodes(body.clone());
        let (body2, fin2) = plain.split_finalize();
        assert_eq!(body2, body);
        assert!(fin2.is_none());
        assert_eq!(plain.len(), 4);
        assert!(!plain.is_empty());
        assert!(plain.get(0).is_some() && plain.get(4).is_none());
    }

    #[test]
    fn per_segment_vm_treats_finalize_as_a_no_op() {
        let batch = Batch::new(1).with_column("k", vec![Value::Int(1)]);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[Opcode::Finalize {
                agg_parts: vec![].into(),
                num_group_keys: 0,
                distinct: false,
                order_by: None,
                limit: Some(0),
            }],
        )
        .unwrap();
        assert!(vm.take_output().is_empty());
    }

    #[test]
    fn semi_filter_keeps_only_allowed_keys() {
        let batch = Batch::new(3)
            .with_column("k", vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            .with_column("v", vec![Value::Int(10), Value::Int(20), Value::Int(30)]);
        let allowed: HashSet<String> = ["1", "3"].iter().map(|s| s.to_string()).collect();
        let out = semi_filter(&batch, "k", &allowed).unwrap();
        assert_eq!(out.num_rows, 2);
        assert_eq!(out.columns["v"], vec![Value::Int(10), Value::Int(30)]);
        assert!(semi_filter(&batch, "nope", &allowed).is_err());
    }
}
