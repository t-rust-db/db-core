//! Spike for db-core#141, round 2 -- addresses methodology gaps found in
//! review of `tests/spike/01_neon_batch_kernels`:
//!
//! 1. `reduce_sum::<f64>()` is a serial dependency chain (FP addition
//!    isn't associative, Rust has no fast-math flag), so it does NOT
//!    autovectorize like the int sum does. Fixed here with the same
//!    4-accumulator unroll used for int -- note this changes summation
//!    order and thus the exact result, which is a deliberate kernel
//!    design tradeoff, not a bug.
//! 2. At 1M rows/8MB+ per column, `map_*` kernels are memory-bandwidth
//!    bound, not compute bound -- the typed-vs-tagged ratio there mostly
//!    reflects bytes-moved (`size_of::<Value>()` vs `size_of::<i64>()`),
//!    not vectorization. This file runs each map/reduce kernel at BOTH a
//!    cache-resident size (repeated many times) and the original
//!    streaming size, and reports GB/s so the two effects are separable.
//! 3. Kernels are `#[inline(never)]` so they survive as disassemblable
//!    symbols and aren't specialized against the fixture data.
//! 4. Map kernels equalize slice lengths up front (`let n = ...min...`)
//!    so LLVM can drop bounds checks instead of relying on fragile
//!    loop-versioning that varies across compiler versions.
//! 5. `filter_*` now has a count-only variant (pure predicate cost, no
//!    allocation) alongside the original materializing variant (which
//!    reuses one `Vec` via `clear()`/`extend()` instead of allocating a
//!    fresh one per rep).
//!
//! `01_neon_batch_kernels.rs` (same folder) is left as-is; this file
//! supersedes it for drawing conclusions -- see `README.md` (this
//! folder) for the updated results and write-up.
//!
//! # Running
//!
//! ```sh
//! cargo test --release --test 02_neon_adjusted -- --ignored --nocapture
//! # or: make -C tests/spike/001_neon_batch_kernels run-02
//! ```
//!
//! To disassemble a specific kernel (now addressable thanks to
//! `#[inline(never)]`):
//!
//! ```sh
//! cargo build --release --tests
//! otool -tV target/release/deps/02_neon_adjusted-* | rg -A40 'map_add_typed'
//! ```

use db_core::vm::batch::Value;
use std::borrow::Cow;
use std::hint::black_box;
use std::mem::size_of;
use std::time::{Duration, Instant};

/// Streaming size: large enough that columns don't fit in L2 (Apple
/// M-series L2 is a few MB/core), so `map_*`/`reduce_*` here measure
/// memory bandwidth as much as compute.
const ROWS_STREAMING: usize = 1_000_000;

/// Cache-resident size: `i64`/`f64` columns at this size are ~128KB,
/// comfortably inside L2, so repeating the kernel many times over the
/// same buffer isolates compute/vectorization from DRAM bandwidth.
const ROWS_CACHED: usize = 16_384;
/// How many passes over the cache-resident buffer per timed rep, so the
/// timed duration is long enough to dominate `Instant` overhead.
const CACHED_ITERS: usize = 64;

fn time_it<F: FnMut()>(reps: usize, mut f: F) -> (Duration, Duration) {
    f(); // untimed warmup: pays first-touch page faults once, not in-band
    let mut samples: Vec<Duration> = (0..reps)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .collect();
    samples.sort();
    (samples[reps / 2], samples[0]) // (median, min)
}

fn report(kernel: &str, typed: Duration, tagged: Duration) {
    let speedup = tagged.as_secs_f64() / typed.as_secs_f64();
    println!(
        "{kernel:<32} typed={:>10?}  tagged={:>10?}  speedup={speedup:.2}x",
        typed, tagged
    );
}

