# ADR 0011: Scalar functions, comparison, and coercion live in `db-core` root, consumed by every `vm` executor

> Source: `#122` — `vm::row::functions` held ~30 scalar functions
> (`substr`/`like`/`glob`/`trim`/...) plus `like_match`/`glob_match`,
> pure `fn(&[Value]) -> Result<Value, FunctionError>` logic with no
> dependency on `vm::row`'s cursor/register/opcode machinery, that
> `vm::batch`/`vm::stream` will eventually need too.

## Status

Accepted, resolving `#122`.

## Context

Same shape ADR 0010 already resolved for `Value`/`Collation`: exactly
one correct behavior per scalar function, independent of which executor
evaluates it, sitting inside `vm::row` where only that executor could
reach it. `vm::batch`/`vm::stream` have no scalar-function dispatch of
their own today, but when either grows one, the honest options are the
same three ADR 0010 already weighed:

1. Leave it in `vm::row::functions`; `batch`/`stream` grow their own
   copies when they need them. Cheapest today, guarantees drift later —
   the same risk ADR 0010's `Value` duplication already demonstrated.
2. Hoist the dependency-free logic to `db_core` root, `vm::row`
   re-exports it, `batch`/`stream` call the same registry directly.
3. A third micro-crate. More repos for a few hundred lines, the same
   objection ADR 0010 raised against this option for `Value`.

Auditing `vm::row::functions` for the move surfaced that it isn't
self-contained: it calls `vm::row::compare::compare` (cross-type
ordering) and `vm::row::coerce::coerce_text_to_numeric`/
`cast_to_integer` (text-to-numeric coercion, checked arithmetic). Both
are themselves pure `Value`-only logic ported from sqlite-rs
(`vdbe::compare`/`vdbe::coerce`, ADR 0008) with no row-VM dependency —
the same shape as `functions` itself. Hoisting `functions` alone while
leaving `compare`/`coerce` behind the `vm-row` feature would mean
`db_core::functions` (meant to be feature-free, exactly like
`db_core::value`) silently required `vm-row` to actually build — a
`batch`/`stream` consumer would have to enable a row-VM feature it
never uses just to link. Both move too.

## Decision

**Option 2.** `db_core::compare`, `db_core::coerce`, and
`db_core::functions` become feature-free, dependency-free root modules
(alongside `db_core::value`, ADR 0010), holding exactly the same
`compare`/`coerce_text_to_numeric`/checked-arithmetic/scalar-function
bodies and `FunctionError` that `vm::row` held before, unchanged.
`vm::row::compare`, `vm::row::coerce`, and `vm::row::functions` each
become a one-line re-export shim (`pub use crate::compare::compare;`
etc.), the exact pattern `vm::row::value` already uses for
`db_core::value`. Every internal `vm::row` caller (`aggregate.rs`,
`logic.rs`, `cast.rs`, `cursor.rs`, `vm.rs`) keeps resolving
`super::compare`/`super::coerce`/`super::functions` unchanged through
the shim -- no behavior change, no call-site edits.

`batch`/`stream` have a documented path (this ADR, plus `lib.rs`'s
module docs) to call `db_core::functions::call(name, args)` directly
from their own execution loop once either needs scalar functions --
row-at-a-time vs. columnar iteration is a call-site concern, not a
reason to duplicate the function body. Neither needs one today, so
neither is wired up as part of this change.

`like_match`/`glob_match` move with `functions` (same file, same
reasoning: pure text-matching logic, exposed for a future `LIKE`/`GLOB`
operator in any executor, not just `row`'s).

## Consequences

- Exactly one scalar-function registry/comparison/coercion
  implementation across the crate; a bug fix or a new function lands
  once, visible to every executor immediately, not three times on three
  different schedules.
- `db_core::functions`/`compare`/`coerce` compile under
  `--no-default-features` (confirmed): a consumer needing only scalar
  functions and no VM/parser/codegen surface can already get them today,
  the same guarantee ADR 0010 recorded for `value`.
- `date`/`time`/`json` functions (if ever added) are a separate,
  later question -- this ADR is about *where* scalar functions live, not
  adding new ones (sqlite-rs's own `vdbe::functions` has neither today,
  per db-core#90's investigation).
- Option 1 is closed: closed by the same drift argument ADR 0010 already
  made, not re-litigated here.
- Option 3 stays available if a future consumer needs these without the
  rest of `db-core`'s parser/VM surface -- same escape hatch ADR 0010
  left open, still unused.
