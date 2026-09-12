# ADR 0022: Two unbounded streams joined

## Status

Proposed (#368/#372, v2 follow-up to epic #317 / ADR-0019)

## Context

ADR-0019's cross-mode join fixes `HashBuild` on the SQLite lookup side
specifically *because* it is bounded by construction (a dimension table,
materialized whole once per query). The stream/batch side always probes
because it may be unbounded (a live tail via `storage::stream`'s
`Segment`/`TailSource` adapters, ADR-0018).

Joining two stream tables — e.g. two live tails, or a live tail against a
`.parquet` batch source — has no side that is bounded by construction.
Neither can be materialized whole up front the way a SQLite dimension
table can: a live tail has no end, and even a large `.parquet` scan is a
resource commitment `engine::resolve` currently sidesteps entirely by
having the SQLite side available as the always-safe build side.

## Decision

Do not attempt an unbounded/unbounded hash join. Instead, scope stream-to-
stream joins to a **windowed join**: both sides bounded by the same
mechanism the stream engine already uses for aggregation windows
(`engine::stream`'s window/standing-query machinery, ADR-0018 phase 6),
requiring an explicit time window (e.g. a `WITHIN`/range-vector-style bound
already familiar from `count_over_time(...) RANGE ...`) on the join
condition or query. Within a window, both sides are finite (bounded by the
stream ring's retained segments for that time range,
`src/storage/stream/ring.rs`), so `HashBuild`/`HashProbe` applies
unchanged — build the smaller/left-bound side, probe the other, exactly as
today's lookup-table build/probe assignment, just with both sides sourced
from `storage::stream` instead of one being `engine::row`.

An unwindowed stream-stream join (no time bound at all) stays rejected as
`ErrorKind::Unsupported` — this ADR does not lift that restriction, it
narrows it to "unsupported without a window", which is a smaller, honestly
statable gap than "unsupported, full stop".

### Rejected alternative: nested-loop join with a memory cap

Considered joining unbounded sides via a bounded-memory nested loop (spill
to disk past a byte budget, similar in spirit to the stream ring's
eviction). Rejected for v1: this reintroduces a cost-based join strategy
ADR-0019 deliberately avoided (fixed planner rule, not cost heuristic), and
a spilling join is a substantially larger implementation than reusing the
existing windowed-aggregation machinery that already bounds a stream by
time.

### Rejected alternative: require one side to be `.parquet` (batch), never
two live tails

Considered narrowing scope further to "one live tail + one bounded batch
file" only. Rejected as insufficiently useful: the windowed-join design
above already gives two live tails a principled bound (the window), so
excluding that case buys no simplicity — the windowing mechanism is the
same either way.

## Structure

- `src/engine/resolve.rs` (or a new sibling module, e.g.
  `engine::resolve_windowed`, if the windowed variant's dispatch logic
  grows large enough to warrant separation — TBD at implementation time)
  gains a windowed-join path: require a window bound on the query, resolve
  both sides through `storage::stream`, hash-build the side the planner
  already prefers (smaller estimated cardinality within the window, or a
  fixed left/right rule mirroring ADR-0019 if cardinality isn't cheaply
  known).
- `codegen::batch::compile_join` needs no opcode change — same
  `HashBuild`/`HashProbe` pair, now fed by two `storage::stream` adapters
  instead of one stream + one row-engine-backed batch.
- New `ErrorKind::Unsupported` message distinguishing "no window on a
  stream-stream join" from today's generic rejection.

## Consequences

- Ships a real but narrower capability (windowed stream-stream join) rather
  than the full unbounded case, consistent with ADR-0019's "measured, not
  assumed" standard for scope decisions.
- Depends on the window-bound syntax already used for standing queries
  (ADR-0018 phase 6) being reusable in a join context — needs a grammar
  check as part of implementation, not assumed here.
- Fully unbounded stream-stream joins remain out of scope; if a real use
  case demands them, it needs its own ADR (a spilling or approximate join
  strategy is a different engineering problem than anything in ADR-0019 or
  this ADR).
