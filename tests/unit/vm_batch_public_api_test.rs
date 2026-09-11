// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for the batch VM's public entry points --
//! `vm::engine::run`/`run_join` -- driven with hand-built `Program`s
//! instead of whatever `codegen::batch` currently emits. Covers a plain
//! scan+filter+emit over a single `InMemorySegment`, a hash join via
//! `run_join`, and a `VmError` a caller can hit directly.

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

use db_core::codegen::batch::compile_join;
use db_core::parser::column::parse;
use db_core::vm::batch::{
    compare_for_order, AggFunc, Batch, Instruction, JoinKind, MapOp, Opcode, Program, Segment,
    Source, TopN, Value, Vm, VmError, WindowFunc,
};
use db_core::vm::engine::{run, run_join, run_join_segments, InMemorySegment, JoinProgram};
use std::sync::Arc;

#[test]
fn scan_filter_and_emit_over_a_single_segment() {
    let batch = Batch::new(3)
        .with_column("id", vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        .with_column(
            "name",
            vec![
                Value::Str("a".into()),
                Value::Str("b".into()),
                Value::Str("c".into()),
            ],
        );
    let segments = [InMemorySegment(batch)];

    let program = Program::new(vec![
        Instruction::new(Opcode::LoadColumn {
            reg: 0,
            column: "id".into(),
        }),
        Instruction::new(Opcode::LoadColumn {
            reg: 1,
            column: "name".into(),
        }),
        Instruction::new(Opcode::LoadConst {
            reg: 2,
            value: Value::Int(1),
        }),
        Instruction::new(Opcode::Map {
            dst: 3,
            op: MapOp::Gt,
            a: 0,
            b: 2,
        }),
        Instruction::new(Opcode::Filter { predicate: 3 }),
        Instruction::new(Opcode::Emit {
            registers: vec![0, 1].into(),
        }),
        Instruction::new(Opcode::Halt),
    ]);

    let rows = run(&segments, &program).unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(2), Value::Str("b".into())],
            vec![Value::Int(3), Value::Str("c".into())],
        ]
    );
}

#[test]
fn map_like_and_glob_filter_by_pattern_and_negate() {
    let batch = Batch::new(3).with_column(
        "name",
        vec![
            Value::Str("west".into()),
            Value::Str("north".into()),
            Value::Str("south".into()),
        ],
    );
    let segments = [InMemorySegment(batch)];

    let program = Program::new(vec![
        Instruction::new(Opcode::LoadColumn {
            reg: 0,
            column: "name".into(),
        }),
        Instruction::new(Opcode::LoadConst {
            reg: 1,
            value: Value::Str("%out%".into()),
        }),
        Instruction::new(Opcode::Map {
            dst: 2,
            op: MapOp::Like { negated: false },
            a: 0,
            b: 1,
        }),
        Instruction::new(Opcode::Filter { predicate: 2 }),
        Instruction::new(Opcode::Emit {
            registers: vec![0].into(),
        }),
        Instruction::new(Opcode::Halt),
    ]);
    let rows = run(&segments, &program).unwrap();
    assert_eq!(rows, vec![vec![Value::Str("south".into())]]);
}

