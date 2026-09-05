//! Spike for db-core#141: does typed-contiguous storage actually let
//! LLVM autovectorize (NEON) `vm::batch` kernels on aarch64-apple-darwin,
//! and how much does that beat today's `Vec<Value>` tagged-enum
//! representation (`src/vm/batch.rs`)?
//!
//! This spike makes NO production code changes (per #141's non-goals). It
//! builds two representations of the same four column types --
//! `Int`/`Float`/`Bool`/`Str` -- side by side:
//!
//! - "typed": a plain contiguous `Vec<i64>` / `Vec<f64>` / `Vec<bool>` /
//!   `Vec<String>`, standing in for #130's future typed `Batch` columns.
//! - "tagged": `Vec<db_core::vm::batch::Value>`, today's actual
//!   representation, single-column but otherwise doing the same work.
//!
//! ...and times representative opcodes from #141's hypothesis table
//! against both, at a batch size sized to make the effect visible
//! (`N` batches of `BATCH_SIZE` rows, see below) while still finishing in
//! well under a second even under `--release`.
//!
//! # Running
//!
//! Debug builds are not representative of autovectorization (LLVM barely
//! optimizes at `-O0`), so this spike is `#[ignore]`d by default. Run it
//! explicitly in release mode:
//!
//! ```sh
//! cargo test --release --test 01_neon_batch_kernels -- --ignored --nocapture
//! # or: make -C tests/spike/001_neon_batch_kernels run-01
//! ```
//!
//! To confirm *why* a kernel is fast or slow (NEON `ldp`/`fadd v` vs
//! scalar), disassemble the release binary for the specific symbol, e.g.:
//!
//! ```sh
//! cargo build --release --tests
//! otool -tV target/release/deps/<test-binary> | rg -A40 'map_add_typed'
//! ```
//!
//! or use `cargo asm --test 01_neon_batch_kernels <symbol>` if
//! `cargo-asm` is installed.

use db_core::vm::batch::Value;
use std::borrow::Cow;
use std::hint::black_box;
use std::time::{Duration, Instant};

/// Number of `BATCH_SIZE`-row batches concatenated into one flat run --
/// large enough that startup/measurement noise is negligible and any
/// vectorization win is visible above noise.
const ROWS: usize = 1_000_000;

/// Run `f` enough times to get a stable wall-clock reading, return the
/// median of `reps` timings.
fn time_it<F: FnMut()>(reps: usize, mut f: F) -> Duration {
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

fn report(kernel: &str, typed: Duration, tagged: Duration) {
    let speedup = tagged.as_secs_f64() / typed.as_secs_f64();
    println!(
        "{kernel:<28} typed={:>10?}  tagged={:>10?}  speedup={speedup:.2}x",
        typed, tagged
    );
}

// ---------------------------------------------------------------------
// Fixture data
// ---------------------------------------------------------------------

fn int_typed(n: usize) -> Vec<i64> {
    (0..n as i64).collect()
}

fn int_tagged(n: usize) -> Vec<Value> {
    (0..n as i64).map(Value::Int).collect()
}

fn float_typed(n: usize) -> Vec<f64> {
    (0..n).map(|i| i as f64 * 1.5).collect()
}

fn float_tagged(n: usize) -> Vec<Value> {
    (0..n).map(|i| Value::Float(i as f64 * 1.5)).collect()
}

fn bool_typed(n: usize) -> Vec<bool> {
    (0..n).map(|i| i % 3 == 0).collect()
}

fn bool_tagged(n: usize) -> Vec<Value> {
    (0..n).map(|i| Value::Bool(i % 3 == 0)).collect()
}

fn str_typed(n: usize) -> Vec<String> {
    // A handful of distinct short strings, like a low-cardinality text
    // column (e.g. a status/category field) rather than n unique heap
    // allocations of a synthetic benchmark that would never occur in
    // practice.
    const WORDS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];
    (0..n).map(|i| WORDS[i % WORDS.len()].to_string()).collect()
}

fn str_tagged(n: usize) -> Vec<Value> {
    const WORDS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];
    (0..n)
        .map(|i| Value::Str(Cow::Owned(WORDS[i % WORDS.len()].to_string())))
        .collect()
}

// ---------------------------------------------------------------------
// Kernel: Map arithmetic (int) -- hypothesis: High, autovectorizes
// ---------------------------------------------------------------------

fn map_add_typed(a: &[i64], b: &[i64], out: &mut [i64]) {
    for i in 0..a.len() {
        out[i] = a[i] + b[i];
    }
}

fn map_add_tagged(a: &[Value], b: &[Value], out: &mut [Value]) {
    for i in 0..a.len() {
        out[i] = match (&a[i], &b[i]) {
            (Value::Int(x), Value::Int(y)) => Value::Int(x + y),
            _ => Value::Null,
        };
    }
}

// ---------------------------------------------------------------------
// Kernel: Map arithmetic (float) -- hypothesis: High, autovectorizes
// ---------------------------------------------------------------------

