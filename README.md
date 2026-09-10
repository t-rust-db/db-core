# db-core

Shared SQL language/execution layer for the t-rust-db family of engines
(`sqlite-rs`, row-oriented; `column-rs`, columnar) — one crate, so
engines share types/expr/parser/join/vm/codegen/emit without duplicating them,
each gated behind Cargo features so a consumer builds only what it uses.

Physical storage is the `storage` module (`src/storage/`: `row`/`column`,
`stream` planned — [ADR 0006](.openspec/adr/0006-storage-consolidation-into-db-storage.md),
[ADR 0016](.openspec/adr/0016-absorb-db-storage-as-workspace-member.md)),
behind the `storage-row`/`storage-column` features. The language/execution
layer (`parser`/`vm`/`codegen`) stays storage-agnostic: it never imports
`storage`; `storage` imports only `value` and `schema`.

The `engine` module ([ADR 0017](.openspec/adr/0017-engine-seam.md)) is
the client-facing seam over the modes: open a file, run SQL, get `Cell`s --
`engine::row` (SQLite files) and `engine::column` (Parquet files) today, stream to follow.

Was six separate crates (`sql-types`, `sql-expr`, `sql-parser`,
`sql-join`, `sql-vm`, `sql-codegen`) until this repo's merge into one —
see `CHANGELOG.md`. The module boundaries are unchanged, just no longer
crate boundaries.

## Layout

- **`types`** — `Literal`/`Value`, the base value representation. No
  syntax, no evaluation. Always compiled.
- **`join`** — `JoinHashTable` (a flat open-addressing multimap) and
  join-kind emit semantics. Always compiled (small, no dependencies) —
  its only consumer today is `vm-batch`.
- **`parser`** — tokenizer + recursive-descent parser, producing
  `parser::ast::Select` — the crate's single AST (ADR 0002), consumed
  directly by both the batch and row planners. Two Cargo-feature-gated
  sections: `parser-column` (column-rs's analytics subset, default on —
  a validator over the same AST, enforcing which constructs the batch
  planner accepts) and `parser-row` (sqlite-rs's full grammar —
  DDL/DML/transactions/`PRAGMA`). See `src/parser/grammar.ebnf` for the
  actual EBNF both sections implement.
- **`vm`** — three execution engines over a compiled query: `vm-batch`
  (vectorized/columnar, default on — this is column-rs's VM), `vm-row`
  (cursor-driven, sqlite-rs-style, default on), `vm-stream` (push-driven,
  live/unbounded sources — not yet implemented). Each has its own opcode
  set; they are not expected to converge into one. `vm::batch::AggFunc`
  is shared by both the batch and row planners.
- **`codegen`** — the planner, AST → executable `vm` `Program` (sqlite-rs's
  meaning of "codegen", [ADR 0007](.openspec/adr/0007-program-instruction-mirror-sqlite-rs.md)):
  `codegen-batch` (default on, needs `vm-batch` and `parser-column`;
  moved here from column-rs's `query.rs`), `codegen-row` (default on,
  the sqlite-rs-style planner), `codegen-stream` (not yet implemented).
- **`emit`** — ahead-of-time Rust-source emitter: a planned `Program` →
  `const PROGRAM` source text for rustc (batch-only, no sqlite-rs
  equivalent). `emit-batch` (default on, needs `codegen-batch`),
  `emit-row`/`emit-stream` (not yet implemented).

## Feature flags

```toml
# column-rs's actual dependency shape:
db-core = { git = "...", default-features = false, features = ["parser-column", "vm-batch", "codegen-batch", "emit-batch"] }
```

`default = ["parser-column", "vm-batch", "codegen-batch", "emit-batch"]` so a plain
`cargo test` exercises real content. A consumer that only needs one
execution mode sets `default-features = false` and lists exactly the
features it uses — the others' modules and dependencies (e.g. `rayon`,
needed only by `vm-batch`) then never compile.
