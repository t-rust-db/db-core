# ADR 0006: All physical storage lives in `db-storage`; `db-core` is storage-agnostic

## Status

Accepted. Supersedes the crate-location aspects of ADR 0003 and ADR 0004.

## Decision

The axis of variation in this project is storage, not language.
`db-core`'s parser, planners, VMs and functions are shared across
row, column and stream execution; what differs per mode is how bytes get
to and from disk. So:

- **`db-storage` is the single home for physical storage**, structured
  as feature-gated modules per execution mode: `row` (pages, WAL,
  b-tree, schema reader, record codec), `column` (mmap-based readers,
  Parquet), `stream` (append-only log formats).
- **`db-core` holds only storage-agnostic code**: `parser`, `codegen`,
  `vm`, `value`, `schema`, `functions`, `compare`, `coerce`, `types`,
  `expr`, `join`.
- **Dependency direction is `db-storage -> db-core`, never the reverse.**
  `db-storage` consumes `db-core`'s leaf types (`Value`, `Collation`,
  `TableSchema`, ...; ADR 0010, ADR 0014). `db-core` has no dependency on
  `db-storage` under any feature: `vm::row` reaches storage through a
  cursor trait the embedding application implements (ADR 0008), and
  `vm::batch` receives segments the application has already read.

## Rationale

Storage-mode-specific code in `db-core` would give the crate a second
internal axis beside its real job, and none of that code is usable by
the other modes. The same one-repo-per-concern pattern is used
elsewhere in the family (`db-extensions`: one repo, feature-gated
extensions).

## Consequences

- A `vm-batch`-only build of `db-core` pulls in zero storage code; so
  does a `vm-row`-only build.
- The composition root (an application crate) wires a `db-storage`
  cursor into `db-core`'s `Cursor` trait. That adapter is the
  application's, not either library's.
- ADR 0003's two-VFS-traits decision is unaffected: the two traits are
  co-located in `db-storage`, not unified.
