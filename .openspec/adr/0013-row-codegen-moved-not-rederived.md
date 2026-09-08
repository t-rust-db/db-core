# ADR 0013: Row codegen is moved from Lab271/sqlite-rs, never re-derived

> Source: `#219` — move sqlite-rs's codegen + planner into
> `db_core::codegen::row` verbatim; supersedes `#175`, `#212`–`#218`,
> `#216`.

## Status

Accepted, resolving `#219`.

## Context

`db_core::codegen::row` began (`#20`, `#91`–`#97`) as a *re-derivation* of
sqlite-rs's `src/codegen/**`: the same design, re-typed feature by feature
against db-core's own AST and `vm::row` opcodes, each ticket porting one
slice. By v0.68.1 that copy was 15.7k lines against sqlite-rs's 21.1k,
compiled 3,089 of 5,822 oracle statements in the sqlite-rs#19 shadow run,
and still returned 23 wrong or failing corpus cases where it did compile.
Every open codegen-row ticket named a sqlite-rs module the re-derivation
had not reached yet, and sqlite-rs's codegen passed every oracle suite.

The pre-conditions for a *move* rather than a re-derivation were all met
by then and none were used: sqlite-rs's codegen imports only
`crate::parser::ast` (= `db_core::parser::ast`, sqlite-rs#17), `crate::vdbe`
(= `db_core::vm::row`, sqlite-rs#18), `crate::schema::TableSchema` (a
field-for-field twin of `codegen::row::TableSchema` since ADR 0012), and
`crate::planner` (pure `Stats`/`PlanCost`/`estimate_*` except for
`load_stats`, which reads `sqlite_stat1` through storage). Both crates
deny the same clippy set and run the same `check-mvl-limit` gate, so the
code is admissible on arrival.

Options considered:

1. Keep re-deriving: finish `#175`, `#212`–`#218`, `#216` one by one.
2. Move sqlite-rs's `src/codegen/**` and the pure half of `src/planner.rs`
   into `codegen::row` with only module-path rewrites, delete the
   re-derived tree, and make sqlite-rs's `src/codegen` a facade over
   `db_core::codegen::row` (the `src/vdbe.rs` pattern).

## Decision

Option 2, and as a standing rule for the rest of the migration: **row
codegen is moved from Lab271/sqlite-rs, never re-derived.**

- `src/codegen/row.rs` + `src/codegen/row/**` are sqlite-rs's
  `src/codegen.rs` + `src/codegen/**` (minus `shadow.rs`, the #19
  measurement switch) as of t-rust-db/sqlite-rs `751e291` (v0.19.1, Lab271
  synced sha `7701d18`), with these path rewrites only: `crate::vdbe` →
  `crate::vm::row`, `crate::schema` → `crate::codegen::row`,
  `crate::planner` → `crate::codegen::row::planner`,
  `crate::parser::error` → `crate::parser::row::error`,
  `crate::parser::tokenizer::Span` → `crate::parser::Span`. Module layout
  and public names are sqlite-rs's (`compile_statement(sql, schemas,
  views)`, `compile_select_*`, `explain_query_plan`, `output_column_names`,
  `leading_keywords`, `expand_with_clause`, `resolve_views`, …).
- `codegen::row::planner` is `src/planner.rs`'s pure half (`Stats`,
  `PlanCost`, `estimate_scan_cost`, `estimate_index_cost`,
  `is_skip_scan_worthwhile`, `is_automatic_index_worthwhile`).
  `load_stats` stays storage-side in sqlite-rs.
- db-core owns exactly two additions inside the tree, both marked
  `db-core#219` in place: the schema structs at the bottom of `row.rs`
  (from sqlite-rs's `schema` module, which db-core may not depend on —
  ADR 0008/0012; `with_computed_rowid_alias` uses the crate's parser
  instead of db-storage's string scanner), and `dispatch.rs`'s
  `SELECT`/`WITH`/`EXPLAIN` arms (`compile_select_statement`,
  `explain_select_statement`, `compile_eqp_program`), which are the pure
  half of sqlite-rs's CLI `query.rs::compile_select_program` — sqlite-rs
  dispatches `SELECT` from its binary, db-core has no binary.
- Ownership transfers on the move (sqlite-rs ADR-0039: "a path that has
  been repointed leaves the Lab271-leading regime"). Codegen changes now
  land in db-core; sqlite-rs consumes them through its facade.

**Amended 2026-09-08 (#235).** The original decision kept the moved
files byte-comparable with sqlite-rs's tree (MC/DC vectors in a separate
`codegen::row::mcdc` module, a `diff -r` drift check against sqlite-rs).
That is withdrawn: db-core owns its own testing strategy, one of three
core strategies (db-core, db-cli, db-storage) with sqlite-rs holding a
fourth of its own. Nothing in db-core's layout exists for sqlite-rs's
benefit -- MC/DC vectors live in the file whose decision they discharge,
like everywhere else in the crate, and any drift check against sqlite-rs
is sqlite-rs's concern. Only the core rule stands: this codegen was moved
in, is owned here, and is never re-derived.

## Consequences

- Behaviour db-core's re-derivation had grown that sqlite-rs lacks
  (`HAVING` over a `JOIN`; `DISTINCT` + `FULL JOIN` + `ORDER BY`; a
  computed projection in a scalar/`IN` subquery) is re-added *on top of*
  the moved code in a follow-up commit, as ordinary codegen changes owned
  by db-core.
- `compile_statement` takes `views` as a third argument (sqlite-rs's
  shape); `compile_statement_with_views` and the re-derived entry points
  (`compile_select_join`, `compile_eqp_program` in `eqp.rs`, …) are gone.
- `Stats` threads through `compile_select_with_catalog_and_stats`,
  `compile_select_joined`, `explain_query_plan`: `#117`/`#144` (stats cost
  model) and `#118` (N-way joins) arrive implemented.
