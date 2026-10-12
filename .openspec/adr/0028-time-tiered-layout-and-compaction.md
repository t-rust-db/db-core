# ADR 0028: Time-tiered physical layout and compaction for ingested tables

## Status

Accepted (#573; builds on ADR 0027). Part (a) built: layouts, hour close,
day merge, sort key, deferred deletes; see "As built" at the end.

## Context

ADR 0027 defines *how* sealed pages become a Parquet file atomically. It
does not say *which* pages, *how big* a file is, or *where* it goes.
Those pull in opposite directions:

- **Storage and retention** want few, large files: one partition per
  day, files around 1 GB, row groups of 10⁵–10⁶ rows.
- **Processing** wants the recent past cheap: the last hour is queried
  far more than last month.
- **Memory** bounds the hot tier: a page is 4096 rows, and at 1k
  records/s an hour of pages (with `raw`) is about 2 GB.

Indicative sizes (≈ 500 B JSON record, ≈ 50 B compressed in Parquet,
≈ 600 B in memory):

| Rate | Rows/hour | Parquet/hour | Parquet/day | 1 hour in memory |
|---|---|---|---|---|
| 100/s | 360 k | ~18 MB | ~430 MB | ~200 MB |
| 1k/s | 3.6 M | ~180 MB | ~4.3 GB | ~2 GB |
| 10k/s | 36 M | ~1.8 GB | ~43 GB | ~20 GB |

A per-page flush (4096-row files) is ruled out: footer overhead, poor
encoding and thousands of files per day. Keeping the whole hour hot is
ruled out above ~100/s.

## Decision

### Partition by arrival time

Files are partitioned by `observed_ts` (arrival), not by `timestamp`
(event time). Because `observed_ts` and the LSN are assigned together and
both are non-decreasing (ADR 0027), **every arrival-time interval is a
contiguous LSN range**. Consequences:

- a closed hour or day is never reopened — there are no late records;
- an hour closes the moment a record with a later hour arrives (or a
  timer fires on an idle table); no grace period;
- every compaction merges adjacent LSN ranges, so ADR 0027's manifest
  invariant holds by construction.

Event-time queries prune on `timestamp` min/max (file entry and row
group), not on directory names. For log and transaction capture the two
times differ by seconds, so pruning is nearly as tight.

### Three levels

```
events/
├── manifest.json, LOCK, wal/          ADR 0027
├── tmp/                               every output is built here, then renamed into place
├── staging/date=D/hour=H/*.parquet    L1 partial: spills of the open hour
├── live/date=D/hour=H/*.parquet       L1: one closed hour, 1+ files ≤ max_file_size
└── archive/date=D/*.parquet           L2: one closed day, 1–N files ≤ max_file_size; final
```

| Level | Area | Holds | Bounded by |
|---|---|---|---|
| L0 | memory + `wal/` | the open hour's unflushed pages | `hot_memory_budget`, not the clock |
| L1 partial | `staging/` | the open hour's spills | the hour |
| L1 | `live/` | closed hours of open days | the day |
| L2 | `archive/` | closed days | `retention` |

Each manifest file entry gains `level`, `area` and its partition.

- **L0 → staging (spill).** If `hot_memory_budget` is exceeded mid-hour,
  the oldest pages are flushed as a partial file into `staging/`. Pages
  never straddle an hour boundary: the head page seals at the boundary.
- **L0 + staging → live (hour close).** At hour close the hour's
  remaining pages and its partial files (if any) are written as one
  closed hour — decode + re-encode, so row groups reach size — and
  published into `live/` in one step. Without spills this is a plain
  flush.
- **L1 → L2 (day merge).** A closed day's hour files are concatenated in
  LSN order into day files. Row groups never straddle an hour and are
  capped at `row_group_max_rows`, so "last hour of yesterday" still
  prunes to the matching row groups. Hour files are already in LSN
  order and their row groups already sorted (below), so this needs no
  sort; where an hour file's row groups are already within bounds, their
  column chunks are copied byte-for-byte and only the footer (offsets) is
  rewritten.

### Time partitions, secondary sort key inside row groups

A file has one physical order, so a second access path (`WHERE
event_type = 'login'`) comes from clustering, not from a second copy.
Time selects the **row group** (row groups never straddle an hour);
inside a row group, rows are **sorted by `sort_key`** (e.g.
`event_type, timestamp`) when the L1 file is written — the first point
where rows are rewritten anyway (L0 pages stay in arrival order). Every
row group then holds each `event_type` as one contiguous run, so:

- row-group `event_type` min/max become tight (row-group pruning, #458,
  works on the secondary key too);
- a future page index (ADR 0026's page-skipping follow-up) narrows a
  surviving row group to the pages of that run;
- dictionary-encoded runs compress better.

Arrival order inside an hour is not preserved in Parquet; results that
need it `ORDER BY observed_ts`. LSN ownership is per file, so sorting
rows within a file does not affect ADR 0027's invariant.

### Publishing: one rename per output

Every output — a spill, a closed hour, a closed day — is written under
`tmp/` (outside every area, so no glob ever matches a half-written
output) and published with **one** rename: a single file, or for an
output of several files (an hour or day above `max_file_size`) its
directory, `tmp/<id>/` → `live/date=D/hour=H/` or `archive/date=D/`. A
directory listing therefore sees a closed hour or a closed day either
whole or not at all. Once published, a `live/` hour directory and an
`archive/` day directory are never modified, only removed whole.

**Invariant: `archive/` is final.** No compaction takes an `archive/`
file as input; only retention removes it, a whole day at a time. Inputs
and outputs of every compaction are in different areas: `staging/` →
`live/` at hour close, `live/` → `archive/` at day close.

### The compactor

A background **compactor thread in the writer process** — the only
process that may change the manifest (ADR 0027). Compaction is a flush
whose input is files instead of pages:

1. Write the output under `tmp/`, `fsync`, publish it with one rename
   (above), `fsync` the parent directory.
2. **Commit:** one manifest swap removes the inputs and adds the output,
   with the output's `lsn_lo`/`lsn_hi` = the inputs' union. No snapshot
   sees those rows twice or not at all.
3. Delete the inputs once (a) no in-process snapshot references them
   (`Arc` count) and (b) `delete_grace` has passed, for external readers
   that glob the directory and are not tracked.

The compactor reads one row group at a time; its memory is bounded by
`row_group_max_rows`, independent of file size. Recovery deletes
unreferenced outputs exactly as ADR 0027 deletes unreferenced flushes.

### Reading with external tools (DuckDB)

db-core readers go through the manifest and never read anything twice
(ADR 0027). A tool that globs files instead sees no snapshot, and while a
compaction's inputs wait out `delete_grace` next to its output, a glob
that covers both areas reads those rows twice. No order of renames and
deletes avoids that: a glob that is not a snapshot gets either
duplicates (output first) or gaps (inputs first). The layout therefore
decides what external tools may read:

- **Supported, exact: `archive/` only.** Archive files are final and each
  day is published whole, so this glob can never duplicate or tear a day:

  ```sql
  CREATE VIEW events AS
  SELECT * FROM read_parquet('events/archive/*/*.parquet',
                             hive_partitioning = true,
                             union_by_name = true);   -- Tier-3 columns differ per file
  ```

  It covers closed days. Live and recent queries go through db-core
  (column-rs, loglume, the embedding application), which also sees
  `live/`, `staging/` and the WAL.

- **Best-effort: `archive/` + `live/`, archive wins per day.** Adds
  today's closed hours, dropping `live/` rows for any day already in
  `archive/`, which removes the `delete_grace` window:

  ```sql
  CREATE VIEW events_fresh AS
  WITH a AS (SELECT * FROM read_parquet('events/archive/*/*.parquet',
                                        hive_partitioning = true, union_by_name = true)),
       l AS (SELECT * FROM read_parquet('events/live/*/*/*.parquet',
                                        hive_partitioning = true, union_by_name = true))
  SELECT * FROM a
  UNION ALL BY NAME
  SELECT * FROM l WHERE date NOT IN (SELECT DISTINCT date FROM a);
  ```

  DuckDB lists the two globs separately, so a query that runs across the
  instant a day-merge commits can still read that day twice or not at
  all (once per day). Not for counts that must be exact.

- **Never: `staging/`.** Partial files are merged and removed every hour.

A retention delete during a long external query surfaces as a
missing-file error, not a wrong answer.

**Future extension (Option A, not decided):** exact fresh reads for
DuckDB via a Delta Lake log (`_delta_log/`) generated from the manifest —
a flush as `add`, a compaction as `remove` + `add`, read with
`delta_scan`. `manifest.json` stays the source of truth and the Delta log
a derived, rebuildable view, so there is still one commit point. Open
before adopting it: Delta's table-wide schema vs. our per-file schemas
and type widening, its fixed partition columns vs. `live/` (date, hour)
and `archive/` (date), and log checkpoints. Deferred together with the
broader table-format study (ADR 0027).

### Retention

Dropping data older than N days is a manifest commit removing whole L2
partitions, followed by the same deferred delete.

### Configuration

```toml
[layout]
hot_memory_budget = "512MB"   # L0 spills to partial L1 files beyond this
l1_period = "1h"
l2_period = "1d"
max_file_size = "1GB"
row_group_max_rows = 1_000_000
sort_key = ["event_type", "timestamp"]   # within each row group; empty = arrival order
delete_grace = "10m"          # external readers of replaced files
retention = "90d"
```

No further levels until a workload measurably needs one.

## Consequences

- Read amplification is bounded: a query over the last hour touches L0
  plus at most a handful of L1 files; a query over history touches about
  one file per GB per day.
- Write amplification is about 2× (L1 once, L2 once; partial hours one
  more re-encode); at 1k/s that is ~4.3 GB/day rewritten.
- Recovery replays at most `hot_memory_budget` worth of WAL: about
  15 minutes at 1k/s with 512 MB.
- The layout values are workload-dependent and are expected to change
  once ingest rates are measured; the ADR 0027 commit protocol is not.
- Event-time partitioning, if ever needed, reintroduces late records,
  reopened partitions and a close grace; it is a new decision, not a
  config switch.

## As built (#573, part a)

**Layout is a per-table choice.** `TableOptions.layout` is
`Layout::Flat`, ADR 0027's one file per flush in `data/` and the default,
or `Layout::Tiered(TierOptions)`, this ADR. The manifest (version 2)
records the kind and each file's `area` (`data`, `live`, `archive`);
version 1 reads as flat. Opening a table with the other kind fails.
`TierOptions` holds `sort_key`, `row_group_max_rows`, `max_file_size` and
`delete_grace_ns`. They are options, not TOML, and may change between
opens. Arrival time comes from `TableOptions.clock`, the system clock by
default.

**Amendment: the whole hour is sorted, then cut into row groups.**
Sorting rows *within* a row group cannot change that row group's min/max,
so it would not let statistics skip row groups on the sort key. It only
tightens page statistics, which need several pages per chunk and a page
index. A closed hour is therefore sorted by `sort_key` as a whole, then
cut into row groups of at most `row_group_max_rows`, so each row group
holds a range of the key. With `sort_key = ["kind"]`, a test shows
`WHERE kind = 'logout'` skipping 2 of 3 row groups, and none in arrival
order. In consequence:

- A closed hour is always **one file** (its LSN range is the hour's).
  `max_file_size` applies at the day merge, which starts a new file at an
  hour boundary once a file reaches it. A very busy hour can exceed the
  size.
- Row groups are no longer contiguous LSN runs. The day merge checks only
  that each input holds as many rows as its LSN range.
- Building the hour holds its rows as values in memory, besides the
  pages. `staging/` spills (part b) bound both.

**Hours and days close on a call, not a timer.** `flush()` writes every
closed hour, and seals a head whose hour has passed, so an idle table's
hour closes on the next call. `compact()` first flushes every closed
hour, then merges each closed day. Merging a day before its last hour is
in `live/` would publish its archive directory twice. The compactor
thread and an idle timer come with #574's ingest thread. `Ack::Flushed`
is refused on a tiered table until spills exist.

**Deferred deletes are in memory.** Replaced files are deleted by the
first `flush()`/`compact()` after `delete_grace_ns`. In-process snapshots
need no tracking, since a mapped file stays readable after it is deleted.
`Snapshot::open` in another process re-reads the manifest when a listed
file has vanished. After a crash, reopening deletes every unlisted file
at once, without the grace period.

**The day merge decodes and re-encodes** one row group at a time,
widening to the day's schema. The byte-for-byte chunk copy is a later
optimisation.

**Recovery** removes unlisted files in `data/`, `live/` and `archive/`
and the directories left empty. A crash after each step of a flush and a
compaction (write, publish, commit, release) recovers every record once.

**Checked:**
- DuckDB over `archive/*/*.parquet` (`hive_partitioning`,
  `union_by_name`) returns exactly the closed days' rows under their
  `date` (`make check-parquet-write-oracle`, case `tiered_table`).
- The flush-timing property test runs half its seeds tiered, with a
  moving clock, random compactions and random sort keys.

**Not yet (part b):**
- `staging/` spills under `hot_memory_budget`
- retention
- the byte-copy merge
- the compactor thread
