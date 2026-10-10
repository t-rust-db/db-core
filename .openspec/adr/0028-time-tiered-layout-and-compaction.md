# ADR 0028: Time-tiered physical layout and compaction for ingested tables

## Status

Proposed (#TBD; builds on ADR 0027)

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
L0  hot   pages + WAL          bounded by hot_memory_budget, not by the clock
L1  hour  date=D/hour=H/        one file per hour (several if > max_file_size)
L2  day   date=D/               1–N files ≤ max_file_size, in time order
```

Each manifest file entry gains `level` (1 or 2) and its partition.

- **L0 → L1 (flush).** At hour close the hour's pages are flushed as one
  L1 file. If `hot_memory_budget` is exceeded mid-hour, the oldest pages
  are flushed early as a **partial** L1 file for the current hour. Pages
  never straddle an hour boundary: the head page seals at the boundary.
- **L1 → L1 (hour merge).** A closed hour with more than one partial file
  is merged into one hour file (decode + re-encode, so row groups reach
  size).
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

### The compactor

A background **compactor thread in the writer process** — the only
process that may change the manifest (ADR 0027). Compaction is a flush
whose input is files instead of pages:

1. Write the output via tmp + `fsync` + rename.
2. **Commit:** one manifest swap removes the inputs and adds the output,
   with the output's `lsn_lo`/`lsn_hi` = the inputs' union. No snapshot
   sees those rows twice or not at all.
3. Delete the inputs once (a) no in-process snapshot references them
   (`Arc` count) and (b) `delete_grace` has passed, for external readers
   that glob the directory and are not tracked.

The compactor reads one row group at a time; its memory is bounded by
`row_group_max_rows`, independent of file size. Recovery deletes
unreferenced outputs exactly as ADR 0027 deletes unreferenced flushes.

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
