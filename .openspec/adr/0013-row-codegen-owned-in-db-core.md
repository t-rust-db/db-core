# ADR 0013: `codegen::row` is the one row planner, owned in `db-core`

## Status

Accepted.

## Decision

`codegen::row` is the row engine's planner: it compiles `parser::ast`
into `vm::row::Program`s (ADR 0008) for every statement kind the row
engine executes, and it is developed here. No other copy of a row planner
exists, and none is derived from this one or tracked against it.

**Layout.** `src/codegen/row.rs` holds the shared emission machinery
(`Emitter`, `RegAlloc`, `Label`/`Target`/`CondTargets` for jump-mode
condition compilation, `Scope`/`TableBinding` for column resolution) and
these submodules:

- `dispatch` -- `compile_statement(sql, schemas, views)`: keyword-sniffs
  one statement and routes it to the right parser/compiler pair,
  including `SELECT`/`WITH` (`compile_select_statement`) and `EXPLAIN
  QUERY PLAN` (`explain_select_statement`, rendered to a `Program` by
  `compile_eqp_program`). `DispatchError` wraps parse and codegen
  failures.
- `expr` -- value-mode (`compile_value`) and jump-mode (`compile_cond`)
  expression compilation. Conditions compile to control flow, never to
  an intermediate boolean register.
- `select` -- single-table scans and their index-aware fast paths
  (`index_scan`, `range_scan`, `limit_scan`), projection and `ORDER BY`,
  N-way joins with cost-based access-path choice and join reordering
  (`joins`, `join_access`, `join_order`, `join_full`), grouped and
  hash-grouped aggregation (`aggregate`), compound `SELECT`, and
  `EXPLAIN QUERY PLAN` (`eqp`).
- `subquery` -- CTE expansion, view expansion, `FROM`-subquery
  flattening and materialization, `WHERE` predicate push-down, scalar/
  `IN`/`EXISTS` subqueries with correlation analysis, hoisting and
  memoization.
- `stmt` -- `INSERT`, `UPDATE`, `DELETE`, with `index_maintenance`
  keeping every index in step.
- `ddl`, `analyze`, `pragma`, `transaction` -- the remaining statement
  kinds.
- `planner` -- the pure cost model: `Stats` (row counts and per-index
  `avg_eq` decoded from `sqlite_stat1` rows), `PlanCost`,
  `estimate_scan_cost`, `estimate_index_cost`, `is_skip_scan_worthwhile`,
  `is_automatic_index_worthwhile`. Loading `sqlite_stat1` from storage is
  the embedding application's job; it hands `Stats` in.

**Schema input** is `db_core::schema::{TableSchema, IndexSchema,
IndexedColumn, ViewSchema}` (ADR 0014), re-exported from `codegen::row`
for convenience.

## Consequences

- Behaviour gaps in the row planner are `codegen::row` tickets, fixed
  here. The planner's public names (`compile_statement`,
  `compile_select*`, `explain_query_plan`, `output_column_names`,
  `leading_keywords`, `expand_with_clause`, `resolve_views`, ...) are
  the API downstream engines build on.
- Statistics are an input, not a dependency: the planner never reads a
  database file.
- Testing follows ADR 0015: inline unit tests per module, black-box
  compile-then-execute suites in `tests/unit`, and MC/DC vectors in the
  file whose decision they discharge.
