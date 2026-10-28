# ADR 0030: The column dialect follows DuckDB where DuckDB and sqlite3 differ

## Status

Accepted. Amends [ADR 0011](0011-shared-scalar-functions.md).

## Context

ADR 0011 put every scalar function, comparison and coercion in one set of
root modules (`functions`, `compare`, `coerce`) on the grounds that
"exactly one correct behaviour exists per scalar function, independent of
which executor evaluates it".

The engines have different references, though:

- **The row engine** is a port of sqlite3 and is checked against sqlite3
  3.53.4 (the row-function oracle, `unit_sqlite_parity`).
- **The column engine** (`vm::batch`, the Parquet and ingest-table engines)
  and **the stream engine**, which reuses its planner and VM, are checked
  against DuckDB (ADR 0000 (f), `make check-column-oracle`). #635 already
  chose DuckDB arithmetic for them: `/` gives a DOUBLE and integer
  overflow is an error.

#679 found the column engine giving sqlite3's answer where DuckDB's
differs, with no error:

- `LIKE` matched case-insensitively (sqlite3) where DuckDB's `LIKE` is
  case-sensitive and has `ILIKE` for the other case.
- `regexp_extract` returned NULL without a match where DuckDB returns `''`.
- A string compared with a number never matched where DuckDB converts it
  to the number's type or raises a conversion error.

## Decision

Where DuckDB and sqlite3 give different answers, the column dialect gives
DuckDB's and the row dialect sqlite3's.

- **One function, one behaviour** still holds where the two agree, and
  still lives in the root modules (ADR 0011).
- **Where they differ**, the difference is a parameter or a second entry
  point next to the shared one, not a second module:
  - `functions::like_match` (sqlite3: ASCII case folded) and
    `functions::like_match_exact` (DuckDB: case-sensitive; `ILIKE`
    lowercases both sides);
  - `vm::batch`'s comparison converts a string to the other side's type,
    while `compare` keeps sqlite3's type ordering for the row engine.
