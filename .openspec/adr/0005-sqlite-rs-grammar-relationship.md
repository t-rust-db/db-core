# ADR 0004: sqlite-rs's grammar is the canonical `sql-parser`; column/stream are enforced subsets

> Source: `#10` — how does sqlite-rs's SQL grammar relate to `db-core`'s
> `sql-parser`?

## Status

Accepted, resolving `#10`.

## Context

`#10` posed three options:

- **A** — sqlite-rs's parser becomes *the* `sql-parser`; column-rs's
  grammar becomes a documented, enforced subset.
- **B** — two parser crates coexist permanently, sharing only a
  tokenizer/`Span`.
- **C** — sqlite-rs's parser never moves to `db-core`; only the storage
  layer (vfs/pager/btree/record/schema) is shared. `#10` itself leaned
  toward this, absent a concrete reuse case.

Since `#10` was filed, `#19`/`#23`/ADR 0002 already moved ahead of the
decision: `sql-parser` is one crate with `column`/`row` Cargo-feature
sections, sharing a tokenizer and `Span` but **not** one AST (ADR 0002's
amendment — `row::ast` is sqlite-rs's own AST, ported unchanged, not
folded into `sql_expr::Query`). That mechanism is closer to Option A
than C, but `#10` never formally closed the question of *intent*:
is sqlite-rs's grammar the arm column/stream should converge toward, or
just a coincidentally-adjacent module?

## Decision

**Option A, in intent, using the mechanism ADR 0002 already built.**
sqlite-rs's grammar (`sql_parser::row`) is the canonical, full-parity SQL
surface for `db-core` — not a permanently-separate dialect (rejecting
`#10`'s own lean toward C). Rationale, per this session:

- **sqlite-rs is first.** Full SQLite grammar/semantics parity is the
  actual goal; `db-core`'s row/column/stream execution modes exist to
  give that one language three execution strategies, not three
  languages. column-rs and any future streaming engine should converge
  *toward* sqlite-rs's grammar, not the reverse.
- **DDL stays row-only.** `CREATE TABLE`/`CREATE INDEX`/`ALTER`/etc. have
  no meaning for a batch/streaming query over an already-defined,
  externally-managed schema (a Parquet file, a log topic) — this is not
  a gap to close, it's a permanent, correct asymmetry. `sql_parser::row`
  keeps DDL/DML/transactions/`PRAGMA` exclusively.
- **The gap runs the other way.** Where `column`/`stream` and `row`
  overlap (`SELECT`-shaped queries: projections, filters, joins, window
  functions, `GROUP BY`), `column`/`stream` are the ones missing options
  sqlite-rs's grammar already has — not the reverse. Closing that gap
  means extending `sql_parser::column`/a future `sql_parser::stream`
  toward `row`'s existing `SELECT` grammar, not inventing new syntax
  sqlite-rs doesn't support.

This does **not** reopen ADR 0002's amendment: `row`/`column` (and
`stream`, once real) keep separate ASTs and separate Cargo-feature
sections in one crate, because DDL/DML genuinely have no analytics-query
equivalent to unify against. What this ADR settles is the *direction of
convergence* for the parts that do overlap, which `#10` left implicit.

## Consequences

- `sql_parser::column`'s grammar is tracked as an explicit, enforced
  subset of `sql_parser::row`'s `SELECT` grammar going forward. New
  `SELECT`-shaped syntax added to `row` that a `column`/`stream`
  consumer could use is a candidate for backporting into `column`;
  syntax invented in `column` that has no `row` equivalent should be
  treated as a gap in `column`'s design, not a feature to push upstream
  into sqlite-rs's grammar.
- `#20`'s `sql-codegen` (batch/row/stream emitters) and `#18`'s
  `sql_vm::row` migration are unaffected in scope by this ADR — it
  confirms their premise (one integration plan, sqlite-rs as the
  reference implementation) rather than changing it.
- `grammar/column-rs.ebnf` should be reviewed against `grammar/
  sqlite.ebnf`'s `SELECT` production for concrete gaps (e.g. subquery
  forms, `CASE`, additional window frame options) as a follow-up —
  tracked as a new issue, not enumerated here since it requires a real
  side-by-side grammar diff.
- Resolves `#10`: Option A, using ADR 0002's already-built mechanism.