#[test]
fn run_join_assembles_a_joined_batch_from_build_and_probe_programs() {
    // Right (build) side: id, payload.
    let right = Batch::new(2)
        .with_column("id", vec![Value::Int(1), Value::Int(2)])
        .with_column(
            "payload",
            vec![Value::Str("x".into()), Value::Str("y".into())],
        );
    // Left (probe) side: fk into the right table.
    let left = Batch::new(2).with_column("fk", vec![Value::Int(2), Value::Int(1)]);

    let build = Program::new(vec![
        Instruction::new(Opcode::LoadColumn {
            reg: 0,
            column: "id".into(),
        }),
        Instruction::new(Opcode::LoadColumn {
            reg: 1,
            column: "payload".into(),
        }),
        Instruction::new(Opcode::HashBuild {
            key_cols: vec![0].into(),
            payload_cols: vec![1].into(),
            table: 0,
        }),
        Instruction::new(Opcode::Halt),
    ]);
    let probe = Program::new(vec![
        Instruction::new(Opcode::LoadColumn {
            reg: 0,
            column: "fk".into(),
        }),
        Instruction::new(Opcode::HashProbe {
            key_cols: vec![0].into(),
            table: 0,
            payload_dst: vec![1].into(),
            kind: JoinKind::Inner,
        }),
        Instruction::new(Opcode::Halt),
    ]);
    // `body` runs over the assembled joined batch as its own fresh
    // segment (via `run` inside `run_join`), so it re-loads the columns
    // `run_join` wrote into it ("fk", "payload") by name rather than
    // reusing the build/probe programs' register numbers.
    let body = Program::new(vec![
        Instruction::new(Opcode::LoadColumn {
            reg: 0,
            column: "fk".into(),
        }),
        Instruction::new(Opcode::LoadColumn {
            reg: 1,
            column: "payload".into(),
        }),
        Instruction::new(Opcode::Emit {
            registers: vec![0, 1].into(),
        }),
        Instruction::new(Opcode::Halt),
    ]);

    let plan = JoinProgram {
        left_columns: vec!["fk".to_string()],
        right_columns: vec!["payload".to_string()],
        build,
        probe,
        payload_dst: vec![1],
        body,
    };

    let rows = run_join(&left, &right, &plan).unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(2), Value::Str("y".into())],
            vec![Value::Int(1), Value::Str("x".into())],
        ]
    );
}

/// #272: the probe side split into segments must give exactly the rows a
/// single-segment `run_join` gives -- INNER and LEFT, with a `GROUP BY`
/// body whose per-segment partial aggregates the trailing `Combine` merges.
/// Keys are spread so every group has rows in more than one segment, and
/// some fact rows have no matching customer to exercise the LEFT path.
fn join_fixture() -> (Vec<Batch>, Batch) {
    let customers = Batch::new(3)
        .with_column(
            "bench_customers.customer_id",
            vec![Value::Int(0), Value::Int(1), Value::Int(2)],
        )
        .with_column(
            "bench_customers.tier",
            vec![
                Value::Str("bronze".into()),
                Value::Str("silver".into()),
                Value::Str("gold".into()),
            ],
        );
    // 12 fact rows over 3 segments; customer_id 3 has no dimension row.
    let facts: Vec<Batch> = (0..3i32)
        .map(|seg| {
            let ids: Vec<Value> = (0..4i32)
                .map(|i| Value::Int(i64::from((seg * 4 + i) % 4)))
                .collect();
            let amounts: Vec<Value> = (0..4i32)
                .map(|i| Value::Float(f64::from(seg * 4 + i)))
                .collect();
            Batch::new(4)
                .with_column("bench.customer_id", ids)
                .with_column("bench.amount", amounts)
        })
        .collect();
    (facts, customers)
}

fn concat(batches: &[Batch]) -> Batch {
    let mut merged: std::collections::HashMap<String, Vec<Value>> =
        std::collections::HashMap::new();
    let mut num_rows = 0;
    for b in batches {
        num_rows += b.num_rows;
        for (name, values) in &b.columns {
            merged
                .entry(name.clone())
                .or_default()
                .extend(values.iter().cloned());
        }
    }
    let mut all = Batch::new(num_rows);
    for (name, values) in merged {
        all = all.with_column(name, values);
    }
    all
}

fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by(|a, b| compare_for_order(&a[0], &b[0], false));
    rows
}

#[test]
fn run_join_segments_matches_single_segment_run_join_for_inner_join_group_by() {
    let (facts, customers) = join_fixture();
    let plan = compile_join(
        &parse(
            "SELECT bench_customers.tier, SUM(bench.amount) FROM bench \
             JOIN bench_customers ON bench.customer_id = bench_customers.customer_id \
             GROUP BY bench_customers.tier",
        )
        .unwrap(),
    )
    .unwrap();

    let single = run_join(&concat(&facts), &customers, &plan).unwrap();
    let segmented = run_join_segments(
        facts.into_iter().map(InMemorySegment).collect(),
        &customers,
        &plan,
    )
    .unwrap();

    assert_eq!(sorted(segmented), sorted(single.clone()));
    // Sanity: three tiers, customer 3's rows dropped by the INNER join.
    assert_eq!(single.len(), 3);
}

