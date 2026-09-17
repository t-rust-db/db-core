# ADR 0026: Late materialization — two-phase segment decode

## Status

Proposed (#460; builds on #265 selection vector, #436 chunked columnar
output, #456 streaming, #458 row-group pruning)

## Context

A filtered projection decodes every projected column for every row, then
discards the rows the predicate rejects. For
`SELECT id, amount, region, customer_id FROM bench WHERE amount > 9900`
at 1% selectivity, three of four columns are fully decoded for rows that
never survive.

This is eager in two independent places, not one:

- `codegen/batch.rs::compile()` emits `LoadColumn` for every projected
  column before the `Filter` opcode, by design — `Filter` only shrinks
  registers that are already live, so anything loaded afterwards would
  desync from the filtered length (see the comment at the top of
  `compile()`).
- `engine/column.rs::RowGroupSegment::load()` decodes every column
  `Program::columns_to_load()` names, for the whole row group, before the
  VM executes a single opcode. `columns_to_load()` does not distinguish a
  predicate column from a projection-only column.

Fixing the codegen order alone changes nothing: `load()` already decodes
everything before `Vm::run` starts. Fixing the storage layer alone has no
selection to consume, since selection is a VM-side concept
(`vm::batch::Selection`, introduced by #265) computed only after `Filter`
runs.

`RowGroupReader` (`storage/column/parquet/parquet_file.rs`) also only
supports whole-column decode (`read_int64_column`, `read_string_column`,
etc.) — there is no API to decode a column at a given set of row
positions.

## Decision

Split segment decode into two phases, driven by which columns the
compiled `Program` reads before vs. after `Filter`:

1. **Predicate phase.** `RowGroupSegment::load()` decodes only the
   columns the WHERE clause references (`Program`'s pre-`Filter`
   `LoadColumn` set), for the whole row group, and returns a `Batch`
   containing those columns. The VM runs up through `Filter`, producing
   `Selection { base_len, indices }`.
2. **Projection phase.** For the row positions in `indices`, decode the
   remaining projected columns via a new positional read API on
   `RowGroupReader` — `read_*_column_at(&self, positions: &[u32])` —
   added alongside the existing whole-column reads, one variant per
   physical type. The VM resumes with those columns loaded directly at
   selected positions (no `resolve_selection` compaction needed for
   them, since they were never loaded at full length).

Both phases stay within `Segment::load()`'s existing contract (`fn load
(&self) -> Result<Arc<Batch>, VmError>`) by giving `RowGroupSegment` the
compiled `Program` and letting it drive the VM internally for phase 1
before returning — the alternative (exposing partial decode as two
separate trait methods) would leak selection-vector machinery across the
`engine`/`vm` seam (ADR-0017) that this ADR does not otherwise disturb.

`codegen/batch.rs::compile()` is changed to classify each `LoadColumn` as
predicate-only or projection-only based on whether the column is
referenced by the WHERE clause, and to defer projection-only loads to
after `Filter`, reading through the pending `Selection` rather than
triggering `resolve_selection` (matching how `Emit` and `GroupReduce`
already resolve selection lazily).

## Consequences

- Correctness-neutral: output rows and values are unchanged; only which
  columns get decoded, and when, changes.
- A predicate over a column that is not projected, multiple predicate
  columns, NULLs in the predicate column, and zero-survivor row groups
  must all still round-trip correctly — these become regression tests,
  not new mechanism.
- Segment-split invariance (#404) still holds: phase 1 + phase 2 per
  segment together still decode the same physical row set as a single
  eager load would, so splitting a scan into more/fewer segments cannot
  change results.
- `is_streamable` (#456) and chunked columnar output (#436) are
  unaffected: the two-phase split happens inside a single segment's
  `load()`, before `Vm::run` returns a `Batch`/chunk to the caller.
- Page-level skipping (mentioned in #460 as a stretch goal, dependent on
  #458's row-group statistics extending to pages) is out of scope here;
  this ADR only removes *row*-level over-decoding within a surviving row
  group. A future ADR can extend `read_*_column_at` to skip whole pages
  whose row range contains no selected position.
- `filter_1pct`/`filter_50pct` parity validation against the external
  `column-rs` benchmark suite is out of scope for the PR that implements
  this ADR; the PR instead reports decoded-column/row counts from
  in-repo instrumentation. Parity validation is a follow-up, matching how
  #458 was scoped.
