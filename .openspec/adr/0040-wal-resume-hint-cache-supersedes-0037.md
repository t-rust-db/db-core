# ADR 0040: `Pager`-cached WAL resume hint supersedes ADR-0037's per-flush rescan

> Moved from t-rust-db/tools `sqlite-rs/.openspec/adr/0035-wal-resume-hint-cache-supersedes-0026.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.

Date: 2026-08-29

Status: Accepted

Supersedes: ADR-0037 ("Alternatives rejected" — caching a `WalWriter`
handle/resume state across flushes)

## Context

#640 (follow-up from #635's profiling) measured `WalWriter::open_existing`'s
full read-and-rescan of `-wal` on every commit at ~6-7ms even against a
near-empty, freshly-created WAL file — a meaningful fraction of the commit
path's total cost, and exactly the scenario ADR-0037's own "Consequences"
section predicted as the trigger for revisiting the rescan-per-flush
trade-off: *"will need revisiting — most likely by tracking the resume
offset/checksum on `Pager` across calls, invalidated on mode switches — if a
long-lived WAL under sustained write load makes per-commit rescanning
measurably slow."*

ADR-0037 rejected caching a `WalWriter` handle on `Pager` "for now", citing
two problems that needed solving first: invalidating the cache on
`switch_wal_to_journal`/`switch_journal_to_wal` (which delete or recreate
`-wal`), and tolerating a torn/short file from a `Drop`-order crash or a
concurrent external writer between flushes.

## Decision

**Cache a small `WalResumeHint` (header, append offset, running checksum,
and the file size that state is only valid against) on `Pager`, not a whole
`WalWriter` handle.** `flush_wal_locked` hands its cached hint into
`WalWriter::open_existing`, which now takes an `Option<&WalResumeHint>`:

- If the `-wal` file's current size matches `hint.expected_size` *and* a
  cheap 32-byte re-read of the header matches `hint.header` byte-for-byte,
  the hint is trusted and the full read-and-rescan is skipped entirely —
  the writer resumes directly from the cached offset/running checksum.
- Any mismatch (a concurrent writer or checkpoint changed the file, a mode
  switch replaced it, a crash left it torn) falls back to exactly the
  full read + `last_valid_frame_state` rescan ADR-0037 already established,
  so correctness never depends on the cache being right — only commit
  latency does.

`Pager::wal_resume: Option<wal::WalResumeHint>` is populated after every
successful `WalWriter::sync`, mirroring the `wal_shm` handle cache (#437)
already on `Pager`: lazily populated, and reset to `None` at the same three
sites `wal_shm` is reset at — `switch_wal_to_journal`,
`switch_journal_to_wal`, and `recreate_wal_locked` (the #422 "`-wal`
vanished out from under this connection" recovery path) — since each of
those deletes or recreates the underlying file, making any cached hint
stale by construction.

This resolves ADR-0037's rejected-alternative concerns without needing a
`WalWriter` handle's own lifetime managed across calls: the hint is `Copy`
data, not a live file handle, so there is no fd to keep valid across a mode
switch, and the size+header check — not trust in the cache's own
invalidation completeness — is what makes a torn file or concurrent writer
safe to resume past.

## Alternatives rejected

- **Cache the whole `WalWriter` (ADR-0037's original rejected option)**:
  still avoids managing an fd's lifetime across mode switches more cleanly
  than a plain data hint would — a cached handle would need its own
  re-validation logic duplicating what the hint's size/header check already
  does, for no benefit over caching just the state the handle would
  otherwise re-derive from a rescan anyway.
- **Trust the file size alone, skip the header re-read**: rejected — sizes
  can coincide across a generation change this cache wasn't told about
  (e.g. an external checkpoint truncates `-wal` back to header-only, then a
  fresh writer's own frames happen to grow it back to the same total length
  the old generation had), which would resume against the wrong
  salts/checksum chain and silently corrupt the file. The extra 32-byte
  read is O(1), not O(WAL size), so it costs nothing measurable while
  closing that gap.
- **Track `mxFrame` from `-shm` instead of a `Pager`-local hint**: rejected
  — `mxFrame` publication is a best-effort, non-atomic `pwrite` (ADR-0037's
  own accepted residual risk), and a torn read of it would be a worse
  invalidation signal than the on-disk `-wal` file's own actual size, which
  is exactly what a writer is about to append onto regardless.

## Consequences

- A commit against an already-warm cache costs one `size()` stat plus one
  32-byte header read instead of reading and rescanning the entire `-wal`
  file — the dominant cost #640 measured is gone for the common case of
  consecutive commits from the same long-lived `Pager`.
- The full rescan path (and its cost) is unchanged and still exercised
  automatically whenever the hint can't be trusted, so a first commit after
  `Pager::open`, a mode switch, or a concurrent writer's interleaved commit
  pays exactly what ADR-0037 always charged — no new failure mode, only a
  narrower set of calls that pay it.
- `Pager` now carries one more `Copy` field (`wal_resume`) alongside
  `wal_shm`, with the same three invalidation sites — a future change to
  either cache's invalidation logic should double-check the other still
  agrees, since they're deliberately kept in lockstep rather than merged
  into one struct (the `-shm` handle is a live resource with its own
  lifetime; the resume hint is plain data with none).
