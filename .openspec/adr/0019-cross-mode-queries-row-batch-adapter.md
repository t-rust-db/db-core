# ADR 0019: Cross-mode queries — row→batch adapter for SQLite lookup sides

## Status

Proposed (#312, part of epic #317)

## Decision

A cross-mode query joins a **driving** source — stream `.log` (#305's
`Segment`/`TailSource` adapters) or batch `.parquet` — to one or more
**lookup** sides: `.sqlite` tables. Both sides are `vm::batch::Source`s;
the join itself is `vm::batch`'s existing `HashBuild`/`HashProbe`
(`src/vm/batch.rs:335-361`), planned by the existing join-side selection
in `codegen::batch::compile` (`src/codegen/batch.rs:1490-1560`). Batch
mode already joins table to table today — this ADR does not introduce
joins, it introduces one new piece: a **row→batch adapter**, `impl
vm::batch::Segment`/`Source` over a SQLite table read through
`engine::row`, materializing `Program::columns_to_load()`
(`src/vm/batch.rs:557`) into `vm::batch::Value`.

### Materialization: whole table (v1)

The lookup side materializes in full, once per query, on
`Source::next_batch()`'s first call. Dimension tables are the assumed
shape (hosts, services, users — thousands of rows, not billions); a full
scan through `engine::row` is bounded and simple. Key-restricted
materialization (`WHERE key IN (…)`, pushing the driving side's observed
keys down as row-engine seeks) is rejected for v1: `engine::row`'s
`TableCursor::seek` (`src/engine/row/adapter.rs:284`,
`src/storage/row/btree.rs:290`) is a single-key point seek, not a
batched IN-list operator, so "key-restricted" today means one seek call
per distinct key the driving side produces — a per-row round trip
through the row engine's cursor machinery, not a cheap win over a single
table scan for the dimension-table sizes this ADR targets. The switch:
revisit key-restricted materialization only when a lookup table's row
count or the row engine's per-seek cost make a full scan measurably
worse than one-seek-per-key — measured, not assumed (same standard as
the stream ring's hot-window claim, ADR-0018).

### Snapshot semantics

Per-query. Whole-table materialization reads the SQLite table once, at
adapter construction; the result is a private, immutable snapshot for
the life of that query. A live tail joined against a `.sqlite` lookup
sees one consistent lookup snapshot per emission, never a lookup table
that changes mid-query. No transaction or locking scheme is needed
beyond what a single read through `engine::row` already provides.

### Value crossing

`vm::batch::Value` (`src/vm/batch.rs:93`) and `crate::value::Value`
(`src/value.rs:18`) are different types on purpose (ADR-0010,
ADR-0014: the row VM never sees batch values, and vice versa). Today
only `value::Value → engine::Cell` exists (`src/engine.rs:100`); no
`vm::batch::Value` conversion exists yet. The adapter owns a new,
explicit `value::Value → vm::batch::Value` conversion, applied per cell
as `engine::row` rows are materialized into a `Batch`. This conversion
lives with the adapter (parallel to the row engine's existing `Value →
Cell`), not in `db-core::value` or `vm::batch` — it is a boundary
concern between two engines, not a property of either value type.

### Which side builds

`HashBuild` always takes the SQLite lookup side; the driving
(stream/batch) side probes. This is a **planner rule**, not a
cost-based decision: the lookup side is bounded by construction (whole
table materialized up front), the driving side is not (a live tail is
unbounded, a `.parquet` scan can be arbitrarily large). `codegen::batch`
already picks build/probe by which side of a `Join` it resolves as
"right" (`src/codegen/batch.rs:1490-1545`); cross-mode compilation
fixes the lookup side as that right/build side unconditionally, rather
than exposing it to whatever cost heuristic future table-to-table joins
might grow.

### Grammar

No new syntax. The parser's `FromClause`/`Join` AST
(`src/parser/ast.rs:252-280`) already supports multiple `TableRef`s and
join constraints — this is what row mode's existing JOIN support is
built on. A cross-mode query is exactly that grammar, where one
`TableRef` resolves to a `.log`/`.parquet` source and another resolves
to a `.sqlite` table through the new adapter. #316 (parser: multi-source
`FROM` spelling) is therefore descoped to *resolution*, not grammar: no
parser change, only the planner/engine-lookup step that decides what
kind of source a `TableRef` names.

### Schema

Join-column resolution uses `Engine::tables()`
(`src/engine.rs:304-333`, #310, closed), implemented by both
`RowEngine` (`src/engine/row.rs:348`) and `BatchEngine`
(`src/engine/column.rs:403`). The adapter's `Segment`/`Source` impl
reads schema through the same trait a SQLite `RowEngine` already
exposes; no new schema type.

### Rejected alternative: a separate `federate` operator

The epic's first sketch proposed a `federate` lookup-join operator
above the row and batch engines. Rejected: it would duplicate a join
`vm::batch` already implements, and — unlike the adapter approach —
could not share `codegen::batch`'s existing join planner or its
`EXPLAIN` output (#315 depends on cross-mode joins showing up in the
same plan tree as any other batch join, not a separate operator
family).

## Structure

- `src/engine/row/batch_adapter.rs` (new) — `impl vm::batch::Segment`
  for a materialized SQLite table snapshot, `impl vm::batch::Source`
  driving it from `Engine::tables()` + `engine::row` cursors; the
  `value::Value → vm::batch::Value` conversion.
- `src/codegen/batch.rs` — extend join-side resolution so a `TableRef`
  naming a `.sqlite` table always resolves to the build side (existing
  `HashBuild`/`HashProbe` machinery, no opcode change).
- No `src/parser` change (see Grammar, above).

## Consequences

- Whole-table materialization means a cross-mode query's memory cost is
  bounded by the lookup table's size, not the driving side's — the
  opposite of a naive nested-loop join, and the reason the build side is
  fixed rather than cost-chosen.
- Key-restricted materialization stays future work; if it lands, it
  needs either a batched multi-key seek in `engine::row` or an index
  structure that doesn't yet exist there (see #298's single-key `seek`,
  `src/storage/row/btree.rs:290`).
- `#316` becomes a resolution-layer ticket, not a grammar ticket — worth
  reflecting back onto epic #317 so it isn't scoped as parser work.
