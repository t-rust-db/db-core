# ADR 0011: Scalar functions, comparison and coercion live in `db-core` root modules

## Status

Accepted.

## Decision

`db_core::functions` (the scalar-function registry, `call(name, args)`,
`like_match`/`glob_match`, `FunctionError`), `db_core::compare`
(cross-type ordering) and `db_core::coerce` (text-to-numeric coercion,
checked arithmetic) are feature-free, dependency-free root modules,
alongside `db_core::value` (ADR 0010). They compile under
`--no-default-features`.

`vm::row::{functions, compare, coerce}` are one-line re-export shims over
them, so the row VM's internal callers resolve `super::functions` etc.
unchanged. `vm::batch` and `vm::stream` call the root modules directly
when they need scalar functions; row-at-a-time versus columnar iteration
is a call-site concern, not a reason for a second function body.

## Rationale

Exactly one correct behaviour exists per scalar function, independent
of which executor evaluates it. The three modules move together because
`functions` calls `compare` and `coerce`; hoisting one without the others
would make a feature-free module silently require `vm-row`.

## Consequences

- A bug fix or a new function lands once and is visible to every
  executor.
- `date`/`time`/`json` functions, if ever added, are a separate decision
  about *what* to add; this ADR fixes *where* scalar functions live.