fn map_mul_typed(a: &[f64], b: &[f64], out: &mut [f64]) {
    for i in 0..a.len() {
        out[i] = a[i] * b[i];
    }
}

fn map_mul_tagged(a: &[Value], b: &[Value], out: &mut [Value]) {
    for i in 0..a.len() {
        out[i] = match (&a[i], &b[i]) {
            (Value::Float(x), Value::Float(y)) => Value::Float(x * y),
            _ => Value::Null,
        };
    }
}

// ---------------------------------------------------------------------
// Kernel: Map boolean And -- hypothesis: High if predicate becomes bitmask
// ---------------------------------------------------------------------

fn map_and_typed(a: &[bool], b: &[bool], out: &mut [bool]) {
    for i in 0..a.len() {
        out[i] = a[i] && b[i];
    }
}

fn map_and_tagged(a: &[Value], b: &[Value], out: &mut [Value]) {
    for i in 0..a.len() {
        out[i] = match (&a[i], &b[i]) {
            (Value::Bool(x), Value::Bool(y)) => Value::Bool(*x && *y),
            _ => Value::Null,
        };
    }
}

// ---------------------------------------------------------------------
// Kernel: Reduce Sum (int) -- hypothesis: High, + multiple accumulators
// ---------------------------------------------------------------------

fn reduce_sum_typed(a: &[i64]) -> i64 {
    a.iter().sum()
}

/// Same reduction, but with 4 independent accumulators (per #141: "2-4
/// independent accumulators, multiple NEON pipes on M-series") to see
/// whether that beats the naive single-accumulator loop even without
/// hand intrinsics -- LLVM can interleave these across vector lanes.
fn reduce_sum_typed_unrolled(a: &[i64]) -> i64 {
    let mut acc = [0i64; 4];
    let chunks = a.chunks_exact(4);
    let rem = chunks.remainder();
    for c in chunks {
        acc[0] += c[0];
        acc[1] += c[1];
        acc[2] += c[2];
        acc[3] += c[3];
    }
    let mut total = acc[0] + acc[1] + acc[2] + acc[3];
    for v in rem {
        total += v;
    }
    total
}

fn reduce_sum_tagged(a: &[Value]) -> i64 {
    let mut total = 0i64;
    for v in a {
        if let Value::Int(x) = v {
            total += x;
        }
    }
    total
}

// ---------------------------------------------------------------------
// Kernel: Reduce Sum (float)
// ---------------------------------------------------------------------

fn reduce_sum_f64_typed(a: &[f64]) -> f64 {
    a.iter().sum()
}

fn reduce_sum_f64_tagged(a: &[Value]) -> f64 {
    let mut total = 0.0f64;
    for v in a {
        if let Value::Float(x) = v {
            total += x;
        }
    }
    total
}

// ---------------------------------------------------------------------
// Kernel: Filter compaction (int > threshold) -- hypothesis: Medium
// ---------------------------------------------------------------------

fn filter_typed(a: &[i64], threshold: i64) -> Vec<i64> {
    a.iter().copied().filter(|&x| x > threshold).collect()
}

