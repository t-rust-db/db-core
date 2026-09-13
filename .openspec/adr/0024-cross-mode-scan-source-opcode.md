# ADR 0024: `ScanSource` opcode — compiling cross-mode build sides into `vm::batch`

## Status

Accepted (#382/#383, evolves ADR-0019 / ADR-0022)

## Context

ADR-0019 and ADR-0022 both compile the *join* itself into `vm::batch`
opcodes (`HashBuild`/`HashProbe`, `compile_join`), but the **build side**
never enters the opcode program:

- `resolve::run_query` materializes the SQLite lookup side via
  `cross_mode::scan_table_as_batch` (`src/engine/cross_mode.rs:47-56`)
  in plain Rust, *before* the VM runs, then hands the resulting `Batch`
  into `vm::engine::run_join_segments` (`src/vm/engine.rs:356-378`)
  alongside the compiled plan.
- `resolve::run_stream_stream_query` (ADR-0022) does the same thing with
  `materialize_segments` (`src/engine/resolve.rs:369-403`) for its
  windowed build side.

The driving/probe side does not have this problem: it already reaches
the VM through `vm::batch::Segment`/`Source` (`src/vm/batch.rs:668-683`),
a trait implemented uniformly by stream segments, `InMemorySegment`, and
`JoinedSegment` (`src/vm/engine.rs:27-38`, `:388-421`). `run_join_segments`
and `run` are already generic over `S: Segment` for that side
(`src/vm/engine.rs:55`, `:356`). Only `right: &Batch` — the build side —
is hard-required to already exist as a materialized value.

Two consequences follow directly from this gap:

1. **No `explain_opcodes` for cross-mode queries.** `Engine::explain_opcodes`
   (`src/engine.rs:408`) has no cross-mode implementation at all —
   `run_query`/`run_stream_stream_query` are free functions in
   `engine::resolve`, never wired to the `Engine` trait, so there is no
   opcode program to dump. This is the gap db-studio#54's F3 view papers
   over with a placeholder message.
2. **Two execution paths instead of one.** A single-engine batch query
   compiles once and the VM runs it end to end; a cross-mode query
   compiles the join but runs the build-side materialization as separate
   Rust glue first. `resolve.rs` is doing partial execution, not pure
   planning/dispatch.

This ADR closes that gap for the build side only, without touching the
row VM, without changing the driving side (already uniform), and without
introducing a cost-based join planner.

## Decision

Add one opcode, `ScanSource`, to `vm::batch::Opcode`
(`src/vm/batch.rs:277-436`) that means "materialize the build side from
source `N` into a `Batch`," where source `N` is described by a new,
closed enum:

```rust
enum ScanSource {
    RowTable { table: TableId /* SQLite, via engine::row */ },
    Stream   { engine: StreamHandle, columns: ColumnSet, scope: Option<Scope> },
    InMemory(Batch), // already-materialized value, e.g. a literal/CTE-ish input
}
```

`ScanSource` is a **closed enum, not a trait object.** Each variant
names its origin mode explicitly; `explain_opcodes` formats each variant
with a label ("row", "stream", "in-memory") rather than an opaque
"scan." This is the load-bearing choice against blurring row/batch/
stream: a `Box<dyn Segment>` would make the build side's origin
invisible past construction, which is exactly the boundary this ADR is
required to keep sharp. The enum is closed (no external impls), so
`compile_join` and the VM's opcode interpreter can exhaustively match it
— adding a fourth source kind is a compiler-enforced, visible change to
both sites, not a silent trait impl elsewhere.

`compile_join` (`src/codegen/batch.rs:1226`) emits a `ScanSource` opcode
for the build side instead of assuming it is handed a pre-built `Batch`
by the caller. `HashBuild` then consumes the opcode's output the same
way it consumes any other `Batch`-producing step today — no change to
`HashBuild`/`HashProbe` themselves (`src/vm/batch.rs:335-361`).

### Zero cost for row mode

`vm::batch::Opcode` and `vm::row`'s opcode set/interpreter
(`src/vm/row/program.rs`) are already separate types with separate
interpreters. `ScanSource` is added only to `vm::batch::Opcode`; row-mode
queries never construct, match on, or pay for it. This is a structural
guarantee (different enum, different interpreter loop), not a
performance claim requiring a benchmark.

### Driving side: out of scope, deliberately

The driving side already goes through `Segment`/`Source` uniformly
(`src/vm/batch.rs:668-683`) and is wrapped into a `JoinedSegment`
(`src/vm/engine.rs:388-407`) regardless of its origin; there is no
opcode-level information gained by re-expressing it as `ScanSource`.
Its plan is already surfaced separately through `explain_plan`/
`table_stats` (`src/engine/resolve.rs:198-236`), labeled stream/batch by
construction. Extending `ScanSource` to the driving side would be scope
expansion with no functional gain — rejected (see Rejected alternatives).

### `resolve.rs` becomes a real `Engine`

A new cross-mode `Engine`-implementing struct is added — in
`engine/resolve.rs` itself, or a sibling `engine/cross_mode_engine.rs` —
implementing `open`, `mode`, `run_query`, `explain_plan`,
`explain_opcodes`, `stats`, `tables` (`src/engine.rs:386-435`). This does
not violate ADR-0019/ADR-0010/ADR-0014's row/batch value-type
separation: that rule is enforced structurally by `engine/row.rs`
importing only `vm::row::*` (`src/engine/row.rs:39`) and never
`vm::batch`. `engine/resolve.rs` already imports `vm::batch::{Batch,
Segment, Value}` and `vm::engine::run_join_segments`
(`src/engine/resolve.rs:53-54`) — cross-mode code already lives on the
`vm::batch` side of that line, the same side `BatchEngine`
(`src/engine/column.rs:27`) and `StreamEngine` (`src/engine/stream.rs:42`)
already occupy. `engine/row.rs` is untouched by this ADR.

With a real `Engine` impl, `resolve::run_query`/`run_stream_stream_query`
shrink to: parse → resolve table identities → build the two
`Segment`/`ScanSource` values → call the ordinary compile+execute path.
No separate `run_join_segments` call remains once the build side is an
opcode instead of a pre-materialized argument.

### `explain_opcodes` and `explain_plan` both updated

`OpcodeSection` (`src/engine.rs:268`) gains a label identifying which
lane (row/stream/batch) each section's opcodes belong to, so a
cross-mode dump can show the `ScanSource` opcode's origin alongside the
join opcodes in one program — closing db-studio#54's gap with a real
dump, not a placeholder. `explain_plan`'s existing cross-mode output
(`resolve.rs:204-246`, `:444-485`, built on `codegen::batch::explain`) is
extended in step so both explain mechanisms describe the same compiled
program consistently; this ADR does not introduce a divergent third
explain format. Concretely: `explain_plan`'s `TableStats.source` labels
(`"stream {path}"`/`"sqlite {path}"`, `resolve.rs:204-236`, `:445-473`)
are already independent of the opcode program and need no shape change
— the requirement is that they keep naming the same build/driving sides
`explain_opcodes`'s new lane labels name, not that `explain_plan`'s
format itself changes. If a future edit to either drifts the two out of
sync, that is a bug against this ADR's consistency requirement, not a
new decision to make.

### Hash-join strategy: left open on purpose

`HashBuild`/`HashProbe` already carry a `table: usize` indirection key
(`src/vm/batch.rs:337-360`); today the build side is chosen once, always
SQLite/JOIN-target, by planner rule (ADR-0019, ADR-0021's
`compile_join_build_side`), never by cost. Making the build side an
opcode is a **prerequisite** for later cost-aware build-side selection
(e.g. choosing the smaller side to hash-build once cardinality is known
at plan time), not a decision to add that heuristic now. This ADR adds
no cost-based selection; ADR-0019's fixed planner rule is unchanged.

## Rejected alternatives

### `Box<dyn Segment>` / open trait object for the build side

Would let `HashBuild` accept any `Segment` impl uniformly, including the
driving side's existing mechanism. Rejected: an open trait object erases
which mode a build source came from once past construction, which is
exactly the row/batch/stream boundary this ADR must keep sharp per the
epic's explicit requirement. A closed enum keeps every source kind
visible and exhaustively matchable at zero runtime cost over a trait
object (no vtable dispatch, and `match` sites must be updated when a
variant is added).

### Generic `Opcode<S: Segment>`, monomorphized per source kind

Would give the same "zero-cost, no vtable" property as the enum via
monomorphization instead of a tag, but requires `Program`/`Vm`
(`src/vm/batch.rs`, `src/vm/engine.rs`) to become generic over the
opcode type — a much larger, riskier refactor of every opcode
consumer for no behavioral gain over a closed enum. Rejected for this
ADR's scope; revisit only if a future need (not identified here) makes
the enum's exhaustiveness a liability.

### Extend `ScanSource` to the driving side too, for symmetry

Rejected — see "Driving side: out of scope, deliberately," above. The
driving side's uniform `Segment` mechanism already works and is already
visible to `explain_plan`; wrapping it in `ScanSource` would duplicate
that visibility at opcode level with no new information.

## Structure

- `src/vm/batch.rs`: new `Opcode::ScanSource(ScanSource)` variant; new
  `enum ScanSource { RowTable, Stream, InMemory }`.
- `src/vm/engine.rs`: opcode interpreter gains the `ScanSource` arm,
  materializing each variant into a `Batch` the same way
  `scan_table_as_batch`/`materialize_segments` do today, but as a step
  in program execution rather than a precondition of it. `right: &Batch`
  parameters that only existed to receive this pre-materialization are
  removed once callers emit `ScanSource` instead.
- `src/codegen/batch.rs`: `compile_join` emits `ScanSource` for the
  build side instead of assuming a pre-built `Batch` argument.
- `src/engine/cross_mode.rs`: `scan_table_as_batch`'s materialization
  logic moves to (or is called from) the `ScanSource::RowTable` arm in
  `vm/engine.rs`; the free function's call sites in `resolve.rs` are
  replaced by the opcode path.
- `src/engine/resolve.rs`: new cross-mode `Engine` impl (struct name TBD
  at implementation time); `run_query`/`run_stream_stream_query`/
  `explain_plan`/`materialize_segments` shrink to planning/dispatch as
  described above. `materialize_segments`'s logic moves to
  `ScanSource::Stream`'s arm.
- `src/engine.rs`: `OpcodeSection` gains a lane label
  (row/stream/batch) so a cross-mode `explain_opcodes` dump can identify
  which physical engine each section of the program came from.
- No `src/vm/row` change — row mode's opcode set and interpreter are
  untouched, per "Zero cost for row mode," above.

## Consequences

- Cross-mode queries (ADR-0019's stream/SQLite join, ADR-0022's windowed
  stream-stream join) compile to and execute from one opcode program;
  `resolve.rs`'s role shrinks to planning/dispatch, closing the
  "two execution paths" gap from the epic.
- `Engine::explain_opcodes` produces a real, meaningful dump for
  cross-mode queries for the first time, closing db-studio#54's gap.
- `ScanSource`'s closed-enum shape means adding a fourth source kind
  (e.g. a future `.parquet`-as-driving-side adapter, explicitly deferred
  by ADR-0022) requires a visible, compiler-checked update at both the
  opcode definition and every exhaustive match — a deliberate tradeoff
  against the flexibility (and boundary risk) of an open trait object.
- Build-side selection stays a fixed planner rule (ADR-0019/ADR-0021)
  in this ADR; a cost-aware build-side choice remains explicitly future
  work, now unblocked at the opcode level rather than newly decided.
- `engine/row.rs` and `vm/row` are untouched; the row/batch/stream module
  boundary this ADR was required to preserve is unchanged in shape,
  only in which module (`engine/resolve.rs`) now implements `Engine`
  alongside `RowEngine`/`BatchEngine`/`StreamEngine`.
