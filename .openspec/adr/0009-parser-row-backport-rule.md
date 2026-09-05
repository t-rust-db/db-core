# ADR 0009: While two copies of the row parser exist, Lab271/sqlite-rs leads and `parser::row` back-ports

> Source: `#84` — `parser::row` drifted ~200 lines ahead of the grammar
> ADR 0005 calls canonical. Amends ADR 0005 (which stays accepted).

## Status

Accepted, resolving `#84`. Retires when t-rust-db/sqlite-rs#17 deletes
sqlite-rs's `src/parser` and depends on `db_core::parser::row` — from then
on there is one parser and no rule is needed.

## Context

ADR 0005 makes sqlite-rs's grammar the canonical SQL surface for db-core.
`parser::row` was ported from it mechanically (`#23`). Since then two
features landed here first and never went back:

- inline window functions — `OVER (PARTITION BY … ORDER BY …)`,
  `ast::WindowDef` (`#74`/`#67`);
- `KEY` demoted from reserved keyword to a bare identifier via
  `Parser::expect_bareword_ci` (a partial fix for Lab271/sqlite-rs#696).

Meanwhile the org move (t-rust-db/sqlite-rs#1) decided the opposite
direction of authority for engine behaviour: **Lab271/sqlite-rs is leading
until it is archived** (sqlite-rs ADR-0039). Two copies with two
directions of authority is how drift becomes divergence.

## Decision

While both copies exist:

1. **Grammar changes land in Lab271/sqlite-rs first.** A `parser::row`
   change that Lab271 does not have is a back-port debt: it is filed there
   in the same PR that lands it here, and linked from the db-core ticket.
   The two existing debts are filed as Lab271/sqlite-rs#701 (window
   grammar) and #702 (`KEY` bareword).
2. **The drift check is the diff itself**, run before closing any
   `parser::row` ticket:

   ```sh
   diff -r <sqlite-rs>/src/parser <db-core>/src/parser/row
   ```

   The only tolerated difference is structural (`mod.rs`, `Span` living
   in `crate::parser` instead of `tokenizer.rs`). Anything else is either
   a pending Lab271 ticket or a bug.
3. **ADR 0005's direction of convergence is unchanged**: `column`/`stream`
   still converge toward `row`; this ADR only fixes which *copy* of `row`
   is authoritative while there are two.

## Consequences

- `#84`'s "empty diff" criterion transfers to Lab271/sqlite-rs#701/#702
  and is re-checked by t-rust-db/sqlite-rs#17 before the repoint.
- Until then t-rust-db/sqlite-rs#17 may still proceed: `parser::row` is a
  strict superset of Lab271's grammar (nothing Lab271 accepts is rejected
  here), so the repoint changes what is *accepted*, never what is
  rejected — with the two Lab271 tickets recording exactly what.
- One-off cost: a `parser::row` PR carries a Lab271 ticket. Deliberate
  friction; the alternative is a permanent fork of the grammar.
