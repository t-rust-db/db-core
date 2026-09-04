# Alignment with sqlite-rs

sqlite-rs started as a separate, pre-existing project
(github.com/iheitlager/sqlite-rs, developed under a Schuberg
Philis-owned context) and is being absorbed into `t-rust-db`
(`t-rust-db/sqlite-rs`). This note records what was checked to avoid
collisions between the two codebases' conventions (naming, types,
copyright headers) as the org boundary between them closes, and what
was deliberately left unresolved rather than silently decided.

Folded in from the standalone `t-rust-db/grammar` repo (`db-core#43`),
trimmed of sections that were live decisions at the time but are now
settled fact already captured elsewhere (`sql-vm`'s batch/row/stream
consolidation: ADR 0001; `sql-parser`'s row/column split housing
sqlite-rs's own grammar: ADR 0002/0005) — see the archived grammar
repo's `ALIGNMENT.md`/`DECISIONS.md` history if that narrative is ever
needed.

## Checked and fine

- **Crate namespacing.** t-rust-db's crates (`sql-types`, `sql-expr`,
  `sql-parser`, `sql-join`, `db-storage`, `db-cli`) don't collide with
  any sqlite-rs crate name — sqlite-rs is a single crate (`sqlite-rs`),
  not published as a set of libraries.
- **Grammar notation.** `grammar.ebnf`'s EBNF style (notation, V-block-
  less "not supported" section, maintenance rule) mirrors sqlite-rs's
  own grammar conventions, so the two read the same way side by side
  without actually merging into one file (they describe genuinely
  different languages — see `grammar.ebnf`'s own header).

## Real collisions found — deliberately NOT resolved here

### 1. Three `Value` types exist across the ecosystem

| Type | Location | Variants |
|------|----------|----------|
| `sqlite_rs::record::value::Value` | sqlite-rs, `src/record/value.rs` | SQLite's own storage classes |
| `sql_types::Value` | `db-core`, `sql-types/src/lib.rs` | `Int, Float, Str, Null` |
| `column_rs::vm::Value` | `column-rs`, `src/vm.rs` | `Int, Float, Bool, Str(Cow), Null` |

`sql_types::Value` is currently **unused** by column-rs's own VM — it
was extracted speculatively during the September 2026 restructure (no
runtime `Value` existed in the original `sql.rs`, one was added "per
spec"). The VM still runs entirely on its own, richer
`column_rs::vm::Value`.

**Not resolved:** which (if any) of these should become the one shared
`Value` type is an open design question, not something to decide inside
a grammar/alignment pass. Building `sql-join`'s `JoinHashTable` generic
rather than hardcoding any one `Value` was a direct consequence of
leaving this open.

### 2. Copyright header convention

Every sqlite-rs source file carries:
```rust
// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
```

t-rust-db repos currently carry **no per-file copyright header** — just
doc comments, with an `Apache-2.0` `LICENSE` file at the repo root and
`license.workspace = true` in `Cargo.toml`.

**Deliberately not copied.** t-rust-db is not a Schuberg Philis-owned
org; stamping "Copyright 2026 Schuberg Philis" on files that aren't part
of that codebase would be a false attribution, not an alignment. If
t-rust-db wants a per-file header at some point, it should name its own
actual owner/org, decided explicitly rather than inherited by copy-paste
from a different project's convention.

## What to check again before any future integration

- If `sql-types::Value` ever gains real users (beyond the placeholder
  `Literal`-conversion it has today), re-diff it against both
  `column_rs::vm::Value` and sqlite-rs's `Value` before assuming it's
  "the" shared type.
- If t-rust-db repos ever move under an org that wants copyright
  headers, decide the actual copyright holder explicitly — don't
  default to sqlite-rs's.
