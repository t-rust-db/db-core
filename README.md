# db-core

Shared SQL language/execution layer for the t-rust-db family of engines
(`sqlite-rs`, row-oriented; `column-rs`, columnar) — one crate, so
engines share types/expr/parser/join/vm/codegen without duplicating them,
each gated behind Cargo features so a consumer builds only what it uses.

Physical storage is **not** here — see `db-storage` (`row`/`column`/
`stream` modules, per [ADR 0006](.openspec/adr/0006-storage-consolidation-into-db-storage.md)).
`db-core` is storage-agnostic by design.

Was six separate crates (`sql-types`, `sql-expr`, `sql-parser`,
`sql-join`, `sql-vm`, `sql-codegen`) until this repo's merge into one —
see `CHANGELOG.md`. The module boundaries are unchanged, just no longer
crate boundaries.

## Layout

- **`types`** — `Literal`/`Value`, the base value representation. No
  syntax, no evaluation. Always compiled.
- **`expr`** — the expression/query AST: `Expr`, `Query`, `BinOp`,
  `AggFunc`, `WindowFunc`, `JoinKind`, etc. `Expr` and `Query` are
  mutually recursive, which is why they live together. Always compiled.
- **`join`** — `JoinHashTable` (a flat open-addressing multimap) and
  join-kind emit semantics. Always compiled (small, no dependencies) —
  its only consumer today is `vm-batch`.
- **`parser`** — tokenizer + recursive-descent parser, producing
  `expr::Query`. Two Cargo-feature-gated sections: `parser-column`
  (column-rs's analytics subset, default on) and `parser-row`
  (sqlite-rs's full grammar — DDL/DML/transactions/`PRAGMA`). See
  `src/parser/grammar.ebnf` for the actual EBNF both sections implement.
- **`vm`** — three execution engines over a compiled query: `vm-batch`
  (vectorized/columnar, default on — this is column-rs's VM), `vm-row`
  (cursor-driven, sqlite-rs-style — not yet implemented, `#18`),
  `vm-stream` (push-driven, live/unbounded sources — not yet
  implemented). Each has its own opcode set; they are not expected to
  converge into one.
- **`codegen`** — one emitter per `vm` executor: `codegen-batch`
  (default on, needs `vm-batch`), `codegen-row`/`codegen-stream` (not
  yet implemented).

## Feature flags

```toml
# column-rs's actual dependency shape:
db-core = { git = "...", default-features = false, features = ["parser-column", "vm-batch"] }
```

`default = ["parser-column", "vm-batch", "codegen-batch"]` so a plain
`cargo test` exercises real content. A consumer that only needs one
execution mode sets `default-features = false` and lists exactly the
features it uses — the others' modules and dependencies (e.g. `rayon`,
needed only by `vm-batch`) then never compile.
