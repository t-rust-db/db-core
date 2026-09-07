# ADR 0012: `codegen::row::TableSchema`/`IndexSchema` grow to a superset, not a trait

> Source: `#205` (sub-ticket of `#175`) — `codegen::row::TableSchema` is a
> "placeholder single-table schema" lossy relative to
> `db_storage::row::schema::TableSchema`.

## Status

Accepted, resolving `#205`.

## Context

`codegen::row::TableSchema`/`IndexSchema` (`src/codegen/row.rs`) carry only
`name`, `columns`, `column_types`, `rowid_alias`, `root_page`, and
`indexes: Vec<IndexSchema>` (itself just `name`, `root_page`, ascending-only
`columns: Vec<String>`). `db_storage::row::schema::TableSchema` additionally
carries `column_collations: Vec<Collation>`, `without_rowid`, `strict`,
`is_virtual`, `sql`, and richer `IndexSchema { unique, columns: Vec<IndexedColumn>
{ name, desc, collation } }`. Callers building `db-core`'s schema from
db-storage's today drop all of that, blocking collation-aware codegen,
WITHOUT ROWID tables, STRICT tables, and UNIQUE-index paths.

There is no existing conversion function to fix — `db-core` has no
dependency on `db-storage` at all (per ADR 0008: "db-core does not depend
on db-storage"), so `codegen::row::TableSchema` is populated by literal
construction in whichever crate calls db-core's codegen (today: test
builders in this crate only; in production, sqlite-rs/db-storage). This is
greenfield, not a refactor of a lossy converter.

Options considered:

1. Grow `codegen::row::TableSchema`/`IndexSchema` to a superset carrying
   every field `db_storage::row::schema::TableSchema` has. Callers populate
   the extra fields; a non-lossy conversion becomes possible at the call
   site.
2. Make codegen generic over a schema trait that both db-core's placeholder
   type and db-storage's real type implement.

## Decision

**Option 1.** Grow `TableSchema`/`IndexSchema` in place.

This follows the same shape as ADR 0010 (`Value`/`Collation`): db-core
already owns `Collation` (`src/value.rs`) and db-storage depends on db-core
for it, never the reverse (ADR 0008 stands unchanged — this decision adds
no dependency in either direction, since db-core's `TableSchema` needs no
knowledge of db-storage's type to grow its own fields). It is natural for
db-core's schema type to be the superset that db-storage/sqlite-rs convert
into, mirroring how `Value`/`Collation` already work.

The codebase does have one existing trait-based storage-agnostic-boundary
pattern (`SchemaStorage`, `src/vm/row/schema_storage.rs`), which could
support option 2. But that trait is operation-based (create/populate/insert
master-row calls), not data-shaped — adapting it to a schema *data
structure* like `TableSchema` would mean designing new trait machinery from
scratch, for no benefit over a plain struct here: codegen only ever reads
schema fields, it never needs to be generic over how they're stored.

## Consequences

- `TableSchema` gains `column_collations: Vec<Collation>`, `without_rowid:
  bool`, `strict: bool`, `is_virtual: bool`, `sql: String`.
- `IndexSchema.columns` changes from `Vec<String>` to `Vec<IndexedColumn>`
  (`{ name, desc, collation }`); `IndexSchema` gains `unique: bool`.
- All in-crate test builders (the only current construction sites) are
  updated to populate/default the new fields; existing codegen behavior is
  unchanged (still assumes `Collation::Binary`/ascending-only until a later
  ticket wires the new fields into actual comparison/scan codegen — see
  `#175`, `#206`).
- A future db-storage/sqlite-rs-side conversion from
  `db_storage::row::schema::TableSchema` into this superset is no longer
  lossy.
