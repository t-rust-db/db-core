# ADR 0006: All physical storage (row/column/stream) consolidates into `db-storage`; `db-core` stays storage-agnostic

> Source: session discussion, 2026-09-04 — following up on ADR 0003 and
> the `sql-sys` dissolution (termios -> db-cli, dead; fcntl -> follows
> vfs) to settle where the *rest* of the row storage stack (pager,
> btree, schema, record) belongs once vfs itself moves

## Status

Accepted. Supersedes ADR 0003's and ADR 0004's **crate location** for
`sql-vfs`/`sql-pager`/`sql-header`/`sql-record` (and the still-open
`sql-btree`/`sql-schema` tickets' target repo) — not their content, and
not ADR 0003's core "two traits, not one" reasoning, which still holds.

## Context

ADR 0003 put `sql-vfs` in `db-core` because, at the time, sqlite-rs's own
`pager`/`btree`/`vdbe` migration path was assumed to land in `db-core`
crate-by-crate (ADR 0001's phased plan: `sql-sys` -> `sql-vfs` ->
`sql-pager`/`sql-btree` -> `sql-schema` -> `sql_vm::row`). Two things
changed since:

1. **`sql-sys` is dissolving.** Its `termios` half is dead code
   (`db-cli` already independently reimplements raw-mode terminal
   handling against `libc` directly — `db-cli/src/editor.rs`). Its
   `fcntl` half has exactly one consumer, `sql-vfs`'s locking/WAL code —
   nothing in `db-core` proper needs it.
2. **The actual axis of variation in this project is storage, not
   language.** `db-core`'s parser/expr/vm/codegen/functions are meant to
   be genuinely shared across row/column/stream execution. What differs
   per mode is entirely how bytes get to and from disk: sqlite-style
   pages+WAL for `row`, Parquet for `column`, append-only log formats
   for `stream`. That's exactly the shape `db-storage` was already
   named for, and exactly the pattern already used elsewhere in this
   project (`db-extensions`: one repo, `fts`/`geo`/`blob-json` as
   feature-gated modules; `sql-vm`/`sql-parser`/`sql-codegen`: one crate,
   `batch`/`row`/`stream` as feature-gated modules).

Continuing to grow sqlite-rs's storage stack as separate `db-core`
crates (`sql-vfs`, `sql-pager`, `sql-header`, `sql-record`, and the
still-open `sql-btree`/`sql-schema`) means `db-core` accumulates a
*second* internal axis (storage-mode-specific crates) alongside its real
job (language/VM), and none of those crates are usable by `column`/
`stream` execution at all — they're 100% row-specific, sitting in
`db-core` only because that's where the migration started.

## Decision

**`db-storage` becomes the single repo for all physical storage,
structured exactly like `db-extensions`/`sql-vm`: one repo, one
Cargo workspace, feature-gated modules per execution mode.**

```
db-storage/
  row/       sql-vfs, sql-pager, sql-header, sql-record, sql-btree
             (#16), sql-schema (#17) — sqlite-rs's storage stack,
             moved in as-is, not redesigned. fcntl (currently
             sql-sys, db-core) folds in here too as a private
             module — sql-vfs's locking code is its only consumer,
             and both now live in the same crate.
  column/    today's db-storage::{Vfs, VfsFile} (mmap-based),
             db-parquet folded in (per the earlier db-storage +
             db-parquet consolidation decision)
  stream/    log-format storage — new, informed by loglume's
             mmap-based file reading and log-format parsing
             (~/wc/lab271/loglume) but not a wholesale import of
             it (loglume is a full CLI/TUI product, not a
             storage-engine crate)
```

`db-core` keeps: `sql-types`, `sql-expr`, `sql-parser`, `sql-join`,
`sql-vm` (opcodes), `sql-codegen`, and the future `functions` crate —
all storage-agnostic. `sql-sys` is deleted outright, not moved as a
crate: `termios` is dead code (removed), `fcntl` moves into
`db-storage/row` as a private implementation detail of `sql-vfs`, not a
public crate of its own — it never had a second consumer to justify
being one.

**This does not reopen ADR 0003's "two traits, not one" decision.**
`db-storage/row`'s `Vfs` (sqlite-rs's, full ACID surface, zero `unsafe`
outside vendored FFI) and `db-storage/column`'s `Vfs` (mmap-based,
read-only, one documented `unsafe`) stay two separate, differently-
shaped traits for the same safety reasons ADR 0003 already established
(SIGBUS risk under concurrent mutation, `ADR-0009`'s zero-unsafe
policy) — they are now co-located in one repo's feature-gated modules,
not unified into one interface. Same trade already made for `sql-vm`'s
`batch`/`row`/`stream` opcode sets (ADR 0001: "consolidated location,
not shared representation").

**Dependency direction:** `db-core`'s `sql_vm::row` (currently a stub,
`#18`) will depend on `db-storage/row` once it gets real content — VDBE
execution is inherently cursor/page-coupled, there is no storage-
agnostic way to implement it. `sql_vm::batch`/`stream` similarly depend
on `db-storage/column`/`stream` respectively, each only under its own
Cargo feature, matching the existing `default-features = false,
features = ["batch"]` pattern column-rs already uses. `db-core` itself
(the crate boundary, not any one feature-gated module inside it) never
becomes storage-agnostic-in-name-only by accident — a `batch`-only build
of `sql-vm` still pulls in zero storage-layer code.

## Consequences

- **Physical migration required** for already-merged work: `sql-vfs`,
  `sql-pager`, `sql-header`, `sql-record` move out of `db-core`'s
  workspace into `db-storage/row`. This is real, non-trivial work on
  code that landed via `#13`/`#14`/`#15` (all merged) — tracked as its
  own migration ticket, not silently implied by this ADR.
- `#16` (`sql-btree`) and `#17` (`sql-schema`) — currently scoped to
  land in `db-core` — are re-targeted to `db-storage/row` instead. Their
  actual content/acceptance criteria are unaffected; only the target
  repo changes.
- `#18` (`sql_vm::row` real content) is unaffected in scope, but its
  dependency now reads as `db-storage/row` rather than sibling `db-core`
  crates — its own acceptance criteria already listed the same
  prerequisite crates, just assumed they'd be `db-core`-local.
- `sql-sys` deletion: `termios` removed outright (dead code); `fcntl`
  becomes a private module inside `db-storage/row`'s `sql-vfs`
  equivalent, no longer a standalone public crate.
- `db-storage`'s existing `{Vfs, VfsFile}` (mmap-based) and `db-parquet`
  fold into `db-storage/column`, per the earlier (separately-agreed)
  storage-repo-consolidation decision — this ADR places that decision
  inside the same repo-wide `row`/`column`/`stream` structure rather
  than as a bespoke two-crate merge.
- `db-storage/stream` starts from nothing — no extraction target exists
  yet, only loglume as a design reference. Scoped as its own epic.
