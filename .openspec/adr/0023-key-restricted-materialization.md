# ADR 0023: Key-restricted materialization of the SQLite lookup side

## Status

Proposed (#368/#371, v2 follow-up to epic #317 / ADR-0019)

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

**Do not implement key-restricted materialization yet.** This ADR
documents the measurement this needs before a design is chosen, rather
than picking an implementation ahead of evidence — matching the standard
ADR-0019 already set for this exact question.

Concretely, before any code changes:

1. Benchmark whole-table materialization against a synthetic
   one-seek-per-key baseline across lookup table sizes (1K, 10K, 100K,
   1M rows) and probe-side selectivity (probe touches 1%, 10%, 100% of
   lookup keys) — using the existing perf-probe convention (row benches
   drift ±10%; compare compiled queries against `main`'s `src`, per this
   crate's perf-change testing practice).
2. Only if the benchmark shows a lookup table size/selectivity region
   where whole-table materialization measurably loses, design one of:
   - **Batched multi-key seek in `engine::row`**: extend
     `TableCursor` (or add a sibling) with an `IN`-list seek that avoids
     one B-tree descent per key — a `storage::row::btree` change, not an
     `engine::resolve` change.
   - **An index structure for the lookup side** that supports range or
     batch lookups more cheaply than repeated point seeks — a larger
     change, only justified if the batched-seek approach above still
     underperforms.
3. If the benchmark instead confirms whole-table materialization holds up
   across realistic dimension-table sizes (which ADR-0019 assumed but did
   not measure), close this as "measured, not needed" rather than
   implementing speculatively.

This ADR is therefore a **measurement plan**, not an implementation
decision — the actual mechanism (if any) is deferred to whichever of the
two options above the benchmark data supports.

### Rejected alternative: implement batched seek now, without measuring

Considered building the batched multi-key seek first since it's the more
obviously useful of the two options if key-restriction turns out to help
at all. Rejected: this is exactly the kind of unmeasured perf work this
crate's own convention warns against (see `perf-changes-need-ab-probe`
practice) — building it before confirming the whole-table scan is actually
the bottleneck risks solving a problem that measurement would show doesn't
exist for the dimension-table sizes ADR-0019 targets.

## Structure

- No production code changes from this ADR alone.
- New benchmark(s) under the crate's existing perf-probe harness,
  comparing `cross_mode::scan_table_as_batch`
  (`src/engine/cross_mode.rs`) whole-table materialization against a
  synthetic repeated-seek baseline, parameterized by table size and
  selectivity.
- Follow-up ADR (0024+) only if the benchmark motivates one of the two
  design options above; this ADR's acceptance criterion is the benchmark
  data and a documented decision, not new lookup-path code.

## Consequences

- Keeps ADR-0019's existing whole-table-materialization behavior
  unchanged until data says otherwise — no regression risk from this ADR.
- Turns an assumption ("full scan is fine for dimension-table sizes") into
  a measured claim, which is the standard this crate already holds other
  perf-sensitive changes to.
- Sub-ticket #371 tracks writing and running the benchmark; a follow-up
  ticket (only opened if warranted) would track whichever implementation
  the data supports.
