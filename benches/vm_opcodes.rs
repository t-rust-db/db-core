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

use db_core::vm::batch::{
    compare_for_order, Batch, MapOp, Opcode as BatchOpcode, Value as BatchValue, Vm as BatchVm,
    WindowFunc,
};
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

fn bench_batch_filter_many_registers(r: &mut common::Report) {
    // #265: isolates Filter's own cost across several live registers, with
    // no consumer after it -- pre-#265 this eagerly compacted every one of
    // the 4 registers; now it's a single index-buffer allocation
    // regardless of how many registers happen to be live.
    let ids: Vec<BatchValue> = (0..ROWS as i64).map(BatchValue::Int).collect();
    let batch = Batch::new(ROWS)
        .with_column("a", ids.clone())
        .with_column("b", ids.clone())
        .with_column("c", ids.clone())
        .with_column("d", ids);
    let program = [
        BatchOpcode::LoadColumn {
            reg: 0,
            column: "a".into(),
        },
        BatchOpcode::LoadColumn {
            reg: 1,
            column: "b".into(),
        },
        BatchOpcode::LoadColumn {
            reg: 2,
            column: "c".into(),
        },
        BatchOpcode::LoadColumn {
            reg: 3,
            column: "d".into(),
        },
        BatchOpcode::LoadConst {
            reg: 4,
            value: BatchValue::Int(ROWS as i64 / 2),
        },
        BatchOpcode::Map {
            dst: 5,
            op: MapOp::Gt,
            a: 0,
            b: 4,
        },
        BatchOpcode::Filter { predicate: 5 },
    ];
    r.bench(
        "vm_opcodes/batch::Filter (4 live registers, no consumer)",
        || {
            let mut vm = BatchVm::new();
            vm.execute(black_box(&batch), &program)
        },
    );
}

fn bench_string_order_by_sort(r: &mut common::Report) {
    // #266: isolates compare_for_order's Str/Str case -- pre-#266 every
    // comparison allocated two Strings via to_string(); now it's a direct
    // &str compare. Sorts the same shuffled string column each call
    // (Vec::sort_by doesn't mutate its input's identity, only order).
    let values: Vec<BatchValue> = (0..ROWS)
        .map(|i| BatchValue::Str(format!("row-{:06}", (i * 2654435761) % ROWS).into()))
        .collect();
    r.bench("vm_opcodes/batch::compare_for_order (Str/Str sort)", || {
        let mut rows = values.clone();
        rows.sort_by(|a, b| compare_for_order(black_box(a), black_box(b), false));
        rows
    });
}

fn bench_window_partition_by_string(r: &mut common::Report) {
    // #266: isolates compute_window's partition-key building -- pre-#266
    // this stringified+joined every partition column per row; now it's a
    // typed GroupKey (#263) with no stringify at all.
    let ids: Vec<BatchValue> = (0..ROWS as i64).map(BatchValue::Int).collect();
    let parts: Vec<BatchValue> = (0..ROWS)
        .map(|i| BatchValue::Str(format!("group-{}", i % 100).into()))
        .collect();
    let batch = Batch::new(ROWS)
        .with_column("id", ids)
        .with_column("part", parts);
    let program = [
        BatchOpcode::LoadColumn {
            reg: 0,
            column: "id".into(),
        },
        BatchOpcode::LoadColumn {
            reg: 1,
            column: "part".into(),
        },
        BatchOpcode::Window {
            func: WindowFunc::RowNumber,
            arg: None,
            offset: None,
            partition_by: vec![1].into(),
            order_by: vec![(0, false)].into(),
            dst: 2,
        },
    ];
    r.bench("vm_opcodes/batch::Window (PARTITION BY Str)", || {
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

/// A `GroupReduce` key column cycling through `cardinality` distinct
/// values -- `cardinality == ROWS` is the worst case (every row its own
/// group), `cardinality << ROWS` the common low/medium-cardinality case.
fn group_key_column(rows: usize, cardinality: usize) -> Vec<BatchValue> {
    (0..rows as i64)
        .map(|i| BatchValue::Int(i % cardinality as i64))
        .collect()
}

fn bench_batch_group_reduce(r: &mut common::Report) {
    // #263: typed GroupKey vs. the old to_string/join string key --
    // low/medium cardinality should land near the ~2-3x #183 spike found;
    // unique cardinality (every row its own group) is the spike's
    // documented regression case, kept here so a real run always reports
    // both instead of only the favorable one.
    for (label, cardinality) in [
        ("low (100 groups)", 100),
        ("medium (1% of rows)", ROWS / 100),
        ("unique (no grouping)", ROWS),
    ] {
        let batch = Batch::new(ROWS).with_column("key", group_key_column(ROWS, cardinality.max(1)));
        let program = [
            BatchOpcode::LoadColumn {
                reg: 0,
                column: "key".into(),
            },
            BatchOpcode::GroupReduce {
                group_by: vec![0].into(),
                aggs: vec![(db_core::vm::batch::AggFunc::Count, None)].into(),
                agg_dst: vec![1].into(),
            },
        ];
        r.bench(&format!("vm_opcodes/batch::GroupReduce ({label})"), || {
            let mut vm = BatchVm::new();
            vm.execute(black_box(&batch), &program)
        });
    }
}

fn bench_batch_hash_join(r: &mut common::Report) {
    // #272: fact/dimension probe -- ROWS probe rows against a 1%-cardinality
    // build side with a `Str` payload (the parity `join` shape). The probe
    // used to allocate a key `Vec`, a `get_all` `Vec` and a payload clone
    // per row; now zero allocations per row for integer keys.
    let dim = ROWS / 100;
    let right = Batch::new(dim)
        .with_column("id", (0..dim as i64).map(BatchValue::Int).collect())
        .with_column(
            "tier",
            (0..dim)
                .map(|i| BatchValue::Str(["bronze", "silver", "gold"][i % 3].into()))
                .collect(),
        );
    let left = Batch::new(ROWS).with_column(
        "fk",
        (0..ROWS as i64)
            .map(|i| BatchValue::Int(i % dim as i64))
            .collect(),
    );
    let mut builder = BatchVm::new();
    builder
        .execute(
            &right,
            &[
                BatchOpcode::LoadColumn {
                    reg: 0,
                    column: "id".into(),
                },
                BatchOpcode::LoadColumn {
                    reg: 1,
                    column: "tier".into(),
                },
                BatchOpcode::HashBuild {
                    key_cols: vec![0].into(),
                    payload_cols: vec![1].into(),
                    table: 0,
                },
            ],
        )
        .unwrap();
    let tables = builder.join_tables();
    let probe = [
        BatchOpcode::LoadColumn {
            reg: 0,
            column: "fk".into(),
        },
        BatchOpcode::HashProbe {
            key_cols: vec![0].into(),
            table: 0,
            payload_dst: vec![1].into(),
            kind: db_core::vm::batch::JoinKind::Inner,
        },
    ];
    r.bench("vm_opcodes/batch::HashProbe (1% dim, Str payload)", || {
        let mut vm = BatchVm::with_join_tables(tables.clone());
        vm.execute(black_box(&left), &probe)
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
    bench_batch_filter_many_registers(&mut report);
    bench_batch_reduce(&mut report);
    bench_batch_group_reduce(&mut report);
    bench_string_order_by_sort(&mut report);
    bench_window_partition_by_string(&mut report);
    bench_batch_hash_join(&mut report);
    bench_batch_emit(&mut report);
    bench_batch_emit_duplicate_register(&mut report);
    bench_row_scan_column(&mut report);
    bench_row_compare(&mut report);
    report.finish();
}
