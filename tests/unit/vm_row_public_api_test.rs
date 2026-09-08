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

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use db_core::vm::row::{
    execute, Cursor, EphemeralTableCursor, ExecError, InMemoryCursor, Instruction, Opcode, Program,
    Value, Vm, P4,
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

/// db-core#226: an `OpenDup` sibling shares the ephemeral table's row
/// store (`Rc<RefCell<Vec<_>>>`). Every `Cursor` method takes and
/// releases its borrow within the call, so mutating through one cursor
/// while the other is mid-scan must never trip `RefCell`'s runtime
/// borrow check -- and, because rows are kept sorted by rowid and the
/// scan is positioned by rowid rather than by index, the mid-scan
/// cursor must still visit exactly the surviving rows in order.
#[test]
fn ephemeral_dup_sibling_can_mutate_while_the_original_is_mid_scan() {
    let mut a = EphemeralTableCursor::new();
    for rowid in 1..=5 {
        a.insert(rowid, vec![Value::Integer(rowid * 10)]);
    }
    let mut b = a.dup().expect("ephemeral tables support OpenDup");

    assert!(a.rewind());
    assert_eq!(a.rowid(), 1);

    // Through the sibling, while `a` is positioned on row 1: delete the
    // row `a` would visit next-but-one, and append one past the end.
    assert!(b.seek(3));
    assert!(b.delete());
    assert!(b.insert(6, vec![Value::Integer(60)]));

    // Read the column *inside* the scan: once `next()` has returned
    // false there is no current row, and `column()` on a positionless
    // cursor is one of the production `expect`s db-core#231 turns into
    // a typed error -- this test exercises the RefCell sharing, not that.
    let mut visited = vec![(a.rowid(), a.column(0))];
    while a.next() {
        visited.push((a.rowid(), a.column(0)));
    }
    assert_eq!(
        visited,
        vec![
            (1, Value::Integer(10)),
            (2, Value::Integer(20)),
            (4, Value::Integer(40)),
            (5, Value::Integer(50)),
            (6, Value::Integer(60)),
        ]
    );

    // And the other way round: `b` scanning while `a` mutates.
    assert!(b.rewind());
    assert!(a.seek(2));
    assert!(a.delete());
    let mut visited = vec![b.rowid()];
    while b.next() {
        visited.push(b.rowid());
    }
    assert_eq!(visited, vec![1, 4, 5, 6]);
}