#[test]
fn run_join_segments_matches_single_segment_run_join_for_left_join_group_by() {
    let (facts, customers) = join_fixture();
    let plan = compile_join(
        &parse(
            "SELECT bench_customers.tier, COUNT(bench.amount) FROM bench \
             LEFT JOIN bench_customers ON bench.customer_id = bench_customers.customer_id \
             GROUP BY bench_customers.tier",
        )
        .unwrap(),
    )
    .unwrap();

    let single = run_join(&concat(&facts), &customers, &plan).unwrap();
    let segmented = run_join_segments(
        facts.into_iter().map(InMemorySegment).collect(),
        &customers,
        &plan,
    )
    .unwrap();

    assert_eq!(sorted(segmented), sorted(single.clone()));
    // LEFT keeps customer 3 as a NULL-tier group: four groups.
    assert_eq!(single.len(), 4);
    assert!(single.iter().any(|row| row[0] == Value::Null));
}

/// #272: a probe-side error inside a segment (here: the left column the
/// probe program loads does not exist) is a typed `VmError`, not a panic
/// or an empty result -- `Segment::load` is fallible for exactly this.
#[test]
fn run_join_segments_reports_a_probe_error_from_inside_a_segment() {
    let (_, customers) = join_fixture();
    let plan = compile_join(
        &parse(
            "SELECT bench_customers.tier FROM bench \
             JOIN bench_customers ON bench.customer_id = bench_customers.customer_id",
        )
        .unwrap(),
    )
    .unwrap();
    let bad_left = Batch::new(1).with_column("bench.not_customer_id", vec![Value::Int(1)]);
    let err = run_join_segments(vec![InMemorySegment(bad_left)], &customers, &plan).unwrap_err();
    assert!(
        matches!(err, VmError::UnknownColumn { .. }),
        "expected UnknownColumn, got {err:?}"
    );
}

/// #272: `Vm::with_join_tables` shares a built table by `Arc`; a probe VM
/// seeded with it finds the table, and taking a register moves it out.
#[test]
fn vm_join_tables_are_shared_between_build_and_probe_vms() {
    let right = Batch::new(1)
        .with_column("id", vec![Value::Int(7)])
        .with_column("payload", vec![Value::Str("seven".into())]);
    let mut builder = Vm::new();
    builder
        .execute(
            &right,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "id".into(),
                },
                Opcode::LoadColumn {
                    reg: 1,
                    column: "payload".into(),
                },
                Opcode::HashBuild {
                    key_cols: vec![0].into(),
                    payload_cols: vec![1].into(),
                    table: 0,
                },
            ],
        )
        .unwrap();

    let left = Batch::new(2).with_column("fk", vec![Value::Int(7), Value::Int(8)]);
    let mut prober = Vm::with_join_tables(builder.join_tables());
    prober
        .execute(
            &left,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "fk".into(),
                },
                Opcode::HashProbe {
                    key_cols: vec![0].into(),
                    table: 0,
                    payload_dst: vec![1].into(),
                    kind: JoinKind::Inner,
                },
            ],
        )
        .unwrap();
    assert_eq!(
        prober.take_register(1).unwrap(),
        vec![Value::Str("seven".into())]
    );
    assert!(
        matches!(
            prober.take_register(1),
            Err(VmError::UnknownRegister { register: 1, .. })
        ),
        "a taken register is gone"
    );
}

