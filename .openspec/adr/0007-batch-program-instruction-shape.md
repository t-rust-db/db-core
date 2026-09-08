# ADR 0007: Batch programs are `Program`/`Instruction` with typed operands and an explicit barrier

## Status

Accepted.

## Decision

**Shape.** The batch planner's output is

```rust
pub struct Program { pub instructions: Vec<Instruction> }
pub struct Instruction { pub opcode: Opcode, pub comment: Option<String> }
```

Output metadata is encoded in the instruction stream, never in sidecar
fields: which columns to load is derived by scanning for `LoadColumn`
(`Program::columns_to_load()`), and the aggregate/ordering/limit shape is
carried by the terminal opcodes. `comment` is the `EXPLAIN` listing
convention shared with the row VM.

**Operands are typed on `Opcode`'s variants**, not `p1..p5` integer
slots. Batch opcodes such as `GroupReduce` carry variable-length operand
lists (`group_by`, `aggs`, `agg_dst`) that fixed integer slots plus a
dynamically typed payload would only degrade. This is the one deliberate
divergence from the row VM's instruction shape (ADR 0008), and the two
opcode sets remain two types (ADR 0001).

**The parallel-to-sequential barrier is explicit.** Batch execution
runs per segment in parallel and then merges. `vm::engine` treats every
instruction before the barrier as the parallel phase and the barrier
plus anything after it as the sequential phase. The barrier is the
tail shape `Combine [Sort] [Limit]`:

- `Combine { agg_parts, num_group_keys, distinct }` merges per-segment
  partial states and finalizes them (one opcode for both steps, since
  nothing observes the partially merged state);
- `Sort { col, descending }` and `Limit { n }` are emitted only when the
  query has them.

The engine finds the barrier by position, not by asserting it is the
last instruction, so a planner may emit further sequential-phase
instructions after it. The per-segment `Vm::step` treats these three as
no-op control opcodes.

**Vocabulary.** `codegen` means the planner (AST to `Program`). The
ahead-of-time Rust-source renderer over the batch planner's output is
`codegen::batch::emit`, gated by `emit-batch`. It is batch-only; there
is no row or stream emitter, and none is reserved.

**Dependency direction.** Everything that touches a concrete storage
format (`ParquetFile`, row-group segments, the query engine wiring) stays
in the application. A `db-core`-defined `TableSource` trait implemented
by `db-storage` is rejected: it would couple two independent libraries
to save a small amount of wiring in the one crate whose job is that
wiring, and the same engine also runs over parsed log lines, so the
adapter is inherently per application.

## Consequences

- `vm::batch::Opcode` and `vm::row::Opcode` share vocabulary (`Sort`)
  but not types; no compile-time collision, no shared enum.
- Plan-shape checks (eligibility for the emitter, `EXPLAIN` trees)
  derive from the `(Combine, Sort, Limit)` tail that
  `Program::split_finalize` returns.
