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

use db_core::vm::batch::{
    compare_for_order, AggFunc, Batch, Instruction, JoinKind, MapOp, Opcode, Program, Segment,
    Source, TopN, Value, Vm, VmError, WindowFunc,
};
use db_core::vm::engine::{run, run_join, InMemorySegment, JoinProgram};

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
    fn load(&self) -> Batch {
        self.0.clone()
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