/// #272 (found by the per-segment join tests): a `COUNT` merged across
/// segments must stay `Int`, exactly as a single segment's `COUNT` is --
/// `Combine` used to sum partial counts as `Float`, so the result *type*
/// depended on how many segments the scan happened to have.
#[test]
fn count_merged_across_segments_stays_an_integer() {
    let seg = |ids: Vec<i64>| {
        InMemorySegment(
            Batch::new(ids.len()).with_column("k", ids.into_iter().map(Value::Int).collect()),
        )
    };
    let segments = vec![seg(vec![1, 1, 2]), seg(vec![1, 2, 2]), seg(vec![])];
    let program = Program::new(vec![
        Instruction::new(Opcode::LoadColumn {
            reg: 0,
            column: "k".into(),
        }),
        Instruction::new(Opcode::GroupReduce {
            group_by: vec![0].into(),
            aggs: vec![(AggFunc::Count, None)].into(),
            agg_dst: vec![1].into(),
        }),
        Instruction::new(Opcode::Emit {
            registers: vec![0, 1].into(),
        }),
        Instruction::new(Opcode::Combine {
            agg_parts: vec![
                db_core::vm::batch::AggPart::GroupKey,
                db_core::vm::batch::AggPart::Count,
            ]
            .into(),
            num_group_keys: 1,
            distinct: false,
        }),
    ]);
    let mut rows = run(&segments, &program).unwrap();
    rows.sort_by(|a, b| compare_for_order(&a[0], &b[0], false));
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Int(3)],
            vec![Value::Int(2), Value::Int(3)],
        ]
    );
}

/// 0.76.1: a segment whose `load` fails (storage-side, not a VM invariant)
/// surfaces through `run` as `VmError::SegmentLoad`, and `Display` names it.
#[test]
fn a_failing_segment_load_is_a_segment_load_error() {
    struct Broken;
    impl Segment for Broken {
        fn load(&self) -> Result<Arc<Batch>, VmError> {
            Err(VmError::SegmentLoad {
                reason: "orders.parquet row group 3: column `amount`: bad page".to_string(),
            })
        }
    }
    let program = Program::new(vec![
        Instruction::new(Opcode::LoadColumn {
            reg: 0,
            column: "amount".into(),
        }),
        Instruction::new(Opcode::Emit {
            registers: vec![0].into(),
        }),
        Instruction::new(Opcode::Halt),
    ]);
    let err = run(&[Broken], &program).unwrap_err();
    assert!(matches!(err, VmError::SegmentLoad { .. }), "got {err:?}");
    assert_eq!(
        err.to_string(),
        "segment load failed: orders.parquet row group 3: column `amount`: bad page"
    );
}

#[test]
fn loading_an_unknown_column_is_a_vm_error() {
    let batch = Batch::new(1).with_column("id", vec![Value::Int(1)]);
    let segments = [InMemorySegment(batch)];
    let program = Program::new(vec![
        Instruction::new(Opcode::LoadColumn {
            reg: 0,
            column: "missing".into(),
        }),
        Instruction::new(Opcode::Halt),
    ]);
    let err = run(&segments, &program).unwrap_err();
    match err {
        VmError::UnknownColumn { column, .. } => assert_eq!(column, "missing"),
        other => panic!("expected VmError::UnknownColumn, got {other:?}"),
    }
}

/// db-core#231: an `Opcode::Window` for a function that needs an argument
/// register but was planned with `arg: None` is a typed error, not a
/// panic -- codegen guarantees the argument, so this only happens with a
/// hand-built (or buggy) program, which is exactly when the process must
/// not abort.
#[test]
fn window_function_without_its_argument_register_is_a_vm_error() {
    let batch = Batch::new(2).with_column("id", vec![Value::Int(1), Value::Int(2)]);
    let segments = [InMemorySegment(batch)];
    for func in [
        WindowFunc::Lag,
        WindowFunc::Lead,
        WindowFunc::FirstValue,
        WindowFunc::LastValue,
    ] {
        let program = Program::new(vec![
            Instruction::new(Opcode::LoadColumn {
                reg: 0,
                column: "id".into(),
            }),
            Instruction::new(Opcode::Window {
                func,
                arg: None,
                offset: None,
                partition_by: vec![].into(),
                order_by: vec![(0, false)].into(),
                dst: 1,
            }),
            Instruction::new(Opcode::Emit {
                registers: vec![1].into(),
            }),
            Instruction::new(Opcode::Halt),
        ]);
        assert_eq!(
            run(&segments, &program).err(),
            Some(VmError::MissingWindowArgument {
                opcode: "Window",
                func
            }),
            "{func:?}"
        );
    }
}

