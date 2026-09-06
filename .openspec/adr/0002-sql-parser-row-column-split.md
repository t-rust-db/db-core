# ADR 0002: One SQL grammar, one AST, dedicated codegen per engine

> Source: `#10`, `#19`, `#57`, `#147`. Supersedes the earlier ADR 0005
> ("sqlite-rs's grammar is the canonical `sql-parser`"), whose content is
> folded in here.

## Status

Accepted. Two of three parts are implemented; the third is tracked as
`#153`. See *Current state* at the end.

## Decision

`db-core` has **one SQL front end and one AST**, and **one planner per
execution engine**. Concretely:

1. **One grammar.** `parser::row` — sqlite-rs's tokenizer and
   recursive-descent grammar, ported from the leading codebase (ADR 0009)
   — is the only SQL parser in the crate. It parses the full SQLite
   surface: `SELECT` with joins, subqueries, CTEs, compound `SELECT`,
   window functions; DDL; DML; transactions; `PRAGMA`; `EXPLAIN`.
   Nothing else tokenizes or parses SQL.

2. **One AST.** `parser::ast` is the crate's AST. It is sqlite-rs's
   `ast.rs`, and it lives directly under `parser` — not under
   `parser::row` — because it is not row's private type: every engine's
   planner consumes it. Features are added to this AST and only this AST.
   No narrower, engine-specific AST exists or is grown alongside it.

3. **Engines enforce their subset by rejecting, not by parsing less.**
   Each execution engine (row, batch, stream) compiles the subset of the
   AST it can execute and returns a clear, span-carrying error for the
   rest. The batch engine rejects DDL/DML/transactions/`PRAGMA`, `WITH`,
   compound `SELECT`, `HAVING`, multi-way and non-equi joins, multi-term
   `ORDER BY`, and so on; the row engine rejects window functions and
   whatever it has not implemented yet. The grammar accepts all of it.
   "What batch supports" is a property of the batch planner, documented
   by its rejection list, not a second grammar that cannot recognize
   the rest.

4. **Dedicated codegen and emitters per engine.** `codegen::row`,
   `codegen::batch`, `codegen::stream` (and `emit::batch`) are separate
   and stay separate. They share the AST as input and nothing about
   their output: row-at-a-time bytecode and vectorized batch programs
   are legitimately different instruction sets with different
   performance and semantics (ADR 0001, ADR 0007, ADR 0008). Sharing the
   *front end* is what buys synergy; sharing the *back end* would
   destroy the reason to have three engines.

5. **sqlite-rs leads.** Full SQLite grammar and semantics parity is the
   goal; the three engines are three execution strategies for one
   language, not three languages. Where batch/stream and row overlap
   (`SELECT`-shaped queries) the gap always runs one way: batch/stream
   are missing options sqlite-rs already has. Closing it means the
   batch/stream planners accepting more of the AST, never syntax
   invented in `db-core` that sqlite-rs does not support. DDL, DML,
   transactions and `PRAGMA` are row-only by design — they have no
   meaning over an externally-managed schema (a Parquet file, a log
   topic) — and that asymmetry is permanent and correct.

6. **Cargo features gate compilation, not grammar.** `parser-row` gates
   the grammar and AST; `parser-column` gates the batch-subset
   validator and implies `parser-row`; `codegen-*`/`emit-*` gate each
   engine's planner and imply the parser features they need. A consumer
   that wants only the row engine builds with `parser-row` +
   `codegen-row` and compiles no batch code, and vice versa. The
   features select *which planners* are compiled; there is exactly one
   grammar regardless of which are on.

## Rationale

The alternatives were tried and failed in this repository, which is why
this ADR states the target flatly rather than weighing options:

- **Two grammars** (column-rs's analytics subset alongside sqlite-rs's
  full grammar) meant every `SELECT` feature was implemented twice or
  existed in only one. Divergence appeared within a day of coexistence:
  a feature was filed against the subset grammar that the full grammar
  already implemented correctly.
- **Two ASTs** (a narrow `expr::Query` for batch, `ast::Select` for row)
  meant the row planner was built against the *narrower* type — the one
  that could never represent `GROUP BY` expressions, expression `LIMIT`,
  aliases, CTEs, or compound `SELECT`, and that the row parser could not
  even produce. Ten thousand lines of planner reachable only from
  hand-built test literals. Worse, features started being *backported*
  into the narrow AST to mirror the full one, which is the opposite of
  retiring it. Two ASTs do not "cost nothing because no code joins
  them"; they cost every planner being written against whichever one
  its author happened to reach for.
- **One shared back end** was never seriously proposed and is rejected
  here for the record: the engines exist because row-at-a-time and
  vectorized execution want different instruction sets.

The analogy that misled the earlier decision was ADR 0001's
"consolidated location, not shared representation" for the VM opcode
sets. That analogy holds for *outputs* — two opcode sets are genuinely
different programs — and does not transfer to *inputs*: two SQL grammars
for `SELECT` are the same language parsed twice.

## Consequences

- Anything the grammar accepts has exactly one AST shape. A planner that
  cannot compile a shape says so with `Unsupported`/`PlanError` naming
  the construct. Silent miscompilation of an unrecognized shape is a
  bug, not a limitation.
- New SQL surface is added once, to `parser::row` + `parser::ast`, then
  each planner opts in. A construct the row engine gains but batch does
  not is a batch limitation, tracked against the batch planner.
- The batch subset validator (today `parser::column::convert_select`)
  is the authoritative, executable statement of what batch accepts. Its
  rejection list is preserved verbatim through refactors (`#153`) and
  gated by the downstream DuckDB oracle suite: a front-end change must
  produce byte-identical batch output.
- `parser::row` back-ports from sqlite-rs (ADR 0009) continue to be the
  way the grammar and AST grow; `db-core` does not fork them.

## Current state (2026-09-06)

| Part | Status | Evidence |
|---|---|---|
| One grammar | **Done** (`#57`) | `parser::column` has no tokenizer or grammar; it parses via `parser::row::parse_select`. |
| One AST — location | **Done** (`#151`) | `src/parser/ast.rs`; `parser::row::ast` is a one-release compatibility re-export. |
| One AST — row planner | **Done** (`#152`) | `codegen::row` (SELECT/DML/aggregate/subquery/DDL) consumes `parser::ast` exclusively. |
| One AST — batch planner | **Open** (`#153`) | `codegen::batch::compile*` and `emit::batch` still take `crate::expr::Query`, produced only by `parser::column::convert_select`'s lowering. `src/expr.rs` (569 lines) survives solely for this; its `Insert`/`Update`/`Delete`/`Assignment` are already dead. `#153` retargets both onto `parser::ast::Select`, demotes `convert_select` to a validator with the same rejection list, relocates `AggFunc` and the window enums out of `expr`, deletes `src/expr.rs`, and fixes the feature graph (`codegen-batch` must imply `parser-column`; `emit-batch` today calls a `parser-column`-gated function without implying it). |
| Cross-mode rejection | **Done** | Batch: `convert_select`. Row: `codegen::row` `Unsupported` arms (window functions, `IS`, `BETWEEN`, `IN (list)`, `LIKE`, `CASE`, `CAST`, parameters, `WITH`, compound — `#149`/`#150`). |
| Dedicated codegen | **Done** | `codegen::{row,batch,stream}`, `emit::batch`. |

Until `#153` lands, the second row of this table is the only place in
the crate where "one AST" is a target rather than a fact.
