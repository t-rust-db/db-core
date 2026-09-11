# ADR 0020: Double quotes are always identifiers, never a string fallback

## Status

Accepted (#350)

## Decision

`"..."` always lexes as an identifier (`scan_quoted_identifier`,
`src/parser/row/tokenizer.rs:996`), never as a string literal. String
literals use `'...'` only (`scan_string`, `src/parser/row/tokenizer.rs:955`).
When a double-quoted identifier doesn't resolve to a real column or table,
resolution fails with an error (e.g. `PlanError::UnknownColumn`,
`src/codegen/row/select.rs:115`) rather than silently reinterpreting the
token as a string.

This is the SQL-standard reading. It is a deliberate divergence from two
sibling tools' compatibility carve-outs, not an oversight:

- **SQLite** falls back to treating an unresolved double-quoted token as a
  string literal — a documented misfeature kept only for backwards
  compatibility (https://sqlite.org/quirks.html#dblquote), not something
  the SQLite team recommends relying on.
- **MySQL** does the same outside `ANSI_QUOTES` `sql_mode` — again an
  opt-out compatibility default, not the ANSI baseline.

db-core targets the ANSI baseline both tools treat as the fallback case,
so it has no fallback: a double-quoted token is a name, and an unresolved
name is an error.

## Consequences

- A query like `WHERE version = "1.9.5"` errors with an unknown-column
  message instead of silently matching the literal string `"1.9.5"`. This
  is intentional: silently guessing intent from a typo'd or
  wrong-database-dialect query is worse than a clear error.
- Do not add a double-quote-to-string fallback to the tokenizer or
  resolver to "fix" this. If cross-tool compatibility for this specific
  pattern is ever required, it must be an explicit, opt-in dialect mode —
  never the default — and should get its own ADR.
