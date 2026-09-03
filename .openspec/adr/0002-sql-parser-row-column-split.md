# ADR 0002: `sql-parser`'s row/column split mechanism

> Source: `#19` — restructure `sql-parser` with dedicated row and column
> sections, superseding `#10`'s earlier "keep sqlite-rs's parser
> permanently separate" lean

## Status

Accepted, **amended** (see Amendment below): the row/column split
mechanism (Cargo features) stands, but "one shared AST type" does not —
`row` gets its own AST, ported from sqlite-rs's, not new
`sql_expr::Query` variants.

## Context

`sql-parser` today implements only column-rs's narrow analytics subset
(~1,300 lines: `SELECT ... FROM ... [JOIN] [WHERE] [GROUP BY] [ORDER
BY] [LIMIT]`, one `parse()`/`parse_explain()` entry point, no feature
gating). sqlite-rs's own parser (`src/parser/*`, 7,361 lines) implements
a much larger, actively-growing SQLite grammar (DDL, DML, transactions,
`PRAGMA`, ...), with its own grammar file
(`t-rust-db/grammar/sqlite.ebnf`) and V-block-tagged production
coverage.

`#10` confirms the direction: `sql-parser` becomes **one crate** with
dedicated row and column sections, mirroring how `sql-vm` already splits
into `batch`/`row`/`stream` modules (ADR 0001) — not two permanently
separate parser crates. What ADR 0001 settled for the VM layer (shared
crate, feature-gated modules, each with its own opcode set) this ADR
settles for the parser layer.

This ADR answers exactly the question `#10` narrowed `#19` down to:
**what's the actual mechanism** for the split — feature flags, a mode
enum, or separate entry points sharing one tokenizer/AST? It does not
attempt the sqlite-rs grammar migration itself (`#19`'s other
acceptance criteria) — that requires sqlite-rs's actual parser source
and the `t-rust-db/grammar` repo, neither of which are checked out in
`db-core`. See Consequences for how that work is tracked.

## Options considered

- **A — Cargo features, mirroring `sql-vm` exactly** (`column`/`row`,
  each gating a module). A consumer that only needs column-rs's subset
  builds with `default-features = false, features = ["column"]`,
  compiling neither sqlite-rs's grammar nor any dependency only it
  needs. Matches the project's established pattern (ADR 0001) instead
  of inventing a second mechanism for the same kind of problem one
  crate over.
- **B — A `ParseMode` enum passed to one shared `parse(input, mode)`
  entry point.** Both grammars always compile into the binary; the
  caller picks at runtime which one applies. Simpler API (no feature
  matrix to reason about), but means every consumer pays sqlite-rs's
  7,000+ line grammar's compile cost even when it only ever calls with
  `ParseMode::Column` — the same tradeoff ADR 0001 rejected for
  `sql-vm`'s executors, for the same reason.
- **C — Separate top-level functions (`parse_row`/`parse_column`)
  sharing a tokenizer and AST layer, no feature gating.** Same
  always-compiled downside as B, with a marginally simpler call site
  (no enum) at the cost of two public entry points instead of one
  mode-dispatched one.

## Decision

**Option A.** `sql-parser` gains `column` and `row` Cargo features,
gating `sql_parser::column` and `sql_parser::row` modules exactly the
way `sql-vm`'s `batch`/`row`/`stream` features gate its modules:

- `column` — column-rs's existing analytics-subset grammar, moved
  unchanged from today's crate root into `sql_parser::column`. On by
  default (`default = ["column"]`), matching `sql-vm`'s rationale:
  plain `cargo test --workspace` needs something to exercise, and it's
  today's only real consumer's actual dependency.
- `row` — reserved for sqlite-rs's DDL/DML/transaction/`PRAGMA` grammar.
  A documented stub for now (see `sql_parser::row`'s doc comment),
  exactly like `sql_vm::row`/`sql_vm::stream` were left as stubs in ADR
  0001 rather than blocking that ADR on unfinished implementation.

Both sections share `sql-parser`'s tokenizer (`Token`/`Tokenizer`, not
duplicated) and produce `sql_expr::Query` — the same AST type, not two
parallel ones — matching this crate's existing doc comment ("Produces
`sql_expr::Query` -- the AST types themselves live in `sql-expr`, not
here"). Whether `sql_expr::Query` needs new variants for row-only
constructs (DDL/DML have no equivalent in `SELECT`-shaped analytics
queries) is real design work deferred to the migration itself (see
Consequences) — this ADR does not extend `Query` speculatively for
grammar that isn't implemented yet, the same restraint ADR 0001 applied
to `sql_vm::row`/`stream`'s opcode sets.

