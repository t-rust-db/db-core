# ADR 0038: `Pager` page cache landed — supersedes ADR-0033's problem statement

> Moved from t-rust-db/tools `sqlite-rs/.openspec/adr/0028-pager-page-cache-landed.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.

Date: 2026-08-25

Status: Accepted

## Context

ADR-0033 documented that `Pager`'s read path had no page cache, and
scoped a follow-up ticket to build one. That follow-up work has since
landed:

- #320 added a bounded LRU `PageCache` to `Pager`
  (`src/storage/row/pager.rs:111-121`, fields at `src/storage/row/pager.rs:202-215`), wired into
  the single `read_page` path (`src/storage/row/pager.rs:1062-1078`) that every
  cursor type — table scans, index cursors, `SeekRowid` — funnels
  through.
- #457 replaced the default hasher with a cheap custom one
  (`BuildHasherDefault<PageNumHasher>`) for the hot get/insert path.
- #459 changed cache hits to return a `Rc<[u8]>` clone (a refcount
  bump) instead of a `Vec<u8>` memcpy — going further than ADR-0033's
  Decision §7, which had explicitly deferred that as separately-scoped
  future work.
- Invalidation is wired into the write path (`get_page_mut`,
  freelist/vacuum paths, `src/storage/row/pager.rs:370-373`, `:707`) exactly as
  ADR-0033's Decision §4 specified.

So ADR-0033's premise — that repeated-seek workloads (`SeekRowid` in a
loop, correlated subqueries, `join`) pay a fresh syscall + allocation
on every page re-read — no longer holds. `Pager::read_page` now serves
repeat reads of the same physical page as a `HashMap` (LRU) hit.

This was discovered while re-investigating why the `join` tier-1
benchmark (`sqlite-rs's former tests/performance/engine.rs:84-87`) is still ~7.4x slower
than the oracle: the obvious next step was to check whether the cache
ADR-0033 called for actually exists, and it does, fully matching the
scoped design. ADR-0033 is accurate as a historical record of the
investigation that led to the cache, but is no longer an accurate
description of current `Pager` behavior — hence this superseding ADR
rather than editing ADR-0033 in place (it's cited from `CHANGELOG.md`
and the ADR index, so it's frozen per this repo's ADR convention).

## Decision

Mark ADR-0033 as superseded by this ADR. No code change accompanies
this ADR — it is a documentation correction. The `join` benchmark's
remaining ~7.4x gap is **not** explained by a missing page cache and
needs its own, separate investigation (tracked as a follow-up, not
scoped here).

## Alternatives rejected

- **Edit ADR-0033 in place to mark it resolved**: rejected — ADR-0033
  is cited by `CHANGELOG.md` (three call sites), which makes it frozen
  under this repo's ADR convention (uncited ADRs may be corrected in
  place; cited ones must be superseded, not edited).

## Consequences

- Future investigations into seek-heavy or repeated-page-read
  benchmarks (`point_lookup`-in-a-loop, correlated subqueries, `join`)
  should not re-attribute slowness to a missing `Pager` page cache —
  that cache exists and is wired into every `PageSource` consumer.
  Remaining gaps in those benchmarks have a different root cause,
  still to be found.
- `VfsPageSource` (`src/storage/row/vfs/page_source.rs`) still has no cache — this
  remains correctly out of scope, per ADR-0033's Alternatives Rejected
  §3: it's a test helper, not the real query path.
