# ADR 0014: One row schema type, defined in `db-core::schema`, consumed by `db-storage`

## Status

Accepted. Supersedes ADR 0012.

## Decision

- `db_core::schema` is a leaf module (no feature gate, no parser or VM
  dependency beyond the one method below) holding `TableSchema`,
  `IndexSchema`, `IndexedColumn` and `ViewSchema`. `codegen::row`
  re-exports them.
- `TableSchema` carries everything the row planner and the storage
  reader need: `name`, `root_page`, `columns`, `column_types`,
  `column_collations`, `rowid_alias`, `without_rowid`, `strict`,
  `is_virtual`, `sql`, `indexes`. `IndexSchema` carries `name`,
  `root_page`, `unique`, `columns: Vec<IndexedColumn { name, desc,
  collation }>`. `ViewSchema` is `name` plus the verbatim `CREATE VIEW`
  text, re-parsed on demand.
- `TableSchema::with_computed_rowid_alias` derives the `INTEGER PRIMARY
  KEY` rowid alias by parsing the table's `CREATE TABLE` text with the
  crate's own parser, and therefore lives behind `parser-row`.
- `db_storage::row::schema` re-exports these four types; the schema
  reader decodes `sqlite_master` straight into them. Dependency direction
  stays `db-storage -> db-core` (ADR 0006, ADR 0010).

## Alternatives rejected

- Two structurally identical types converted at the boundary: every
  caller would copy schemas per statement.
- Defining the type in `db-storage`: inverts the layering.
- A schema trait generic over one implementation: a speculative
  abstraction.

## Consequences

- A storage-backed schema and a hand-built one are the same type; the
  planner cannot tell them apart and never needs to.