/// Print one side's throughput: `bytes` is total bytes touched (reads +
/// writes) for that representation's own element size, since typed and
/// tagged move different amounts of memory for the "same" kernel.
fn report_bandwidth(label: &str, bytes: usize, elapsed: Duration) {
    let gbps = bytes as f64 / elapsed.as_secs_f64() / 1e9;
    println!("{label:<40} {elapsed:>10?}  ({gbps:>6.1} GB/s)");
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

#[inline(never)]
fn map_add_typed(a: &[i64], b: &[i64], out: &mut [i64]) {
    let n = a.len().min(b.len()).min(out.len());
    let (a, b, out) = (&a[..n], &b[..n], &mut out[..n]);
    for i in 0..n {
        out[i] = a[i] + b[i];
    }
}

#[inline(never)]
fn map_add_tagged(a: &[Value], b: &[Value], out: &mut [Value]) {
    let n = a.len().min(b.len()).min(out.len());
    let (a, b, out) = (&a[..n], &b[..n], &mut out[..n]);
    for i in 0..n {
        out[i] = match (&a[i], &b[i]) {
            (Value::Int(x), Value::Int(y)) => Value::Int(x + y),
            _ => Value::Null,
        };
    }
}

// ---------------------------------------------------------------------
// Kernel: Map arithmetic (float) -- hypothesis: High, autovectorizes
// ---------------------------------------------------------------------

#[inline(never)]
fn map_mul_typed(a: &[f64], b: &[f64], out: &mut [f64]) {
    let n = a.len().min(b.len()).min(out.len());
    let (a, b, out) = (&a[..n], &b[..n], &mut out[..n]);
    for i in 0..n {
        out[i] = a[i] * b[i];
    }
}

#[inline(never)]
fn map_mul_tagged(a: &[Value], b: &[Value], out: &mut [Value]) {
    let n = a.len().min(b.len()).min(out.len());
    let (a, b, out) = (&a[..n], &b[..n], &mut out[..n]);
    for i in 0..n {
        out[i] = match (&a[i], &b[i]) {
            (Value::Float(x), Value::Float(y)) => Value::Float(x * y),
            _ => Value::Null,
        };
    }
}

// ---------------------------------------------------------------------
// Kernel: Map boolean And -- hypothesis: High if predicate becomes bitmask
//
// Caveat: `Vec<bool>` is a byte-per-element mask, so this vectorizes as
// byte-wise AND (16 lanes/NEON op) -- a lower bound on the win, not the
// real design. A packed bitmask (1 bit/element, word-wise AND) is 8x
// denser and would show a larger typed/tagged gap than measured here.
// ---------------------------------------------------------------------

#[inline(never)]
fn map_and_typed(a: &[bool], b: &[bool], out: &mut [bool]) {
    let n = a.len().min(b.len()).min(out.len());
    let (a, b, out) = (&a[..n], &b[..n], &mut out[..n]);
    for i in 0..n {
        out[i] = a[i] && b[i];
    }
}

#[inline(never)]
fn map_and_tagged(a: &[Value], b: &[Value], out: &mut [Value]) {
    let n = a.len().min(b.len()).min(out.len());
    let (a, b, out) = (&a[..n], &b[..n], &mut out[..n]);
    for i in 0..n {
        out[i] = match (&a[i], &b[i]) {
            (Value::Bool(x), Value::Bool(y)) => Value::Bool(*x && *y),
            _ => Value::Null,
        };
    }
}

// ---------------------------------------------------------------------
// Kernel: Reduce Sum (int) -- hypothesis: High, + multiple accumulators
// ---------------------------------------------------------------------

#[inline(never)]
fn reduce_sum_typed(a: &[i64]) -> i64 {
    a.iter().sum()
}

/// 4 independent accumulators (per #141: "2-4 independent accumulators,
/// multiple NEON pipes on M-series") -- integer addition is associative
/// so this is bit-exact with `reduce_sum_typed`, just faster.
#[inline(never)]
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

#[inline(never)]
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
//
// `a.iter().sum::<f64>()` is a SERIAL dependency chain: FP addition is
// not associative, and Rust never reassociates float ops without an
// explicit opt-in (there is no `-ffast-math` equivalent), so this runs
// scalar at ~1 `fadd` per few cycles regardless of `--release`. The
// `_unrolled` variant below breaks that chain into 4 independent partial
// sums (like the int version) so LLVM can vectorize/pipeline it -- this
// is a genuine, deliberate reassociation of the sum and will not be
// bit-exact with the naive left-to-right sum. Whether that's acceptable
// for a `Reduce` opcode is exactly the tradeoff #141 needs to weigh.
// ---------------------------------------------------------------------

#[inline(never)]
fn reduce_sum_f64_typed(a: &[f64]) -> f64 {
    a.iter().sum()
}

#[inline(never)]
fn reduce_sum_f64_typed_unrolled(a: &[f64]) -> f64 {
    let mut acc = [0.0f64; 4];
    let chunks = a.chunks_exact(4);
    let rem = chunks.remainder();
    for c in chunks {
        acc[0] += c[0];
        acc[1] += c[1];
        acc[2] += c[2];
        acc[3] += c[3];
    }
    let mut total = (acc[0] + acc[1]) + (acc[2] + acc[3]);
    for v in rem {
        total += v;
    }
    total
}

#[inline(never)]
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
//
// Split into a count-only variant (pure predicate-evaluation cost) and a
// materializing variant that reuses one buffer via clear()/extend() --
// the original spike's `collect()`-per-rep measured the allocator as
// much as the compaction.
// ---------------------------------------------------------------------

#[inline(never)]
fn filter_count_typed(a: &[i64], threshold: i64) -> usize {
    a.iter().filter(|&&x| x > threshold).count()
}

#[inline(never)]
fn filter_count_tagged(a: &[Value], threshold: i64) -> usize {
    a.iter()
        .filter(|v| matches!(v, Value::Int(x) if *x > threshold))
        .count()
}

#[inline(never)]
fn filter_into_typed(a: &[i64], threshold: i64, out: &mut Vec<i64>) {
    out.clear();
    out.extend(a.iter().copied().filter(|&x| x > threshold));
}

#[inline(never)]
fn filter_into_tagged(a: &[Value], threshold: i64, out: &mut Vec<Value>) {
    out.clear();
    out.extend(
        a.iter()
            .filter(|v| matches!(v, Value::Int(x) if *x > threshold))
            .cloned(),
    );
}

// ---------------------------------------------------------------------
// Kernel: string equality count -- hypothesis: None, included for
// completeness/contrast (offsets+bytes vs `Cow<'static, str>` compare).
// Both sides pointer-chase per row; expect ~1x, which is itself the
// finding (the enum tag isn't the bottleneck for strings).
// ---------------------------------------------------------------------

#[inline(never)]
fn str_eq_count_typed(a: &[String], needle: &str) -> usize {
    a.iter().filter(|s| s.as_str() == needle).count()
}

#[inline(never)]
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
fn neon_adjusted_spike() {
    const REPS: usize = 11;

    println!("\n=== db-core#141 NEON spike (adjusted), median of {REPS} runs, min in parentheses ===");
    println!(
        "size_of::<Value>() = {} bytes vs i64/f64 = 8 bytes, bool = 1 byte (this ratio drives the streaming-size results)\n",
        size_of::<Value>()
    );

    // ---- Cache-resident (compute-isolated): CACHED_ITERS passes over ROWS_CACHED ----
    println!("--- cache-resident ({ROWS_CACHED} rows x {CACHED_ITERS} passes/rep) ---");

    let (a, b) = (int_typed(ROWS_CACHED), int_typed(ROWS_CACHED));
    let mut out = vec![0i64; ROWS_CACHED];
    let (t_typed, _) = time_it(REPS, || {
        for _ in 0..CACHED_ITERS {
            map_add_typed(black_box(&a), black_box(&b), &mut out);
        }
        black_box(&out);
    });
    let (at, bt) = (int_tagged(ROWS_CACHED), int_tagged(ROWS_CACHED));
    let mut out_t = vec![Value::Null; ROWS_CACHED];
    let (t_tagged, _) = time_it(REPS, || {
        for _ in 0..CACHED_ITERS {
            map_add_tagged(black_box(&at), black_box(&bt), &mut out_t);
        }
        black_box(&out_t);
    });
    report("map_add(int)/cached", t_typed, t_tagged);

    let (a, b) = (float_typed(ROWS_CACHED), float_typed(ROWS_CACHED));
    let mut out = vec![0.0f64; ROWS_CACHED];
    let (t_typed, _) = time_it(REPS, || {
        for _ in 0..CACHED_ITERS {
            map_mul_typed(black_box(&a), black_box(&b), &mut out);
        }
        black_box(&out);
    });
    let (at, bt) = (float_tagged(ROWS_CACHED), float_tagged(ROWS_CACHED));
    let mut out_t = vec![Value::Null; ROWS_CACHED];
    let (t_tagged, _) = time_it(REPS, || {
        for _ in 0..CACHED_ITERS {
            map_mul_tagged(black_box(&at), black_box(&bt), &mut out_t);
        }
        black_box(&out_t);
    });
    report("map_mul(float)/cached", t_typed, t_tagged);

    let a = int_typed(ROWS_CACHED);
    let (t_typed, _) = time_it(REPS, || {
        for _ in 0..CACHED_ITERS {
            black_box(reduce_sum_typed(black_box(&a)));
        }
    });
    let (t_typed_unrolled, _) = time_it(REPS, || {
        for _ in 0..CACHED_ITERS {
            black_box(reduce_sum_typed_unrolled(black_box(&a)));
        }
    });
    let at = int_tagged(ROWS_CACHED);
    let (t_tagged, _) = time_it(REPS, || {
        for _ in 0..CACHED_ITERS {
            black_box(reduce_sum_tagged(black_box(&at)));
        }
    });
    report("reduce_sum(int)/cached", t_typed, t_tagged);
    report("reduce_sum(int)/cached/unrolled4", t_typed_unrolled, t_tagged);

    let f = float_typed(ROWS_CACHED);
    let (t_naive, _) = time_it(REPS, || {
        for _ in 0..CACHED_ITERS {
            black_box(reduce_sum_f64_typed(black_box(&f)));
        }
    });
    let (t_unrolled, _) = time_it(REPS, || {
        for _ in 0..CACHED_ITERS {
            black_box(reduce_sum_f64_typed_unrolled(black_box(&f)));
        }
    });
    let ft = float_tagged(ROWS_CACHED);
    let (t_tagged, _) = time_it(REPS, || {
        for _ in 0..CACHED_ITERS {
            black_box(reduce_sum_f64_tagged(black_box(&ft)));
        }
    });
    report("reduce_sum(float)/cached/naive", t_naive, t_tagged);
    report("reduce_sum(float)/cached/unrolled4", t_unrolled, t_tagged);

    // ---- Streaming (bandwidth-bound): ROWS_STREAMING, single pass ----
    println!("\n--- streaming ({ROWS_STREAMING} rows, single pass, bandwidth-bound) ---");

    let (a, b) = (int_typed(ROWS_STREAMING), int_typed(ROWS_STREAMING));
    let mut out = vec![0i64; ROWS_STREAMING];
    let (t_typed, _) = time_it(REPS, || map_add_typed(black_box(&a), black_box(&b), &mut out));
    black_box(&out);
    let (at, bt) = (int_tagged(ROWS_STREAMING), int_tagged(ROWS_STREAMING));
    let mut out_t = vec![Value::Null; ROWS_STREAMING];
    let (t_tagged, _) = time_it(REPS, || {
        map_add_tagged(black_box(&at), black_box(&bt), &mut out_t)
    });
    black_box(&out_t);
    report("map_add(int)/streaming", t_typed, t_tagged);
    // bytes touched: 2 reads + 1 write, per representation's own element size
    report_bandwidth(
        "  map_add(int)/streaming typed BW",
        3 * ROWS_STREAMING * size_of::<i64>(),
        t_typed,
    );
    report_bandwidth(
        "  map_add(int)/streaming tagged BW",
        3 * ROWS_STREAMING * size_of::<Value>(),
        t_tagged,
    );

    let a = int_typed(ROWS_STREAMING);
    let (t_typed, _) = time_it(REPS, || {
        black_box(reduce_sum_typed(black_box(&a)));
    });
    let at = int_tagged(ROWS_STREAMING);
    let (t_tagged, _) = time_it(REPS, || {
        black_box(reduce_sum_tagged(black_box(&at)));
    });
    report("reduce_sum(int)/streaming", t_typed, t_tagged);

    let f = float_typed(ROWS_STREAMING);
    let (t_naive, _) = time_it(REPS, || {
        black_box(reduce_sum_f64_typed(black_box(&f)));
    });
    let (t_unrolled, _) = time_it(REPS, || {
        black_box(reduce_sum_f64_typed_unrolled(black_box(&f)));
    });
    let ft = float_tagged(ROWS_STREAMING);
    let (t_tagged, _) = time_it(REPS, || {
        black_box(reduce_sum_f64_tagged(black_box(&ft)));
    });
    report("reduce_sum(float)/streaming/naive", t_naive, t_tagged);
    report("reduce_sum(float)/streaming/unrolled4", t_unrolled, t_tagged);

    // ---- Map and (bool), Filter, string -- streaming only ----
    let (a, b) = (bool_typed(ROWS_STREAMING), bool_typed(ROWS_STREAMING));
    let mut out = vec![false; ROWS_STREAMING];
    let (t_typed, _) = time_it(REPS, || map_and_typed(black_box(&a), black_box(&b), &mut out));
    black_box(&out);
    let (at, bt) = (bool_tagged(ROWS_STREAMING), bool_tagged(ROWS_STREAMING));
    let mut out_t = vec![Value::Null; ROWS_STREAMING];
    let (t_tagged, _) = time_it(REPS, || {
        map_and_tagged(black_box(&at), black_box(&bt), &mut out_t)
    });
    black_box(&out_t);
    report("map_and(bool)/streaming (byte-mask)", t_typed, t_tagged);

    let a = int_typed(ROWS_STREAMING);
    let threshold = ROWS_STREAMING as i64 / 2;
    let (t_typed_count, _) = time_it(REPS, || {
        black_box(filter_count_typed(black_box(&a), threshold));
    });
    let at = int_tagged(ROWS_STREAMING);
    let (t_tagged_count, _) = time_it(REPS, || {
        black_box(filter_count_tagged(black_box(&at), threshold));
    });
    report("filter_count(int)/streaming", t_typed_count, t_tagged_count);

    let mut into = Vec::with_capacity(ROWS_STREAMING / 2);
    let (t_typed_into, _) = time_it(REPS, || {
        filter_into_typed(black_box(&a), threshold, &mut into);
        black_box(&into);
    });
    let mut into_t = Vec::with_capacity(ROWS_STREAMING / 2);
    let (t_tagged_into, _) = time_it(REPS, || {
        filter_into_tagged(black_box(&at), threshold, &mut into_t);
        black_box(&into_t);
    });
    report("filter_into(int)/streaming (reused buf)", t_typed_into, t_tagged_into);

    let a = str_typed(ROWS_STREAMING);
    let (t_typed, _) = time_it(REPS, || {
        black_box(str_eq_count_typed(black_box(&a), "gamma"));
    });
    let at = str_tagged(ROWS_STREAMING);
    let (t_tagged, _) = time_it(REPS, || {
        black_box(str_eq_count_tagged(black_box(&at), "gamma"));
    });
    report("str_eq_count/streaming", t_typed, t_tagged);

    println!(
        "\nHost: {} / NEON is baseline on aarch64 (no runtime dispatch needed) -- \
         see doc comment for how to confirm codegen with otool/cargo-asm.\n",
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
    // Unrolled float sum reassociates -- not bit-exact, but must be close.
    assert!((reduce_sum_f64_typed_unrolled(&f) - reduce_sum_f64_typed(&f)).abs() < 1e-3);

    let count = filter_count_typed(&a, 500);
    assert_eq!(count, filter_count_tagged(&at, 500));

    let mut into = Vec::new();
    filter_into_typed(&a, 500, &mut into);
    let mut into_t = Vec::new();
    filter_into_tagged(&at, 500, &mut into_t);
    let into_t: Vec<i64> = into_t
        .into_iter()
        .map(|v| match v {
            Value::Int(x) => x,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(into, into_t);
    assert_eq!(into.len(), count);

    let s = str_typed(N);
    let st = str_tagged(N);
    assert_eq!(str_eq_count_typed(&s, "beta"), str_eq_count_tagged(&st, "beta"));
}
