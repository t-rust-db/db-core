# ADRs — db-core

Architectural Decision Records for `db-core`: one file per significant,
hard-to-reverse design decision (a chosen data representation, a crate
boundary, a divergence from a sibling engine's design, etc.) -- not a
log of routine changes.

## Naming convention

`NNNN-short-title.md`, numbered sequentially starting at `0001`, title in
kebab-case. Mirrors [sqlite-rs's `adr/`](../../../sqlite-rs/.openspec/adr/)
(e.g. `0009-zero-unsafe.md`), so the two repos' ADR indices read the same
way if ever compared side by side.

## When to add one

Write an ADR when a decision would be expensive to reverse or non-obvious
to a future reader -- e.g. "why does `Join`'s condition stay two
`String`s instead of becoming `Option<(String, String)>` when `CROSS
JOIN` was added" or "why NULL-safe join-key equality lives in each
caller's key conversion rather than in `sql-join` itself" (see
`sql-join::semantics`'s module doc for that reasoning as it stands today
-- promote it here if the decision is later revisited or contested).

This directory is currently empty: no ADR has been written yet.
