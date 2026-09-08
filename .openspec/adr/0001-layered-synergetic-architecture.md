# ADR 0001: Layered, synergetic architecture across db-core

## Status

Accepted.

## Decision

`db-core` is the storage-agnostic core of the database family: the SQL
front end, the query planners, and the execution engines, in one crate.
Three execution strategies share that one crate and one language:

- `vm::row` -- row-at-a-time bytecode execution over cursors (the
  SQLite VDBE model).
- `vm::batch` -- vectorized execution over columnar segments, with a
  parallel phase per segment and a sequential merge phase.
- `vm::stream` -- push-driven execution over live/unbounded sources
  (reserved; not yet implemented).

Each executor, each planner (`codegen::{row,batch,stream}`) and each
parser surface is gated behind its own Cargo feature, so a consumer
compiles exactly the engine it uses (`default-features = false,
features = ["vm-batch", "codegen-batch"]`) and none of the others.
Every feature is on by default so the crate's own tooling reaches every
module.

**Co-located, not shared representation.** The executors live
together because they share the front end (ADR 0002), the value model
(ADR 0010), the scalar functions (ADR 0011) and the schema types (ADR
0014). Their instruction sets are deliberately different types:
`vm::batch::Opcode` and `vm::row::Opcode` are not one enum and are not
expected to become one (ADR 0007, ADR 0008).

**Error handling.** There is no central `Error` type and no external
error-handling crate anywhere in `db-core` (the crate has zero
third-party dependencies). Each module owns a hand-rolled error enum and
composes lower-layer errors by wrapping (`Variant { source: InnerError,
.. }`), never by flattening into one enum. `parser::Span` (`line`,
`column`, `offset`, `len`) is the one shared primitive: every parse
error carries a location.

## Consequences

- Application crates stay thin: anything that is not specific to a
  storage format or a user interface belongs here. Physical storage is
  `db-storage`'s (ADR 0006); the CLI/REPL toolkit is `db-cli`'s.
- Two engines sharing a name for an operation (`Sort` exists in both
  opcode sets) is accepted vocabulary overlap, not a shared type.
- A feature-gated module never leaks into a build that did not ask for
  it: a `vm-batch`-only build compiles no row VM, no row planner and no
  row parser beyond what the batch validator needs.
