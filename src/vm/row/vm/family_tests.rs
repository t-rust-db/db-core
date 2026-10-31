// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The opcode families' error paths (#670): each family function is
//! called directly with one instruction, so a test names the exact branch
//! it reaches -- a `P4` of the wrong kind, a value of the wrong type, a
//! cursor that cannot do what the opcode asks, an opcode dispatched to the
//! wrong family -- and the `ExecError` it must produce.

use super::super::cursor::{HashAggCursor, InMemoryCursor, SorterCursor};
use super::*;
use crate::value::Collation;

fn instr(opcode: Opcode, p1: i32, p2: i32, p3: i32) -> Instruction {
    Instruction::new(opcode, p1, p2, p3)
}

fn with_p4(opcode: Opcode, p1: i32, p2: i32, p3: i32, p4: P4) -> Instruction {
    Instruction::with_p4(opcode, p1, p2, p3, p4)
}

/// `result` is a `MalformedInstruction` from `opcode`.
fn malformed(result: Result<Step, ExecError>, opcode: &str) -> bool {
    matches!(result, Err(ExecError::MalformedInstruction { opcode: o, .. }) if o == opcode)
}

/// `result` is a `TypeMismatch` from `opcode`.
fn mismatch(result: Result<Step, ExecError>, opcode: &str) -> bool {
    matches!(result, Err(ExecError::TypeMismatch { opcode: o, .. }) if o == opcode)
}

/// A VM with an in-memory cursor (no rows) in slot 0, which can delete
/// but neither insert nor update.
fn vm_with_memory_cursor() -> Vm {
    let mut vm = Vm::new();
    vm.open_cursor(0, Box::new(InMemoryCursor::new(Vec::new())))
        .unwrap();
    vm
}

/// A VM with a sorter (no keys) in slot 0, which supports only sorting.
fn vm_with_sorter() -> Vm {
    let mut vm = Vm::new();
    vm.open_cursor(0, Box::new(SorterCursor::new(Vec::new(), None)))
        .unwrap();
    vm
}

/// Every family refuses an opcode that belongs to another one: `step`
/// never sends it there, so it is a malformed program, not a panic.
#[test]
fn a_family_refuses_an_opcode_of_another_family() {
    let column = instr(Opcode::Column, 0, 0, 0);
    let goto = instr(Opcode::Goto, 0, 0, 0);
    let mut vm = Vm::new();
    assert!(malformed(step_control(&mut vm, 0, &column), "step"));
    assert!(malformed(step_expr(&mut vm, &goto), "step"));
    assert!(malformed(step_value(&mut vm, &goto), "step"));
    assert!(malformed(step_cursor_ops(&mut vm, &goto), "step"));
    assert!(malformed(step_sort_agg(&mut vm, &goto), "step"));
    assert!(malformed(step_write(&mut vm, &goto), "step"));
    assert!(malformed(step_schema(&mut vm, &goto), "step"));
}

#[test]
fn control_addresses_past_a_pc_are_malformed() {
    let mut vm = Vm::new();
    // `Init` without a target falls through.
    assert!(matches!(
        step_control(&mut vm, 0, &instr(Opcode::Init, 0, 0, 0)),
        Ok(Step::Next)
    ));
    // A `Gosub` at an address an `i32` cannot hold.
    assert!(malformed(
        step_control(&mut vm, usize::MAX, &instr(Opcode::Gosub, 1, 5, 0)),
        "Gosub"
    ));
    // A `Return` to an address past `i32`, or to a non-integer.
    vm.set_register(0, Value::Integer(i64::MAX)).unwrap();
    assert!(malformed(
        step_control(&mut vm, 0, &instr(Opcode::Return, 0, 0, 0)),
        "Return"
    ));
    vm.set_register(0, Value::Text("x".into())).unwrap();
    assert!(mismatch(
        step_control(&mut vm, 0, &instr(Opcode::Return, 0, 0, 0)),
        "Return"
    ));
    // `IfPos` on a positive register jumps and subtracts `p3`.
    vm.set_register(0, Value::Integer(3)).unwrap();
    assert!(matches!(
        step_control(&mut vm, 0, &instr(Opcode::IfPos, 0, 7, 2)),
        Ok(Step::Jump(7))
    ));
    assert_eq!(vm.register(0).unwrap(), &Value::Integer(1));
}

#[test]
fn function_and_value_opcodes_refuse_a_wrong_p4() {
    let mut vm = Vm::new();
    assert!(malformed(
        step_expr(&mut vm, &with_p4(Opcode::Function, 0, 0, 1, P4::Int(1))),
        "Function"
    ));
    assert!(malformed(
        step_expr(
            &mut vm,
            &with_p4(Opcode::Function, 0, 0, 1, P4::Str("upper".into()))
        ),
        "Function"
    ));
    assert!(malformed(
        step_value(&mut vm, &with_p4(Opcode::String8, 0, 0, 0, P4::Int(1))),
        "String8"
    ));
}

