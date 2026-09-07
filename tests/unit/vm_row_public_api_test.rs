// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for `db_core::execute`, the row VM's sole public entry
//! point. Every existing test that drives it does so with a program
//! `codegen::row` actually emitted, so the opcode combinations exercised
//! are exactly those the current compiler happens to produce. This
//! suite hand-builds `Program`s instead -- scan-and-emit over a
//! pre-wired [`InMemoryCursor`] (the "no factory installed" path
//! `CursorFactory`'s own doc describes), a jump/loop shape, and both
//! `ExecError` paths a caller can hit directly -- independent of
//! whatever codegen currently generates.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use db_core::vm::row::{
    execute, ExecError, InMemoryCursor, Instruction, Opcode, Program, Value, Vm, P4,
};

#[test]
fn scans_a_pre_wired_cursor_and_emits_every_row() {
    // Mirrors `CursorFactory`'s documented fallback: with no factory
    // installed, a cursor pre-wired via `Vm::open_cursor` ahead of time
    // is what `Rewind`/`Next`/`Column` walk.
    let cursor = InMemoryCursor::new(vec![
        vec![Value::Integer(1), Value::Text("a".into())],
        vec![Value::Integer(2), Value::Text("b".into())],
    ]);
    let mut vm = Vm::new();
    vm.open_cursor(0, Box::new(cursor)).unwrap();

    let program = Program::new(vec![
        Instruction::new(Opcode::Rewind, 0, 6, 0), // pc0: no rows -> pc6 (Halt)
        Instruction::new(Opcode::Column, 0, 0, 1), // pc1: reg1 = col0
        Instruction::new(Opcode::Column, 0, 1, 2), // pc2: reg2 = col1
        Instruction::new(Opcode::ResultRow, 1, 2, 0), // pc3
        Instruction::new(Opcode::Next, 0, 1, 0),   // pc4: another row -> pc1
        Instruction::new(Opcode::Goto, 0, 6, 0),   // pc5: exhausted -> Halt
        Instruction::new(Opcode::Halt, 0, 0, 0),   // pc6
    ]);

    let rows = execute(&mut vm, &program).unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Text("a".into())],
            vec![Value::Integer(2), Value::Text("b".into())],
        ]
    );
}

#[test]
fn a_decrement_loop_accumulates_across_iterations() {
    let mut vm = Vm::new();
    vm.set_register(0, Value::Integer(3)).unwrap();
    let program = Program::new(vec![
        Instruction::new(Opcode::DecrJumpZero, 0, 4, 0), // pc0: loop counter -> pc4 when it hits 0
        Instruction::new(Opcode::Integer, 1, 1, 0),      // pc1
        Instruction::new(Opcode::ResultRow, 1, 1, 0),    // pc2
        Instruction::new(Opcode::Goto, 0, 0, 0),         // pc3: back to loop top
        Instruction::new(Opcode::Halt, 0, 0, 0),         // pc4
    ]);
    // Decrements 3 -> 2 -> 1 -> 0, jumping away only once it hits 0, so
    // the guarded body (append a row) runs on the first two passes.
    let rows = execute(&mut vm, &program).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r == &vec![Value::Integer(1)]));
}

#[test]
fn halt_with_a_nonzero_code_and_message_surfaces_as_an_error() {
    let mut vm = Vm::new();
    let program = Program::new(vec![Instruction::with_p4(
        Opcode::Halt,
        19,
        0,
        0,
        P4::Str("UNIQUE constraint failed".to_string()),
    )]);
    let err = execute(&mut vm, &program).unwrap_err();
    match err {
        ExecError::Halted { code, message } => {
            assert_eq!(code, 19);
            assert_eq!(message.as_deref(), Some("UNIQUE constraint failed"));
        }
        other => panic!("expected ExecError::Halted, got {other:?}"),
    }
}

#[test]
fn running_off_the_end_of_the_program_is_a_program_counter_error() {
    let mut vm = Vm::new();
    // No `Halt` at all: `Goto` past the last real instruction runs off
    // the end of `program.instructions`.
    let program = Program::new(vec![Instruction::new(Opcode::Goto, 0, 5, 0)]);
    let err = execute(&mut vm, &program).unwrap_err();
    assert!(matches!(err, ExecError::ProgramCounterOutOfRange { pc: 5 }));
}