- **A function only the column dialect has** (`regexp_extract`,
  `json_extract`'s late parse, the map functions) follows DuckDB outright.
- **Syntax only the column dialect has** is parsed only in the column
  dialect: `ILIKE` is a token only in `Tokenizer::tokenize_with_subscripts`,
  so sqlite3's grammar, and the row engine's parity with it, are
  unchanged.

## Consequences

- A column-engine answer is wrong when it differs from DuckDB's, whatever
  sqlite3 says. `make check-column-oracle`, the column fuzz target (#547)
  and the column sqllogictest suite (#680) check against DuckDB.
- Each difference is named where it is implemented, so a reader of
  `functions` sees which dialect each entry point serves.
- The stream engine shares the column dialect: `LIKE` there is
  case-sensitive too.

## As built (#725): CASE, COALESCE, IFNULL, NULLIF

The row engine already evaluates all of these, lazily and with sqlite3's
typing (the six cases added to `tools/row_functions_cases.sql` agree with
sqlite3). The column dialect, and the stream engine through it, now takes
them with DuckDB's answers.

- **Lowering.** `parser::column` rewrites `CASE` to nested
  `__select(cond, then, else)` calls, one per `WHEN` (a missing `ELSE` is
  NULL; a simple `CASE` compares its operand with each `WHEN` value, so a
  NULL never matches), and `NULLIF(a, b)` to `CASE WHEN a = b THEN NULL ELSE
  a END`. The planner compiles `__select` to `Opcode::Select`, and
  `COALESCE`/`IFNULL` to a `Select` on `IS NOT NULL` whose then arm is the
  first argument and whose else arm is the rest. They work on the VM's
  values, not through `Opcode::Call`, which would turn a DECIMAL into a REAL
  and a boolean into 0/1. They take any expression, an aggregate included,
  so a `CASE` or `COALESCE` over `SUM(x)` runs in `Combine`'s projection.
- **Lazy.** Each arm of a `Select` is a `Branch`: the elementwise ops that
  compute it, and the registers they read. The VM runs them in a VM of its
  own over a copy of those registers holding only the rows that take the
  arm, so an integer overflow or a failed conversion in an arm no row takes
  is not raised, as in DuckDB, and an arm no row takes is never run. Column
  and constant loads stay in the enclosing program, so an arm that is a
  column or a literal has no ops and costs nothing. Generated programs build
  a `Select` with `Opcode::select`, which names only `Opcode`.
- **Types.** The two arms take DuckDB's common type: an integer and a float
  give floats, an integer and a decimal a decimal, two decimals the larger
  scale; text next to a number, or a boolean next to a number, is a
  `BranchTypes` error. An arm with no ops is typed by its whole column, one
  with ops by the values it computed (an arm no row takes has no type, so
  `CASE WHEN c THEN 1 ELSE x * 2.5 END` over rows that all take the then arm
  is an integer column, where DuckDB's is a decimal).
- **One type per column.** A column that is all NULL in one segment next to
  a literal of another numeric type (`coalesce(amount, 0)`) has no type
  there, so that segment's result is the literal's. `vm::engine::run`
  gives each output column that is a `Select` one numeric type across the
  chunks of every segment, so the answer does not depend on how the table
  is split into segments. `run_streaming`, which hands out a segment at a
  time, does not.
- **Stream.** The severity-name rewrite (`severity >= 'WARN'`) now walks
  into function arguments, so it reaches a condition inside a `CASE`, and
  an aggregate over one.
- **Also fixed.** `TRUE` and `FALSE` literals compiled to NULL in the batch
  planner (`WHERE x = true` matched nothing); they are booleans now.

## As built (#802): UNNEST of a list column

`UNNEST(list)` turns one row holding a `LIST<VARCHAR>` into one row per
element, in DuckDB's two places: `FROM events, UNNEST(labels) AS l(label)`
(also `CROSS JOIN UNNEST(...)`), and `SELECT id, UNNEST(labels) AS label`.
A NULL or empty list gives no row; a NULL element gives a row holding NULL.
Without a column alias the value is named `unnest`.

- **Parse.** The row grammar reads `UNNEST(expr) [AS alias[(column)]]` as a
  `TableRefKind::Unnest` (the row dialect's codegen rejects it).
  `parser::column` brings both forms to one shape: a single `Cross` join
  holding the `Unnest`, whose `column` names the virtual column the elements
  are read from. A select-list `UNNEST` moves its list into that join and
  becomes a read of the column, so the planner sees one form. `l.label`
  loses its qualifier. One `UNNEST` per query; it needs a table before it,
  and is no join with a condition. The CROSS JOIN `LIMIT` rule does not
  apply, as an `UNNEST` is bounded by its row.
- **Plan.** Expanding a list changes the row count, so nothing can be
  loaded from the file afterwards. `unnest_prologue` runs first: the list,
  the `WHERE` conditions that do not read the unnested value (they filter
  before the expansion), every other column the query reads, then
  `Opcode::Unnest`, which repeats each live register per element and writes
  the elements to a register of its own. What follows is the ordinary plan,
  with the conditions on the unnested value left as its `WHERE`. A
  `COUNT(DISTINCT x)` key, which wants a register of its own, copies `x`
  (`MapOp::Copy`) instead of loading it again.
- **Where.** The opcode works on segments, so a table directory's files and
  its live WAL pages need nothing else. `EXPLAIN` shows an `UNNEST: list AS
  column` node between the loads and the filter.
- **Not yet.** A literal list, several `UNNEST`s, a lateral reference to
  another joined table, and `UNNEST` over a multi-table (cross-mode) query.
  A non-list value is a `MalformedProgram` execute error. Naming the value
  by its alias in `WHERE`/`GROUP BY` works; `UNNEST(...)` written there does
  not.

## As built (#803): string_split

`string_split(s, sep)` and its alias `str_split` give a `LIST<VARCHAR>`,
so data written before a list type existed (`"a,b,c"`) unnests without a
rewrite: `UNNEST(string_split(labels, ','))` in `FROM` or the `SELECT`
list. It lives in `vm::map_fns` beside the map functions, because
`Opcode::Call`'s row functions cannot carry a list. DuckDB's answers: `''`
gives `['']`, a NULL `s` gives NULL, a NULL `sep` the whole string as one
element, an empty `sep` one element per character, a separator at either end
or doubled gives empty elements. A value that is not text is split as its
text.
