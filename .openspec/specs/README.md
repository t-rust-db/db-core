# Specs — db-core

System specifications for `db-core`: numbered directories, each holding a
`spec.md` that describes a slice of behavior in Given-When-Then scenarios,
with `**Implementation:**`/`**Tests:**` links back to source.

## Naming convention

`NNN-name/spec.md`, numbered starting at `001`. Mirrors
[sqlite-rs's `specs/`](../../../sqlite-rs/.openspec/specs/) (e.g.
`002-parser/spec.md`) for format and cross-linking conventions -- see that
repo's specs for the fuller worked example of the requirement/scenario
format, and its `README.md` for how it tracks spec-to-implementation
coverage.

This directory is currently empty: no spec has been written yet for
`db-core`. Candidates once this crate's surface stabilizes further:
a parser spec (`sql-parser`'s grammar -- though the grammar itself stays
in the separate `t-rust-db/grammar` repo, not duplicated here) and a join
spec (`sql-join`'s `JoinKind`/`should_emit` semantics and the hash table's
correctness invariants).
