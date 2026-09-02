# db-core

Shared SQL foundation for the t-rust-db family of engines (a row-oriented
engine, `sqlite-rs`, and a columnar engine, `column-rs`, both depend on
this workspace rather than duplicating parsing/AST logic).

This is a Cargo workspace, not a single crate, so that engines can depend
on only the pieces they need and so unrelated concerns (parsing vs.
expression AST vs. value types) can evolve independently.

## Layout (phase 1)

- **`sql-types`** — `Literal` (the AST-level literal token) and `Value`
  (the runtime value representation executors operate on), plus the
  conversion between them. No SQL syntax, no evaluation logic.
- **`sql-expr`** — the expression and query AST: `Expr`, `BinOp`, `AggFunc`,
  `WindowFunc`, `WindowSpec`, `OrderBy`, `SelectItem`, `JoinKind`, `Join`,
  and `Query` itself. `Query` lives here rather than in `sql-parser`
  because `Expr::InSubquery` holds a `Query` and `Query::where_clause`
  holds an `Expr` — the two types are mutually recursive and can't be
  split across crates. This crate has no tokenizer and no evaluation;
  it's AST only.
- **`sql-parser`** — the tokenizer and recursive-descent parser. Its
  public API (`parse`, `parse_explain`) turns SQL text into a
  `sql_expr::Query`; it owns no AST types itself, only `ParseError` and
  the parsing machinery.
- **`sql-join`** — shared join infrastructure for t-rust-db engines.
  Starts with `JoinHashTable`, a flat open-addressing multimap — the
  hash table representation, not join semantics or a cost model, is the
  fix column-rs's join benchmark needs. `JoinKind`/NULL-safe semantics
  and a build-side cost model are follow-up additions once an engine
  actually needs them, not built speculatively here.

Dependency direction: `sql-parser` → `sql-expr` → `sql-types`. `sql-join`
depends on none of the others today.

## Roadmap

This is phase 1: enough structure to house the SQL parser cleanly. More
crates will be added as functionality grows beyond what a single row- or
column-oriented executor needs today, for example:

- `sql-string` — string function implementations
- (others as the row and columnar executors converge on shared needs)

Each addition should stay a separate workspace member unless it's tightly
coupled to an existing one, following the same "AST vs. parsing vs.
evaluation" separation established here.
