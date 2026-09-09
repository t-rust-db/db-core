# db-storage

All physical storage for t-rust-db, structured as one feature-gated
module per execution mode (`row`/`column`/`stream` — see `db-core`'s
[ADR 0006](https://github.com/t-rust-db/db-core/blob/main/.openspec/adr/0006-storage-consolidation-into-db-storage.md)).
Today, only `column` is real.

## `column`

A `Vfs`/`VfsFile` abstraction (mmap-based, read-only) plus the Parquet
reader that consumes it — folded together from the standalone
`db-parquet` repo (`#4`) so `column-rs` depends on one crate instead of
two separately-versioned ones.

### Why a `Vfs` trait

`column-rs` originally had its own private `mmap.rs` wrapping `memmap2`.
`db-storage` pulls that into a shared, swappable seam: a trait, not a
concrete file type, so tests can run against fake storage without
touching disk.

**Note:** sqlite-rs's own VFS (`row::vfs`, migrated from `db-core` per
ADR 0006, `db-core#39`) is a **separate trait**, not this one — see
`db-core`'s ADR 0003 for why (mmap-safety vs. pread-only under
concurrent mutation, and `#![forbid(unsafe_code)]` vs. this crate's one
documented `unsafe`). Don't assume `column`'s `Vfs` is "the" storage
trait — `row::vfs::Vfs` is a distinct, non-`pub use`d trait at
`db_storage::row::vfs::Vfs`.

### What's here

- `column/vfs.rs` — the `Vfs` and `VfsFile` traits, plus `MmapRegion`, a
  read-only byte view that's either an OS mmap or an owned in-memory
  buffer.
- `column/posix.rs` — `PosixVfs` / `PosixFile`, backed by `std::fs::File`.
- `column/mmap.rs` — the `memmap2` call, isolated to one function. This
  is the only place in the crate using `unsafe` (required by
  `memmap2`'s `Mmap::map` contract).
- `column/memory.rs` — `MemoryVfs` / `MemoryFile`, an in-memory backend
  for tests: no filesystem involved, so callers can seed exact byte
  layouts.
- `column/parquet/` — a zero-dependency-on-Arrow Parquet reader
  (footer/thrift parsing, page decoding, nested/repeated field
  reconstruction, snappy/zstd decompression). Consumes raw bytes handed
  to it by `column::vfs::Vfs`, doesn't call it directly.

### Usage

```rust
use db_storage::{PosixVfs, Vfs, VfsFile, ParquetFile};

let vfs = PosixVfs;
let file = vfs.open(std::path::Path::new("data.parquet"))?;
let mapped = file.mmap()?; // Deref's to &[u8]
let parquet = ParquetFile::open(&mapped)?;
```

Swap in `MemoryVfs` in tests to avoid touching disk:

```rust
use db_storage::MemoryVfs;

let vfs = MemoryVfs::new();
vfs.insert("data.parquet", some_bytes);
let file = vfs.open(std::path::Path::new("data.parquet"))?;
```

## `row`

sqlite-rs's storage stack, migrated in unchanged in shape (`db-core#39`,
`#16`, `#17`; epic [`#1`](https://github.com/t-rust-db/db-storage/issues/1)):
`row::vfs` (locking, WAL/`-shm` access, with `sql-sys`'s `fcntl` folded
in as a private submodule — its only consumer), `row::pager` (page
cache, WAL, rollback journal, freelist), `row::header` (database header
parsing), `row::record` (varint/serial-type/record decoding),
`row::btree` (table/index b-tree read+write paths, `sqlite_master`
write helpers), `row::schema` (DDL reader), `row::integrity`
(`PRAGMA integrity_check`/`quick_check`), `row::format` (shell-parity
value rendering — an independent sibling, not schema-coupled; checked
before merging it in). Feature-gated behind `row` (off by default,
same pattern as `column`).

## `stream` (not yet started)

Log-format storage — tracked in
[`#3`](https://github.com/t-rust-db/db-storage/issues/3).
