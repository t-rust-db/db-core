# ADR 0022: Windowed stream-to-stream joins

## Status

Accepted (#368/#372, v2 follow-up to epic #317 / ADR-0019) — implemented.

## Context

ADR-0019's cross-mode join fixes `HashBuild` on the SQLite lookup side
specifically *because* it is bounded by construction (a dimension table,
materialized whole once per query). The stream/batch side always probes
because it may be unbounded (a live tail via `storage::stream`'s
`Segment`/`TailSource` adapters, ADR-0018).

Joining two stream tables (two live tails) has no side that is bounded by
construction. Neither can be materialized whole up front the way a SQLite
dimension table can: a live tail has no end.

## Decision

Do not attempt an unbounded/unbounded hash join. Instead, scope stream-to-
stream joins to a **windowed join**, reusing the `SINCE`/`UNTIL` scope
clause `engine::stream` already parses (`Select.scope`, ADR-0018) —
required, not optional, since with no bounded side to hash-build from, the
window is what makes the join finite. An unwindowed stream-stream join
stays rejected as `ErrorKind::Unsupported`, with a message distinguishing
"needs a window" from any other rejection.

### Scope narrowed to stream-vs-stream; `.parquet` dropped

This ADR's first draft mentioned ".parquet batch source" as an alternative
driving side alongside two live tails. Investigation during
implementation found no existing adapter presenting a `.parquet` file as a
cross-mode join side (`engine::cross_mode::scan_table_as_batch` is
SQLite-only; Parquet support in this crate lives entirely in
`storage::column`, with no join-side wrapper). Building that adapter is
separate, uncosted work, so this ADR implements **stream-vs-stream only**
(two `StreamEngine`s) — the shape the issue's acceptance criteria actually
test for.

### The real blocker: self-join column disambiguation, not windowing

The windowing mechanism itself (see Structure) was the easy part. The hard
part, only discovered during implementation: both sides of this join are
literally the same table (`log`), so the query must be a **self-join**
(`FROM log AS a JOIN log AS b ON a.host = b.host`) to let `a.col`/`b.col`
disambiguate the two sides at all. But `parser::column::validate_select`
(the shared grammar validator every mode's SQL goes through, ADR-0002)
already resolves `alias.col` to `real_table.col` for *every* qualified
column, unconditionally. For an ordinary join (two different real tables)
this is invisible and correct; for a self-join, both aliases resolve to
the same real name (`log`), collapsing the only thing that distinguished
`a.host` from `b.host` into `log.host` twice — before `engine::resolve`
ever sees the parsed `Select`.

This is a genuine, general gap: self-joins were never exercised anywhere
in this crate before (no existing test joins a table to itself), so
nothing previously depended on `alias.col` surviving as written when two
aliases collide onto the same real table.

**Fix, in `parser::column::validate_select`:** detect when two aliases in
one `FROM`/`JOIN` resolve to the same real table name, and skip the
alias→real-name rewrite for exactly those aliases (leave `a.col`/`b.col`
as written, instead of collapsing both to `log.col`). This is narrow and
backward-compatible — it only changes behavior for a query shape
(self-join) that had zero prior test coverage — verified against the full
existing suite (1802 lib tests + every integration target, unchanged).

With that fix, `engine::resolve::alias_normalize` (new) rewrites the
`FromClause`'s two `TableRef`s from `log`/`log` to `a`/`b` before handing
the `Select` to `codegen::batch::compile_join` — since the `WHERE`/`ON`/
`SELECT`-list column qualifiers already say `a`/`b` (no longer collapsed),
`compile_join`'s existing table-name-based column classification tells
the two sides apart unmodified. No `codegen::batch` change needed here
(unlike ADR-0021's `compile_join_build_side`) — the `JOIN` target still
always builds, matching the existing stream/SQLite convention; only which
physical `StreamEngine` backs which alias is new, and that's resolved
positionally in `engine::resolve` (`FROM` alias = `left` engine, `JOIN`
alias = `right` engine), not by `codegen::batch`.

### Rejected alternative: nested-loop join with a memory cap

Considered joining unbounded sides via a bounded-memory nested loop (spill
to disk past a byte budget, similar in spirit to the stream ring's
eviction). Rejected for v1: this reintroduces a cost-based join strategy
ADR-0019 deliberately avoided (fixed planner rule, not cost heuristic), and
a spilling join is a substantially larger implementation than reusing the
existing windowing already available on `Select.scope`.

### Rejected alternative: require one side to be `.parquet` (batch), never
two live tails

Considered narrowing scope further to "one live tail + one bounded batch
file" only. Rejected as insufficiently useful even before the `.parquet`
adapter-doesn't-exist finding above: the windowed-join design already
gives two live tails a principled bound (the window), so excluding that
case would have bought no simplicity.

## Structure

- `src/engine/stream.rs`: `resolve_scope` (private → `pub(crate)`, no
  logic change) turns `Select.scope` into a `vm::stream::Scope`. New
  `StreamEngine::segments_in_range(columns, scope)` — the windowed
  counterpart to the existing unbounded `segments()`: bounds candidates by
  observed-time overlap (same rule `select_segments` already applies to an
  ordinary single-table query), returning every held segment unfiltered
  for `Scope::Lines`/`Bytes` (no fixed nanosecond edge).
- `src/parser/column.rs`: `validate_select`'s alias-collection loop
  defers insertion into the rewritten `aliases` map; a second pass counts
  how many aliases resolve to each real table name and excludes any real
  name claimed by more than one alias from the rewrite.
- `src/engine/resolve.rs` (new, alongside the existing stream/SQLite
  path): `resolve_stream_stream_sides` (validates exactly one `JOIN`, both
  sides literally `log`, both aliased with distinct aliases, and
  `select.scope.is_some()`); `alias_normalize` (rewrites both sides'
  `TableRefKind` to their alias, strips `scope` — mirroring
  `codegen::stream`'s own stripping before delegating to the batch
  planner); `materialize_segments` (loads and concatenates windowed
  segments into one `Batch` for the build side, the same shape
  `cross_mode::scan_table_as_batch` produces for a SQLite lookup table);
  `run_stream_stream_query`/`explain_stream_stream_plan` (the public
  entry points, mirroring `run_query`/`explain_plan`'s shape).
- No `codegen::batch` opcode change: the same `HashBuild`/`HashProbe`
  pair, `compile_join` (not `compile_join_build_side`) since the `JOIN`
  side already builds by the existing convention.
- Also fixed in this change (pre-existing, found via `make check-features`
  while verifying this branch): `engine::predicate`/`Engine::compile_predicate`
  were gated on `feature = "vm-batch"` alone, but need `codegen-batch` too
  (`storage-stream` implies `vm-batch` transitively without implying
  `codegen-batch`, same root cause as db-core#356) — now gated on both.

## Consequences

- Ships windowed stream-vs-stream joins; `.parquet`-as-driving-side is
  explicitly out of scope pending its own adapter (not costed here).
- The self-join alias fix in `parser::column` is a small, generally
  applicable correctness fix (any future self-join, in any mode, now
  keeps its aliases distinguishable) rather than a special case wired
  only into `engine::resolve`.
- Fully unbounded stream-stream joins (no window at all) remain out of
  scope; a spilling or approximate join strategy is a different
  engineering problem than anything in ADR-0019 or this ADR.