#[test]
fn cursor_ops_jump_past_an_empty_cursor_and_refuse_a_foreign_one() {
    let mut vm = vm_with_memory_cursor();
    for opcode in [
        Opcode::Rewind,
        Opcode::Last,
        Opcode::IdxRewind,
        Opcode::IdxLast,
    ] {
        assert!(
            matches!(
                step_cursor_ops(&mut vm, &instr(opcode, 0, 9, 0)),
                Ok(Step::Jump(9))
            ),
            "{opcode:?}"
        );
    }
    // An index comparison on a table cursor, which has no index entry.
    vm.set_register(1, Value::Integer(1)).unwrap();
    let key = P4::SeekKey(vec![Collation::Binary]);
    for (opcode, name) in [
        (Opcode::IdxCompareGT, "IdxCompareGT"),
        (Opcode::IdxCompareGE, "IdxCompareGE"),
        (Opcode::IdxLE, "IdxLE"),
    ] {
        assert!(malformed(
            step_cursor_ops(&mut vm, &with_p4(opcode, 0, 9, 1, key.clone())),
            name
        ));
    }
    // An automatic-index insert into a slot that holds another cursor.
    assert!(malformed(
        step_cursor_ops(
            &mut vm,
            &with_p4(Opcode::AutoIndexInsert, 0, 1, 1, key.clone())
        ),
        "AutoIndexInsert"
    ));
}

#[test]
fn sorter_and_hash_aggregation_refuse_wrong_p4s_values_and_cursors() {
    let mut vm = vm_with_memory_cursor();
    assert!(malformed(
        step_sort_agg(&mut vm, &with_p4(Opcode::SorterOpen, 1, 0, 0, P4::Int(0))),
        "SorterOpen"
    ));
    // A bound register that is not a non-negative integer is unbounded.
    vm.set_register(2, Value::Text("x".into())).unwrap();
    assert!(matches!(
        step_sort_agg(
            &mut vm,
            &with_p4(Opcode::SorterOpen, 1, 2, 0, P4::SortKey(Vec::new())).with_p5(1)
        ),
        Ok(Step::Next)
    ));
    assert!(mismatch(
        step_sort_agg(&mut vm, &instr(Opcode::SorterInsert, 0, 2, 0)),
        "SorterInsert"
    ));
    vm.set_register(3, Value::Blob(vec![2, 1].into())).unwrap();
    assert!(malformed(
        step_sort_agg(&mut vm, &instr(Opcode::SorterInsert, 0, 3, 0)),
        "SorterInsert"
    ));
    assert!(malformed(
        step_sort_agg(&mut vm, &with_p4(Opcode::HashAggOpen, 1, 0, 0, P4::Int(0))),
        "HashAggOpen"
    ));
    assert!(mismatch(
        step_sort_agg(&mut vm, &instr(Opcode::HashAggFind, 0, 2, 0)),
        "HashAggFind"
    ));
    assert!(malformed(
        step_sort_agg(&mut vm, &instr(Opcode::HashAggFind, 0, 3, 0)),
        "HashAggFind"
    ));
    assert!(malformed(
        step_sort_agg(&mut vm, &with_p4(Opcode::HashAggStep, 0, 0, 0, P4::Int(0))),
        "HashAggStep"
    ));
    // A step into a hash aggregation that has no current group, with an
    // argument register past the register file (which grows to hold it).
    vm.open_cursor(4, Box::new(HashAggCursor::new(Vec::new())))
        .unwrap();
    let count = P4::AggFunc {
        name: "count".into(),
        arity: 1,
        collation: Collation::Binary,
    };
    assert!(malformed(
        step_sort_agg(&mut vm, &with_p4(Opcode::HashAggStep, 0, 50, 4, count)),
        "HashAggStep"
    ));
}

#[test]
fn aggregate_steps_refuse_wrong_p4s_and_unknown_functions() {
    let mut vm = Vm::new();
    assert!(malformed(
        step_sort_agg(&mut vm, &with_p4(Opcode::AggStep, 0, 0, 0, P4::Int(0))),
        "AggStep"
    ));
    let unknown = P4::AggFunc {
        name: "no_such_aggregate".into(),
        arity: 0,
        collation: Collation::Binary,
    };
    // An unknown aggregate is sqlite3's "no such aggregate" (#892).
    assert!(matches!(
        step_sort_agg(&mut vm, &with_p4(Opcode::AggStep, 0, 0, 0, unknown)),
        Err(ExecError::AggregateFailed(
            crate::vm::row::AggregateError::Unknown { .. }
        ))
    ));
    assert!(malformed(
        step_sort_agg(&mut vm, &with_p4(Opcode::AggFinal, 0, 0, 1, P4::Int(0))),
        "AggFinal"
    ));
    assert!(malformed(
        step_sort_agg(
            &mut vm,
            &with_p4(Opcode::AggFinal, 0, 0, 1, P4::Str("count".into()))
        ),
        "AggFinal"
    ));
    assert!(matches!(
        step_sort_agg(
            &mut vm,
            &with_p4(
                Opcode::AggFinal,
                0,
                0,
                1,
                P4::Str("no_such_aggregate(0)".into())
            )
        ),
        Err(ExecError::AggregateFailed(
            crate::vm::row::AggregateError::Unknown { .. }
        ))
    ));
}

