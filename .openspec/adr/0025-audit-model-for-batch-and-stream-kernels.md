# ADR 0025: Audit model for `vm::batch` / `vm::stream` — kernels, not opcodes

## Status

Accepted (#403/#404/#405/#406/#407; evolves ADR-0000's audit claim; relates
to ADR-0007, ADR-0015, ADR-0018, ADR-0024, and epic #130)

## Context

ADR-0000 does **not** scope its bar to the SQLite profile. It says other
execution modes "reuse the verified core -- parser, value model, planner
infrastructure, gates -- **under the same bar**." What is profile-scoped is
the *measurement*: `tools/check_sqlite_profile.py` reads rustc's dep-info
for `parser-row, vm-row, codegen-row, storage-row, engine-row` and checks
invariants (a), (b) and (c) against the files that build actually compiles.

So the gap is not that batch and stream are outside the bar. It is that
several invariants are unmeasured there, and that we had not said what
auditing their *kernels* means -- for which today's answer is "read the
code." Measured against ADR-0000's invariant table on current main:

| | invariant | gate scope today | batch/stream status |
|---|---|---|---|
| (a) | first-party-only dependency closure | profile | **already met** outside `storage-column`: `vm-batch`, `codegen-batch`, `vm-stream`, `codegen-stream`, `storage-stream`, `engine-stream` pull nothing. The crate's only third-party deps are `memmap2`/`ruzstd`, both `storage-column`-only |
| (b) | `unsafe` confined to named carve-outs | profile | **already met** outside `storage-column`: zero `unsafe` in `vm/batch.rs`, `vm/stream.rs`, `vm/engine.rs`, `codegen/batch.rs`, `storage/stream/`, `engine/`. Two sites in `storage/column/mmap.rs`, not yet named in (b)'s table |
| (c) | mode isolation, both directions | crate-wide | met, gated |
| (d) | every feature builds standalone | crate-wide | met, gated |
| (e) | MC/DC snapshot current, obligations discharged | crate-wide (`MCDC_FILES` = all of `src/`) | met, gated |
| (f) | oracle parity defines correct | profile | **not met** -- no external oracle for Parquet or logs |
| (g) | line-coverage floor | profile | **not measured** for other profiles |

Two of the seven are real gaps; the rest either already hold or already
gate. That is the position this ADR builds on, and it is considerably
stronger than "outside the boundary."

The question that prompted this ADR (db-studio#63) was whether the batch
opcodes are too coarse to audit. The same query — scan, filter, group,
count — compiles to 53 row instructions and 8 batch ones
(`tests/integration/opcode_granularity_test.rs`). That looks like a
75-line-per-instruction problem.

It is not, and the measurement points the other way:

| | opcodes, scan+filter+group+count | interpreter |
|---|---|---|
| `vm::row` | 53 | `src/vm/row/vm.rs`, 5191 lines |
| `vm::batch` | 8 | `src/vm/batch.rs`, 4283 lines |

Row has both the smaller instructions **and** the larger interpreter. Work
did not disappear when SQLite made instructions tiny; it moved into
cursors, the sorter, pseudo-tables, and a planner that must emit a correct
jump graph. Decomposing `GroupReduce` (~100 lines, `batch.rs:1503-1606`)
or `Window` (`compute_window`, ~180 lines, `batch.rs:1918-2100`) into
several smaller opcodes would relocate that code behind more interface
seams, not remove it.

Meanwhile the structural property that *does* distinguish the two engines
is untested. `vm::batch` decomposes aggregation into a per-segment partial
state (`GroupReduce`) and a merge/finalize (`Opcode::Combine`, with
`AggPart`) — explicitly modelled on DuckDB's Combine/Finalize, per the
opcode's own documentation. That is an algebraic law. `vm::row`'s
equivalent is an emergent state machine over a sorter
(`SorterInsert`/`SorterSort` plus `Eq`/`IsNull`/`NotNull`/`Goto` break
detection) which obeys no law and can only be tested by example.

`vm::stream` is not a third case. A plain log scan compiles to the batch
opcode set with `lane: "stream"` as the only difference (ADR-0024). Its
own vocabulary — `Scope`, `IndexPred`, `RangeAggFunc`, `EmitMode` — is
what windowed and standing queries add. Whatever audit model covers
`vm::batch` covers a stream scan for free; the stream-specific surface is
the ring, seal, and eviction path.

## Decision

State the audit unit for `vm::batch` / `vm::stream` as the **kernel** — the
function implementing one opcode — and discharge it with obligations of
three kinds, in this order of authority.

### 1. Segment-split invariance (the headline obligation)

> For a given query and dataset, the result must not depend on how rows
> were divided into segments.

This is `Combine`'s documented Combine/Finalize contract restated as a
test. It is checked by running a query over a fixed dataset under several
segmentations — including degenerate ones (all rows in one segment; empty
segments; maximally uneven splits) — and requiring bit-identical output.

It catches a bug class that neither instruction review nor MC/DC reaches.
`AVG` is the worked example: `AggPart::Avg` carries `(sum, count)` and
divides once, at finalize. An implementation that instead averaged the
per-segment averages is correct on one segment and on evenly-sized
segments, and wrong only on uneven ones. The defect is in the merge, not
in any branch.

The obligation transfers to `vm::stream` unchanged, and there it covers
the harder thing. Stream segment boundaries are a seal policy, not a file
property, and sealed segments are **evicted**, leaving only
`SegmentSummary`/`ColumnSummary` (`storage/stream/segment.rs:82-96`),
documented as answering `COUNT`/`SUM`/`MIN`/`MAX` for a segment nobody can
scan again. A summary is another partial aggregate state. So
"independent of segmentation" extends directly to "independent of how much
of the ring was evicted" — the stream engine's least testable correctness
question, covered by the same property rather than by enumerating
eviction schedules.

### 2. Differential agreement against `vm::row`

Both engines live in one crate and already execute together
(`engine::resolve`, ADR-0019/ADR-0024). Any query expressible in both
modes over equivalent data is a free oracle, in both directions. This
audits batch's coarse kernels *against* row's fine ones without requiring
either to look like the other, and it is the honest answer to "how do we
know the big kernel is right" — not by reading it.

Obligations 1 and 2 are both *internal*: they compare db-core against
itself. A kernel that is wrong the same way before and after a change, or
wrong the same way in both engines, passes both. They therefore complement
invariant (f) rather than substituting for it -- which matters most on the
stream side, where an external oracle is hardest to obtain and obligation 1
carries the most weight. #406 takes up (f) for both modes.

### 3. MC/DC on the value-level kernels

Unchanged from ADR-0015 and already in place for the scalar paths
(`mcdc__vm_batch_apply_map_op_*`, `mcdc__vm_batch_arithmetic_*` in
`batch.rs`). MC/DC stays the right instrument for per-value branching
(null propagation, int/float promotion, comparison) and the wrong one for
merge and shape logic, which obligations 1 and 2 cover.

### What this ADR does not do

- **No opcode decomposition.** `GroupReduce`, `HashProbe`, `Window` and
  `Combine` keep their current granularity. Their altitude is load-bearing:
  it is what lets `vm::stream` reuse the batch opcode set outright, and
  what makes the per-segment/merge split expressible at all.
- **No change to `vm::row`.** The SQLite profile's audit claim
  (ADR-0000) and its per-PR measurement are untouched.
- **No new CI gate in this ADR.** It defines what auditing these kernels
  means and records the measured position above; the gates that follow from
  it are filed separately (#403 profile gates, #407 coverage floors) so each
  can be sequenced on its own merits.

## Rejected alternatives

**Decompose the composite kernels into primitive opcodes** (`GroupReduce`
→ `HashKeys` + `ScatterAgg` + `GroupEmit`; `Window` → `PartitionIds` +
`SortWithin` + `WindowScan`). The plan becomes more legible, but the ~100
and ~180 lines still exist, now behind three contracts each instead of
one. Audit surface rises. Rejected on the row-versus-batch interpreter
sizes above: fine-grained opcodes have not produced a smaller thing to
audit anywhere in this codebase.

**Make `vm::batch` mirror `vm::row`'s opcode set.** Explicitly against
db-core's stated design ("Each has its own opcode set; they are not
expected to converge into one", README) and would destroy the segment/merge
decomposition obligation 1 depends on.

**Fold the profile gates into this ADR.** The measured table above shows
(a) and (b) already hold for a stream profile -- `vm-batch` pulls nothing
(the `rayon` claim at `src/vm.rs:46` and in the README is stale since #42;
`run_morsels` is `std::thread::scope`). Turning that into a gate is real
work with its own sequencing, and the column profile needs a carve-out
decision first. Kept separate: #402, #403, #405.

## Path to the bar

What closing the two real gaps requires, filed rather than decided here:

- **Stream profile gate (#403).** Invariants (a) and (b) pass today for
  `parser-column, vm-batch, vm-stream, codegen-batch, codegen-stream,
  storage-stream, engine-stream`. Generalizing
  `tools/check_sqlite_profile.py` to a named profile table makes that a
  per-PR guarantee rather than a fact about one afternoon's measurement.
- **Column profile position (#405).** A decision, not an implementation:
  name `storage/column/mmap.rs`'s two sites as a carve-out the way
  `fcntl.rs` is (recommended), shrink the surface, or go dependency-free.
  ADR-0000 is the document that changes.
- **Oracle parity (#406).** Column: a pinned DuckDB/pyarrow oracle over the
  same Parquet, in the weekly job, structurally identical to sqlite3's
  role. Stream: name the *oracle by construction* pattern the tests already
  use -- seeded generators plus an independent re-parse, as in
  `tests/unit/engine_stream_public_api_test.rs` -- and apply it
  systematically.
- **Per-profile coverage floors (#407).** Follows the profile table from
  #403.

Obligation 1 itself is #404.

## Relation to epic #130

#130 replaces `Batch`'s `HashMap<String, Arc<Vec<Value>>>` with typed
columnar buffers and migrates every batch opcode family onto them, one PR
per family (its child 4). That is the largest correctness risk in the
backlog, and this ADR's obligations are what make it reviewable:

- Obligation 1 (#404) pins `GroupReduce`'s partial-state shape and
  `Combine`'s merge -- exactly what child 4 rewrites -- and should land
  before the migration starts.
- Obligation 2 gives each family a cross-engine oracle during the port.
- #399 (child of #130) adds the per-segment specialization constraint and
  its specialized-versus-dynamic agreement obligation, which is the same
  idea applied within one engine.
- #406's external oracle is what catches an error the internal obligations
  share.

## Open question: type erasure at the stream/batch boundary

Recorded here as context because it bears on which kernels need
obligation 3, and deliberately **not decided** in this ADR.

Stream columns are typed. A sealed segment stores
`OwnedColumn::{Dict, Str, Int, Float, Bool}`
(`storage/stream/segment.rs:26-42`), and `detect::Format` locks the parser
on at open. But `adapter::materialize` → `column_values`
(`storage/stream/adapter.rs:95-165`) erases that type at the batch
boundary: `OwnedColumn::Int(v)` is boxed into one `Value::Int` per row,
and a `Dict` column is decoded to an owned `String` per row via
`field_str`. `apply_map_op`/`arithmetic`/`compare_values` then rediscover,
per value, what the segment knew per column — which is exactly where the
current MC/DC obligations cluster.

A future specialization would carry the segment's encoding into the batch
and choose a monomorphic kernel per segment. Two constraints on any such
proposal:

- The schema is per **segment**, not per file. A JSONL field can seal as
  `Int` in one segment and `Str` in the next. So the compiled program must
  stay untyped and specialization must be chosen at materialize time —
  one program, many specializations.
- That shape yields its own cheap obligation: **the specialized kernel and
  the dynamic kernel must agree on every segment**, a differential property
  in the spirit of obligation 2. It is what would make specialization
  safe to add incrementally instead of as a trusted rewrite.

No longer deferred: filed as #399, a child of epic #130, where the typed
`Batch` this depends on is being built. The two constraints above are
binding requirements on that epic's design -- they affect its children 1
(ADR + design) and 3 (`Batch` core types), not only the stream child,
because a design that types the program against a source-level schema
serves Parquet and breaks on logs.

## Consequences

- `vm::batch`/`vm::stream` gain a stated audit model. ADR-0000's bar
  always covered them; what was missing was a definition for their kernels
  and measurement for two of its seven invariants.
- The headline obligation is one property covering batch's parallel merge
  and stream's eviction path — the two places where example-based tests
  scale worst.
- Batch opcode granularity is settled as a deliberate decision with a
  recorded rationale, rather than re-litigated per reviewer. db-studio#63's
  UI work (surfacing `lane` and a per-lane legend) becomes the user-facing
  half of the same explanation.
- Implementation cost of the model itself is test-side only: no opcode,
  planner, or interpreter change is required to adopt this ADR. The gates
  and oracles in "Path to the bar" are separately scoped and separately
  filed.
- Epic #130 acquires a correctness argument that does not rest on review of
  the migrated kernels.
- The stream/batch type erasure above is now recorded rather than
  rediscovered; if specialization is taken up, this ADR names the two
  constraints it must satisfy.
