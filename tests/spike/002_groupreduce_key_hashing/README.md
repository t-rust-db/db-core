# Spike: typed hash key for `GroupReduce` (db-core#183)

Research spike for [#183](https://github.com/t-rust-db/db-core/issues/183),
the "cheap proof" child of [#130](https://github.com/t-rust-db/db-core/issues/130)
(arrow-style typed columnar `Batch` epic). No production code changes.

```sh
make help   # list targets
make check  # correctness test (debug, fast)
make run    # perf spike (release) -- prints timings + speedup
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
(0.71x) -- the opposite of the hypothesis's direction. Only 3 iterations
were run at the 1M-row scale to keep the whole spike fast; the
low/medium-cardinality results are stable across 10-50 iterations and
consistently landed in the same 2.3x-2.4x band, so that pattern is
trustworthy, but the high-cardinality regression at 1M rows deserves a
rerun with more iterations before treating it as settled rather than
noise from `HashMap` resize amortization interacting badly with the
typed key's larger `Vec<Value>`-of-2 allocation compared to a single
already-built `String`.

## Go/no-go

**Conditional go.** The typed key is a real, low-risk win (~2-3x, no new
dependencies, `JoinKey`'s pattern already proven in production for
`HashBuild`/`HashProbe`) for the common case -- `GROUP BY` over a
bounded/moderate number of groups, which is the overwhelming majority of
real `GROUP BY` queries. It should be ported into `Opcode::GroupReduce`'s
real implementation as its own follow-up ticket.

It is **not** the 5x-20x #130's expectations table hypothesized, so #130
should revise that row down to ~2-3x rather than treat this spike as
confirming the original number. The high-cardinality (near-unique-key)
regression should be investigated (more iterations, check allocator
behavior/`HashMap` growth pattern) before the follow-up ticket claims a
universal win -- worth a one-line caveat in that ticket rather than a
blocker to starting it, since low/medium cardinality is what real
workloads mostly look like.
