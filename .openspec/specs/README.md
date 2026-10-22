# Specs — db-core

System specifications for `db-core`: numbered directories, each holding a
`spec.md` that describes a slice of behavior in Given-When-Then scenarios,
with `**Implementation:**`/`**Tests:**` links back to source.

## Naming convention

`NNN-name/spec.md`, numbered starting at `001`. Mirrors
[sqlite-rs's `specs/`](https://github.com/t-rust-db/tools/tree/main/sqlite-rs/.openspec/specs/) (e.g.
`002-parser/spec.md`) for format and cross-linking conventions -- see that
repo's specs for the fuller worked example of the requirement/scenario
format, and its `README.md` for how it tracks spec-to-implementation
coverage.

## Specs

Row storage (`storage::row`), moved from sqlite-rs's `.openspec/specs/`
(#575), where they were 003, 006 and 007. A path written `sqlite-rs:...`
is in t-rust-db/tools' `sqlite-rs/` (most of the oracle-backed tests
stayed there); every other path is db-core's.

- [001-file-format](001-file-format/spec.md) — database header and record byte layout (`storage::row::header`, `storage::row::record`)
- [002-btree](002-btree/spec.md) — table and index b-trees (`storage::row::btree`)
- [003-pager](003-pager/spec.md) — pager, rollback journal and WAL (`storage::row::pager`)
