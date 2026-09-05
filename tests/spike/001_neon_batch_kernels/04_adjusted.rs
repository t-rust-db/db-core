//! Spike for db-core#141, round 4 -- fixes round 3
//! (`03_batch_specific.rs`, same folder) after review found its `vm/raw`
//! ratios conflated several distinct costs into one number:
//!
//! 1. **Program ladder instead of one fixed program.** Round 3 timed
//!    `LoadColumn, LoadColumn, Map, Emit` as a single block, so
//!    `Emit`'s row-major transpose (confirmed by reading `Vm::step`'s
//!    `Opcode::Emit` arm: it builds one fresh `Vec<Value>` per output
//!    row, cloning every cell) was invisible inside the ratio -- and
//!    likely dominated it, since materializing ~1M tiny row `Vec`s costs
//!    far more than the `Map`'s per-element `match`. This round times
//!    `Load` alone, `Load+Map`/`Load+Reduce`, and `Load+Map+Emit`
//!    separately and reports the *increments*, so "load/clone cost",
//!    "dispatch+elementwise cost", and "output-transpose cost" are three
//!    numbers, not one blended ratio.
//! 2. **Filter selectivity fixed.** Round 3's filter column held global
//!    row ids (0..1M) compared against a fixed `threshold = BATCH_SIZE/2`
//!    -- true for ~99.95% of rows, so only the first segment did any
//!    real filtering. This round uses a per-segment-local column so
//!    every segment filters at the same ~50% selectivity round 1 used,
//!    and notes that the VM path is algorithmically two-pass (materialize
//!    a `Gt` predicate column, then drain-and-rebuild every register)
//!    versus the raw single fused pass, so `vm/raw > 1` here is *not*
//!    pure interpreter overhead even with zero dispatch cost.
//! 3. **Single-threaded baseline now pays the same `Segment::load()`
//!    cost `run_parallel` does** (a full `Batch::clone()`, standing in
//!    for real decode cost), so the reported parallel speedup isn't
//!    inflated by comparing "clone once + N executes" against "N clones
//!    + N executes".
//! 4. **`Vm::new()`-per-segment cost split from steady-state dispatch**:
//!    a second variant reuses one `Vm` across all segments
//!    (`Vm::clear_registers()` between them) to show how much of the
//!    per-segment number is `HashMap` allocation setup versus per-opcode
//!    work.
//! 5. **Reports ns/row**, not just ratios, so these compose directly with
//!    rounds 1/2's per-element numbers.
//!
//! `run_parallel`'s segment-order guarantee (asserted by the correctness
//! test) comes from `run_morsels` sorting results by segment index before
//! returning (`src/vm/batch.rs`), not from completion order -- confirmed
//! by reading that function, not assumed.
//!
//! No production code changes.
//!
//! # Running
//!
//! ```sh
//! cargo test --release --test 04_adjusted -- --ignored --nocapture
//! # or: make -C tests/spike/001_neon_batch_kernels run-04
//! ```

use db_core::expr::AggFunc;
use db_core::vm::batch::{run_parallel, Batch, MapOp, Opcode, Segment, Value, Vm, BATCH_SIZE};
use std::borrow::Cow;
use std::hint::black_box;
use std::time::{Duration, Instant};

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

fn ns_per_row(d: Duration, rows: usize) -> f64 {
    d.as_secs_f64() * 1e9 / rows as f64
}

fn report_step(label: &str, d: Duration, rows: usize) {
    println!("  {label:<32} {d:>10?}  ({:>6.2} ns/row)", ns_per_row(d, rows));
}

fn report_increment(label: &str, from: Duration, to: Duration, rows: usize) {
    let delta = to.saturating_sub(from);
    println!(
        "  + {label:<30} {delta:>10?}  ({:>6.2} ns/row)",
        ns_per_row(delta, rows)
    );
}

// ---------------------------------------------------------------------
// Fixture: `BATCH_SIZE`-row segments with a per-segment-LOCAL column
// (`a`/`b` reset to 0.. inside every segment) so every segment has the
// same value distribution and the same filter selectivity -- round 3
// used a global row id, which made all but the first segment pass the
// filter almost unconditionally.
// ---------------------------------------------------------------------

