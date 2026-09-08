# ADR 0002: One SQL grammar, one AST, dedicated codegen per engine

## Status

Accepted. ADR 0005 and ADR 0009 are folded into this one.

## Decision

`db-core` has **one SQL front end and one AST**, and **one planner per
execution engine**.

1. **One grammar.** `parser::row` is the only SQL tokenizer and parser
   in the crate. It parses the full SQLite surface: `SELECT` with joins,
   subqueries, CTEs, compound `SELECT`, window functions; DDL; DML;
   transactions; `PRAGMA`; `EXPLAIN`. Nothing else tokenizes or parses
   SQL. Grammar changes land here and only here.

2. **One AST.** `parser::ast` is the crate's AST. It lives directly under
   `parser`, not under `parser::row`, because it is not row's private
   type: every engine's planner consumes it. Features are added to this
   AST and only this AST; no narrower, engine-specific AST exists.

3. **Engines enforce their subset by rejecting, not by parsing less.**
   Each engine compiles the subset of the AST it can execute and returns
   a span-carrying error for the rest. `parser::column::validate_select`
   is the executable statement of what the batch engine accepts; the row
   engine's `CodegenError::Unsupported` arms are the same for row. "What
   an engine supports" is a property of its planner, documented by its
   rejection list, not a second grammar.

4. **Dedicated codegen per engine.** `codegen::row`, `codegen::batch` and
   `codegen::stream` share the AST as input and nothing about their
   output. Row-at-a-time bytecode and vectorized batch programs are
   legitimately different instruction sets with different performance
   and semantics (ADR 0007, ADR 0008). Sharing the front end is what buys
   synergy; sharing the back end would remove the reason to have three
   engines.

5. **Full SQLite grammar and semantics are the target.** The three
   engines are three execution strategies for one language. Where batch
   or stream lack something row has, closing the gap means the batch or
   stream planner accepting more of the AST -- never syntax that SQLite
   does not have. DDL, DML, transactions and `PRAGMA` are row-only by
   design: they have no meaning over an externally managed schema (a
   Parquet file, a log topic). That asymmetry is permanent.

6. **Cargo features gate compilation, not grammar.** `parser-row` gates
   the grammar and AST; `parser-column` gates the batch validator and
   implies `parser-row`; each `codegen-*` feature implies the parser
   features it needs. There is exactly one grammar regardless of which
   features are on.

## Rationale

Two grammars mean every `SELECT` feature is implemented twice or exists
in only one. Two ASTs mean each planner is written against whichever
type its author reached for, and the narrower one can never represent
what the wider one parses. One shared back end would collapse the
engines into one. All three alternatives are rejected for the record.

## Consequences

- Anything the grammar accepts has exactly one AST shape. A planner that
  cannot compile a shape says so by name. Silent miscompilation of an
  unrecognized shape is a bug, not a limitation.
- New SQL surface is added once, to `parser::row` + `parser::ast`, then
  each planner opts in. A construct one engine accepts and another does
  not is a limitation tracked against the second engine's planner.
- The batch validator's rejection list is preserved through refactors:
  a front-end change must not alter which queries batch accepts.
