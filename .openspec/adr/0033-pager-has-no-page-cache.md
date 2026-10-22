# ADR 0033: `Pager` has no page cache — repeated-seek workloads pay full I/O every row

> Moved from t-rust-db/tools `sqlite-rs/.openspec/adr/0022-pager-has-no-page-cache.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.

Date: 2026-08-21
Status: Superseded by [0028](0038-pager-page-cache-landed.md)

## Context

#310/#317 measured the tier-1 `join` bench scenario (`sqlite-rs's former tests/performance/
engine.rs`) at 14-16x slower than the pinned oracle, and attributed it —
provisionally — to "per-row VDBE interpreter dispatch overhead
compounding across two cursors." Profiling that hypothesis (the actual
scope of #317) instead of accepting it found a different, more
consequential root cause.

`EXPLAIN` for the join scenario shows a correct, already-optimal shape:
one `SeekRowid` per outer row against `bench_lookup`'s rowid index — not
a #128-style accidental full scan. Comparing per-row instruction counts
against `full_scan` (which has a *better* ratio, ~3.4x) rules out sheer
dispatch-loop overhead as the explanation: `full_scan`'s per-row body
(`Rowid` + 4×`Column` + `RealAffinity` + `ResultRow` = 7 instructions) is
*larger* than `join`'s per-row body (`Column` + `SeekRowid` + `Rowid` +
2×`Column` + `ResultRow` = 6 instructions), yet `join` is ~4x worse. More
instructions but a better ratio, fewer instructions but a worse ratio —
dispatch count alone doesn't explain the gap.

The actual difference: `full_scan` walks `Next` sequentially (crossing
into a new page only once every many rows, amortizing page reads), while
`join`'s `SeekRowid` re-descends the b-tree from the root on *every*
outer row. Tracing that descent (`TableCursor::seek`, `src/storage/row/btree.rs`)
into the page-read path reveals the real problem:

- `VfsPageSource::read_page`/`Pager::read_page` (`src/storage/row/vfs/page_source.rs`,
  `src/storage/row/pager.rs`) call `file.read_at(&mut buf, offset)` — a fresh
  syscall — and allocate a fresh `Vec<u8>`, **unconditionally, on every
  single call**, for every page number, every time.
- There is no cache anywhere in the read path. `Pager::dirty` (uncommitted
  writes) and `Pager::wal_pages` (the WAL overlay, read once at `open`)
  are the *only* two `HashMap`s in `Pager`, and neither one is a cache of
  already-read *physical file* pages — they hold different, disjoint
  data (uncommitted local writes; the WAL's own overlay), consulted
  before falling through to a real, uncached `source.read_page` call.

For a 53MB single-table b-tree with a root + a couple of interior
levels, a `SeekRowid`-per-row workload re-reads and re-allocates those
same few root/interior pages from disk hundreds of thousands of times —
once per outer row — when a real cache would serve every one of those
after the first as a `HashMap` lookup and a cheap clone. SQLite's own
pager caches pages precisely for this reason (`cache_size` pragma,
default 2000 pages) — the gap here isn't a missing optimization SQLite
also lacks; it's a foundational piece of the pager this codebase never
built.

This also reframes #310's `group_by_agg` finding partially:  #316
already fixed that scenario's *algorithmic* problem (sorting when an
index made it unnecessary), but any remaining gap on scenarios doing
repeated non-sequential page access — `point_lookup` in a loop,
correlated subqueries (#303/sqlite-rs ADR-0021's `correlated_subquery` bench
scenario), any join, `IN`-subquery ephemeral probing against a
large-enough index — will show the same shape, because they all funnel
through the same uncached `read_page`.

## Decision

**Do not build the page cache in this ticket.** #317's own acceptance
criteria was profiling, not a fix — "needs a profiling pass before any
concrete fix can be proposed" — and the fix that's actually indicated
(a real pager-level page cache) is a meaningfully-sized, cross-cutting
piece of work that deserves its own scoped ticket, design review, and
test plan rather than being folded into a "why is join slow" ticket as
an afterthought.

**File the follow-up as a dedicated `Pager` page-cache ticket**, scoped
as follows:

1. **Where:** `Pager` (`src/storage/row/pager.rs`), not `VfsPageSource`. `Pager` is
   the read path every real query goes through (`dump::open`, the CLI
   binary, the tier-1/tier-2 benches) — `VfsPageSource` is a lighter
   helper a few tests use directly and can gain the same treatment
   later if warranted, but isn't the priority.
2. **Shape:** a new `page_cache: RefCell<PageCache>` field on `Pager`.
   `RefCell`, not a plain field, because `PageSource::read_page(&self,
   ...)` takes `&self` — populating/touching an LRU on a read requires
   interior mutability, the same pattern sqlite-rs ADR-0017 already established
   for a writable `Pager` shared as `Rc<RefCell<Pager>>`.
3. **What gets cached:** only pages that came from `self.source
   .read_page(page_num)` — the physical-file fallback branch, reached
   *after* `dirty`/`wal_pages` have both already missed. Never cache
   (or serve from cache) a page found in `dirty` or `wal_pages` — those
   two are already correct, disjoint, and must stay the authoritative
   answer for their respective page numbers.
4. **Invalidation:** the only place a physical page's on-disk bytes can
   become stale relative to a cached copy is `Pager::get_page_mut`
   (dirty-tracking write path, #166). `get_page_mut(page_num)` must
   evict `page_num` from `page_cache` at the moment it's called (before
   `dirty` shadows it) — simpler than trying to keep a cached copy in
   sync with in-flight writes, and correct because `dirty`'s own
   shadow-before-cache-before-source read order (already in place)
   means the evicted slot is never consulted again until the *next*
   post-flush read repopulates it with the actual (now-current) bytes.
5. **Bound:** an LRU, not an unbounded map — capacity on the order of
   SQLite's own `cache_size` default (2000 pages) is a reasonable
   starting point, exposed as a `Pager::open`-time parameter or a named
   constant near the top of `pager.rs` (matching #269's
   `MAX_EPHEMERAL_ROWS`-style precedent for a documented, deliberate
   cap) rather than hard-coded invisibly.
6. **No new dependency.** `[dependencies]` in `Cargo.toml` is
   deliberately minimal today (`nix`, `thiserror`) — `hashlink` (which
   ships an `LruCache`) is already in the dependency graph, but only as
   a transitive dev-dependency (via `rusqlite`, for the tier-1 bench),
   not vetted/available to `src/`. The follow-up should hand-roll a
   small bounded LRU (a `HashMap<u32, Vec<u8>>` plus an access-order
   `VecDeque`/generation counter is enough) rather than promote a new
   production dependency through this codebase's supply-chain vetting
   process for what's a genuinely small piece of logic.
7. **Return-type question, explicitly deferred:** `PageSource::
   read_page` returns `Vec<u8>` (an owned, freshly-allocated buffer) —
   a cache hit under that signature still pays one clone (a memcpy, no
   syscall) to hand the caller its own `Vec<u8>`. Eliminating even that
   clone (e.g. changing the trait to return `Rc<[u8]>`) is a larger,
   crate-wide signature change touching every `PageSource` call site
   (`src/storage/row/btree.rs`, `src/storage/row/btree/index.rs`, the schema reader, ...) — the
   follow-up should land the `Vec<u8>`-cloning cache first (it already
   removes the actual disk I/O, the dominant cost) and treat the
   allocation-free version as a distinct, separately-measured
   optimization only if the clone itself still shows up as significant
   after the cache lands.

## Alternatives rejected

- **Fix `join` specifically** (e.g. a query-plan-level "cache the last
  N seeked pages" trick scoped to the join codegen path): rejected —
  the actual defect is in the shared page-read path every scan/seek
  goes through, not something specific to joins. A join-scoped
  workaround would leave `point_lookup`-in-a-loop, correlated
  subqueries, and any future repeated-seek pattern with the identical
  bug, unfixed.
- **Treat this as confirming the original "VDBE dispatch overhead"
  hypothesis and file the tier-3 bytecode-dispatch bench #111 already
  calls for**: rejected — the instruction-count comparison above
  (`full_scan` vs `join`) already falsifies dispatch-count as the
  driver. A dispatch-overhead bench would measure the wrong thing;
  filing it would spend #111's tier-3 slot on a hypothesis this
  profiling pass just ruled out.
- **Add the cache to `VfsPageSource` instead of/as well as `Pager`**:
  deferred, not rejected outright — `Pager` is the path every real
  query exercises; `VfsPageSource` mostly serves direct test helpers.
  Worth doing once `Pager`'s cache is proven, not before.

## Consequences

- `join`'s (and, by the same mechanism, any repeated-`SeekRowid`
  workload's) tier-1 ratio should improve substantially once the cache
  lands — root/interior pages that are re-descended into on every outer
  row become `HashMap` hits instead of syscalls after the first row.
- This also predicts (testably, once the cache exists) that
  #303/sqlite-rs ADR-0021's `correlated_subquery` bench scenario improves for the
  same reason, independent of whether the memoization follow-up that
  ADR scoped ever lands — the page cache and the value-level memoization
  are two different, complementary optimizations at different layers.
- Follow-up ticket to file: "perf: add a bounded page cache to `Pager`'s
  read path" — scoped per the Decision above (a). `Pager` only, (b) a
  hand-rolled bounded LRU keyed on physical-file pages only, (c)
  eviction tied to `get_page_mut`, (d) no new dependency, (e) the
  `Vec<u8>`-cloning version first, `Rc<[u8]>` as separately-scoped
  future work.