// db-core#223: `vm::batch::Vm`'s own direct API (`new`/`register`/
// `execute`/`run`), `run_parallel`/`run_parallel_top_n`/`TopN`/
// `Segment`, `AggFunc`, and `compare_for_order` -- previously reached
// only transitively through `vm::engine::run`.

struct OneShotSource(Option<Batch>);

impl Source for OneShotSource {
    fn next_batch(&mut self) -> Option<Batch> {
        self.0.take()
    }
}

struct StaticSegment(Batch);

impl Segment for StaticSegment {
    fn load(&self) -> Result<Arc<Batch>, VmError> {
        Ok(Arc::new(self.0.clone()))
    }
}

#[test]
fn vm_execute_then_register_reads_back_a_loaded_column() {
    let batch = Batch::new(2).with_column("id", vec![Value::Int(1), Value::Int(2)]);
    let mut vm = Vm::new();
    vm.execute(
        &batch,
        &[Opcode::LoadColumn {
            reg: 0,
            column: "id".into(),
        }],
    )
    .unwrap();
    assert_eq!(vm.register(0).unwrap(), &[Value::Int(1), Value::Int(2)]);
}

#[test]
fn vm_register_reports_unknown_register() {
    let vm = Vm::new();
    assert_eq!(
        vm.register(0).unwrap_err(),
        VmError::UnknownRegister {
            opcode: "register",
            register: 0
        }
    );
}

#[test]
fn vm_run_drives_a_custom_source_via_next_segment() {
    let batch = Batch::new(1).with_column("id", vec![Value::Int(7)]);
    let mut source = OneShotSource(Some(batch));
    let program = [
        Opcode::LoadColumn {
            reg: 0,
            column: "id".into(),
        },
        Opcode::Emit {
            registers: vec![0].into(),
        },
        Opcode::NextSegment { loop_start: 0 },
        Opcode::Halt,
    ];
    let mut vm = Vm::new();
    let rows = vm.run(&mut source, &program).unwrap();
    assert_eq!(rows, vec![vec![Value::Int(7)]]);
}

#[test]
fn run_parallel_concatenates_every_segment_in_order() {
    let segments = [
        StaticSegment(Batch::new(1).with_column("id", vec![Value::Int(1)])),
        StaticSegment(Batch::new(1).with_column("id", vec![Value::Int(2)])),
    ];
    let program = [
        Opcode::LoadColumn {
            reg: 0,
            column: "id".into(),
        },
        Opcode::Emit {
            registers: vec![0].into(),
        },
    ];
    let rows = db_core::vm::batch::run_parallel(&segments, &program).unwrap();
    assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}

#[test]
fn run_parallel_top_n_keeps_only_the_smallest_n_rows_across_segments() {
    let segments = [
        StaticSegment(Batch::new(2).with_column("id", vec![Value::Int(3), Value::Int(1)])),
        StaticSegment(Batch::new(1).with_column("id", vec![Value::Int(2)])),
    ];
    let program = [
        Opcode::LoadColumn {
            reg: 0,
            column: "id".into(),
        },
        Opcode::Emit {
            registers: vec![0].into(),
        },
    ];
    let spec = TopN {
        col: 0,
        descending: false,
        limit: 2,
    };
    let rows = db_core::vm::batch::run_parallel_top_n(&segments, &program, &spec).unwrap();
    assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}

#[test]
fn agg_func_variants_are_distinct() {
    assert_ne!(AggFunc::Count, AggFunc::Sum);
    assert_ne!(AggFunc::Min, AggFunc::Max);
    assert_eq!(AggFunc::Avg, AggFunc::Avg);
}

#[test]
fn compare_for_order_sorts_null_last_both_directions() {
    use std::cmp::Ordering;
    assert_eq!(
        compare_for_order(&Value::Null, &Value::Int(1), false),
        Ordering::Greater
    );
    assert_eq!(
        compare_for_order(&Value::Null, &Value::Int(1), true),
        Ordering::Greater
    );
    assert_eq!(
        compare_for_order(&Value::Int(1), &Value::Int(2), false),
        Ordering::Less
    );
    assert_eq!(
        compare_for_order(&Value::Int(1), &Value::Int(2), true),
        Ordering::Greater
    );
}

