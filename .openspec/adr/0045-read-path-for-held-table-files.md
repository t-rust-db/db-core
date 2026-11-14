# ADR 0045: The read path for a table's files: parse a footer once, fetch an object store's files by range

Date: 2026-11-12

## Status

Accepted, built (#1052).

## Context

A table reader lives long: a server holds one `Table` handle, a
`StoreReader` or a `TableEngine` for its whole life, and a listed Parquet
file never changes (ADR 0027, 0028). Two costs did not use that:

- **Every query parsed every footer again.** `query_snapshot`,
  `snapshot_files*`, `Snapshot::schema` and the segment counts each called
  `ParquetFile::open(bytes)`, which decodes the footer and builds the
  schema tree. That costs about 0.6-0.7 µs per column chunk the footer
  describes (`ParquetFile::open` on the fixtures: 2.8 µs for 1 row group of
  3 columns, 82 µs for 15 row groups of 8). A day file of 350 row groups and
  30 columns is about 7 ms; a 90-day query paid that 90 times before
  reading a value.
- **An object store's files are fetched whole** (ADR 0044, consequences):
  `TableStore::map` on S3 is one GET of the object, and the reader keeps
  every object in memory. Memory grows with the table with no bound, and a
  cold start downloads all of it. `ParquetFile::open_file` already reads
  only the footer and then one column chunk at a time through `VfsFile`
  (#841), but the table path does not use it.

## Decision

**A held file keeps its parsed footer.** A snapshot's files are
`HeldFile`s: the bytes and a `OnceLock<Arc<ParsedFooter>>`. The first open
parses the footer; every later open of the same held file, in any later
snapshot of the same reader, shares it (`ParquetFile::with_footer`). The
readers already keep a file across snapshots by path (`TableReader`,
`StoreReader`, and the writer since #1051), so the footer lives exactly as
long as the reader holds the file, and goes when the manifest stops listing
it. `ParsedFooter` is the footer plus what is derived from it (the schema
tree, leaf positions, max definition levels); `ParquetFile` holds it behind
an `Arc` and borrows only the bytes. Nothing in the public API changes.

**An object store's files are read by range.** `TableStore::hold` gives a
snapshot its file: by default the whole object, mapped; `Store::S3` a
`StoreFile` opened with `ParquetFile::open_file` instead, so a query costs
a ranged GET for the footer's tail and one for the footer (once per file
per reader, since the footer is kept), then one per column chunk it loads.
The size comes from the manifest (`FileEntry.bytes`), so no HEAD per file.
What a held file reads goes into the store client's read cache, keyed by
(object, offset, length) and evicted least recently used first under
`S3Config::read_cache_bytes` (256 MiB; 0 keeps none). One cache per store
client, so every reader of that store shares the budget. A file opened to
be read once (a flush, a compaction) bypasses it, so a compaction does not
evict what queries read. Local files stay on `mmap`.

- The unit is the whole column chunk, as `column_chunk_bytes` reads it
  today. There is no page index or bloom filter yet (#590), so nothing finer
  could be pruned, and a Snappy page cannot be partly decompressed.
- **Fewer, wider requests (#1059).** A request costs 20-100 ms before its
  first byte, so the request count is what a cold query pays:
  - `open_file` reads the last 64 KiB at once: the footer's length, and the
    footer itself when it fits, so a footer costs one GET, not two. A file
    no larger than that is read whole by it, and its chunks then come
    from the cache.
  - A segment tells the file which chunks it is about to read
    (`RowGroupReader::prefetch` -> `VfsFile::prefetch`, a no-op unless
    remote) in each load: the eager load, both phases of a late
    materialization (ADR 0026: the projection's only when a row survived)
    and a top-N gather. Late materialization still decides which columns;
    only the GET is wider. The S3 file merges chunks at most
    `S3Config::prefetch_gap` apart (1 MiB) into one span, skips what is
    held, and fetches up to `prefetch_per_file` spans at once (8). The
    read cache answers a read from any held range that contains it.
  - A query reads the footers of the files it kept (after key-range
    pruning) 16 at a time, not one file after another.
  - Every ranged GET counts against `S3Config::max_in_flight` (64) for the
    store. A failed prefetch is not an error: the read that wants the
    bytes fetches them and reports one.

## Consequences

- A query over a table of 300 local files (10 columns, 2,000 rows each)
  went from 6.1 to 2.5 ms (`count(*) ... WHERE id = 5`, A/B/A against the
  previous commit). The gain grows with row groups × columns per file.
- A reader holds one parsed footer per listed file. That is small next to
  the bytes it already held whole, and it goes with the file.
- On the S3 test transport, a filtered `count(*)` over three files of a
  wide table read 7.6 KB in 9 GETs where the whole objects are 45.8 KB in
  3; the same query again read nothing.
- On a 30-day, 33-file table of 19 columns behind a fake bucket that counts
  requests (1 ms each), the cold requests of five API calls fell from
  1506/356/666/40/906 to 33/10/33/4/33, and the longest chain of requests
  one after another from 518/132/435/28/493 to 42/11/9/5/8 (#1059). Those
  files are smaller than the tail read, so each is now read whole in one
  GET: more bytes (306 KB instead of 253 KB for the largest call) for far
  fewer requests.
- `snapshot_files` and `snapshot_segments*` keep their signatures: they
  already return `ParquetFile`, so a caller that plans and runs itself gets
  the cached footers and the ranged reads without a change.
- `Snapshot::files()` returns `Result<Vec<..>, IngestError>`: a ranged file
  has no whole bytes until fetched, and that fetch can fail. A caller that
  only needs footers reads them through `snapshot_files` instead, which
  fetches no whole object.
- No dependency and no `unsafe` (`OnceLock` is std): the column profile
  (ADR 0000) and the wasm32 build are unchanged, and the stream profile does
  not compile `storage::ingest`.
