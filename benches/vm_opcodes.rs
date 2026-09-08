//! Per-opcode VM micro-benchmarks (db-core#224): the layer below the
//! end-to-end engine benchmark in the separate `t-rust-db/benchmark`
//! repo -- ns/op for a representative opcode from each of
//! `vm::batch::Opcode` and `vm::row::Opcode`, run over a synthetic
//! N-instruction `Program` (or, for the batch engine, a fixed-size
//! `Batch`). **Report only** -- `make perf`, not a CI gate.
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
    reason = "benches/ is unconstrained like tests/ (db-core#224); criterion's own timing loop needs unwrap/index freely"
)]

use criterion::{black_box, criterion_group, criterion_main, Criterion};

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

fn bench_batch_load_column(c: &mut Criterion) {
    let batch = batch_fixture();
    c.bench_function("vm_opcodes/batch::LoadColumn", |b| {
        b.iter(|| {
            let mut vm = BatchVm::new();
            vm.execute(
                black_box(&batch),
                &[BatchOpcode::LoadColumn {
                    reg: 0,
                    column: "id".into(),
                }],
            )
        });
    });
}

fn bench_batch_map_and_filter(c: &mut Criterion) {
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
    c.bench_function("vm_opcodes/batch::Map+Filter", |b| {
        b.iter(|| {
            let mut vm = BatchVm::new();
            vm.execute(black_box(&batch), &program)
        });
    });
}

fn bench_batch_reduce(c: &mut Criterion) {
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
    c.bench_function("vm_opcodes/batch::Reduce", |b| {
        b.iter(|| {
            let mut vm = BatchVm::new();
            vm.execute(black_box(&batch), &program)
        });
    });
}

fn bench_row_scan_column(c: &mut Criterion) {
    // A plain `Rewind`/`Column`/`Next` scan loop -- the row engine's
    // cheapest, most-executed opcode sequence.
    let program = RowProgram::new(vec![
        RowInstruction::new(RowOpcode::Rewind, 0, 4, 0),
        RowInstruction::new(RowOpcode::Column, 0, 0, 1),
        RowInstruction::new(RowOpcode::Next, 0, 1, 0),
        RowInstruction::new(RowOpcode::Halt, 0, 0, 0),
    ]);
    c.bench_function("vm_opcodes/row::Column (scan loop)", |b| {
        b.iter(|| {
            let mut vm = RowVm::new();
            let mut table = EphemeralTableCursor::new();
            for i in 0..ROWS {
                table.insert(
                    i64::try_from(i).unwrap_or(i64::MAX),
                    vec![RowValue::Integer(i64::try_from(i).unwrap_or(i64::MAX))],
                );
            }
            vm.open_cursor(0, Box::new(table)).unwrap();
            execute(&mut vm, black_box(&program))
        });
    });
}

fn bench_row_compare(c: &mut Criterion) {
    let program = RowProgram::new(vec![
        RowInstruction::new(RowOpcode::Integer, 1, 0, 0),
        RowInstruction::new(RowOpcode::Integer, 2, 1, 0),
        RowInstruction::new(RowOpcode::Eq, 0, 4, 1),
        RowInstruction::new(RowOpcode::Halt, 0, 0, 0),
    ]);
    c.bench_function("vm_opcodes/row::Eq", |b| {
        b.iter(|| {
            let mut vm = RowVm::new();
            execute(&mut vm, black_box(&program))
        });
    });
}

criterion_group!(
    batch_benches,
    bench_batch_load_column,
    bench_batch_map_and_filter,
    bench_batch_reduce
);
criterion_group!(row_benches, bench_row_scan_column, bench_row_compare);
criterion_main!(batch_benches, row_benches);
