//! Spike for db-core#141, round 3 -- exercises the REAL `vm::batch`
//! constructs (`Batch`, `Opcode`, `Program`, `Vm`, `Segment`,
//! `run_parallel`), not the synthetic `Vec<T>`-vs-`Vec<Value>` loops of
//! rounds 1/2 (`01_neon_batch_kernels.rs` / `02_neon_adjusted.rs`, same
//! folder).
//!
//! Rounds 1/2 answered "does LLVM autovectorize a typed-column loop
//! shaped like a future #130 kernel?" This round asks the question #141
//! actually needs before that matters: **how much of the current
//! per-batch cost is opcode/interpreter overhead** (register `HashMap`
//! insert/lookup, `Vec<Value>::clone()` on every `LoadColumn`, the
//! `Filter` drain-and-rebuild, `Emit`'s row-major transpose) **versus the
//! raw elementwise work** the earlier rounds measured? If interpreter
//! overhead dominates at `BATCH_SIZE` (1024) granularity, a SIMD win in
//! the elementwise kernel is diluted; if it's small, rounds 1/2's numbers
//! translate more directly.
//!
//! No production code changes -- this only calls `db_core::vm::batch`'s
//! existing public API.
//!
//! # Running
//!
//! ```sh
//! cargo test --release --test 03_batch_specific -- --ignored --nocapture
//! # or: make -C tests/spike/001_neon_batch_kernels run-03
//! ```

use db_core::expr::AggFunc;
use db_core::vm::batch::{
    run_parallel, Batch, MapOp, Opcode, Segment, Value, Vm, BATCH_SIZE,
};
use std::borrow::Cow;
use std::hint::black_box;
use std::time::{Duration, Instant};

/// Total rows across all segments -- same order of magnitude as rounds
/// 1/2's streaming size, split into real `BATCH_SIZE`-row segments the
/// way the production executor actually sees data.
const ROWS_TOTAL: usize = 1_000_000;

fn time_it<F: FnMut()>(reps: usize, mut f: F) -> Duration {
    f(); // untimed warmup
    let mut samples: Vec<Duration> = (0..reps)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .collect();
    samples.sort();
    samples[reps / 2]
}

fn report(kernel: &str, vm: Duration, raw: Duration) {
    let overhead = vm.as_secs_f64() / raw.as_secs_f64();
    println!(
        "{kernel:<38} vm={:>10?}  raw_loop={:>10?}  vm/raw={overhead:.2}x",
        vm, raw
    );
}

// ---------------------------------------------------------------------
// Fixture: real `Batch`es of `BATCH_SIZE` rows, as a `Segment` impl so
// `run_parallel` (the actual morsel-driven executor, see
// `src/vm/batch.rs::run_parallel`) can drive them directly.
// ---------------------------------------------------------------------

struct PrebuiltSegment(Batch);

impl Segment for PrebuiltSegment {
    fn load(&self) -> Batch {
        // A real `Segment` re-materializes a batch per call (e.g. decoding
        // a Parquet row group); cloning the prebuilt one here stands in
        // for that cost rather than eliding it.
        self.0.clone()
    }
}

fn build_segments(rows_total: usize) -> Vec<Batch> {
    let mut segments = Vec::with_capacity(rows_total.div_ceil(BATCH_SIZE));
    let mut row = 0i64;
    while (row as usize) < rows_total {
        let n = BATCH_SIZE.min(rows_total - row as usize);
        let a: Vec<Value> = (0..n as i64).map(|i| Value::Int(row + i)).collect();
        let b: Vec<Value> = (0..n as i64).map(|i| Value::Int((row + i) % 7)).collect();
        segments.push(
            Batch::new(n)
                .with_column("a", a)
                .with_column("b", b),
        );
        row += n as i64;
    }
    segments
}

fn as_segment_trait_objects(batches: &[Batch]) -> Vec<Box<dyn Segment + '_>> {
    batches
        .iter()
        .map(|b| Box::new(PrebuiltSegment(b.clone())) as Box<dyn Segment + '_>)
        .collect()
}

// ---------------------------------------------------------------------
// Raw-loop reference: the same Add/Sum/Filter work with no VM machinery
// at all (register HashMap, LoadColumn clone, Emit transpose) -- reuses
// `Value` directly so the comparison isolates interpreter overhead, not
// representation.
// ---------------------------------------------------------------------

#[inline(never)]
fn raw_map_add(a: &[Value], b: &[Value]) -> Vec<Value> {
    a.iter()
        .zip(b)
        .map(|(x, y)| match (x, y) {
            (Value::Int(x), Value::Int(y)) => Value::Int(x + y),
            _ => Value::Null,
        })
        .collect()
}

#[inline(never)]
fn raw_reduce_sum(a: &[Value]) -> Value {
    let mut total = 0i64;
    for v in a {
        if let Value::Int(x) = v {
            total += x;
        }
    }
    Value::Int(total)
}

