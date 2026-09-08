# ADR 0012: Superseded by ADR 0014

Superseded. The row schema types (`TableSchema`, `IndexSchema`,
`IndexedColumn`, `ViewSchema`) are defined once in `db_core::schema`
(ADR 0014). The field set this ADR settled -- `column_collations`,
`without_rowid`, `strict`, `is_virtual`, `sql`, per-index `unique` and
`IndexedColumn { name, desc, collation }` -- is that type's field set.