#[test]
fn schema_opcodes_refuse_a_wrong_p4() {
    let mut vm = Vm::new();
    for (opcode, name) in [
        (Opcode::CreateTable, "CreateTable"),
        (Opcode::CreateView, "CreateView"),
        (Opcode::DropView, "DropView"),
        (Opcode::CreateIndex, "CreateIndex"),
        (Opcode::DropTable, "DropTable"),
        (Opcode::DropIndex, "DropIndex"),
        (Opcode::Analyze, "Analyze"),
    ] {
        assert!(
            malformed(
                step_schema(&mut vm, &with_p4(opcode, 0, 0, 0, P4::Int(0))),
                name
            ),
            "{name}"
        );
    }
}

#[test]
fn writes_refuse_wrong_types_bad_records_and_cursors_that_cannot_write() {
    let mut vm = vm_with_memory_cursor();
    vm.set_register(1, Value::Integer(1)).unwrap();
    vm.set_register(2, Value::Text("not a blob".into()))
        .unwrap();
    vm.set_register(3, Value::Blob(vec![2, 1].into())).unwrap();
    // A record whose header names one NULL column: decodes, but an
    // in-memory cursor cannot insert it.
    vm.set_register(4, Value::Blob(vec![2, 0].into())).unwrap();
    for (opcode, name) in [(Opcode::Insert, "Insert"), (Opcode::Update, "Update")] {
        // A non-integer rowid, a non-blob record, a record that does not
        // decode.
        assert!(mismatch(step_write(&mut vm, &instr(opcode, 0, 2, 3)), name));
        assert!(mismatch(step_write(&mut vm, &instr(opcode, 0, 1, 2)), name));
        assert!(malformed(
            step_write(&mut vm, &instr(opcode, 0, 1, 3)),
            name
        ));
    }
    assert!(malformed(
        step_write(&mut vm, &instr(Opcode::Insert, 0, 1, 4)),
        "Insert"
    ));
    assert!(malformed(
        step_write(&mut vm, &instr(Opcode::Update, 0, 1, 4)),
        "Update"
    ));
    // A sorter can neither delete nor rewrite a row.
    let mut sorter = vm_with_sorter();
    sorter.set_register(1, Value::Integer(1)).unwrap();
    sorter
        .set_register(4, Value::Blob(vec![2, 0].into()))
        .unwrap();
    assert!(malformed(
        step_write(&mut sorter, &instr(Opcode::Delete, 0, 0, 0)),
        "Delete"
    ));
    assert!(malformed(
        step_write(&mut sorter, &instr(Opcode::Update, 0, 1, 4)),
        "Update"
    ));
    // AUTOINCREMENT needs the table's name in P4.
    assert!(malformed(
        step_write(
            &mut vm,
            &with_p4(Opcode::NewRowid, 0, 5, 0, P4::Int(0)).with_p5(1)
        ),
        "NewRowid"
    ));
    // An index write into a cursor that is not an index.
    let key = P4::SeekKey(vec![Collation::Binary]);
    assert!(malformed(
        step_write(
            &mut sorter,
            &with_p4(Opcode::IdxInsert, 0, 1, 0, key.clone())
        ),
        "IdxInsert"
    ));
    assert!(malformed(
        step_write(&mut sorter, &with_p4(Opcode::IdxDelete, 0, 1, 0, key)),
        "IdxDelete"
    ));
}

/// `Update` without a combined rewrite deletes the current row and
/// inserts the new one: a record that does not decode, or a cursor that
/// cannot insert, is refused after the delete.
#[test]
fn an_update_through_delete_and_insert_refuses_a_bad_record_or_insert() {
    for (record, name) in [
        (vec![2u8, 1], "undecodable"),
        (vec![2u8, 0], "uninsertable"),
    ] {
        let mut vm = Vm::new();
        vm.open_cursor(
            0,
            Box::new(InMemoryCursor::new(vec![vec![Value::Integer(1)]])),
        )
        .unwrap();
        assert!(matches!(
            step_cursor_ops(&mut vm, &instr(Opcode::Rewind, 0, 9, 0)),
            Ok(Step::Next)
        ));
        vm.set_register(1, Value::Integer(1)).unwrap();
        vm.set_register(2, Value::Blob(record.into())).unwrap();
        // Past the delete: the error is the decode's or the insert's.
        match step_write(&mut vm, &instr(Opcode::Update, 0, 1, 2)) {
            Err(ExecError::MalformedInstruction {
                opcode: "Update",
                reason,
            }) => {
                assert!(!reason.contains("deletion"), "{name}: {reason}");
                assert_eq!(
                    reason.contains("does not support update"),
                    name == "uninsertable",
                    "{name}: {reason}"
                );
            }
            other => panic!("{name}: {other:?}"),
        }
    }
}
