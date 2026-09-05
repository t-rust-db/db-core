# Spike: NEON value for `vm::batch` kernels (db-core#141)

Research spike for [#141](https://github.com/t-rust-db/db-core/issues/141). No
production code changes.

Three tests, run in order:

- **`01_neon_batch_kernels.rs`** — the initial spike: synthetic
  `Vec<T>`-vs-`Vec<Value>` loops shaped like #130's future typed columns.
- **`02_neon_adjusted.rs`** — supersedes 01 for drawing conclusions on
  that question; fixes methodology gaps a review of 01 found (see
  "Round 2" below). 01 is kept as-is rather than edited in place, so the
  review comments that shaped round 2 stay legible against the code they
  were about.
- **`03_batch_specific.rs`** — a different question: exercises the real
  `db_core::vm::batch` constructs (`Batch`, `Opcode`, `Program`, `Vm`,
  `Segment`, `run_parallel`) instead of bare `Vec` loops, to measure how
  much of today's per-batch cost is interpreter/opcode-dispatch overhead
  versus the elementwise work rounds 1/2 measured.
- **`04_adjusted.rs`** — supersedes 03 for drawing conclusions; a review
  of 03 found its single `vm/raw` ratio conflated `LoadColumn`'s clone,
  `Map`'s dispatch, and `Emit`'s row-major transpose into one number, and
  its filter selectivity wasn't comparable to round 1's (see "Round 4"
  below). 03 is kept as-is.

```sh
make help      # list targets
make check     # all four correctness tests (debug, fast)
make run       # all four perf spikes (release) -- or run-01 / run-02 / run-03 / run-04 individually
make asm       # count NEON vector instructions in the round-1/round-2 release binaries
```

## Question

Once [#130](https://github.com/t-rust-db/db-core/issues/130) lands typed
contiguous `Batch` columns, which `vm::batch` opcodes actually gain from
NEON on `aarch64-apple-darwin`, and via which route (autovectorization vs
hand-written `std::arch::aarch64` intrinsics)?

Today `Batch` is `HashMap<String, Vec<Value>>` where `Value` is a 24-byte
tagged enum (`src/vm/batch.rs`) — SIMD has near-zero value on that layout
because every element access is a tag match. Both tests build a plain
typed-contiguous stand-in (`Vec<i64>` / `Vec<f64>` / `Vec<bool>` /
`Vec<String>`) for #130's future columns and compare representative
kernels against the tagged-enum equivalent.

## Hypothesis (from #141)

| Opcode | Expected value | Route to test |
|---|---|---|
| `Map` arithmetic/comparison | High | Autovectorization over `chunks_exact`, verify with `cargo asm` |
| `Map` boolean (And/Or/Not/IsNull) | High if predicate becomes bitmask | Autovectorization |
| `Reduce` Sum/Min/Max/Count | High | Autovectorization + 2-4 independent accumulators |
| `Filter` compaction | Medium | Only candidate for `std::arch::aarch64` intrinsics |
| `GroupReduce`, `HashBuild`/`HashProbe` | Low | Hash-bound; only key hashing benefits |
| `Window`, string Concat, Load*/Emit/control | None | Skip |

## Round 1 results (`01_neon_batch_kernels.rs`)

`make run-01`, 1,000,000 rows, median of 11 runs, Apple Silicon (aarch64):

| Kernel | typed | tagged | speedup |
|---|---|---|---|
| `map_add` (int) | 546.75µs | 1.336ms | 2.44x |
| `map_mul` (float) | 322.17µs | 917.88µs | 2.85x |
| `map_and` (bool) | 460.13µs | 1.002ms | 2.18x |
| `reduce_sum` (int) | 68.58µs | 262.58µs | 3.83x |
| `reduce_sum` (int), unrolled×4 accumulators | 85.88µs | 262.58µs | 3.06x |
| `reduce_sum` (float) | 487.58µs | 548.04µs | 1.12x |
| `filter` compaction (int) | 435.63µs | 2.312ms | 5.31x |
| string equality count | 466.38µs | 444.58µs | 0.95x |

`make asm-01` confirms the release binary contains NEON vector
instructions (1415 `.4s`/`.2d`/`.16b`-style opcodes across the test
binary), so the speedups are genuine autovectorization, not noise.

### Round-1 methodology gaps (why round 2 exists)

A review of round 1 found two issues serious enough to mislead the
#141 decision, plus smaller fixes:

1. **`reduce_sum_f64_typed` (`a.iter().sum::<f64>()`) does not
   autovectorize.** FP addition isn't associative and Rust has no
   fast-math flag, so it's a serial dependency chain, latency-bound —
   which is exactly why its round-1 speedup (1.12x) looked mysteriously
   small next to int's (3.83x). Round 1 already had the fix pattern for
   int (`reduce_sum_typed_unrolled`); round 2 applies the same 4-accumulator
   unroll to float.
2. **At 1M rows (8MB+ per column), `map_*` kernels are measuring memory
   bandwidth as much as vectorization** — well past L1/L2 into
   streaming-from-DRAM territory on M-series. The typed-vs-tagged ratio
   there partly reflects bytes-moved (`size_of::<Value>()` vs
   `size_of::<i64>()`), a real and important effect but a different
   hypothesis than "LLVM emits NEON." Round 2 adds a cache-resident size
   to isolate compute, and reports GB/s so the two effects are
   separable.
3. Smaller: kernels lacked `#[inline(never)]`, so in release mode they
   inline into the test body and can't be disassembled by symbol name
   (`otool | rg map_add_typed` finds nothing). The indexed loop pattern
   (`for i in 0..a.len() { out[i] = a[i] + b[i] }`) leaves bounds checks
   that make vectorization fragile across compiler versions — equalizing
   slice lengths up front removes them. `filter_typed`/`filter_tagged`
   allocate a fresh `Vec` per rep, so they partly benchmark the allocator;
   splitting into a count-only variant and a reused-buffer materializing
   variant separates predicate cost from allocation cost. `map_and` on
   `Vec<bool>` is a byte-mask lower bound, not the packed-bitmask design
   #141 actually proposes.

## Round 2 results (`02_neon_adjusted.rs`)

`make run-02`, median of 11 runs, Apple Silicon (aarch64).
`size_of::<Value>() = 24` bytes vs `i64`/`f64` = 8 bytes.

### Cache-resident (16,384 rows × 64 passes/rep — compute isolated)

| Kernel | typed | tagged | speedup |
|---|---|---|---|
| `map_add` (int) | 476.17µs | 1.289ms | 2.71x |
| `map_mul` (float) | 325.33µs | 1.047ms | 3.22x |
| `reduce_sum` (int) | 84.54µs | 352.42µs | 4.17x |
| `reduce_sum` (int), unrolled×4 | 128.04µs | 352.42µs | 2.75x |
| `reduce_sum` (float), naive | 662.38µs | 704.67µs | **1.06x** |
| `reduce_sum` (float), unrolled×4 | 189.46µs | 704.67µs | **3.72x** |

### Streaming (1,000,000 rows, single pass — bandwidth-bound)

| Kernel | typed | tagged | speedup | typed BW | tagged BW |
|---|---|---|---|---|---|
| `map_add` (int) | 217.63µs | 741.08µs | 3.41x | 110.3 GB/s | 97.2 GB/s |
| `reduce_sum` (int) | 74.33µs | 271.71µs | 3.66x | — | — |
| `reduce_sum` (float), naive | 523.88µs | 546.54µs | **1.04x** | — | — |
| `reduce_sum` (float), unrolled×4 | 143.13µs | 546.54µs | **3.82x** | — | — |
| `map_and` (bool, byte-mask) | 251.58µs | 668.88µs | 2.66x | — | — |
| `filter_count` (int, predicate only) | 68.17µs | 215.71µs | 3.16x | — | — |
| `filter_into` (int, reused buffer) | 344.63µs | 1.270ms | 3.68x | — | — |
| `str_eq_count` | 1.394ms | 1.399ms | **1.00x** | — | — |

`make asm-02` disassembly of `map_add_typed` shows genuine NEON
(`ldp q0,q1` / `add.2d` / `stp q0,q1`). Disassembly of
`reduce_sum_f64_typed` (naive) confirms the hypothesis exactly: LLVM
still emits paired vector *loads* (`ldp q1,q2`) but then extracts scalar
lanes and chains them through a single serial `fadd d0, d0, dN` — the
load is vectorized, the reduction isn't. The `_unrolled` variant breaks
that chain into 4 independent partial sums, which is what recovers the
3.7-3.8x.

## Round 3 results (`03_batch_specific.rs`)

Same total row count (1,000,000), but split into real `BATCH_SIZE`
(1024)-row `Batch` segments and driven through the actual
`Opcode`/`Program`/`Vm` interpreter (`LoadColumn`/`Map`/`Reduce`/`Filter`/
`Emit`), compared against a raw `Vec<Value>` loop doing the identical
work with no VM machinery. `make run-03`:

| Kernel | vm (interpreter) | raw loop | vm/raw overhead |
|---|---|---|---|
| `map_add` (int), per-segment `Vm::execute` | 21.04ms | 1.07ms | **19.6x** |
| `map_add` (int), `run_parallel` (12 threads) | 15.45ms | — | 1.36x vs single-threaded `Vm::execute` |
| `reduce_sum` (int), per-segment `Vm::execute` | 2.53ms | 0.47ms | **5.4x** |
| `filter` (int), per-segment `Vm::execute` | 25.18ms | 1.52ms | **16.6x** |

### What's driving the overhead

Per `Opcode::step` (`src/vm/batch.rs`), each segment pays, on top of the
elementwise work:

- `LoadColumn` clones the whole column `Vec<Value>` into a fresh register
  (`self.registers.insert(*reg, values.clone())`) — twice for a binary
  `Map`.
- Registers live in a `HashMap<usize, Vec<Value>>`, so every `Map`/
  `Reduce`/`Filter` operand is a hash lookup, not an array index.
- `Filter` drains and rebuids every live register's `Vec` element-by-element
  (correctly sized to the survivor count per #110, but still a per-row
  `Vec::push` loop over `Value`, not a vectorizable bulk operation).
- `Emit` transposes column-major registers into row-major output rows
  (`Vec<Vec<Value>>`), which is itself an allocation and copy per row.
- A fresh `Vm` (fresh `HashMap`s) per segment adds allocation churn on top
  — real query execution keeps one `Vm` across segments via `Vm::run`, so
  this specific number overstates the per-segment cost of a longer-lived
  pipeline, but `LoadColumn`'s clone and `Filter`'s rebuild happen every
  segment regardless.
- `run_parallel` recovers some of this via real multi-core parallelism
  (1.36x on 12 available threads for `map_add`, well short of 12x —
  consistent with `Batch::clone()` per segment and lock contention on the
  results `Mutex` in `run_morsels` eating into the scaling).

## Round 3's methodology gaps (why round 4 exists)

A review of round 3 found the single `vm/raw` ratio conflated distinct
costs, and one selectivity bug:

1. **`Emit`'s row-major transpose was invisible inside the ratio.**
   Reading `Vm::step`'s `Opcode::Emit` arm confirms it builds one fresh
   `Vec<Value>` per output row (`rows.push(cols.iter().map(|c|
   c[row].clone()).collect())`), cloning every cell -- for ~1M rows
   that's ~1M small heap allocations, likely dwarfing the `Map`'s
   per-element `match`. Round 3's fixed four-opcode program made this
   inseparable from dispatch cost.
2. **Filter selectivity wasn't comparable to round 1.** Round 3's filter
   column held a global row id (0..1M) against a fixed
   `threshold = BATCH_SIZE/2` -- true for ~99.95% of rows, so only the
   first segment did real filtering; round 1 used 50% selectivity.
3. **Single-threaded baseline didn't pay `Segment::load()`**, which
   `run_parallel` does (a full `Batch::clone()`), so the reported
   parallel speedup was inflated relative to a fair comparison.

Round 4 fixes all three: a **program ladder** (`Load` → `+Map`/`+Reduce`
→ `+Emit`) reporting increments instead of one blended number, a
**per-segment-local filter column** giving consistent ~50% selectivity,
a **matched single-threaded baseline** that also calls `load()`, plus a
**`Vm::new()`-per-segment vs one reused `Vm`** split and **ns/row**
reporting throughout.

## Round 4 results (`04_adjusted.rs`)

`make run-04`, 1,000,000 rows / 977 segments of `BATCH_SIZE`=1024, median
of 11 runs, Apple Silicon (aarch64):

### `map_add(int)` ladder

| Step | time | ns/row |
|---|---|---|
| `Load` (2 columns) | 2.277ms | 2.28 |
| `+ Map` (dispatch + add), increment | 0.144ms | 0.14 |
| `+ Emit` (row transpose), increment | **17.789ms** | **17.79** |
| raw fused loop (no VM) | 0.824ms | 0.82 |

`vm(load+map+emit)/raw` = **27.30x**, but `vm(load+map only)/raw` =
**5.70x** — i.e. dropping `Emit` from the ratio cuts the apparent
overhead by ~5x. `Emit`'s per-row transpose (17.79 ns/row) is by far the
largest single cost in the whole program, over 20x the raw loop's total
per-row cost (0.82 ns/row) on its own.

### `Vm::new()` per segment vs one reused `Vm`

| Variant | time | ns/row |
|---|---|---|
| Fresh `Vm` per segment | 4.396ms | 4.40 |
| One `Vm` reused, `clear_registers()` | 4.346ms | 4.35 |

Per-segment `Vm::new()` setup cost: **~0.05 ns/row** — negligible. The
register `HashMap`'s per-segment allocation is not where the overhead
is.

### `reduce_sum(int)` ladder

| Step | time | ns/row |
|---|---|---|
| `Load` (1 column) | 0.750ms | 0.75 |
| `+ Reduce` (dispatch + sum), increment | 0.898ms | 0.90 |
| `+ Emit` (1 row/segment), increment | 0.108ms | 0.11 |
| raw fused loop (no VM) | 0.378ms | 0.38 |

`vm/raw` = **6.64x**. Unlike `map_add`, `Emit` here is cheap (one output
row per segment, not one per input row) — the overhead is genuinely
dominated by `Load` + `Reduce` dispatch this time, consistent with
round 3's smaller (5.4-5.7x) ratio for this kernel.

### `filter(int)`, selectivity-matched to round 1 (~50%)

| | time | ns/row |
|---|---|---|
| vm (`LoadConst`+`Map`+`Filter`+`Emit`) | 16.342ms | 16.34 |
| raw fused single-pass filter | 0.995ms | 1.00 |

`vm/raw` = **16.42x** — but the VM path is algorithmically two-pass
(materialize a `Gt` predicate column via `Map`, then `Filter` drains and
rebuilds every live register) versus the raw single fused pass, so part
of this ratio is an algorithmic difference, not pure interpreter
overhead. A fair "overhead-only" comparison would need a raw two-pass
reference implementation, which this round doesn't build (a natural
round 5 candidate if the number matters enough to pin down further).

### Single-threaded (paying `load()`) vs `run_parallel`

| | time | ns/row |
|---|---|---|
| single-threaded, incl. `load()` | 24.397ms | 24.40 |
| `run_parallel` | 13.706ms | 13.71 |

Speedup: **1.78x** on 12 available threads — higher than round 3's
uncorrected 1.36-1.58x, because round 3's single-threaded baseline
skipped the `Batch::clone()` `load()` cost that `run_parallel` always
pays. Still well short of 12x, consistent with `Batch::clone()` per
segment and `run_morsels`' results-`Mutex` contention capping scaling
regardless of how the elementwise kernel is implemented.

## Conclusions

- **Int/bool `Map`, `Reduce`, `Filter`**: confirmed across both rounds —
  2.2-5.3x from autovectorization alone, zero `unsafe`. Streaming-size
  typed `map_add` hits ~110 GB/s, near M-series' single-core DRAM
  bandwidth ceiling, so at 1M rows this is genuinely bandwidth-bound and
  the tagged representation's extra bytes (24 vs 8) explain most of its
  slowdown independent of vectorization — both effects favor typed
  columns, for different reasons.
- **Float `Reduce` is NOT "modest but positive" — it is a straight
  scalar serial chain (1.04-1.12x across both rounds) unless the sum is
  deliberately reassociated** into independent partial sums (3.7-3.8x
  once it is). This is the headline correction from round 2: any
  `Reduce` opcode over float columns needs an explicit multi-accumulator
  implementation to get SIMD value at all; relying on the compiler to
  autovectorize a naive `sum()` will not work. Reassociation changes the
  exact result (no longer bit-exact with left-to-right summation) and
  should be called out as a deliberate accuracy tradeoff if adopted, not
  silently assumed.
- **`map_and` here is a byte-mask (`Vec<bool>`) lower bound**, not the
  packed-bitmask design #141 actually proposes for predicates — a real
  1-bit/element mask with word-wise AND should beat this.
- **`filter_count` vs `filter_into`** (round 2): both still show typed
  wins (3.16x / 3.68x), so round 1's allocator-noise concern didn't
  invalidate the conclusion for this workload size, but the split
  confirms materializing the result costs meaningfully more than
  evaluating the predicate alone — relevant for engines that can push a
  filter down into a selection vector instead of eagerly compacting.
- **String equality remains ~1.00x** in both rounds — both
  representations pointer-chase per row; the tagged enum's tag byte is
  not the bottleneck for strings, reinforcing that string kernels should
  be skipped for SIMD investment.
- **Round 3's headline survives, but round 4 attributes it correctly:
  `Emit`'s row-major transpose, not opcode dispatch, is the dominant
  cost for `Map`-shaped programs.** For `map_add`, `Emit` alone is
  17.79 ns/row versus `Load`+`Map` combined at ~2.4 ns/row and the raw
  loop's 0.82 ns/row — over 85% of the VM path's total time is spent
  building one `Vec<Value>` per output row, not in `LoadColumn`'s clone
  or the `Map` `match`. **This changes the fix, not just the diagnosis**:
  a typed-column #130 rewrite that vectorizes the elementwise kernel but
  still materializes output row-by-row (`Vec<Vec<Value>>`) would capture
  almost none of its theoretical win, because `Emit` would still
  dominate. A columnar `Emit` (append to per-column output buffers,
  transpose only if/when a row-major result is actually needed) is a
  cheaper, more load-bearing fix than the elementwise kernel itself for
  any program with a large `Map`-then-`Emit` shape.
- **`reduce_sum` doesn't have this problem** — its `Emit` writes one row
  per *segment*, not per input row, so it stays cheap (0.11 ns/row) and
  the 6.64x overhead there is genuinely `Load`+`Reduce` dispatch, in
  line with round 3's original (uncorrected) number for this kernel.
  `Reduce`/`GroupReduce`-shaped programs don't need the `Emit` fix;
  `Map`/`Filter`-shaped ones (row-cardinality output) do.
  `Vm::new()`'s per-segment `HashMap` allocation is negligible
  (~0.05 ns/row) either way — not worth optimizing on its own.
- **`filter`'s 16.42x is partly algorithmic, not purely interpreter
  overhead**: the VM path is a genuine two-pass algorithm (materialize a
  `Gt` predicate column, then drain-and-rebuild every live register) vs.
  the raw single fused pass, so some of that ratio would remain even
  with a hypothetically zero-cost interpreter. A `Filter` that fuses
  predicate evaluation and compaction (or reuses `Emit`'s fix, writing
  directly into columnar survivors) would need to be measured
  separately to isolate the two effects — a candidate for a further
  round if `Filter` cost specifically becomes a priority.
- **`run_parallel` scales to ~1.78x on 12 threads** once the
  single-threaded baseline is charged for `Segment::load()` fairly
  (round 3's 1.36-1.58x understated this) — still far short of linear,
  pointing at `Batch::clone()`-per-segment and `run_morsels`' shared
  results `Mutex` as separate, real scaling limits independent of the
  elementwise/SIMD question.

## Recommendation

Autovectorize-only for int/bool `Map`/`Reduce`/`Filter` once #130 lands
typed columns — no hand intrinsics needed for the common case. For float
`Reduce` specifically, ship a multi-accumulator (chunked partial-sum)
implementation deliberately, since the compiler will not do this
automatically, and document the resulting non-associativity as an
accepted tradeoff. Skip SIMD for string ops and hash-bound opcodes
(`GroupReduce`, `HashBuild`/`HashProbe`), matching the original
hypothesis. Revisit `map_and`/predicate kernels once #130 defines the
actual validity-bitmap/selection-vector shape, since a real packed
bitmask should outperform the byte-mask lower bound measured here.

**Sequencing implication, revised after round 4**: before (or alongside)
#130's typed-column work, fix `Emit`'s row-major materialization for
`Map`/`Filter`-shaped programs (the single largest cost measured in this
spike, 17.79 ns/row vs. the raw kernel's 0.82 ns/row) — a typed,
vectorized elementwise kernel gains little if its output still gets
rebuilt one `Vec<Value>` per row afterward. `LoadColumn`'s clone and
register-`HashMap` dispatch are real but comparatively small
(~2.4 ns/row combined for `map_add`); `Vm::new()`'s per-segment setup is
negligible. `Reduce`-shaped programs don't have the `Emit` problem and
are dispatch-bound instead, so any fix should be scoped to output
cardinality, not applied uniformly.
