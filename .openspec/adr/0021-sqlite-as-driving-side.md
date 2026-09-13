# ADR 0021: SQLite as the driving side of a cross-mode join

## Status

Accepted (#368/#371, v2 follow-up to epic #317 / ADR-0019)

## Context

ADR-0019 fixed the cross-mode join's build/probe assignment as a planner
rule, not a cost decision: the SQLite lookup side is always `HashBuild`
(whole table materialized up front, bounded by construction), the
stream/batch side always probes (potentially unbounded — a live tail or a
large `.parquet` scan). `engine::resolve::resolve_sides`
(`src/engine/resolve.rs`) enforced this by rejecting any query that wrote
the SQLite table as `FROM` and the stream table as `JOIN`, with
`ErrorKind::Unsupported`.

The restriction was a **grammar-position rule**, not a capability gap:
`log JOIN hosts` worked but `hosts JOIN log` did not, even though the join
condition and the two sources are otherwise identical. A user writing SQL
naturally (`SELECT * FROM hosts JOIN log ON ...`) hit this as a surprising
rejection rather than a documented limitation.

## Decision

Make `resolve_sides` **order-independent**: detect which of the two
`TableRef`s is the stream table `log` and which is the SQLite lookup table
regardless of which one is written as `FROM` vs `JOIN`. `HashBuild` still
always takes the SQLite side (ADR-0019's planner rule is unaffected — this
ADR only relaxes *which grammar position* the SQLite table may appear in,
not which side builds).

This ADR's first draft assumed `codegen::batch::compile_join` needed no
change ("it already receives an unordered `Select` and picks build/probe
itself"). Implementation showed that's wrong: `run_join_segments`'s
calling convention fixes which physical input feeds `.build` (the
SQLite-scanned `Batch`) and which feeds `.probe` (the stream segments).
`compile_join`'s existing position-based assignment (the `JOIN` target
always builds) silently produces a *wrong* plan once the SQLite table is
written as `FROM` — it would try to build on the stream side, exactly the
unboundedness ADR-0019 exists to avoid. `compile_join_build_side(select,
build_table)` is added alongside the existing `compile_join`: it takes an
explicit table name and assigns build/probe roles by identity, falling
back to today's position-based behavior when the name matches the `JOIN`
target. This keeps `compile_join`'s existing callers and tests
byte-for-byte unchanged (no `build_table` argument, same position-based
result).

### The LEFT JOIN restriction

A `LEFT JOIN`'s probe side is always the side whose unmatched rows are
kept (`JoinKind::Left`'s `HashProbe`), and probe execution is fixed to the
stream segments by `run_join_segments`'s calling convention. So `hosts
LEFT JOIN log` (keep all `hosts` rows, null-fill unmatched `log` columns)
would need the *build* side (SQLite) to be the one whose unmatched rows
are kept — a `RIGHT JOIN`-shaped algorithm, which isn't implemented.
`compile_join_build_side` rejects this specific combination
(`from_builds && join.op != JoinOp::Inner`) with
`PlanError::UnsupportedJoinKind`; `resolve_sides` gives it a clearer
message pointing at the fix (write it as `log LEFT JOIN hosts`, or use
`INNER JOIN`). `log LEFT JOIN hosts` (today's only supported `LEFT JOIN`
shape) is unaffected.

Concretely: `resolve_sides` stops asserting `from_name == stream::TABLE`
and instead checks `{from_name, join_name} == {stream::TABLE,
sqlite_table}` in either order, returning which one is the lookup table,
and rejects `LEFT JOIN` when the SQLite table is `FROM`. The one-JOIN,
one-lookup-table v1 scope is otherwise unchanged.

### What does not change

- Still exactly one `JOIN`, still exactly one stream table, still exactly
  one SQLite lookup table.
- `EXPLAIN` (`explain_plan`, `src/engine/resolve.rs`) needs no change: it
  derives its plan-node labels from resolved table identity via its own
  closure, independent of `compile_join`/`compile_join_build_side` and
  independent of grammar position.
- `compile_join`'s existing callers (its own unit tests in
  `src/codegen/batch.rs`) are unaffected — `compile_join_build_side` is a
  new function, not a signature change.

### Rejected alternative: reject `hosts JOIN log` at parse time with a
rewrite hint

Considered auto-rewriting `hosts JOIN log` to the canonical `log JOIN
hosts` form and running the existing code unchanged. Rejected: silently
rewriting a user's query (even one that is semantically equivalent) means
`EXPLAIN` and error messages no longer reflect what the user wrote, which
ADR-0000 §(c)'s spirit (no silent reinterpretation) argues against.
Assigning build/probe roles by identity inside `compile_join_build_side`
achieves the same order-independence without rewriting anything the user
wrote.

## Structure

- `src/codegen/batch.rs` — `compile_join_impl` is the shared body of both
  `compile_join` (unchanged signature, `build_override: None`) and the new
  `compile_join_build_side(select, build_table)` (`build_override:
  Some(build_table)`); it assigns build/probe roles by identity when an
  override is given, and rejects `LEFT JOIN` combined with a `FROM`-side
  override as `PlanError::UnsupportedJoinKind`.
- `src/engine/resolve.rs` — `resolve_sides` becomes order-independent and
  rejects `LEFT JOIN` with the SQLite table as `FROM` with a clearer
  message; `run_query` calls `compile_join_build_side(&select,
  &lookup_table)` instead of `compile_join`.
- `tests/unit/engine_resolve_public_api_test.rs` —
  `rejects_sqlite_as_the_driving_side` is replaced by
  `accepts_sqlite_as_the_driving_side_for_inner_join` and
  `rejects_left_join_with_sqlite_as_the_driving_side`.
- No parser change.

## Consequences

- Removes a rejection surface that had no principled reason beyond
  "not implemented yet" for `INNER JOIN` — closes a real ergonomics gap.
- `LEFT JOIN` with the SQLite table as `FROM` stays rejected, now for a
  documented, correct reason (would need `RIGHT JOIN` semantics) rather
  than a blanket v1-scope rejection.
- Slightly more logic in `compile_join`'s shared implementation (an
  identity-based build/probe swap plus one new rejection branch), but no
  new runtime cost for either call path: the join plan produced for a
  given physical assignment is identical either way.
- Does not touch the "two unbounded streams joined" or "key-restricted
  materialization" limits (ADR-0022, ADR-0023) — those remain separately
  out of scope.