#[inline(never)]
fn raw_filter_gt(a: &[Value], threshold: i64) -> Vec<Value> {
    a.iter()
        .filter(|v| matches!(v, Value::Int(x) if *x > threshold))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------
// The spike
// ---------------------------------------------------------------------

#[test]
#[ignore = "release-only perf spike; run with `cargo test --release -- --ignored --nocapture`"]
fn batch_specific_spike() {
    const REPS: usize = 11;
    let batches = build_segments(ROWS_TOTAL);
    let n_segments = batches.len();
    println!(
        "\n=== db-core#141 NEON spike (round 3, real vm::batch constructs): \
         {ROWS_TOTAL} rows across {n_segments} segments of BATCH_SIZE={BATCH_SIZE}, median of {REPS} runs ===\n"
    );

    // -- Map add: LoadColumn a, LoadColumn b, Map{Add}, Emit -- via the
    // single-threaded `Vm::execute` path, one fresh `Vm` per segment
    // (matching how a real per-segment loop is driven before #130's
    // typed columns exist).
    let program_add = [
        Opcode::LoadColumn {
            reg: 0,
            column: Cow::Borrowed("a"),
        },
        Opcode::LoadColumn {
            reg: 1,
            column: Cow::Borrowed("b"),
        },
        Opcode::Map {
            dst: 2,
            op: MapOp::Add,
            a: 0,
            b: 1,
        },
        Opcode::Emit {
            registers: Cow::Borrowed(&[2]),
        },
    ];
    let t_vm = time_it(REPS, || {
        for batch in &batches {
            let mut vm = Vm::new();
            vm.execute(batch, &program_add).unwrap();
            black_box(vm.take_output());
        }
    });
    let t_raw = time_it(REPS, || {
        for batch in &batches {
            let a = &batch.columns["a"];
            let b = &batch.columns["b"];
            black_box(raw_map_add(a, b));
        }
    });
    report("map_add(int)/per-segment Vm::execute", t_vm, t_raw);

    // -- Same program, through `run_parallel` (real morsel-driven
    // executor: `std::thread::available_parallelism()` worker threads).
    let segments = as_segment_trait_objects(&batches);
    let t_parallel = time_it(REPS, || {
        black_box(run_parallel(&segments, &program_add).unwrap());
    });
    println!(
        "{:<38} parallel={:>10?}  ({:.2}x vs single-threaded Vm::execute, {} threads available)",
        "map_add(int)/run_parallel",
        t_parallel,
        t_vm.as_secs_f64() / t_parallel.as_secs_f64(),
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
    );

    // -- Reduce sum: LoadColumn a, Reduce{Sum}, Emit -- per-segment
    // partial (matches `run_parallel`'s documented per-segment-only
    // semantics; cross-segment merge is a `Finalize` concern, out of
    // scope here).
    let program_sum = [
        Opcode::LoadColumn {
            reg: 0,
            column: Cow::Borrowed("a"),
        },
        Opcode::Reduce {
            func: AggFunc::Sum,
            src: Some(0),
            dst: 1,
        },
        Opcode::Emit {
            registers: Cow::Borrowed(&[1]),
        },
    ];
    let t_vm = time_it(REPS, || {
        for batch in &batches {
            let mut vm = Vm::new();
            vm.execute(batch, &program_sum).unwrap();
            black_box(vm.take_output());
        }
    });
    let t_raw = time_it(REPS, || {
        for batch in &batches {
            black_box(raw_reduce_sum(&batch.columns["a"]));
        }
    });
    report("reduce_sum(int)/per-segment Vm::execute", t_vm, t_raw);

    // -- Filter: LoadColumn a, LoadConst threshold, Map{Gt}, Filter, Emit.
    let threshold = BATCH_SIZE as i64 / 2;
    let program_filter = [
        Opcode::LoadColumn {
            reg: 0,
            column: Cow::Borrowed("a"),
        },
        Opcode::LoadConst {
            reg: 1,
            value: Value::Int(threshold),
        },
        Opcode::Map {
            dst: 2,
            op: MapOp::Gt,
            a: 0,
            b: 1,
        },
        Opcode::Filter { predicate: 2 },
        Opcode::Emit {
            registers: Cow::Borrowed(&[0]),
        },
    ];
    let t_vm = time_it(REPS, || {
        for batch in &batches {
            let mut vm = Vm::new();
            vm.execute(batch, &program_filter).unwrap();
            black_box(vm.take_output());
        }
    });
    let t_raw = time_it(REPS, || {
        for batch in &batches {
            black_box(raw_filter_gt(&batch.columns["a"], threshold));
        }
    });
    report("filter(int)/per-segment Vm::execute", t_vm, t_raw);

    println!(
        "\nInterpretation: vm/raw > 1x quantifies per-opcode interpreter overhead \
         (register HashMap insert/lookup, LoadColumn's Vec<Value> clone, Emit's row-major \
         transpose) on top of the elementwise work rounds 1/2 measured -- this is the \
         overhead a typed-column #130 rewrite would still pay unless the opcode dispatch \
         itself also changes.\n"
    );
}

/// Correctness check independent of the `#[ignore]`d perf run.
#[test]
fn vm_program_matches_raw_loop() {
    let batches = build_segments(BATCH_SIZE * 3 + 17); // not a whole number of segments
    let program_add = [
        Opcode::LoadColumn {
            reg: 0,
            column: Cow::Borrowed("a"),
        },
        Opcode::LoadColumn {
            reg: 1,
            column: Cow::Borrowed("b"),
        },
        Opcode::Map {
            dst: 2,
            op: MapOp::Add,
            a: 0,
            b: 1,
        },
        Opcode::Emit {
            registers: Cow::Borrowed(&[2]),
        },
    ];
    for batch in &batches {
        let mut vm = Vm::new();
        vm.execute(batch, &program_add).unwrap();
        let vm_out: Vec<Value> = vm.take_output().into_iter().map(|row| row[0].clone()).collect();
        let raw_out = raw_map_add(&batch.columns["a"], &batch.columns["b"]);
        assert_eq!(vm_out, raw_out);
    }

    let segments = as_segment_trait_objects(&batches);
    let parallel_out: Vec<Value> = run_parallel(&segments, &program_add)
        .unwrap()
        .into_iter()
        .map(|row| row[0].clone())
        .collect();
    let sequential_out: Vec<Value> = batches
        .iter()
        .flat_map(|b| raw_map_add(&b.columns["a"], &b.columns["b"]))
        .collect();
    assert_eq!(parallel_out, sequential_out);
}
