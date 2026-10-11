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

1. Encode P₁…Pₖ under `<table>/tmp/`; `fsync`.
2. Publish with one rename — a file, or the directory of a multi-file
   output (ADR 0028) — and `fsync` the parent directory.
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
2. Empty `tmp/`, and delete any Parquet file under the table not listed
   in the manifest (published before step 3; its rows are still in the
   WAL).
3. Replay `wal/*.wal` frames with `lsn > flushed_lsn` into pages; stop at
   the first frame whose CRC fails and truncate there.
4. `next_lsn = max(replayed, flushed_lsn) + 1`.

Recovery is idempotent; its cost is bounded by how much un-flushed WAL
ADR 0028's memory budget allows.

**Amendment (#571): only a torn tail is truncated.** A segment file is
complete and fsynced before the next one is created, so a crash can only
tear the end of the last file. Step 3 therefore truncates only when the
bad frame (incomplete or failing its CRC) is in the last file and no
valid frame follows it. Anything else — a bad frame in an earlier file, a
bad frame followed by valid ones, an LSN gap, or a missing file — is
damage to data that may already have been acknowledged as durable;
truncating there would drop it silently, so opening fails with
`WalError::Corrupt` instead and leaves the files untouched.

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

**Amendment (#572): as built.** `TableWriter::snapshot()` returns a
`Snapshot` (manifest, memory-mapped files, sealed pages plus a copy of
the head); `engine::column::table::query_snapshot` runs one `SELECT` over
it. Not yet built: minmax/dictionary pruning and `order_key_bound` for
pages (they are loaded and filtered), and the `durable_lsn` read option.

The table's columns are the union of every file's and page's columns,
each field widened over all of them by the flush rule (INT + FLOAT →
DOUBLE, any other mix → string). Each segment then reads as that schema:
a column it lacks is NULL, and a column it stores with another type is
cast by the rule a flush writes by (`snapshot::cast`), so a record reads
the same before and after its flush. A row group that needs neither is
the plain `RowGroupSegment`; one that does decodes eagerly and fixes its
columns up. Footer statistics prune and bound a top-N only through
columns stored with the table's type: an INT64 file's statistics say
nothing about the string comparison a widened column gets. A property
test checks that query results do not depend on where seals and flushes
fell, in and out of process.

One case is not invariant: a field that sees integers above 2⁵³, floats
and strings. A flush whose pages had integers and floats stores the
integer as DOUBLE, rounded; once a string widens the table's column to
text, it reads as the rounded number, where the same integer flushed
without a float reads exactly. (A whole DOUBLE renders like the integer,
`1` not `1.0`, so smaller integers are unaffected.) Within one page, a
type conflict loses values in the parser before any of this (#584).

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

**Amendment (#572): as built.** `Snapshot::open(dir)` does steps 1–3
without the `LOCK` and without changing a file. A bad or partial frame
ends the read only in the last WAL file (the writer may be writing it);
in an earlier file it is corruption, as in recovery. It starts again
when the files no longer reach down to `flushed_lsn + 1` or one vanishes
mid-read, up to 20 times, then fails. The reader sees at least
`durable_lsn`, and may see more: a frame written but not yet fsynced is
read too, and a power cut could still take it back. Missing columns read
as NULL, as above. `tests/table_reader.rs` reads a table from another
process while `tests/helpers/table_probe.rs` appends, seals and flushes,
and checks each read reaches the writer's last durable ack and holds
every LSN once.

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

**Amendment (#572):** the engine is `engine::column::table::TableEngine`,
`Mode::Batch`, opened on the directory; the table is named after it.
Each `run_query` reads a fresh snapshot. `BatchEngine` stays the
single-file engine. Joins, `IN (SELECT ...)` and window functions are
unsupported, as in `BatchEngine`.

### Where tables differ from DuckDB (#572)

Flushed files are plain Parquet that DuckDB and pyarrow read exactly
(`make check-parquet-write-oracle` checks types, every value and the
statistics, for a flushed table too). Where the *table* behaves
differently from DuckDB, deliberately:

| Aspect | db-core table | DuckDB |
|---|---|---|
| Timestamps (`timestamp`, `observed_ts`) | INT64 `TIMESTAMP(NANOS)`, `isAdjustedToUTC = false`, values UTC by convention | Same marking as its own `TIMESTAMP_NS`, read at full precision. A UTC-marked nanosecond column would read as microsecond `TIMESTAMPTZ` (truncated), which is why we don't mark it |
| Timestamp range | full `i64` | `i64::MAX` and `i64::MIN + 1` are ±infinity; instants in late 1677 cannot be converted (also in files DuckDB writes) |
| Which rows a reader sees | the manifest's files plus the WAL tail / pages above `flushed_lsn` | reads files only: never the unflushed tail. A glob over `data/` can also catch a file published but not yet committed (between flush steps 2 and 3), which recovery deletes; read the manifest's file list, or `archive/` once ADR 0028 lands |
| Schema across files | each file has the fields its pages had | needs `read_parquet(..., union_by_name = true)` across files |
| Field type conflicts | INT and FLOAT widen to DOUBLE; any other mix becomes a string, per file and again across the table at read time. An integer above 2⁵³ in a field that also has floats and strings may read exact or rounded depending on flush timing; a conflict inside one page loses values (#584) | `read_json` unifies to a common supertype, often `VARCHAR`/`JSON` |
| Lines that are not JSON in a `json` table | kept as a row (`raw`, `message`), fields NULL | `read_json` fails, or drops them with `ignore_errors` |
| Nested objects | flattened one level to `a.b`; deeper objects are not columns (still in `raw`) | `STRUCT` columns |
| Arrays | the JSON text, as a string | `LIST` columns |
| String values | kept as written, JSON escapes included (ADR 0018's zero-copy parser does not unescape) | unescaped |
| Reserved names | `timestamp`, `observed_ts`, `severity`, `severity_text`, `facility`, `message`, `raw` are always columns; a JSON key with one of those names is not a separate column (the alias table may promote it, e.g. `msg` → `message`) | every key is a column |
| `severity` | INT32 (OpenTelemetry SeverityNumber) plus `severity_text` | whatever the source had |

## Structure

Feature split and client packaging: ADR 0029.

- `src/storage/column/parquet/write/` — the writer.
- `src/storage/ingest/` — `wal.rs` (frames, group commit), `table.rs`
  (writer, ingest thread, watermarks), `manifest.rs` (format, atomic
  replace, invariant check), `recover.rs`, `snapshot.rs`.
- `src/engine/column/table.rs` — `TableEngine`, `query_snapshot`, the
  table schema, NULL-fill and casts; `src/engine/column.rs` holds the
  `TableSegment` enum over row groups and pages (it borrows the
  `RowGroupSegment` lifetimes, outside the qualified subset).

## Consequences

- No separate hot store: the most recent records are queryable through
  the same engine and SQL as history, the moment they are accepted.
- One writer per table. Readers in-process and in other db-core
  processes go through the manifest and never read a row twice. Tools
  that glob files (DuckDB) have no snapshot; ADR 0028 confines them to
  `archive/`, which is exact, and records a Delta Lake log derived from
  the manifest as the future extension for exact fresh reads.
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
