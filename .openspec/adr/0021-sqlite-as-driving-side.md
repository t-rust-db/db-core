# ADR 0021: SQLite as the driving side of a cross-mode join

## Status

Proposed (#368/#371, v2 follow-up to epic #317 / ADR-0019)

## Context

ADR-0019 fixed the cross-mode join's build/probe assignment as a planner
rule, not a cost decision: the SQLite lookup side is always `HashBuild`
(whole table materialized up front, bounded by construction), the
stream/batch side always probes (potentially unbounded — a live tail or a
large `.parquet` scan). `engine::resolve::resolve_sides`
(`src/engine/resolve.rs:59-110`) enforces this by rejecting any query that
writes the SQLite table as `FROM` and the stream table as `JOIN`, with
`ErrorKind::Unsupported` and a message pointing at this restriction.

The restriction is a **grammar-position rule**, not a capability gap: today
`log JOIN hosts` works but `hosts JOIN log` does not, even though the join
condition and the two sources are otherwise identical. A user writing SQL
naturally (`SELECT * FROM hosts JOIN log ON ...`) hits this as a surprising
rejection rather than a documented limitation.

## Decision

Make `resolve_sides` **order-independent**: detect which of the two
`TableRef`s is the stream table `log` and which is the SQLite lookup table
regardless of which one is written as `FROM` vs `JOIN`, then compile the
join exactly as today — SQLite side still always resolves to `HashBuild`,
stream side still always probes. This is a resolution-layer change only;
`codegen::batch::compile_join` (`src/codegen/batch.rs:1197`) is unaffected
since it already receives an unordered `Select` and picks build/probe
itself.

Concretely: `resolve_sides` stops asserting `from_name == stream::TABLE` and
instead checks `{from_name, join_name} == {stream::TABLE, sqlite_table}` in
either order, returning which one is the lookup table. The one-JOIN,
one-lookup-table v1 scope is otherwise unchanged.

### What does not change

- `HashBuild` still always takes the SQLite side (ADR-0019's planner rule
  is unaffected — this ADR only relaxes *which grammar position* the
  SQLite table may appear in, not which side builds).
- Still exactly one `JOIN`, still exactly one stream table, still exactly
  one SQLite lookup table.
- `EXPLAIN` (`explain_plan`, `src/engine/resolve.rs:187-222`) needs no
  change beyond the same order-independent lookup, since it already labels
  sides by resolved table identity, not by `FROM`/`JOIN` position.

### Rejected alternative: reject `hosts JOIN log` at parse time with a
rewrite hint

Considered auto-rewriting `hosts JOIN log` to the canonical `log JOIN
hosts` form and running the existing code unchanged. Rejected: silently
rewriting a user's query (even one that is semantically equivalent) means
`EXPLAIN` and error messages no longer reflect what the user wrote, which
ADR-0000 §(c)'s spirit (no silent reinterpretation) argues against. Making
`resolve_sides` genuinely order-independent is barely more code and avoids
the rewrite entirely.

## Structure

- `src/engine/resolve.rs` — `resolve_sides` becomes order-independent;
  update its doc comment and the `ErrorKind::Unsupported` rejection (now
  only fires for genuinely unrecognized table combinations, not for
  grammar position).
- `tests/unit/engine_resolve_public_api_test.rs` —
  `rejects_sqlite_as_the_driving_side` is repurposed into
  `accepts_sqlite_as_the_driving_side` (or a new test added alongside the
  existing `joins_warn_log_lines_against_a_sqlite_hosts_lookup_table`,
  written with the table order swapped).
- No `codegen::batch` change, no new opcode, no parser change.

## Consequences

- Removes a rejection surface that had no principled reason beyond
  "not implemented yet" — closes a real ergonomics gap.
- Slightly more parsing logic in `resolve_sides` (match against a set of
  two rather than assert a fixed order), but no new runtime cost: the join
  plan produced is identical either way.
- Does not touch the "two unbounded streams joined" or "key-restricted
  materialization" limits (ADR-0022, ADR-0023) — those remain separately
  out of scope.
