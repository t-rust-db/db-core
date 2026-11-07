# ADR 0044: An object store (S3) for striped tables

Date: 2026-11-07

## Context

#1015: a table has to live on a local filesystem (a laptop, a container) and
on S3 (AWS), and in trials on Garage, a self-hosted S3-compatible store. A
caller hands db-core a directory path, so the backend has to be here. The
ingest write path
(ADR 0027-0029) leaned on what only a filesystem gives: `rename`, `fsync`,
append, `flock`, and directory renames.

Garage v2.1.0 ignores `If-Match` / `If-None-Match` on PutObject (tested: a
second create-only put of an existing key overwrites it). AWS S3 and MinIO
honour them.

## Decision

**A `Store` holds the manifest and the Parquet files; the WAL, the writer
lock and the scratch `tmp/` stay on the writer's local disk.** An object store
cannot append or fsync, so the WAL keeps its durability on local disk and S3
holds what has been flushed. A table opens with
`TableWriter::open_in(local_dir, store, options)`; `open(path)` is the same
with the directory as its own store, so path callers see no change. A reader
of a store alone (`Snapshot::open_in`, `StoreReader`) sees what the writer has
flushed, not the records still in its WAL.

**`TableStore` is the narrow surface**: `get`, `open` (positioned reads),
`map`, `put` (atomic, the manifest's commit point), `publish` (move a finished
local file in), `delete` (idempotent), `list`, `remove_empty_dirs`, `lease`.
`Store` is a concrete enum over the implementations (`LocalFs`, `S3Store`, a
test-only in-memory store), not `dyn`, so the ingest code stays monomorphic
and inside the qualified subset. `LocalFs` keeps the previous behaviour:
every existing ingest, crash and reader test passes unchanged.

**Two commit modes, chosen by probing the store when it opens.**

- *Conditional puts* (AWS S3, MinIO). The manifest is replaced only by a put
  that matches the version this writer last read or wrote (`If-Match`, or
  `If-None-Match: *` for the first), so a writer that lost the table cannot
  overwrite the one that took it over. The writer also holds a **lease**
  object (`LOCK`: owner and expiry, renewed by a thread, taken over once
  expired; a clean shutdown releases it), so a second writer on another
  machine is refused when it opens. Lease times are wall-clock, so machines
  sharing a prefix need clocks within a fraction of the TTL (30 s default).
- *No conditional puts* (Garage). A single PUT of the manifest is atomic, and
  the writer's local lock keeps a second writer on the *same machine* out. A
  second machine on the same prefix is not protected. The mode is chosen, not
  assumed: `CommitMode::ConditionalPut` fails to open a store that ignores the
  header, and `Auto` falls back.

**On a store with conditional puts, `publish` never replaces a different
object.** File names come from LSN ranges, so a stale writer and the one that
took over can both choose the same name; a plain put let the stale writer
overwrite a file the new manifest lists (found by a test). `publish` creates
the object only if absent and accepts an existing one that is identical (a
retry). The manifest fence protects the manifest; this protects the files it
lists.

**No TLS in db-core.** The S3 client (`storage-s3`, off by default, no
dependencies) implements SigV4 with its own SHA-256 and HMAC (checked against
the FIPS, RFC 4231 and AWS example vectors), listing, multipart upload and
ranged reads over a `Transport` trait. `PlainHttp` speaks `http://` (Garage,
MinIO on a private network). AWS is HTTPS only, so against AWS the embedding
application supplies a TLS `Transport`. A TLS stack in db-core would give up
the first-party-only closure (ADR 0000) and the MIT/Apache license allow-list
in `deny.toml`. If the project decides to accept that, a separate opt-in
feature (`storage-s3-tls`) can add it behind the same trait. The S3 module is
on the documented `dyn` boundary list (`MVL_LIMIT_EXCLUDE`) because the
transport is a trait object, as the `Vfs` implementors are.

**Directory renames become per-object publishes.** A flush, spill, hour close
or day merge publishes each output as its own object, then commits the
manifest. A crash part-way leaves uncommitted objects, which recovery deletes
by listing the area prefixes and removing what the manifest does not list.

## Consequences

- The `RowEngine` and SQLite files stay on the local filesystem, as decided.
- `TableEngine` (the column engine's table reader) opens a table in a store
  too: `TableEngine::open_in(name, store)` and `open_at("s3://...")`. Its
  reader is a `StoreReader`, so queries see the flushed files and keep no
  WAL-tail statistics. `TableReader` itself, the incremental reader of a
  local directory's WAL tail, stays local.
- Reading a remote table fetches whole objects for `mmap` and ranged GETs for
  `read_at`; a reader of many small files pays one request per read.
- A reader's retry when a file vanished is a 1 ms sleep, tuned for a local
  disk; a remote store would want a longer backoff.
- Verification against AWS S3 itself waits on a TLS transport. Garage is
  tested in CI with a container (SingleWriter mode); the conditional-put paths
  are tested against an in-process S3 fake and the in-memory store.
