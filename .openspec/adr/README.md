# ADRs — db-core

Architectural Decision Records for `db-core`: one file per significant,
hard-to-reverse design decision (a chosen data representation, a crate
boundary, a divergence from a sibling engine's design, etc.) -- not a
log of routine changes.

## Naming convention

`NNNN-short-title.md`, numbered sequentially starting at `0001`, title in
kebab-case. Numbers are stable; a retired or superseded ADR keeps its
number and file, reduced to a pointer at what replaced it.

`0000-charter.md` sits outside the sequence on purpose: it is not a
decision among alternatives but the charter every numbered ADR must be
consistent with. Nothing precedes it, nothing supersedes it; a conflict
resolves toward it.

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
- [0016](0016-absorb-db-storage-as-workspace-member.md) — db-storage absorbed as the `storage` module
- [0017](0017-engine-seam.md) — `engine`: the client-facing seam over execution modes
- [0018](0018-stream-engine.md) — Stream engine: indexed live-log query as storage + prologue/epilogue around the batch VM
- [0019](0019-cross-mode-queries-row-batch-adapter.md) — Cross-mode queries: row→batch adapter for SQLite lookup sides, reusing `vm::batch`'s existing join
- [0020](0020-double-quote-identifier-semantics.md) — Double quotes are always identifiers, never a string fallback (unlike SQLite/MySQL)
- [0021](0021-sqlite-as-driving-side.md) — Cross-mode joins: make SQLite-as-driving-side order-independent, not unsupported
- [0022](0022-unbounded-stream-stream-joins.md) — Cross-mode joins: windowed (not unbounded) stream-to-stream joins
- [0023](0023-key-restricted-materialization.md) — Cross-mode joins: measure before building key-restricted lookup materialization
- [0024](0024-cross-mode-scan-source-opcode.md) — Cross-mode joins: compile the build side into `vm::batch` via a `ScanSource` opcode, unifying cross-mode `explain_opcodes` with single-engine queries
- [0025](0025-audit-model-for-batch-and-stream-kernels.md) — Audit model for `vm::batch`/`vm::stream`: the kernel, not the opcode, is the audit unit; segment-split invariance as the headline obligation
- [0026](0026-late-materialization.md) — Late materialization: two-phase segment decode
- [0027](0027-write-path-wal-and-flush.md) — Write path: record WAL, three watermarks (visible/durable/flushed), atomic flush to Parquet via a manifest
- [0028](0028-time-tiered-layout-and-compaction.md) — Time-tiered layout (hot → hour → day) partitioned by arrival time, hours sorted by a sort key into row groups, per-table choice next to the flat layout; secondary access paths (sort key, bloom filters, projections)
- [0029](0029-ingest-packaging.md) — Packaging the write path: `storage-column-write`/`ingest-read`/`ingest` features, `column-ingest` binary, readers in the `tools` workspace
- [0030](0030-column-dialect-follows-duckdb.md) — The column dialect follows DuckDB where DuckDB and sqlite3 differ (amends 0011)

Row storage (`storage::row`), moved from sqlite-rs's `.openspec/adr/` (#575); each file names its original sqlite-rs number:

- [0031](0031-shm-access-pread-not-mmap.md) — `-shm` access via `pread`/`pwrite`, not `mmap`
- [0032](0032-hot-journal-auto-recovery.md) — Hot-journal recovery is automatic in `Pager::open`, not a separate opt-in call
- [0033](0033-pager-has-no-page-cache.md) — `Pager` has no page cache — repeated-seek workloads pay full I/O every row
- [0034](0034-leaf-cell-splice.md) — In-place leaf/index cell splice with real freeblock bookkeeping
- [0035](0035-hot-journal-recovery-reserved-probe-single-fd.md) — Hot-journal recovery probes RESERVED and shares one fd with `Pager`
- [0036](0036-passive-only-checkpoint-linear-frame-scan.md) — PASSIVE-only checkpoint, single non-blocking lock, linear frame scan
- [0037](0037-wal-writer-reopens-and-rescans-per-flush.md) — WAL writer reopens and rescans the `-wal` file on every flush
- [0038](0038-pager-page-cache-landed.md) — `Pager` page cache landed — supersedes ADR-0033's problem statement
- [0039](0039-vendor-nix-subset.md) — Vendor a `nix` subset: reintroduce a single, narrow `unsafe` boundary
- [0040](0040-wal-resume-hint-cache-supersedes-0037.md) — `Pager`-cached WAL resume hint supersedes ADR-0037's per-flush rescan
- [0041](0041-pragma-synchronous-fsync-policy.md) — `PRAGMA synchronous` fsync-skip policy, and why `SynchronousMode` lives in `header.rs`
- [0042](0042-macos-plain-fsync-not-fullfsync.md) — On macOS, `Vfs::sync` calls plain `fsync(2)`, not `std`'s `F_FULLFSYNC`
