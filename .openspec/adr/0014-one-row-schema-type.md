# ADR 0014: One row schema type, defined in `db-core::schema`, consumed by `db-storage`

> Source: t-rust-db/sqlite-rs#19 — repointing sqlite-rs's codegen at
> `db_core::codegen::row` (moved in #219, ADR 0013) hit two identical but
> distinct `TableSchema` types: db-storage's `row::schema::TableSchema`
> (what `read_schema` returns) and db-core's `codegen::row::TableSchema`
> (what every `compile_*` takes).

## Status

Accepted, 2026-09-08.

## Context

ADR 0012 grew `codegen::row::TableSchema`/`IndexSchema` to a field-for-field
superset of db-storage's so nothing would be lost at the boundary — but a
superset is still a second type. sqlite-rs's shadow switch bridged it with a
`to_core_schema` copy per statement; a facade-based repoint cannot, because a
facade re-exports functions whose signatures name db-core's type while every
caller holds db-storage's. The same shape was resolved once already for the
row `Value` (ADR 0010): define it once in a db-core leaf module, let
db-storage re-export it.

## Decision

- `db_core::schema` is a new leaf module (no feature gate, no parser/VM
  dependency, like `db_core::value`) holding `TableSchema`, `IndexSchema`,
  `IndexedColumn`, `ViewSchema`. `codegen::row` re-exports them unchanged.
- `TableSchema::with_computed_rowid_alias` re-parses `CREATE TABLE` text and
  therefore lives behind `parser-row`; db-storage enables that feature (zero
  third-party cost, ADR 0007/0009 direction unchanged).
- db-storage's `row::schema` re-exports these four types and drops its own
  definitions and its hand-rolled `rowid_alias_from_sql`, exactly as it
  re-exports `Value`/`Collation` from `db_core::value`.
- Dependency direction stays db-storage → db-core; db-core never sees
  db-storage.

## Alternatives rejected

- Keep two types and convert at the boundary: forces every sqlite-rs caller
  (CLI, REPL, tests) to copy schemas per statement and makes facades
  impossible.
- Define the type in db-storage and have db-core depend on it: inverts the
  layering (db-core is the leaf; ADR 0010).
- A trait over both types: generic codegen over a schema trait for one
  implementation is the speculative abstraction the project's rules forbid.

## Consequences

`codegen::row::TableSchema` paths keep working (re-export). db-storage 0.6.0
becomes a pure re-exporter for schema types; sqlite-rs's `crate::schema`
facade yields db-core's type, so `src/codegen`/`src/planner` can become
facades over `db_core::codegen::row` with no conversion. ADR 0012 stands
(the field set is unchanged); only the definition's home moves.
