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

use db_core::vm::batch::{Batch, Instruction, JoinKind, MapOp, Opcode, Program, Value, VmError};
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
