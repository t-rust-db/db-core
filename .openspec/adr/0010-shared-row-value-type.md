# ADR 0010: One row `Value` type, defined in `db-core::value`, consumed by `db-storage`

> Source: `#83` — `vm::row::Value`/`Collation` duplicated `db-storage`'s
> `row::record::{Value, Collation}`.

## Status

Accepted, resolving `#83`.

## Context

sqlite-rs has exactly one `Value` (`record::Value`), and its VDBE imports
it. The extraction split that in two: `db-storage::row::record::Value`
(what the b-tree decodes) and `db_core::vm::row::Value` (what the VM
computes over), byte-for-byte identical — same for `TextEncoding`,
`Collation`, `compare_text`, and `format_real` (a third copy in
`db-storage::row::format`). Any real-storage cursor adapter (which per
ADR 0008 lives in the consumer, t-rust-db/sqlite-rs#18) would have to
convert every cell between two identical enums, and collation-aware
comparison would exist twice with the potential to drift.

Options considered in `#83`:

1. `db-core` (`vm-row`) depends on `db-storage` for `record::Value`.
   Contradicts ADR 0008 ("db-core does not depend on db-storage").
2. `db-core` defines the type once, `db-storage` depends on `db-core`
   for it.
3. A third micro-crate holding ~200 lines.

## Decision

**Option 2.** `db_core::value` is a feature-free, dependency-free module
holding `Value`, `TextEncoding`, `Collation`, `compare_text` and
`format_real`; `vm::row::value` re-exports it; `db-storage::row::record`
and `row::format` re-export it (`db-storage` gains
`db-core = { default-features = false }`, which pulls in nothing — db-core
has zero third-party dependencies since `#42`).

The dependency direction is `db-storage → db-core`, never the reverse:
ADR 0008 stands. This is the same direction ADR 0001 already established
for `Span` ("`sql-error` holds `Span` only … because sqlite-rs already
solved this problem"): leaf types every layer shares live in `db-core`,
and storage consumes them.

## Consequences

- `db_storage::row::record::Value` **is** `db_core::vm::row::Value`; the
  sqlite-rs cursor adapter passes cells through untouched.
- `db-storage` releases now track a `db-core` tag. Bumping it is a
  reviewed dependency update like any other (sqlite-rs ADR-0040 applies
  the same rule one level down).
- Option 3 stays available if a future consumer needs `Value` without
  the rest of `db-core`'s parser/VM surface; today `default-features =
  false` already compiles only `types`/`expr`/`join`/`value`.
- Alternative 1 is closed: a `db-storage` dependency in `db-core` would
  be an ADR 0008 violation, not an oversight.
