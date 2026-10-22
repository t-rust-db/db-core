# Spike: a zstd decoder of our own (db-core#697)

Research spike for [#697](https://github.com/t-rust-db/db-core/issues/697):
can a decoder that writes straight into a pre-sized output beat `ruzstd` on
Parquet page decoding? No production code changes.

Outcome: yes, 1.8-2.5x `ruzstd`'s speed on real pages, about libzstd's. That
became #700, now `src/storage/parquet/compression/zstd.rs`. This directory
keeps the prototype it grew from.

```sh
make help   # list targets
make check  # prototype == input on libzstd frames (debug, fast)
make run    # prototype vs ruzstd (release, prints MB/s)
```

- **`prototype.rs`**: the decoder. No ring buffer, no `Read`, no dictionaries,
  checksum skipped.
- **`01_zstd_decoder.rs`**: the harness. Needs the `zstd` CLI.

The original numbers came from every zstd page of real DuckDB/pyarrow Parquet
files, extracted through a throwaway `RowGroupReader::spike_zstd_pages` hook
and a `zstd_spike` example. Those are not kept (they patched production code);
see the earlier commits on the `spike/697_zstd_decoder` branch
before the rebase. The harness here uses synthetic page-like frames instead.
