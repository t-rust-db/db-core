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
