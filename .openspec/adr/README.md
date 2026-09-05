# ADRs — db-core

Architectural Decision Records for `db-core`: one file per significant,
hard-to-reverse design decision (a chosen data representation, a crate
boundary, a divergence from a sibling engine's design, etc.) -- not a
log of routine changes.

## Naming convention

`NNNN-short-title.md`, numbered sequentially starting at `0001`, title in
kebab-case. Mirrors [sqlite-rs's `adr/`](../../../sqlite-rs/.openspec/adr/)
(e.g. `0009-zero-unsafe.md`), so the two repos' ADR indices read the same
way if ever compared side by side.

## When to add one

Write an ADR when a decision would be expensive to reverse or non-obvious
to a future reader -- e.g. "why does `Join`'s condition stay two
`String`s instead of becoming `Option<(String, String)>` when `CROSS
JOIN` was added" or "why NULL-safe join-key equality lives in each
caller's key conversion rather than in `sql-join` itself" (see
`sql-join::semantics`'s module doc for that reasoning as it stands today
-- promote it here if the decision is later revisited or contested).

## Index

- [0001](0001-layered-synergetic-architecture.md) — Layered, synergetic architecture across db-core
- [0002](0002-sql-parser-row-column-split.md) — `sql-parser`'s row/column split mechanism
- [0003](0003-vfs-trait-reconciliation.md) — `db-storage::{Vfs, VfsFile}` vs sqlite-rs's `vfs::{Vfs, VfsFile}` — two traits, not one
- [0004](0004-sql-pager-extraction.md) — `sql-pager` extraction, and the `sql-header` dependency it forced
- [0005](0005-sqlite-rs-grammar-relationship.md) — sqlite-rs's grammar is the canonical `sql-parser`; column/stream are enforced subsets
- [0006](0006-storage-consolidation-into-db-storage.md) — All physical storage (row/column/stream) consolidates into `db-storage`; `db-core` stays storage-agnostic
- [0007](0007-program-instruction-mirror-sqlite-rs.md) — Batch execution mirrors sqlite-rs's Program/Instruction shape
- [0008](0008-vm-row-opcode-and-cursor-design.md) — `vm::row`'s opcode identity and cursor abstraction
- [0010](0010-shared-row-value-type.md) — One row `Value` type, defined in `db-core::value`, consumed by `db-storage`
- [0009](0009-parser-row-backport-rule.md) — While two copies of the row parser exist, Lab271/sqlite-rs leads and `parser::row` back-ports
