# OpenSpec — db-core

Specifications, architectural decisions, and design documents for
`db-core` (`sql-types`, `sql-expr`, `sql-parser`, `sql-join`) — the shared
SQL frontend and join infrastructure multiple t-rust-db engines
(`column-rs`, and eventually others) depend on.

This directory is scaffolding: `adr/` and `specs/` are empty so far. No
content is invented here ahead of an actual decision or spec being
written -- see each subdirectory's own README for what belongs in it and
when to add the first entry.

## Grammar

`db-core`'s own SQL grammar does **not** live here -- it lives in the
separate `t-rust-db/grammar` repo, alongside this repo (`grammar/column-rs.ebnf`),
since it documents `sql-parser`'s accepted syntax and is meant to be
compared side by side with sibling engines' grammars in that same repo.
Any change to `sql-parser`'s grammar must update that file in the same
PR/commit (its own maintenance rule).

## Reference

Modeled on [sqlite-rs's `.openspec/`](../../sqlite-rs/.openspec/README.md)
for directory layout and naming conventions -- that repo's `adr/` and
`specs/` are the fuller example to follow once `db-core` accumulates its
own decisions and specs.
