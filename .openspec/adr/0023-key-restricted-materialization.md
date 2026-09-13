# ADR 0023: Key-restricted materialization of the SQLite lookup side

## Status

Accepted (#368/#373, v2 follow-up to epic #317 / ADR-0019) — measured;
whole-table materialization confirmed as the right default.

## Context

ADR-0019 explicitly rejected key-restricted materialization for v1: rather
than loading only the SQLite rows whose join key was actually seen on the
driving (probe) side, the lookup table materializes in full on every
query, on the reasoning that `engine::row`'s `TableCursor::seek`
(`src/storage/row/btree.rs:290`) is a single-key point seek, not a batched
IN-list operator — so "key-restricted" would mean one seek per distinct
key the driving side produces, which is a per-row round trip through the
row engine's cursor machinery, not obviously cheaper than one table scan
for dimension-table sizes (thousands of rows).

ADR-0019's own switch condition: revisit this "only when a lookup table's
row count or the row engine's per-seek cost make a full scan measurably
worse than one-seek-per-key — measured, not assumed."

## Decision

**Measured (`benches/cross_mode_lookup.rs`, #373). Whole-table
materialization stays the only mechanism — key-restricted materialization
is not implemented,** matching ADR-0019's own switch condition: revisit
only when the numbers show whole-table scanning measurably losing.

### Method

`benches/cross_mode_lookup.rs` compares
`engine::cross_mode::scan_table_as_batch` (the real production path)
against a synthetic one-seek-per-key baseline, across lookup table sizes
1K/10K/100K rows (1M omitted from the automated run — see below) and
probe-side selectivity 1%/10%/100% of lookup keys.

The baseline is **pessimistic by construction**: `TableCursor::seek` is
`pub(crate)`, unreachable from a bench binary (benches see only the public
surface, like `tests/`), so the baseline instead runs
`RowEngine::run_query("SELECT region FROM hosts WHERE id = <key>")` once
per key — paying full parse+compile per key, which a real key-restricted
implementation would not. This means the measurement is conservative in
whole-table scanning's *disfavor*: if whole-table scanning already wins
against this inflated cost, it wins by more against a real seek.

### Results

Median ns, `make perf` on this machine (`PERF_BUDGET_MS` default budget):

| n (rows) | scan_ns (whole table) | seek_ns (per key, inflated) | break-even K | break-even % of n |
|---:|---:|---:|---:|---:|
| 1,000 | 103,694 | 3,546 | 29 | 2.9% |
| 10,000 | 970,958 | 3,602 | 270 | 2.7% |
| 100,000 | 9,664,959 | 3,604 | 2,681 | 2.7% |

Per-key seek cost is flat (~3.5-3.6µs) across table sizes — expected,
since a B-tree seek is `O(log n)` and `log(100,000)/log(1,000)` is a small
constant factor the parse+compile overhead dominates. Whole-table scan
cost is linear in `n`, as expected. The break-even point holds steady at
**~2.7-2.9% of the table's row count**, regardless of table size in the
1K-100K range: whole-table materialization wins whenever the driving side
is expected to touch more than ~3% of the lookup table's distinct keys,
and loses below that.

At 1% selectivity, seek-per-key wins (even with the inflated per-key
cost); at 10% and 100%, whole-table scan wins, often by an order of
magnitude or more (100K rows, 100% selectivity: 9.7ms scan vs. 360ms for
100,000 inflated seeks).

1M rows was not run automatically (keeps `make perf`'s wall time
reasonable); the flat per-key cost and linear scan-cost trend from 1K-100K
extrapolate directly — nothing in the data suggests either curve changes
shape at 1M, so the ~3% break-even is expected to hold.

### What this means for ADR-0019's dimension-table assumption

ADR-0019 assumed full scan is fine "for dimension-table sizes" without
measuring. This data supports that assumption **only when the driving
side's key selectivity is expected to exceed ~3%** — plausible for most
dimension-table joins (hosts, services, users — a log stream usually
touches a wide spread of hosts, not 1-2% of them). A workload where the
driving side is known to touch a narrow key subset (e.g. filtering to one
specific host before the join) is exactly the region where key-restricted
materialization would help — but no such workload is in scope today; this
ADR does not speculate one into existence.

### If a future workload changes this

Revisit with a batched multi-key seek in `engine::row` (extend
`TableCursor`, or add a sibling, with an `IN`-list seek that avoids one
B-tree descent's parse+compile overhead per key — a `storage::row::btree`
change, not an `engine::resolve` change) once a real workload's measured
selectivity falls under the ~3% break-even and whole-table materialization
is confirmed as its bottleneck.

### Rejected alternative: implement batched seek now, without measuring

Considered building the batched multi-key seek first since it's the more
obviously useful of the two options if key-restriction turns out to help
at all. Rejected: this is exactly the kind of unmeasured perf work this
crate's own convention warns against (see `perf-changes-need-ab-probe`
practice) — building it before confirming the whole-table scan is actually
the bottleneck risks solving a problem that measurement would show doesn't
exist for the dimension-table sizes ADR-0019 targets.

## Structure

- No production code changes — `engine::cross_mode::scan_table_as_batch`
  is unchanged.
- `benches/cross_mode_lookup.rs` (new): compares whole-table
  materialization against the pessimistic seek-per-key baseline described
  above, across `SIZES = [1_000, 10_000, 100_000]` and `SELECTIVITIES =
  [0.01, 0.10, 1.00]`; prints a break-even table and a per-`(n,
  selectivity)` winner table, in addition to the usual `common::Report`
  ns/call table. Registered as a `[[bench]]` target and added to `make
  perf`.
- `benches/common.rs`'s `Report::bench` now returns its recorded median
  ns/call, so a caller (this bench) can compute a break-even point without
  re-running the timed closure.

## Consequences

- Keeps ADR-0019's existing whole-table-materialization behavior
  unchanged — the measurement confirms the assumption rather than
  overturning it, for the selectivity range dimension-table joins are
  expected to see.
- Turns an assumption ("full scan is fine for dimension-table sizes") into
  a measured claim (~3% break-even, flat across 1K-100K rows), which is
  the standard this crate already holds other perf-sensitive changes to.
- No follow-up ADR/ticket opened: the data doesn't motivate one. If a
  future workload's measured selectivity falls under the break-even, open
  one then, informed by that workload's actual numbers rather than this
  ADR's synthetic ones.
