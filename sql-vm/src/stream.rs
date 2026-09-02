//! `StreamExecutor`: the push-driven query VM for live/unbounded sources --
//! one of `sql-vm`'s three executors (see crate root docs).
//!
//! **Not yet implemented.** The intended shape (see `projects/database-rs/
//! unified-vm-vision.md` in the my-brain notes this was designed from):
//! a `CompiledExpr` shared with [`super::batch`] so the same filter logic
//! evaluates one record at a time (`eval_record(&Record) -> bool`) for low
//! latency, or micro-batches a buffer and calls into `batch`'s own
//! `eval_batch` for throughput -- not a separate execution engine
//! reimplementing filter/aggregate logic from scratch. Primary consumer:
//! loglume (live log tailing from files, Docker, journald, K8s), not
//! column-rs, which reads bounded Parquet files and has no live-append
//! source.
//!
//! [`super::batch`] is the reference for what "done" looks like here.
