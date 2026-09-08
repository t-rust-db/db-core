# ADRs — db-core

Architectural Decision Records for `db-core`: one file per significant,
hard-to-reverse design decision (a chosen data representation, a crate
boundary, a divergence from a sibling engine's design, etc.) -- not a
log of routine changes.

## Naming convention

`NNNN-short-title.md`, numbered sequentially starting at `0001`, title in
kebab-case. Numbers are stable; a retired or superseded ADR keeps its
number and file, reduced to a pointer at what replaced it.

ADRs describe the architecture as it stands. They are not a changelog
and do not narrate how code arrived; `CHANGELOG.md` and git history do
that.

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
- [0002](0002-sql-parser-row-column-split.md) — One SQL grammar, one AST, dedicated codegen per engine
- [0003](0003-two-vfs-traits.md) — Two VFS traits, not one
- [0004](0004-header-and-pager-in-db-storage.md) — Database header and pager are `db-storage::row` modules
- 0005 — *retired*; folded into 0002
- [0006](0006-storage-lives-in-db-storage.md) — All physical storage lives in `db-storage`; `db-core` is storage-agnostic
- [0007](0007-batch-program-instruction-shape.md) — Batch programs are `Program`/`Instruction` with typed operands and an explicit barrier
- [0008](0008-vm-row-opcode-and-cursor-design.md) — `vm::row`'s opcode set and cursor abstraction
- [0009](0009-retired-single-parser.md) — *retired*; one parser, folded into 0002
- [0010](0010-shared-row-value-type.md) — One row `Value` type, defined in `db-core::value`, consumed by `db-storage`
- [0011](0011-shared-scalar-functions.md) — Scalar functions, comparison and coercion live in `db-core` root modules
- [0012](0012-codegen-schema-superset.md) — *superseded* by 0014
- [0013](0013-row-codegen-owned-in-db-core.md) — `codegen::row` is the one row planner, owned in `db-core`
- [0014](0014-one-row-schema-type.md) — One row schema type, defined in `db-core::schema`, consumed by `db-storage`
- [0015](0015-testing-strategy.md) — db-core's testing strategy
