# ADR 0045: The read path for a table's files: parse a footer once, fetch an object store's files by range

Date: 2026-11-12

## Status

Accepted. The footer cache is built (#1052, first part). Reading an object
store's files by range, with a column-chunk cache under a byte budget, is
the decided second part and is not built yet.

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

**An object store's files are read by range (second part).** For a store
that is not the local filesystem, a held file is a range-reading `VfsFile`
over the store's object, opened with `ParquetFile::open_file`: one ranged
GET for the footer, then one per column chunk a query loads. The size comes
from the manifest (`FileEntry.bytes`), so no HEAD per file. Fetched chunks
go into an LRU keyed by (path, offset) with a byte budget, an option on the
reader (default 256 MiB). Local files stay on `mmap`.

- The unit is the whole column chunk, as `column_chunk_bytes` reads it
  today. There is no page index or bloom filter yet (#590), so nothing finer
  could be pruned, and a Snappy page cannot be partly decompressed.
- No prefetch or coalescing of ranges at first. Chunks are loaded lazily by
  the segment workers, and late materialization (ADR 0026) skips columns a
  `WHERE` rejects, so fetching a precomputed list would fetch more. A
  request costs 20-100 ms before its first byte; coalescing is the next
  step if the request count shows in the probe.

## Consequences

- A query over a table of 300 local files (10 columns, 2,000 rows each)
  went from 6.1 to 2.5 ms (`count(*) ... WHERE id = 5`, A/B/A against the
  previous commit). The gain grows with row groups × columns per file.
- A reader holds one parsed footer per listed file. That is small next to
  the bytes it already held whole, and it goes with the file.
- `snapshot_files` and `snapshot_segments*` keep their signatures: they
  already return `ParquetFile`, so a caller that plans and runs itself gets
  the cached footers without a change. `Snapshot::files()` still returns
  whole bytes, which a range-read file will not have; the second part adds
  an accessor for those rather than changing it.
- No dependency and no `unsafe` (`OnceLock` is std): the column profile
  (ADR 0000) and the wasm32 build are unchanged, and the stream profile does
  not compile `storage::ingest`.