fn filter_tagged(a: &[Value], threshold: i64) -> Vec<Value> {
    a.iter()
        .filter(|v| matches!(v, Value::Int(x) if *x > threshold))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------
// Kernel: string equality count -- hypothesis: None, included for
// completeness/contrast (offsets+bytes vs `Cow<'static, str>` compare).
// ---------------------------------------------------------------------

fn str_eq_count_typed(a: &[String], needle: &str) -> usize {
    a.iter().filter(|s| s.as_str() == needle).count()
}

fn str_eq_count_tagged(a: &[Value], needle: &str) -> usize {
    a.iter()
        .filter(|v| matches!(v, Value::Str(s) if s.as_ref() == needle))
        .count()
}

// ---------------------------------------------------------------------
// The spike
// ---------------------------------------------------------------------

#[test]
#[ignore = "release-only perf spike; run with `cargo test --release -- --ignored --nocapture`"]
fn neon_batch_kernel_spike() {
    const REPS: usize = 11;

    println!("\n=== db-core#141 NEON spike: {ROWS} rows, median of {REPS} runs ===\n");

    // -- Map add (int) --
    let (a, b) = (int_typed(ROWS), int_typed(ROWS));
    let mut out = vec![0i64; ROWS];
    let t_typed = time_it(REPS, || map_add_typed(black_box(&a), black_box(&b), &mut out));
    black_box(&out);

    let (at, bt) = (int_tagged(ROWS), int_tagged(ROWS));
    let mut out_t = vec![Value::Null; ROWS];
    let t_tagged = time_it(REPS, || {
        map_add_tagged(black_box(&at), black_box(&bt), &mut out_t)
    });
    black_box(&out_t);
    report("map_add(int)", t_typed, t_tagged);

    // -- Map mul (float) --
    let (a, b) = (float_typed(ROWS), float_typed(ROWS));
    let mut out = vec![0.0f64; ROWS];
    let t_typed = time_it(REPS, || map_mul_typed(black_box(&a), black_box(&b), &mut out));
    black_box(&out);

    let (at, bt) = (float_tagged(ROWS), float_tagged(ROWS));
    let mut out_t = vec![Value::Null; ROWS];
    let t_tagged = time_it(REPS, || {
        map_mul_tagged(black_box(&at), black_box(&bt), &mut out_t)
    });
    black_box(&out_t);
    report("map_mul(float)", t_typed, t_tagged);

    // -- Map and (bool) --
    let (a, b) = (bool_typed(ROWS), bool_typed(ROWS));
    let mut out = vec![false; ROWS];
    let t_typed = time_it(REPS, || map_and_typed(black_box(&a), black_box(&b), &mut out));
    black_box(&out);

    let (at, bt) = (bool_tagged(ROWS), bool_tagged(ROWS));
    let mut out_t = vec![Value::Null; ROWS];
    let t_tagged = time_it(REPS, || {
        map_and_tagged(black_box(&at), black_box(&bt), &mut out_t)
    });
    black_box(&out_t);
    report("map_and(bool)", t_typed, t_tagged);

    // -- Reduce sum (int) --
    let a = int_typed(ROWS);
    let t_typed = time_it(REPS, || {
        black_box(reduce_sum_typed(black_box(&a)));
    });
    let t_typed_unrolled = time_it(REPS, || {
        black_box(reduce_sum_typed_unrolled(black_box(&a)));
    });
    let at = int_tagged(ROWS);
    let t_tagged = time_it(REPS, || {
        black_box(reduce_sum_tagged(black_box(&at)));
    });
    report("reduce_sum(int)", t_typed, t_tagged);
    report("reduce_sum(int)/unrolled4", t_typed_unrolled, t_tagged);

    // -- Reduce sum (float) --
    let a = float_typed(ROWS);
    let t_typed = time_it(REPS, || {
        black_box(reduce_sum_f64_typed(black_box(&a)));
    });
    let at = float_tagged(ROWS);
    let t_tagged = time_it(REPS, || {
        black_box(reduce_sum_f64_tagged(black_box(&at)));
    });
    report("reduce_sum(float)", t_typed, t_tagged);

    // -- Filter compaction (int) --
    let a = int_typed(ROWS);
    let t_typed = time_it(REPS, || {
        black_box(filter_typed(black_box(&a), ROWS as i64 / 2));
    });
    let at = int_tagged(ROWS);
    let t_tagged = time_it(REPS, || {
        black_box(filter_tagged(black_box(&at), ROWS as i64 / 2));
    });
    report("filter(int)", t_typed, t_tagged);

    // -- String equality count --
    let a = str_typed(ROWS);
    let t_typed = time_it(REPS, || {
        black_box(str_eq_count_typed(black_box(&a), "gamma"));
    });
    let at = str_tagged(ROWS);
    let t_tagged = time_it(REPS, || {
        black_box(str_eq_count_tagged(black_box(&at), "gamma"));
    });
    report("str_eq_count", t_typed, t_tagged);

    println!(
        "\nHost: {} / target_feature detection is compile-time on aarch64 (NEON is baseline, \
         no runtime dispatch needed) -- see doc comment for how to confirm codegen with otool/cargo-asm.\n",
        std::env::consts::ARCH
    );
}

/// Sanity check the two representations agree, independent of the
/// `#[ignore]`d perf run -- this one runs in normal `cargo test`.
#[test]
fn kernels_typed_and_tagged_agree() {
    const N: usize = 997; // not a multiple of any obvious vector width

    let (a, b) = (int_typed(N), int_typed(N));
    let mut out = vec![0i64; N];
    map_add_typed(&a, &b, &mut out);
    let (at, bt) = (int_tagged(N), int_tagged(N));
    let mut out_t = vec![Value::Null; N];
    map_add_tagged(&at, &bt, &mut out_t);
    for i in 0..N {
        assert_eq!(Value::Int(out[i]), out_t[i]);
    }

    assert_eq!(reduce_sum_typed(&a), reduce_sum_tagged(&at));
    assert_eq!(reduce_sum_typed_unrolled(&a), reduce_sum_typed(&a));

    let f = float_typed(N);
    let ft = float_tagged(N);
    assert!((reduce_sum_f64_typed(&f) - reduce_sum_f64_tagged(&ft)).abs() < 1e-6);

    let filtered = filter_typed(&a, 500);
    let filtered_t: Vec<i64> = filter_tagged(&at, 500)
        .into_iter()
        .map(|v| match v {
            Value::Int(x) => x,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(filtered, filtered_t);

    let s = str_typed(N);
    let st = str_tagged(N);
    assert_eq!(str_eq_count_typed(&s, "beta"), str_eq_count_tagged(&st, "beta"));
}