/// #266: `compare_for_order`'s `Str`/`Str` case compares `&str` directly
/// instead of `a.to_string().cmp(&b.to_string())` -- this reimplements
/// the old, pre-#266 logic verbatim as a reference and checks the two
/// agree on every pair drawn from a pool spanning every `Value` variant
/// (including `NULL`, `NaN`, and mixed-type pairs), both directions, over
/// many random combinations. A deterministic xorshift PRNG keeps this
/// dependency-free and reproducible (a fixed seed always exercises the
/// same pairs) rather than pulling in a property-testing crate for one
/// test.
#[test]
fn compare_for_order_agrees_with_the_old_stringify_based_comparator() {
    use std::cmp::Ordering;

    fn reference_compare_for_order(a: &Value, b: &Value, descending: bool) -> Ordering {
        let ord = match (matches!(a, Value::Null), matches!(b, Value::Null)) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
                _ => a.to_string().cmp(&b.to_string()),
            },
        };
        if descending {
            ord.reverse()
        } else {
            ord
        }
    }

    let pool = [
        Value::Null,
        Value::Int(0),
        Value::Int(1),
        Value::Int(-5),
        Value::Int(i64::MIN),
        Value::Int(i64::MAX),
        Value::Float(0.0),
        Value::Float(-0.0),
        Value::Float(1.5),
        Value::Float(-2.25),
        Value::Float(f64::NAN),
        Value::Float(f64::INFINITY),
        Value::Float(f64::NEG_INFINITY),
        Value::Bool(true),
        Value::Bool(false),
        Value::Str("".into()),
        Value::Str("a".into()),
        Value::Str("abc".into()),
        Value::Str("ABC".into()),
        Value::Str("\u{0}".into()),
        Value::Str("z".into()),
    ];

    // xorshift64: tiny, deterministic, no external crate.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for _ in 0..5000 {
        let a = &pool[(next() as usize) % pool.len()];
        let b = &pool[(next() as usize) % pool.len()];
        let descending = next() % 2 == 0;
        assert_eq!(
            compare_for_order(a, b, descending),
            reference_compare_for_order(a, b, descending),
            "a={a:?} b={b:?} descending={descending}"
        );
    }
}

/// db-core#232: an `Emit` with no registers used to emit zero rows -- a
/// planner bug read as an empty result. It is a typed error now, as are
/// `HashBuild`/`HashProbe` with no key columns.
#[test]
fn emit_with_no_registers_is_a_malformed_program_error() {
    let batch = Batch::new(2).with_column("id", vec![Value::Int(1), Value::Int(2)]);
    let program = Program::new(vec![
        Instruction::new(Opcode::Emit {
            registers: vec![].into(),
        }),
        Instruction::new(Opcode::Halt),
    ]);
    assert!(matches!(
        run(&[InMemorySegment(batch)], &program),
        Err(VmError::MalformedProgram { opcode: "Emit", .. })
    ));
}

/// db-core#232: a non-numeric partial aggregate used to merge as `0.0`
/// into a plausible wrong SUM; NULL (a segment that saw no rows) is the
/// additive identity, anything else non-numeric is a planner bug.
#[test]
fn finalize_rejects_a_non_numeric_partial_aggregate() {
    use db_core::vm::batch::AggPart;
    use db_core::vm::engine::finalize;
    let ok = finalize(
        &[AggPart::Sum],
        0,
        false,
        None,
        None,
        vec![vec![Value::Null], vec![Value::Int(3)]],
    )
    .unwrap();
    assert_eq!(ok, vec![vec![Value::Float(3.0)]]);
    assert!(matches!(
        finalize(
            &[AggPart::Sum],
            0,
            false,
            None,
            None,
            vec![vec![Value::Str("x".into())], vec![Value::Int(3)]],
        ),
        Err(VmError::MalformedProgram {
            opcode: "Combine",
            ..
        })
    ));
}
