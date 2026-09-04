# ADR 0008: `vm::row`'s opcode identity and cursor abstraction

> Source: db-core#18 ("db_core::vm::row real content from sqlite-rs's
> src/vdbe"), session 2026-09-04. Resolves the two open questions
> `src/vm/row.rs`'s stub doc comment and ADR 0007's consequences section
> both left explicit but unanswered.

## Status

Accepted for the slice landing now (value semantics: `value`/`compare`/
`affinity`/`cast`/`coerce` + the `Opcode`/`Program`/`Instruction`
skeleton). The execution loop, cursor wiring, and everything past it
remain future work under db-core#18's own tracking-issue framing.

## Context

Two questions were left open by prior work:

1. **Opcode-set identity** (`src/vm/row.rs`'s stub): does `vm::row` port
   sqlite-rs's ~65 VDBE opcodes near-verbatim, or define its own set
   decoupled from sqlite-rs's? `vm::mod.rs` already states
   `batch::Opcode` and a future `row::Opcode` "are NOT the same type,
   and are not expected to become one" — but doesn't say whether `row`'s
   set is a port or an original design.
2. **Storage dependency direction** (raised in db-core#18's description
   and ADR 0007's consequences): does `vm::row` depend on `db-storage`
   directly to drive real B-tree cursors (sqlite-rs's own design), or
   define a storage-agnostic cursor trait it depends on instead? ADR
   0007 rejected a `TableSource`-trait-style coupling for column-rs's
   `batch` path, but noted that rejection was app-specific, not
   necessarily binding for `row`.

## Decision

**Opcode identity: `vm::row::Opcode` is a mechanical port of sqlite-rs's
VDBE opcode set**, not a new design — matching how `parser::row`
(db-core#23) and `codegen::row` (db-core#20, still blocked) are both
described as ports, not reimplementations. Following ADR 0007's
precedent for `batch`, `Instruction`/`Program` keep sqlite-rs's shape
(`Program { instructions: Vec<Instruction> }`) but operands stay typed
on the `Opcode` enum's variants rather than raw `p1..p5` integer slots —
the same departure ADR 0007 made for `batch`, for the same reason
(row opcodes like `MakeRecord` or `SeekRowid` carry structured operands
that integer slots would degrade). `comment: Option<String>` on
`Instruction` mirrors sqlite-rs's `EXPLAIN` convention, unchanged from
ADR 0007.

**Cursor abstraction: storage-agnostic trait, not a direct `db-storage`
dependency.** `vm::row` defines its own cursor trait (name/shape decided
when the execution-loop phase lands, not by this ADR) that an adapter
crate implements over `db-storage::row::btree::TableCursor` — the same
shape as ADR 0007's `TableSource`-rejection reasoning: `db-core` stays
storage-agnostic (ADR 0006), and the adapter lives at the composition
root (an app crate, or `db-storage` itself gaining an optional adapter
feature) rather than creating a `db-core` → `db-storage` dependency edge.
This is a **narrower** decision than ADR 0007's batch-side rejection —
it only says the boundary is a trait, not that no crate anywhere depends
on both; the follow-up ticket for the execution-loop phase (`db-core#18`
sub-ticket, filed alongside this ADR) owns the trait's actual shape.

## Consequences

- `vm::row`'s `Opcode` variants that land in *this* slice (value
  comparison/coercion/cast primitives) carry no cursor or storage
  concept at all — they operate on `record::Value` only, so this
  decision doesn't yet need to be exercised in code. It governs the
  next phase (execution loop + cursor trait), tracked as a follow-up
  issue against db-core#18.
- `vm::row::value` gains its own `Value`/`Collation`/`compare_text`,
  ported from sqlite-rs's `record::{value.rs, collation.rs}` — not
  reused from `vm::batch::Value` (`Cow<'static, str>`-based, designed
  for AOT-emitted `const` literals) since the two `Opcode` sets are
  already established as separate types with separate value models
  (ADR 0001, ADR 0007's consequences).
- `codegen::row` (db-core#20) now has a concrete, mechanically-ported
  target to eventually emit against, once the execution-loop phase adds
  enough of `Opcode` for a real program to be constructible.
