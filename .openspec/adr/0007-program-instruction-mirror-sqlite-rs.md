# ADR 0007: Batch execution mirrors sqlite-rs's Program/Instruction shape

> Source: session discussion, 2026-09-04 — after `db-core` consolidated
> into one crate (ADR 0006) and the storage layer landed in
> `db-storage::row`, the batch path's planner was still hiding in
> column-rs, and its compiled output (`Plan`) had a different shape from
> sqlite-rs's `Program` for no principled reason.

## Status

Accepted. Implemented across `db-core` (`504fcc2`, `c59fe73`, `2e8c3b6`)
and column-rs (planner consumed from `db-core`, local `codegen.rs`
deleted). sqlite-rs is the leading codebase in this family; where the two
engines can share a shape, batch converges toward row, not the reverse.

## Context

Before this ADR, column-rs's `query.rs` held the entire batch planner
(`compile()`, `Plan`, `AggPart`, `post_process`, `output_column_names`,
join/window program assembly, EXPLAIN) — ~1,300 lines that never touched
`ParquetFile`, i.e. storage-agnostic code living in the storage-specific
app. Its output, `Plan`, carried the opcode list plus five *sidecar*
fields (`columns_to_load`, `agg_parts`, `num_group_keys`, `order_by`,
`limit`) consumed by a post-VM `post_process()` step.

sqlite-rs's VDBE has `Program { instructions: Vec<Instruction> }` with
`Instruction { opcode, p1, p2, p3, p4, p5 }` — output metadata encoded
*in* the instruction stream (`ResultRow` carries its register range), no
sidecar, no post-VM step (the VM is the whole execution).

Separately, the word "codegen" meant opposite things in the two engines:
in sqlite-rs (and SQLite itself) it is AST → executable bytecode, i.e.
*the planner*; in column-rs it was compiled plan → **Rust source text**,
an ahead-of-time emitter sqlite-rs has no equivalent of. `db-core`'s
`codegen` module was named for the column-rs meaning — so migrating
sqlite-rs's `codegen/` there would have produced one module whose `batch`
and `row` submodules did unrelated jobs under one word.

## Options considered

**On `Program` shape:**
- **A — Keep `Plan` with sidecar fields**, just renamed. Zero risk,
  but keeps batch structurally unlike row for no reason.
- **B — Mirror sqlite-rs: `Program`/`Instruction`, metadata in the
  stream, post-processing as an opcode.** Requires deciding how a
  cross-segment step fits a per-segment VM (see Decision).

**On `Instruction` operands:**
- **p1..p5 integer slots**, literally identical to sqlite-rs. Batch
  opcodes like `GroupReduce` carry three variable-length slices
  (`group_by`, `aggs`, `agg_dst`); forcing those through fixed integer
  slots plus a dynamically-typed `P4` payload discards type safety for a
  C-heritage memory layout the batch VM doesn't need.
- **Typed operands on the existing `Opcode` enum**, with `Instruction`
  wrapping it. Same *structure and conventions* as sqlite-rs (one
  instruction list, EXPLAIN-listable, metadata in-stream), Rust-native
  operands.

**On "codegen":**
- Keep column-rs's meaning; migrate sqlite-rs's compiler under a
  different name. Puts the leading codebase's vocabulary second.
- **Adopt sqlite-rs's meaning**: `codegen` = planner; rename the AOT
  emitter to `emit`.

## Decision

**Program shape: Option B, typed operands.**

```rust
pub struct Program { pub instructions: Vec<Instruction> }
pub struct Instruction { pub opcode: Opcode, pub comment: Option<String> }
```

`comment` mirrors sqlite-rs's EXPLAIN-listing convention. Operands stay
typed inside `Opcode` — the deliberate departure from `p1..p5`, chosen
because batch opcodes carry structured operands that integer slots would
degrade.

**Output metadata moves into the stream.** `columns_to_load` was always
derivable (scan the program for `LoadColumn`) — now it is
(`Program::columns_to_load()`), not stored. The other four sidecar fields
become the terminal opcode:

```rust
Opcode::Finalize { agg_parts, num_group_keys, order_by, limit }
```

**`Finalize` is the parallel→sequential barrier, handled by the engine
layer.** Batch execution is per-segment-parallel (`run_parallel` over row
groups) followed by a cross-segment merge — a shape sqlite-rs's
single-threaded VDBE never needs. This is now explicit rather than
implicit: `vm::engine` treats every instruction *before* the first
`Finalize` as the **parallel phase** (run per-segment) and `Finalize` plus
anything after it as the **sequential phase** (run once over the merged
output). The per-segment `Vm::step` treats `Finalize` as a no-op control
opcode, joining the existing `Scan | NextSegment | Halt => {}` arm —
extending an existing precedent, not inventing a new pattern. Today
`Finalize` is always the last instruction; the engine looks for its
*position* rather than asserting it is at `len - 1`, so a future planner
can emit sequential-phase instructions after it without redesigning the
engine.

