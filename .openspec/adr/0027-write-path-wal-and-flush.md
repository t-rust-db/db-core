# ADR 0027: Write path — record WAL, three watermarks, flush to Parquet

## Status

Proposed (#TBD)

## Context

Every mode db-core has today reads data someone else wrote: `engine::row`
writes SQLite files, but `engine::column` (ADR 0017) only reads Parquet
and `engine::stream` (ADR 0018) only tails a log file another process
appends to. There is no way to *ingest* records — structured JSON or
free-text log lines — and query them as a column table.

Two options were considered:

1. **Batch export.** Buffer records, periodically write a Parquet file.
   Records are invisible until their batch is flushed, so a live use case
   (a session's recent events) needs a separate hot store — the
   "PostgreSQL hot window" of the DetACT event-store proposal.
2. **Queryable write-ahead tail.** Records land in an in-memory page that
   is queryable immediately, are made durable by a separate WAL thread,
   and the oldest pages are flushed to Parquet. Queries read the union of
   Parquet and the in-memory tail.

Option 1 is option 2 with memory-level acknowledgement, large pages and
no tail query. Option 2 is chosen; its first implementation phase (the
Parquet writer) delivers option 1 on its own.

Most of option 2 already exists. ADR 0018's `storage::stream` turns
appended bytes into sealed, indexed `Segment`s (`storage/stream/
segment.rs`, ≤ `SEGMENT_MAX_ROWS` = 4096 rows, minmax/dictionary/bloom)
held in a `Ring`, and presents them to `vm::batch` as `Segment`s;
`jsonl.rs` already parses JSON records into `LogBatch` columns.
`engine::column` already presents Parquet row groups as `Segment`s with
statistics pruning (#458) and `order_key_bound` (#464). `vm::engine::run`
already runs one program over a list of segments with a `Combine`
barrier. What is missing is (a) a durability contract on append, (b) a
Parquet writer, and (c) a handoff rule between the tail and the files
that never duplicates or drops a row.

## Decision

### One writer, LSNs, three watermarks

A table is a directory owned by **one writer process**, enforced by an
exclusive lock on `<table>/LOCK` (`std::fs::File::try_lock`). Clients
submit records over a channel; a single **ingest thread** owns the head
page. For each record it assigns a monotonically increasing **LSN**
(`u64`) and an `observed_ts` (wall clock, clamped to be non-decreasing),
parses the record into the head page, and appends its framed bytes to
the WAL buffer — one step, one owner, no locks on the write path.

Three watermarks track a record's progress, with the invariant
`flushed_lsn ≤ durable_lsn ≤ visible_lsn`:

| Watermark | Advanced by | Meaning |
|---|---|---|
| `visible_lsn` | ingest thread | in a page; queryable |
| `durable_lsn` | WAL thread, after `fsync` | survives a crash |
| `flushed_lsn` | flush, at the manifest commit | in Parquet; WAL may be dropped |

### Acknowledgement is a level, not a thread model

The WAL thread is always separate. What a caller waits for is chosen per
write (with a per-table default):

```rust
enum Ack { Memory, Durable, Flushed }
fn append(&self, record: &[u8], ack: Ack) -> Result<Lsn, WriteError>;
```

- `Memory` returns once the record is in the head page; a crash loses up
  to one group-commit interval.
- `Durable` (default) returns once `durable_lsn ≥ lsn`.
- `Flushed` returns once `flushed_lsn ≥ lsn` (tests, backfills).

The WAL thread **group-commits**: drain the buffer, one `write`, one
`fsync`, advance `durable_lsn`, wake waiters. One fsync covers every
record that arrived during the previous one, so `Durable` costs latency
(one fsync), not throughput.

### WAL format

One file per page: `<table>/wal/<lsn_lo:020>.wal`. Frames are
`[len u32][crc32c u32][lsn u64][payload]`, payload being the record bytes
as received. Per-frame CRCs detect a torn tail, which plain JSON Lines
cannot. File-per-page makes discarding flushed pages an `unlink`, never
a truncate. The SQLite WAL in `storage/row/pager/wal.rs` is page-frame
shaped and is not reused.

### Pages

A page is a `storage::stream::Segment` extended with `lsn_lo`/`lsn_hi`.
The head page is the only mutable one; it seals at `SEGMENT_MAX_ROWS` or
an idle timeout, exactly as in ADR 0018, and a sealed page is immutable
and shared as `Arc<Segment>`. The schema is ADR 0018's: Tier-1 `raw`,
Tier-2 `timestamp`/`observed_ts`/`severity`/`facility`/`message`,
Tier-3 promoted fields with the existing type rules (first non-null type
wins, conflicts degrade to `Str`).

### Flush: the commit protocol

A flush takes the oldest sealed pages P₁…Pₖ (a contiguous LSN range) and
writes them as one Parquet file. Which pages, and where the file goes,
is ADR 0028's business; the commit is this one:

1. Encode P₁…Pₖ to `<path>.tmp`; `fsync` the file.
2. Rename to `<path>`; `fsync` the directory.
3. **Commit:** write `manifest.json.tmp` with the new file entry and
   `flushed_lsn = Pₖ.lsn_hi`; `fsync`; rename over `manifest.json`;
   `fsync` the directory.
4. Publish a new in-memory `Arc<Snapshot>`.
5. Unlink `wal/*.wal` whose `lsn_hi ≤ flushed_lsn`; release the pages
   from the ring (readers holding the old snapshot keep them alive).

The manifest rename is the **only** commit point. Each file entry carries
`path`, `lsn_lo`, `lsn_hi`, `rows`, `bytes`, `observed_ts` and
`timestamp` min/max; `lsn_lo`/`lsn_hi` are also written to the file's
Parquet key-value metadata so a file describes itself.

**Manifest invariant:** file LSN ranges are pairwise disjoint and their
union is exactly `[1, flushed_lsn]`. A checker for it runs in tests and
on open.

### Recovery

1. Read `manifest.json` → files, `flushed_lsn`.
2. Delete any `*.tmp`, and any Parquet file under the table not listed in
   the manifest (written before step 3; its rows are still in the WAL).
3. Replay `wal/*.wal` frames with `lsn > flushed_lsn` into pages; stop at
   the first frame whose CRC fails and truncate there.
4. `next_lsn = max(replayed, flushed_lsn) + 1`.

Recovery is idempotent; its cost is bounded by how much un-flushed WAL
ADR 0028's memory budget allows.

### Reads: one snapshot, one segment list

A query takes one `Arc<Snapshot>` = { manifest files, sealed pages with
`lsn > flushed_lsn`, a copy of the head page taken under its lock (≤
4096 rows) }. Its segments are:

- Parquet row groups — `engine::column`'s `RowGroupSegment`, pruned by
  footer statistics;
- sealed pages — ADR 0018's adapters, pruned by minmax/dictionary/bloom,
  plus `order_key_bound` from their minmax (new);
- the head copy — never pruned.

All go to `vm::engine::run` unchanged. Every LSN has exactly one owner in
a snapshot, so the union has no duplicates and no gaps. The default read
level is `visible_lsn`; a query option restricts it to `durable_lsn` for
callers that must not see a record a crash could still erase.

### Readers in other processes

The head page lives only in the writer's memory, but everything durable
is on disk, so another process (column-rs, loglume, db-studio) reads a
table without a server, IPC or the `LOCK`:

1. Read `manifest.json` → files, `flushed_lsn`.
2. Read `wal/*.wal` frames with `lsn > flushed_lsn`, in file order, up to
   the first frame whose CRC fails (the durable tail, possibly still
   being written).
3. If a WAL file named in step 2 has disappeared — a flush committed in
   between — start again at step 1.

Such a reader sees up to `durable_lsn`; only an in-process reader sees
`visible_lsn`. With `Ack::Memory` writes, other processes trail by one
group-commit interval. The snapshot this builds is the same type the
writer uses, so clients consume one API (ADR 0029).

Files have different schemas (Tier-3 fields come and go). A column
missing from a segment reads as NULL; today `engine/column.rs` reports a
missing column as `VmError::SegmentLoad`, which changes for this mode.

### Parquet writer: `storage::column::parquet::write`

v1 is deliberately small:

- flat schema only (`raw` keeps nested content reachable via
  `json_extract`, #307);
- data page v1, PLAIN encoding; RLE_DICTIONARY for `OwnedColumn::Dict`
  and low-cardinality strings; RLE definition levels for nullability;
- column-chunk min/max statistics (pruning depends on them);
- snappy compression (a pure-Rust encoder beside the existing decoder);
  zstd later;
- the footer via a compact-protocol encoder added to `thrift.rs`.

Type mapping from `OwnedColumn`: `Int` → INT64, `Float` → DOUBLE, `Bool`
→ BOOLEAN, `Str` → BYTE_ARRAY (UTF8), `Dict` → dictionary-encoded
BYTE_ARRAY; `timestamp`/`observed_ts` → INT64 `TIMESTAMP(NANOS)`.

### Logfile mode: the source file is the WAL

When the input is a log file another process appends to (ADR 0018's
`LogFile` tail), that file is already durable; copying every line into
our WAL would write it twice. In this mode:

- the LSN is the byte offset of the line in the source file, and the
  manifest records `source = { path, file identity }` with
  `flushed_lsn` as an offset;
- there is no `wal/` directory and no `Ack` (nothing to wait for);
- recovery resumes the tail at `flushed_lsn`;
- **rotation forces a flush**: a changed file identity or `len <
  head_off` (already detected for truncation) flushes the hot tier before
  following the new file, which starts a new LSN epoch recorded in the
  manifest. Rows in the hot tier are lost only if the rotated file is
  deleted before that flush completes.

Other-process readers see only flushed data in this mode (the source
file is not in the manifest's format); in-process readers see everything
tailed.

### Engine seam

The table is opened as `Mode::Batch` (ADR 0017) on a directory that
contains `manifest.json`; no new mode. Writing is the Rust `append` API
above; SQL `INSERT` is out of scope.

## Structure

Feature split and client packaging: ADR 0029.

- `src/storage/column/parquet/write/` — the writer.
- `src/storage/ingest/` — `wal.rs` (frames, group commit), `table.rs`
  (writer, ingest thread, watermarks), `manifest.rs` (format, atomic
  replace, invariant check), `recover.rs`, `snapshot.rs`.
- `src/engine/column.rs` — directory open, NULL-fill for missing columns,
  snapshot segment list.

## Consequences

- No separate hot store: the most recent records are queryable through
  the same engine and SQL as history, the moment they are accepted.
- One writer per table. Readers in-process and in other db-core
  processes go through the manifest. Tools that glob the Parquet files
  directly (DuckDB `read_parquet('events/**/*.parquet')`) see only
  complete files (tmp + rename) and only flushed data, and during ADR
  0028's `delete_grace` they also see a compaction's inputs next to its
  output — duplicates. Correct external reads need the manifest.
- The writer is new code with an external oracle: every file it writes
  must round-trip through db-core's reader and through DuckDB
  `read_parquet` (benchmark parity suite).
- Correctness obligations, as tests: crash at each flush step 1–5
  recovers to the same row set; torn WAL tail; segment-split invariance
  (ADR 0025) across the page/row-group boundary; the manifest invariant.
- Out of scope: deletes and corrections, SQL `INSERT`, multi-table
  transactions, more than one writer, and a standard table format for
  the manifest (Delta Lake, Iceberg, DuckLake) — deferred for separate
  study. `manifest.json` is db-core's own format; replacing it later
  changes the commit point's encoding, not the protocol above.
- Field names follow ADR 0018 and map one-to-one onto the OpenTelemetry
  Logs data model (`timestamp` ↔ `Timestamp`, `observed_ts` ↔
  `ObservedTimestamp`, `severity` ↔ `SeverityNumber`, `message` ↔
  `Body`, Tier-3 ↔ `Attributes`, `source`/`resource` ↔ `Resource`), so
  OTel input needs aliasing only.
