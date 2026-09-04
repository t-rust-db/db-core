# ADR 0008: `vm::row`'s opcode identity and cursor abstraction

> Source: db-core#18 ("db_core::vm::row real content from sqlite-rs's
> src/vdbe"), session 2026-09-04. Resolves the two open questions
> `src/vm/row.rs`'s stub doc comment and ADR 0007's consequences section
> both left explicit but unanswered.

## Status

Accepted, **revised** (db-core#51 session, 2026-09-04): the original
decision below generalized ADR 0007's typed-operand design from
`vm::batch` to `vm::row` too. That was wrong and is corrected in the
Decision section — `vm::row::Instruction` uses sqlite-rs's literal
`p1..p5` operand slots, not typed named fields, and its `Opcode`
variants are bare tags (an exhaustive tag list, matching sqlite-rs's
own by name), not structs. The original mistake and correction are kept
below rather than rewritten, since the *why* (a wrong precedent-reuse,
caught by explicit user review: "We want full parity in db-core with
sqlite-rs. Where did it drift?") is itself useful history.

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
described as ports, not reimplementations.

**~~Following ADR 0007's precedent for `batch`, `Instruction`/`Program`
keep sqlite-rs's shape but operands stay typed on the `Opcode` enum's
variants rather than raw `p1..p5` integer slots — the same departure
ADR 0007 made for `batch`.~~ Corrected (db-core#51 session):** that
departure was never justified for `row`. ADR 0007's typed-operand
choice for `batch` exists because *some* batch opcodes (`GroupReduce`)
carry variable-length operand lists that don't fit five integer slots
without a dynamically-typed escape hatch — a `vm::batch`-specific
problem. `vm::row`'s opcodes, being a literal VDBE port, have no such
problem and are expected to be emitted by a literal port of sqlite-rs's
own codegen (`codegen::row`, db-core#20) — which emits `p1..p5`
directly. Reshaping them into typed fields would force `codegen::row`
to translate sqlite-rs's actual output into a different shape for no
reason, defeating the "unambiguous target" this ADR's own Consequences
section originally promised.

**Corrected decision:** `vm::row::Instruction` uses sqlite-rs's literal
operand shape:

```rust
pub struct Instruction {
    pub opcode: Opcode,     // a bare tag enum, one variant per sqlite-rs opcode
    pub p1: i32,
    pub p2: i32,
    pub p3: i32,
    pub p4: P4,             // dynamically-typed fourth operand, sqlite-rs's own enum
    pub p5: u16,
    pub comment: Option<String>,  // ADR 0007's EXPLAIN convention, kept
}
```

`Opcode` lists every variant sqlite-rs's VDBE has (opcode-identity
parity), whether or not `vm::row`'s dispatch loop implements it yet —
matching sqlite-rs's own convention for its `Opcode::ALL`/`_exhaustive`
pattern. The fused compare-and-jump opcodes (`Eq`/`Ne`/`Lt`/`Le`/`Gt`/
`Ge`) are ported as such, not as a register-writing `Compare` opcode —
an earlier, since-corrected draft of db-core#18 did exactly that
substitution and needed the same fix (see db-core#51's PR).

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

- `vm::row::value` gains its own `Value`/`Collation`/`compare_text`,
  ported from sqlite-rs's `record::{value.rs, collation.rs}` — not
  reused from `vm::batch::Value` (`Cow<'static, str>`-based, designed
  for AOT-emitted `const` literals) since the two `Opcode` sets are
  already established as separate types with separate value models
  (ADR 0001, ADR 0007's consequences).
- db-core#18's originally-landed `Opcode::Compare`/`Cast`/`Arith`/
  `Logic`/`Not`/`BitNot`/`Neg` (typed-struct variants) were replaced by
  db-core#51's PR with bare-tag `Eq`/`Ne`/`Lt`/`Le`/`Gt`/`Ge` (fused
  jump), `Cast`, `Add`/`Subtract`/etc., `Not`/`BitNot` over `p1..p5` —
  a breaking change to the just-landed API, done in the same PR that
  added the execution loop rather than as a separate cleanup, since
  #51's new control-flow opcodes were going to be written against
  whichever shape `Compare` had anyway.
- `codegen::row` (db-core#20) now has a concrete, literally-portable
  target: sqlite-rs's own codegen emits `p1..p5` directly, so a
  mechanical port needs no operand-shape translation layer.
- `vm::row`'s cursor-trait/storage-dependency decision (the second half
  of this ADR, unchanged by the correction above) still governs the
  execution-loop phase, landed by db-core#51: a storage-agnostic
  `Cursor` trait (`vm::row::cursor`) plus an in-memory mock, with real
  `db-storage` wiring deferred again.