**Naming: sqlite-rs's vocabulary wins.** `db-core::codegen` = the
planner (AST → `Program`); the batch planner moved here from column-rs.
The former `db-core::codegen::batch` (AOT Rust-source emitter) is now
`db-core::emit::batch`; Cargo features renamed `codegen-*` → `emit-*`
for the emitter, with `codegen-*` now gating the planner. `emit` is
batch-only and has no row counterpart by design.

**Dependency direction is unchanged.** The planner moved into `db-core`
*because* it never touched `ParquetFile` — everything that does
(`leaf_columns`, `resolve_columns`, `RowGroupSegment`, `read_whole_table`,
`QueryEngine`) stays in column-rs as the app-side `Segment`/storage glue.
`db-core` remains storage-agnostic (ADR 0006); `db-storage` remains
independent of `db-core`. The app is the composition root. A proposal to
have `db-storage` implement a `db-core`-defined `TableSource` trait was
considered and **rejected**: it would couple two independent libraries
to save ~150 lines in an app whose entire job is that wiring, and would
create a Cargo dependency cycle the moment `vm::row` wanted to drive
`db-storage::row::btree` cursors directly the way sqlite-rs's VDBE does.
It also fails the loglume test — loglume needs the same engine over
parsed log lines, not Parquet, so the adapter is inherently per-app.

## Consequences

- column-rs `query.rs`: 1,914 → 613 lines; `codegen.rs` (771 lines, a
  near-verbatim duplicate of `emit::batch`) deleted. column-rs is now
  genuinely the Parquet glue + CLI it was always meant to be (ADR 0001's
  "as thin as possible").
- `db-core` gains `codegen::batch` (planner, 1,295 lines) and
  `vm::engine` (phase-aware orchestrator, 495 lines). Tests 90 → 253 —
  the planner's tests moved with it.
- Two opcode sets remain two types: `vm::batch::Opcode` (columnar) and
  the reserved `vm::row` (sqlite-rs's VDBE opcodes, still a stub). ADR
  0001's "consolidated location, not shared representation" holds.
- Anyone porting sqlite-rs's `codegen/` into `db-core` (ticket #20) now
  has an unambiguous target: `db-core::codegen::row`, producing a
  row-flavoured `Program`, with no `emit::row` to build.

## Addendum (db-core#48): `Finalize` split into `Combine`/`Sort`/`Limit`

Validated against DuckDB's execution model (2026-09-06): DuckDB runs the
four steps this ADR's single `Finalize` bundled — merge parallel
partials, finalize them, `ORDER BY`, `LIMIT` — as **four staged
operators** across pipeline boundaries: `Combine` → `Finalize` → a
separate `Sort` operator → a separate `Limit` operator. The one-opcode
bundling above was correct but coarser than that reference shape, and was
the one real granularity divergence from it.

**Split, not redesigned.** The barrier semantics this ADR already
described — `engine::run` finds the barrier *by position*, not by
asserting it is the program's last instruction, specifically so "a future
planner can emit sequential-phase instructions after it without
redesigning the engine" — is exactly the hook this split uses. No engine
redesign was needed, only:
- `Opcode::Finalize` → `Opcode::Combine { agg_parts, num_group_keys,
  distinct }` (the barrier: merge + finalize, still one opcode — see
  naming below) plus optional trailing `Opcode::Sort { col, descending }`
  and `Opcode::Limit { n }`.
- `Program::split_finalize` now recognizes the tail shape `Combine [Sort]
  [Limit]` (trying the longest shape first) instead of a single opcode.
- `codegen::batch::compile`/`compile_window` always emit `Combine`
  (mirroring the old always-emitted `Finalize`), and emit `Sort`/`Limit`
  only when the query actually has them.
- `#108`/`#109`'s eligibility checks re-derive their plan-shape detection
  from the `(Combine, Sort, Limit)` tuple `split_finalize` returns,
  instead of one struct's fields — same conditions, same result.

**Naming.** DuckDB itself uses *both* `Combine()` (merge thread-local
partial states) and `Finalize()` (compute the final value from the
merged state) as two distinct steps; db-core's `merge_rows`/
`finalize_row` are those two steps back to back with no observable
boundary between them (nothing needs to observe partially-merged,
not-yet-finalized state), so one opcode named `Combine` covers both —
matching DuckDB's first stage name, not inventing a third. `Sort`/`Limit`
were checked against `vm::row::program::Opcode`'s own vocabulary (the
sqlite-rs-mirroring VDBE opcode set, ADR 0002/db-core#18): `row` already
has its own `Sort` (a different, standalone in-place sort primitive) and
no `Limit`/`Combine`. Since `vm::batch::Opcode` and `vm::row::program::Opcode`
are separate types in separate modules (this ADR's own "two opcode sets
remain two types" above), there is no compile-time collision — and the
vocabulary overlap on `Sort` is accepted rather than avoided: both name
the same underlying operation (ordering rows), just in different
executors' terms, the same way `Filter`/`Emit`/`Halt` already appear in
spirit (if not name) on both sides.