fn build_segments(rows_total: usize) -> Vec<Batch> {
    let mut segments = Vec::with_capacity(rows_total.div_ceil(BATCH_SIZE));
    let mut remaining = rows_total;
    while remaining > 0 {
        let n = BATCH_SIZE.min(remaining);
        let a: Vec<Value> = (0..n as i64).map(Value::Int).collect();
        let b: Vec<Value> = (0..n as i64).map(|i| Value::Int(i % 7)).collect();
        segments.push(Batch::new(n).with_column("a", a).with_column("b", b));
        remaining -= n;
    }
    segments
}

struct PrebuiltSegment(Batch);

impl Segment for PrebuiltSegment {
    fn load(&self) -> Batch {
        // Stands in for real decode cost (e.g. a Parquet row group) --
        // deliberately not free, see round 3's doc comment.
        self.0.clone()
    }
}

fn owned_segments(batches: &[Batch]) -> Vec<Box<dyn Segment>> {
    batches
        .iter()
        .map(|b| Box::new(PrebuiltSegment(b.clone())) as Box<dyn Segment>)
        .collect()
}

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
fn adjusted_batch_specific_spike() {
    const REPS: usize = 11;
    let batches = build_segments(ROWS_TOTAL);
    let n_segments = batches.len();
    println!(
        "\n=== db-core#141 NEON spike (round 4, adjusted vm::batch ladder): \
         {ROWS_TOTAL} rows across {n_segments} segments of BATCH_SIZE={BATCH_SIZE}, median of {REPS} runs ===\n"
    );

    // ---- Ladder: Load -> +Map -> +Emit (map_add) ----
    println!("--- map_add(int) ladder: Load, Load+Map, Load+Map+Emit ---");

    let program_load = [
        Opcode::LoadColumn { reg: 0, column: Cow::Borrowed("a") },
        Opcode::LoadColumn { reg: 1, column: Cow::Borrowed("b") },
    ];
    let program_load_map = [
        Opcode::LoadColumn { reg: 0, column: Cow::Borrowed("a") },
        Opcode::LoadColumn { reg: 1, column: Cow::Borrowed("b") },
        Opcode::Map { dst: 2, op: MapOp::Add, a: 0, b: 1 },
    ];
    let program_load_map_emit = [
        Opcode::LoadColumn { reg: 0, column: Cow::Borrowed("a") },
        Opcode::LoadColumn { reg: 1, column: Cow::Borrowed("b") },
        Opcode::Map { dst: 2, op: MapOp::Add, a: 0, b: 1 },
        Opcode::Emit { registers: Cow::Borrowed(&[2]) },
    ];

    let t_load = time_it(REPS, || {
        for batch in &batches {
            let mut vm = Vm::new();
            vm.execute(batch, &program_load).unwrap();
            black_box(vm.register(0).unwrap());
            black_box(vm.register(1).unwrap());
        }
    });
    let t_load_map = time_it(REPS, || {
        for batch in &batches {
            let mut vm = Vm::new();
            vm.execute(batch, &program_load_map).unwrap();
            black_box(vm.register(2).unwrap());
        }
    });
    let t_load_map_emit = time_it(REPS, || {
        for batch in &batches {
            let mut vm = Vm::new();
            vm.execute(batch, &program_load_map_emit).unwrap();
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

    report_step("Load (2 cols)", t_load, ROWS_TOTAL);
    report_increment("Map (dispatch+add)", t_load, t_load_map, ROWS_TOTAL);
    report_increment("Emit (row transpose)", t_load_map, t_load_map_emit, ROWS_TOTAL);
    report_step("raw fused loop (no VM)", t_raw, ROWS_TOTAL);
    println!(
        "  => vm(load+map+emit)/raw = {:.2}x   vm(load+map only)/raw = {:.2}x\n",
        t_load_map_emit.as_secs_f64() / t_raw.as_secs_f64(),
        t_load_map.as_secs_f64() / t_raw.as_secs_f64(),
    );

    // ---- Vm::new() per segment vs one reused Vm ----
    let t_fresh_vm = time_it(REPS, || {
        for batch in &batches {
            let mut vm = Vm::new();
            vm.execute(batch, &program_load_map).unwrap();
            black_box(vm.register(2).unwrap());
        }
    });
    let t_reused_vm = time_it(REPS, || {
        let mut vm = Vm::new();
        for batch in &batches {
            vm.execute(batch, &program_load_map).unwrap();
            black_box(vm.register(2).unwrap());
            vm.clear_registers();
        }
    });
    println!("--- Vm::new() per segment vs one Vm reused across segments (Load+Map) ---");
    report_step("fresh Vm per segment", t_fresh_vm, ROWS_TOTAL);
    report_step("one Vm reused, clear_registers()", t_reused_vm, ROWS_TOTAL);
    println!(
        "  => per-segment Vm::new() setup cost: {:.2} ns/row\n",
        ns_per_row(t_fresh_vm.saturating_sub(t_reused_vm), ROWS_TOTAL)
    );

    // ---- Reduce sum ladder: Load -> +Reduce -> +Emit ----
    println!("--- reduce_sum(int) ladder: Load, Load+Reduce, Load+Reduce+Emit ---");

    let program_load_a = [Opcode::LoadColumn { reg: 0, column: Cow::Borrowed("a") }];
    let program_load_reduce = [
        Opcode::LoadColumn { reg: 0, column: Cow::Borrowed("a") },
        Opcode::Reduce { func: AggFunc::Sum, src: Some(0), dst: 1 },
    ];
    let program_load_reduce_emit = [
        Opcode::LoadColumn { reg: 0, column: Cow::Borrowed("a") },
        Opcode::Reduce { func: AggFunc::Sum, src: Some(0), dst: 1 },
        Opcode::Emit { registers: Cow::Borrowed(&[1]) },
    ];

    let t_load_a = time_it(REPS, || {
        for batch in &batches {
            let mut vm = Vm::new();
            vm.execute(batch, &program_load_a).unwrap();
            black_box(vm.register(0).unwrap());
        }
    });
    let t_load_reduce = time_it(REPS, || {
        for batch in &batches {
            let mut vm = Vm::new();
            vm.execute(batch, &program_load_reduce).unwrap();
            black_box(vm.register(1).unwrap());
        }
    });
    let t_load_reduce_emit = time_it(REPS, || {
        for batch in &batches {
            let mut vm = Vm::new();
            vm.execute(batch, &program_load_reduce_emit).unwrap();
            black_box(vm.take_output());
        }
    });
    let t_raw_sum = time_it(REPS, || {
        for batch in &batches {
            black_box(raw_reduce_sum(&batch.columns["a"]));
        }
    });

    report_step("Load (1 col)", t_load_a, ROWS_TOTAL);
    report_increment("Reduce (dispatch+sum)", t_load_a, t_load_reduce, ROWS_TOTAL);
    report_increment("Emit (1 row/segment)", t_load_reduce, t_load_reduce_emit, ROWS_TOTAL);
    report_step("raw fused loop (no VM)", t_raw_sum, ROWS_TOTAL);
    println!(
        "  => vm(load+reduce+emit)/raw = {:.2}x\n",
        t_load_reduce_emit.as_secs_f64() / t_raw_sum.as_secs_f64()
    );

    // ---- Filter, selectivity-matched to round 1 (~50%), local column ----
    println!("--- filter(int), ~50% selectivity (per-segment-local column) ---");
    let threshold = BATCH_SIZE as i64 / 2;
    let program_filter = [
        Opcode::LoadColumn { reg: 0, column: Cow::Borrowed("a") },
        Opcode::LoadConst { reg: 1, value: Value::Int(threshold) },
        Opcode::Map { dst: 2, op: MapOp::Gt, a: 0, b: 1 },
        Opcode::Filter { predicate: 2 },
        Opcode::Emit { registers: Cow::Borrowed(&[0]) },
    ];
    let t_vm_filter = time_it(REPS, || {
        for batch in &batches {
            let mut vm = Vm::new();
            vm.execute(batch, &program_filter).unwrap();
            black_box(vm.take_output());
        }
    });
    let t_raw_filter = time_it(REPS, || {
        for batch in &batches {
            black_box(raw_filter_gt(&batch.columns["a"], threshold));
        }
    });
    report_step("vm (LoadConst+Map+Filter+Emit)", t_vm_filter, ROWS_TOTAL);
    report_step("raw fused single-pass filter", t_raw_filter, ROWS_TOTAL);
    println!(
        "  => vm/raw = {:.2}x -- NOTE: the VM path is algorithmically two-pass \
         (materialize a Gt predicate column, then drain-and-rebuild every live \
         register) vs raw's single fused pass, so this ratio is not pure \
         dispatch overhead even with a hypothetically free interpreter.\n",
        t_vm_filter.as_secs_f64() / t_raw_filter.as_secs_f64()
    );

    // ---- Single-threaded (paying load()) vs run_parallel, apples-to-apples ----
    println!("--- map_add(int): single-threaded (paying Segment::load()) vs run_parallel ---");
    let owned = owned_segments(&batches);
    let t_single_with_load = time_it(REPS, || {
        for seg in &owned {
            let batch = seg.load();
            let mut vm = Vm::new();
            vm.execute(&batch, &program_load_map_emit).unwrap();
            black_box(vm.take_output());
        }
    });
    let t_parallel = time_it(REPS, || {
        black_box(run_parallel(&owned, &program_load_map_emit).unwrap());
    });
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    report_step("single-threaded, incl. load()", t_single_with_load, ROWS_TOTAL);
    report_step("run_parallel", t_parallel, ROWS_TOTAL);
    println!(
        "  => speedup = {:.2}x on {threads} available threads (round 3's uncorrected \
         figure, which didn't charge the single-threaded baseline for load(), \
         understated this)\n",
        t_single_with_load.as_secs_f64() / t_parallel.as_secs_f64()
    );
}

/// Correctness check independent of the `#[ignore]`d perf run.
#[test]
fn vm_program_matches_raw_loop() {
    let batches = build_segments(BATCH_SIZE * 3 + 17); // not a whole number of segments
    let program_add = [
        Opcode::LoadColumn { reg: 0, column: Cow::Borrowed("a") },
        Opcode::LoadColumn { reg: 1, column: Cow::Borrowed("b") },
        Opcode::Map { dst: 2, op: MapOp::Add, a: 0, b: 1 },
        Opcode::Emit { registers: Cow::Borrowed(&[2]) },
    ];
    for batch in &batches {
        let mut vm = Vm::new();
        vm.execute(batch, &program_add).unwrap();
        let vm_out: Vec<Value> = vm.take_output().into_iter().map(|row| row[0].clone()).collect();
        let raw_out = raw_map_add(&batch.columns["a"], &batch.columns["b"]);
        assert_eq!(vm_out, raw_out);
    }

    // Order guarantee: `run_parallel` -> `run_morsels` sorts its results by
    // segment index before returning (src/vm/batch.rs), so this is pinning
    // a documented contract, not assuming completion-order luck.
    let owned = owned_segments(&batches);
    let parallel_out: Vec<Value> = run_parallel(&owned, &program_add)
        .unwrap()
        .into_iter()
        .map(|row| row[0].clone())
        .collect();
    let sequential_out: Vec<Value> = batches
        .iter()
        .flat_map(|b| raw_map_add(&b.columns["a"], &b.columns["b"]))
        .collect();
    assert_eq!(parallel_out, sequential_out);

    // Filter selectivity is now per-segment-local, so any full-size
    // segment (not just the first, as round 3's global row id caused)
    // exercises real ~50% selectivity. The trailing partial segment (17
    // rows here) is below the threshold entirely, so check a full one.
    let threshold = BATCH_SIZE as i64 / 2;
    let full_segment = batches.iter().find(|b| b.num_rows == BATCH_SIZE).unwrap();
    let raw_filtered = raw_filter_gt(&full_segment.columns["a"], threshold);
    assert!(!raw_filtered.is_empty() && raw_filtered.len() < full_segment.num_rows);
}
