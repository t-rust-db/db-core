# Spike: typed hash key for `GroupReduce` (db-core#183)

Research spike for [#183](https://github.com/t-rust-db/db-core/issues/183),
the "cheap proof" child of [#130](https://github.com/t-rust-db/db-core/issues/130)
(arrow-style typed columnar `Batch` epic). No production code changes.

Two tests, run in order:

- **`01_groupreduce_key_hashing.rs`** — the initial spike: stringify-vs-typed
  key across three row counts and three cardinalities.
- **`02_groupreduce_key_hashing_adjusted.rs`** — supersedes 01 for drawing
  conclusions on the high-cardinality result. 01 found a real ~2-3x win at
  low/medium cardinality but an unexpected regression (0.71x) at the
  highest cardinality tested (1M rows, unique groups) with only 3
  iterations at that scale. Round 2 re-measures just that case with a
  warm-up pass plus 20 iterations (vs. 3), and adds a 5M-row unique-key
  case to see whether the regression widens, narrows, or reverses. 01 is
  kept as-is rather than edited in place, so the review comment that
  motivated round 2 stays legible against the code it was about.

```sh
make help      # list targets
make check     # both correctness tests (debug, fast) -- or check-01 / check-02
make run       # both perf spikes (release) -- or run-01 / run-02
```

## Question

`Opcode::GroupReduce` (`src/vm/batch.rs`) builds its per-row group key by
cloning each key `Value`, calling `Value::to_string()` on each, joining
with a NUL separator into a fresh `String`, then hashing that `String` in
a `HashMap<String, usize>` -- 3-4 heap allocations per row for the key
alone. `Opcode::HashBuild`/`HashProbe` already solve "hash a `Vec<Value>`
key" properly via `JoinKey`'s per-variant `Hash` impl (no stringification).
Does porting that trick to `GroupReduce` deliver #130's hypothesized
5x-20x on this opcode?

`GroupKey` (this spike, not `JoinKey`) uses `Value`'s own derived
`PartialEq` (`Null == Null`) rather than `JoinKey`'s NULL-poisoned join
equality (`NULL != NULL`) -- `GROUP BY` must group `NULL`s together;
join equality must not.

## Method

`01_groupreduce_key_hashing.rs` builds synthetic key columns (2 columns,
alternating `Int`/`Str`, cycling through a fixed cardinality) at three row
counts (10K/100K/1M) and three cardinalities (low/fixed, medium/1% of
rows, high/unique-per-row), and times grouping under both strategies.

A fast correctness test (`stringify_and_typed_keys_agree_on_group_count`,
no `--ignored` needed) confirms both strategies produce the same group
count, including when a key column carries `NULL`s.

## Results

Apple Silicon, `--release`, 2 key columns (alternating `Int`/`Str`):

| rows | cardinality | stringify (avg/iter) | typed (avg/iter) | speedup |
|------|-------------|----------------------|-------------------|---------|
| 10,000 | low (100 groups) | 31.2 µs | 9.2 µs | **3.4x** |
| 10,000 | medium (1% of rows, 100 groups) | 21.6 µs | 9.1 µs | **2.4x** |
| 10,000 | high (unique, 10,000 groups) | 27.0 µs | 16.1 µs | **1.7x** |
| 100,000 | low (100 groups) | 1.10 ms | 0.47 ms | **2.4x** |
| 100,000 | medium (1% of rows, 1,000 groups) | 1.09 ms | 0.47 ms | **2.3x** |
| 100,000 | high (unique, 100,000 groups) | 1.38 ms | 0.81 ms | **1.7x** |
| 1,000,000 | low (100 groups) | 36.3 ms | 15.3 ms | **2.4x** |
| 1,000,000 | medium (1% of rows, 10,000 groups) | 38.0 ms | 15.9 ms | **2.4x** |
| 1,000,000 | high (unique, 1,000,000 groups) | 68.8 ms | 96.7 ms | **0.71x (regression)** |

Against #130's expectations table row 1 ("5x-20x on this opcode"): the
typed key is a real, consistent **~2-3x** win at low/medium group
cardinality across every row count tested -- welcome, but well short of
the hypothesized 5x-20x. At the highest cardinality tested (every row its
own group, 1M distinct groups), the typed key is **slower**, not faster
(0.71x) -- the opposite of the hypothesis's direction.

**Round 2** (`02_groupreduce_key_hashing_adjusted.rs`) re-checked the
high-cardinality case with a 3-iteration warm-up plus 20 timed iterations
(vs. round 1's 3, untimed cold start), and added a 5M-row unique-key case:

| rows | cardinality | iterations | stringify (avg/iter) | typed (avg/iter) | speedup |
|------|-------------|------------|------------------------|--------------------|---------|
| 1,000,000 | unique | 20 | 10.05 ms | 12.83 ms | **0.78x** |
| 5,000,000 | unique | 20 | 63.95 ms | 86.43 ms | **0.74x** |

**The regression is real, not noise.** It holds at both 1M and 5M rows
under a properly warmed-up, 20-iteration measurement, and if anything
widens slightly as cardinality grows (0.78x → 0.74x). The likely cause:
at near-unique cardinality the `HashMap` itself (not the key
construction) dominates -- both strategies pay the same growth/rehash
cost, but the typed key's `Eq` compares a `Vec<Value>` element-by-element
(a `Str` variant among them) while equal-length interned-shape `String`
comparison in the stringify path may be cheaper per probe at this
specific string shape (`"group-N"`, short and numeric-suffixed). This is
a hypothesis, not confirmed further here -- worth profiling if the
follow-up ticket below wants to chase the last bit of this case.

## Go/no-go

**Conditional go, scoped to bounded cardinality.** The typed key is a
real, low-risk win (~2-3x, no new dependencies, `JoinKey`'s pattern
already proven in production for `HashBuild`/`HashProbe`) for the common
case -- `GROUP BY` over a bounded/moderate number of groups, which is the
overwhelming majority of real `GROUP BY` queries. It should be ported
into `Opcode::GroupReduce`'s real implementation as its own follow-up
ticket.

It is **not** the 5x-20x #130's expectations table hypothesized, so #130
should revise that row down to ~2-3x rather than treat this spike as
confirming the original number. The high-cardinality (near-unique-key)
regression is confirmed real (round 2), not a measurement artifact -- the
follow-up ticket should either accept a documented regression at that
extreme (rare in practice: `GROUP BY` over a column with as many distinct
values as rows is close to a no-op grouping-wise) or investigate the
`Eq`/probe-cost hypothesis above before shipping unconditionally.