`column-rs`'s existing consumption (`sql_parser::parse`,
`sql_parser::parse_explain`, `ParseError`, `Span`) is re-exported at the
crate root unchanged, so this ADR's scaffolding is a pure move — no
consumer-visible API break, no behavior change, all 28 existing tests
pass unchanged.

## Consequences

- `sql-parser`'s `Cargo.toml` gains a `[features]` section mirroring
  `sql-vm`'s comment-for-comment where the reasoning is identical (why
  `column` defaults on, why each section is gated independently).
- The actual sqlite-rs grammar migration — porting `src/parser/*`'s
  7,361 lines into `sql_parser::row`, extending `sql_expr::Query` for
  DDL/DML/transactions/`PRAGMA`, running sqlite-rs's full parser test
  suite against it, and updating `t-rust-db/grammar` (both
  `column-rs.ebnf` staying accurate and a promoted/referenced sqlite
  grammar file) — is **not done by this ADR** and cannot be done from
  `db-core` alone: it requires sqlite-rs's parser source and the
  grammar repo, neither present in this worktree. Tracked as follow-up
  issues rather than folded into `#19`, per `#19`'s own "Additional
  Notes" ("almost certainly several PRs, not one").
- `sql_parser::row` staying a stub for now means `#19`'s acceptance
  criteria around sqlite-rs's test suite and the grammar repo remain
  open — this ADR unblocks them by fixing the mechanism, it doesn't
  close them out.
- Once real `row` grammar lands, `sql_parser::column`'s existing
  `ParseError`/`Span` shape is the reference for what `row`'s own
  parse-error type should look like (composing, not centralizing, per
  ADR 0001's error-handling decision) — most likely its own
  `RowParseError` rather than reusing `ParseError`, since sqlite-rs's
  own parser errors are shaped differently than column-rs's subset
  ever needed. Left to the migration ticket, not decided here.

## Amendment (2026-09-03, during `#23`'s AST slice)

Starting the actual grammar migration (`#23`) surfaced the "real design
work" this ADR deferred above: `sql_parser::row::ast`, sqlite-rs's own
AST (`src/parser/ast.rs`, ~15 statement types — `Select`, `Update`,
`Insert`, `Delete`, `CreateTable`, `CreateIndex`, `CreateView`, `Drop*`,
`Begin`/`Commit`/`Rollback`, `Pragma`, `Analyze`, `Explain`), turned out
not to fold cleanly into `sql_expr::Query` as new variants. Folding it
in would mean redesigning an AST shape sqlite-rs has already built,
tested, and wired its own codegen against, purely to satisfy this ADR's
original "one shared AST" framing — for no consumer that needs it: no
code anywhere joins a `column`-parsed and a `row`-parsed statement into
one value today, so there is no actual call site "one AST type" was
protecting.

**Revised decision:** `row` and `column` do **not** share an AST type.
`sql_parser::row::ast` is sqlite-rs's own AST, migrated in unchanged
(mechanical port, not a redesign) — the same "consolidated location,
not shared representation" trade ADR 0001 already made and named
explicitly for `sql_vm::batch::Opcode` vs. a future
`sql_vm::row::Opcode` ("Nothing in this ADR claims they should become
one opcode enum"). What the two sections *do* still share, unchanged
from the original decision: the Cargo-feature split mechanism, and
`sql_parser::Span` (`row::ast`'s nodes and `row::tokenizer`'s tokens
both carry it, reusing `column`'s primitive rather than a second
`Span`).

This also settles the parse-error question the original Consequences
section left open: `row` needs its own `ParseError`-equivalent shaped
for its own `ast` types regardless, so "not reusing `column`'s
`ParseError`" was already the likely answer; this amendment confirms it
follows from the same AST decision, not a separate one.
