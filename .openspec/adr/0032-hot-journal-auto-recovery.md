# ADR 0032: Hot-journal recovery is automatic in `Pager::open`, not a separate opt-in call

> Moved from t-rust-db/tools `sqlite-rs/.openspec/adr/0016-hot-journal-auto-recovery.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.

**Status:** Accepted · **Date:** 2026-08-18

## Context

V1 (003-pager Requirement 1) made `Pager::open` refuse to open a database
with a hot rollback journal, returning `PagerError::HotJournal` — safe, but
useless for any caller that actually wants to keep working with the
database (every real `sqlite3` client just recovers and proceeds). #172
adds the write path (`Pager::flush` journals pre-transaction page content)
and needed to decide what `open` does once recovery is possible.

## Decision

`Pager::open` recovers a detected hot journal automatically and
transparently — replaying its checksum-valid records into the main file,
truncating back to the journal's recorded pre-transaction page count, and
deleting the journal — before proceeding with the rest of `open`. There is
no separate `Pager::recover()` entry point a caller must remember to call
first; `PagerError::HotJournal` is retained only for a `-journal` file
whose header/records don't parse (a genuinely corrupt journal recovery
can't safely act on), not for the ordinary "found a hot journal" case.

This matches stock `sqlite3`'s own behavior (opening a database with a hot
journal transparently rolls it back, per `pager.c`), which is exactly what
`sqlite-rs:tests/corpus/journal_interop_test.rs::our_journal_recovers_through_stock_sqlite3`
and `sqlite-rs:tests/tiers/tier0.rs::t0_hot_journal_recovers_committed_state` /
`src/storage/row/pager.rs`'s `hot_journal_fixture_recovers_committed_state` both lean
on: the acceptance criterion is interop with a real `sqlite3`'s observable
behavior, not just its on-disk format.

## Alternatives rejected

- **Keep V1's refuse-and-explain, add a separate `Pager::recover_hot_journal`
  callers must invoke first.** Rejected: every real caller that hits
  `HotJournal` would immediately call it and retry `open` — the two-step
  dance adds an easy-to-forget step with no safety benefit, since recovery
  from a checksum-valid journal is unconditionally safe to run before any
  page is read (the same ordering `open` already used for hot-journal
  *detection*).
- **Return a value from `open` indicating "recovered from a hot journal"**
  (e.g. `Ok((Pager, RecoveryOutcome))`) instead of a plain `Result<Self,
  PagerError>`. Rejected as unnecessary API surface for this ticket's
  scope: nothing in the codebase yet needs to distinguish "opened cleanly"
  from "opened after recovering" — revisit if a caller (e.g. a future
  `PRAGMA journal_mode` reporting path) actually needs to know.

## Consequences

- `PagerError::HotJournal`'s meaning narrows: it no longer means "a hot
  journal exists", it means "a `-journal` file exists, is confirmed hot by
  its magic, but its header/records don't parse enough to recover from
  safely" (surfaced as `PagerError::Journal` in the actually-implemented
  code — `HotJournal` itself is now unreachable from `Pager::open` and
  kept only as documentation of the superseded V1 contract; a future
  ticket may remove it once nothing else references it, per this repo's
  ADR uncited-carve-out convention).
- Every test that previously asserted `Err(PagerError::HotJournal { .. })`
  against the checked-in `journalstates/hot_journal.db` fixture had to move
  to a scratch-temp-dir copy first, since recovery now mutates the main
  file and deletes the journal in place — asserting against the
  checked-in path directly would corrupt the fixture on every test run.
