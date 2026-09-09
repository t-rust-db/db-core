//! Per-opcode VM micro-benchmarks : the layer below the
//! end-to-end engine benchmark in the separate `t-rust-db/benchmark`
//! repo -- ns/op for a representative opcode from each of
//! `vm::batch::Opcode` and `vm::row::Opcode`, run over a synthetic
//! N-instruction `Program` (or, for the batch engine, a fixed-size
//! `Batch`). **Report only** -- `make perf`, not a CI gate (ADR 0015, tier 6).
//!
//! **Scoped down**: neither `Opcode` enum is exhaustively covered here.
//! Each is ~20-90 variants, most sharing one of a handful of execution
//! shapes (a register load, a binary op, a jump/seek, a cursor read);
//! benchmarking a representative one per shape gives the same
//! regression-localizing signal a full one-per-variant sweep would, at
//! a fraction of the maintenance cost. Widening this to more variants
//! (or a macro that generates one benchmark per `Opcode::name()`) is a
//! natural follow-up once a specific opcode needs its own tracked
//! number.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    dead_code,
    reason = "benches/ is unconstrained like tests/ (ADR 0015, tier 6)"
)]

mod common;

use std::hint::black_box;

use db_core::vm::batch::{Batch, MapOp, Opcode as BatchOpcode, Value as BatchValue, Vm as BatchVm};
use db_core::vm::row::{
    execute, Cursor, EphemeralTableCursor, Instruction as RowInstruction, Opcode as RowOpcode,
    Program as RowProgram, Value as RowValue, Vm as RowVm,
};

const ROWS: usize = 4096;

fn batch_fixture() -> Batch {
    let ids: Vec<BatchValue> = (0..ROWS as i64).map(BatchValue::Int).collect();
    Batch::new(ROWS).with_column("id", ids)
}

fn bench_batch_load_column(r: &mut common::Report) {
    let batch = batch_fixture();
    r.bench("vm_opcodes/batch::LoadColumn", || {
        let mut vm = BatchVm::new();
        vm.execute(
            black_box(&batch),
            &[BatchOpcode::LoadColumn {
                reg: 0,
                column: "id".into(),
            }],
        )
    });
}

fn bench_batch_map_and_filter(r: &mut common::Report) {
    let batch = batch_fixture();
    let program = [
        BatchOpcode::LoadColumn {
            reg: 0,
            column: "id".into(),
        },
        BatchOpcode::LoadConst {
            reg: 1,
            value: BatchValue::Int(2048),
        },
        BatchOpcode::Map {
            dst: 2,
            op: MapOp::Gt,
            a: 0,
            b: 1,
        },
        BatchOpcode::Filter { predicate: 2 },
        BatchOpcode::Emit {
            registers: vec![0].into(),
        },
    ];
    r.bench("vm_opcodes/batch::Map+Filter", || {
        let mut vm = BatchVm::new();
        vm.execute(black_box(&batch), &program)
    });
}

fn bench_batch_reduce(r: &mut common::Report) {
    let batch = batch_fixture();
    let program = [
        BatchOpcode::LoadColumn {
            reg: 0,
            column: "id".into(),
        },
        BatchOpcode::Reduce {
            func: db_core::vm::batch::AggFunc::Sum,
            src: Some(0),
            dst: 1,
        },
        BatchOpcode::Emit {
            registers: vec![1].into(),
        },
    ];
    r.bench("vm_opcodes/batch::Reduce", || {
        let mut vm = BatchVm::new();
        vm.execute(black_box(&batch), &program)
    });
}

fn bench_batch_emit(r: &mut common::Report) {
    // Isolates Emit's per-row transpose cost (#262): a bare
    // LoadColumn+Emit program, so the increment over LoadColumn alone
    // (bench_batch_load_column) is ~all Emit.
    let batch = batch_fixture();
    let program = [
        BatchOpcode::LoadColumn {
            reg: 0,
            column: "id".into(),
        },
        BatchOpcode::Emit {
            registers: vec![0].into(),
        },
    ];
    r.bench("vm_opcodes/batch::Emit", || {
        let mut vm = BatchVm::new();
        vm.execute(black_box(&batch), &program)
    });
}

fn bench_batch_emit_duplicate_register(r: &mut common::Report) {
    // Same register emitted twice (e.g. `SELECT a, a`) -- exercises the
    // clone-on-repeat path added in #262, distinct from the single-use
    // move path measured by `bench_batch_emit`.
    let batch = batch_fixture();
    let program = [
        BatchOpcode::LoadColumn {
            reg: 0,
            column: "id".into(),
        },
        BatchOpcode::Emit {
            registers: vec![0, 0].into(),
        },
    ];
    r.bench("vm_opcodes/batch::Emit (duplicate register)", || {
        let mut vm = BatchVm::new();
        vm.execute(black_box(&batch), &program)
    });
}

fn bench_row_scan_column(r: &mut common::Report) {
    // A plain `Rewind`/`Column`/`Next` scan loop -- the row engine's
    // cheapest, most-executed opcode sequence. The table is filled once,
    // outside the timed region: `Rewind` repositions the cursor on every
    // call, so only the scan itself is measured.
    let program = RowProgram::new(vec![
        RowInstruction::new(RowOpcode::Rewind, 0, 4, 0),
        RowInstruction::new(RowOpcode::Column, 0, 0, 1),
        RowInstruction::new(RowOpcode::Next, 0, 1, 0),
        RowInstruction::new(RowOpcode::Halt, 0, 0, 0),
    ]);
    let mut vm = RowVm::new();
    let mut table = EphemeralTableCursor::new();
    for i in 0..ROWS {
        table.insert(
            i64::try_from(i).unwrap_or(i64::MAX),
            vec![RowValue::Integer(i64::try_from(i).unwrap_or(i64::MAX))],
        );
    }
    vm.open_cursor(0, Box::new(table)).unwrap();
    assert!(execute(&mut vm, &program).is_ok());
    r.bench("vm_opcodes/row::Column (scan loop)", || {
        execute(&mut vm, black_box(&program))
    });
}

fn bench_row_compare(r: &mut common::Report) {
    let program = RowProgram::new(vec![
        RowInstruction::new(RowOpcode::Integer, 1, 0, 0),
        RowInstruction::new(RowOpcode::Integer, 2, 1, 0),
        RowInstruction::new(RowOpcode::Eq, 0, 4, 1),
        RowInstruction::new(RowOpcode::Halt, 0, 0, 0),
    ]);
    r.bench("vm_opcodes/row::Eq", || {
        let mut vm = RowVm::new();
        execute(&mut vm, black_box(&program))
    });
}

fn main() {
    let mut report = common::Report::new("vm_opcodes");
    bench_batch_load_column(&mut report);
    bench_batch_map_and_filter(&mut report);
    bench_batch_reduce(&mut report);
    bench_batch_emit(&mut report);
    bench_batch_emit_duplicate_register(&mut report);
    bench_row_scan_column(&mut report);
    bench_row_compare(&mut report);
    report.finish();
}
