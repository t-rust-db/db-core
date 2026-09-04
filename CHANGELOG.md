# Changelog

All notable changes to db-core. Format follows [Keep a Changelog](https://keepachangelog.com/), versioning follows [SemVer](https://semver.org/). Pre-1.0: minor bumps may break the public API.

**Versioning policy:** one crate, one version, one tag per release.

## [0.16.0] - 2026-09-04

### Added

- **`vm::row`'s value-semantics slice** (#18, ADR 0008) -- the first real content in `db_core::vm::row`, ported from sqlite-rs's VDBE with zero I/O/storage coupling: `value` (`Value`/`Collation`/`compare_text`/`format_real`), `compare` (cross-type ordering: NULL < numeric < text < blob), `logic` (three-valued logic / NULL propagation), `affinity` (column type affinity), `cast` (`CAST` conversion), `coerce` (text-to-numeric coercion and checked arithmetic, overflow promotes to REAL). A partial `Opcode`/`Instruction`/`Program` skeleton covers just these ops; the execution loop, cursor trait, and remaining opcodes are tracked in #51.
- **ADR 0008** resolves `vm::row`'s two open design questions: `Opcode` is a mechanical port of sqlite-rs's VDBE opcode set (typed operands, following ADR 0007's precedent for `batch`), and the eventual cursor abstraction will be a storage-agnostic trait rather than a direct `db-core` -> `db-storage` dependency.

## [0.15.0] - 2026-09-04

### Added

- **`codegen::batch::expand_star(query, schema)`** (#46) -- resolves `SelectItem::Star` against a caller-supplied schema (column names in order), replacing it with `SelectItem::Column` entries; mixed `SELECT id, * FROM t` keeps `id` first. `db-core` has no Parquet/schema access itself, so this is meant to be called once by the schema-aware caller (e.g. column-rs's `QueryEngine`) before `compile`/`compile_join`/`compile_semi_join`/`compile_window`. A query with no `Star` is returned unchanged.
- **`PlanError::StarWithAggregation`** -- returned by `expand_star` when `*` is combined with `GROUP BY` or an aggregate/window select item, since the row shape is no longer well-defined to expand `*` against.

## [0.14.0] - 2026-09-04

### Added

- **`SELECT DISTINCT`** support in column-rs's grammar (#47), including `DISTINCT` combined with `GROUP BY` (dedup applied after the GROUP BY hash-aggregate merge, matching DuckDB's semantics). No new opcode: `distinct: bool` is threaded through `expr::Query` and the terminal `Opcode::Finalize`; deduplication runs in `vm::engine::finalize` as a stable pass over the fully materialized cross-segment output, after the GROUP BY merge and before `ORDER BY`/`LIMIT`.

### Changed

- `vm::engine::bounded_scan_limit` and the `ORDER BY`/`LIMIT` top-N fast path in `run` now fall back to the general (fully materializing) path when `distinct` is set, since both bypass the full materialization DISTINCT's dedup pass needs before sort/limit can run correctly.

## [0.13.0] - 2026-09-04

### Added

- **`vm::batch::Program`/`Instruction`** mirroring sqlite-rs's `vdbe::program` shape (ADR 0007): a `Program` is a `Vec<Instruction>`, each `Instruction` a typed `Opcode` plus an optional `EXPLAIN` comment. Operands stay typed and named on the `Opcode` enum (not sqlite-rs's `p1..p5` integer slots). `Program::columns_to_load()` derives the input columns from the `LoadColumn` instructions; `Program::split_finalize()` separates the body from the terminal `Finalize`.
- **`Opcode::Finalize { agg_parts, num_group_keys, order_by, limit }`** -- the terminal opcode of a planned flat program, carrying the cross-segment merge/`ORDER BY`/`LIMIT` metadata that column-rs's `Plan` used to hold as sidecar fields. The per-segment `Vm` treats it as a no-op control opcode (like `Scan`/`Halt`); `vm::engine::run` applies it once over the concatenated per-segment output. `AggPart` moves here from column-rs.
- **`vm::engine`** (gated with `vm-batch`): `run(segments, &Program)` -- body per segment via `run_parallel`/`run_parallel_top_n` (or a sequential bounded scan for a bare `LIMIT`), then `Finalize` once; `finalize()` (column-rs's former `query::post_process`, moved unchanged); `run_join`/`JoinProgram` (the `HashBuild`/`HashProbe` two-phase driver, from `execute_joined`); `semi_filter`; `InMemorySegment`.

- **`codegen::batch`** -- the columnar planner, moved from column-rs's `src/query.rs` (it never touched Parquet): `compile(&Query) -> Program` (the former `Plan` struct is gone -- its `columns_to_load` is `Program::columns_to_load()`, its `agg_parts`/`num_group_keys`/`order_by`/`limit` are the terminal `Opcode::Finalize`), `compile_join -> JoinProgram`, `compile_semi_join -> SemiJoinProgram`, `compile_window -> Program` (window queries are now an ordinary flat program: `LoadColumn`s, `Window`s, `Emit`, `Finalize`), `output_column_names`, `PlanError`, and the `EXPLAIN` plan tree (`explain(&Query, &dyn Fn(&str) -> TableStats) -> Vec<PlanNode>`; storage supplies only each table's row-group/row counts). Instructions carry `EXPLAIN` comments (`r0 = region`, `WHERE id > 1000`, `GROUP BY region`, ...).
- **`emit::batch::generate(crate_name, sql)`** -- the end-to-end "SQL text to `.rs` source" entry point, moved from column-rs's `src/codegen.rs` (which is deleted there). `render_flat` now takes the planned `&Program` and emits it whole (including `Finalize`, with instruction comments as trailing `//` comments); the generated binary calls `{crate_name}::query::run_program(&file, PROGRAM)` -- no more `COLUMNS_TO_LOAD`/`AGG_PARTS`/`NUM_GROUP_KEYS`/`ORDER_BY`/`LIMIT` sidecar consts.

### Changed

- **Breaking:** `codegen` module renamed to `emit`, and its Cargo features `codegen-batch`/`codegen-row`/`codegen-stream` to `emit-batch`/`emit-row`/`emit-stream` (ADR 0007). In this family *codegen* means what sqlite-rs means by it (AST -> executable VM program, i.e. the planner); the ahead-of-time Rust-source emitter (`render_flat`/`render_joined`/`render_semi_join`/`render_windowed`, `const PROGRAM` in a `.rs` file) is now `emit::batch`. The `codegen` module name is reused for the planner (see below). Consumers of the emitter change `db_core::codegen::batch::*` -> `db_core::emit::batch::*` and the feature name.

- **Breaking:** `sql-types`, `sql-expr`, `sql-parser`, `sql-join`, `sql-vm`, `sql-codegen` merged into one crate, `db-core` (lib name `db_core`), as modules (`types`, `expr`, `parser`, `join`, `vm`, `codegen`). Module boundaries unchanged; only the crate boundary went away. Cargo features renamed to stay unique in a flat namespace: `column`/`row` (parser) -> `parser-column`/`parser-row`; `batch`/`row`/`stream` (vm) -> `vm-batch`/`vm-row`/`vm-stream`; `batch`/`row`/`stream` (codegen) -> `codegen-batch`/`codegen-row`/`codegen-stream`. Consumers depending on the six old crates by name (e.g. `sql-vm = { git = ..., package = "sql-vm", features = ["batch"] }`) need one `db-core` dependency instead, with the renamed features (e.g. `features = ["parser-column", "vm-batch"]`).

### Removed

- `sql-vfs`, `sql-pager`, `sql-header`, `sql-record`, `sql-sys` (#39): moved out of `db-core` into `db-storage`'s new `row` module per [ADR 0006](.openspec/adr/0006-storage-consolidation-into-db-storage.md) -- ADR 0003/0004 updated with pointers to the new location, their extraction reasoning otherwise unchanged. `sql-sys` doesn't move as a crate: `termios` deleted outright (dead code), `fcntl` folded into `db-storage`'s `row::vfs` as a private module (its only consumer, both before and after).

## [0.11.0] - 2026-09-04

### Added

- `sql-codegen`: new crate, structured exactly like `sql-vm` (`batch`/`row`/`stream` modules, matching Cargo feature pattern, `batch` on by default) (#20). `batch` ports column-rs's private `src/codegen.rs` as the canonical rendering layer (`render_flat`/`render_joined`/`render_semi_join`/`render_windowed` and helpers, plus `AggPart`) — generalized to take a `crate_name` parameter instead of hardcoding `"column_rs"` into generated source, so other `sql_vm::batch` consumers can reuse it. `row` is a documented stub recording sqlite-rs's real `src/codegen/*` structure (20,548 lines) as the port target, blocked on `sql_vm::row` (#18) existing first. `stream` is a pure stub, matching `sql_vm::stream`.

## [0.10.0] - 2026-09-03

### Added

- `sql-header`: SQLite database header (bytes 0-99) parsing, extracted verbatim from sqlite-rs's `src/header` (#15) -- pulled in ahead of `sql-pager` since pager's `JournalMode`/`SynchronousMode` enums live here (see ADR 0004 for why). All 16 of its original tests pass unchanged.
- `sql-pager`: page cache, WAL, rollback journal, and freelist management, extracted verbatim from sqlite-rs's `src/pager/*` (#15), built against `sql-vfs` (#14) and `sql-header`. `PagerError`/`WalError`/`JournalError`/`FreelistError` move unchanged in shape, per this session's standing decision not to speculatively centralize error types into `sql-error`. See ADR 0004 for the crate-split adaptations this forced (a `SharedPager` newtype replacing an orphan-rule-violating `impl PageSource for RefCell<Pager>`, a new `sql-vfs` `test-util` feature since `#[cfg(test)]` doesn't cross crate boundaries, and a couple of `pub(crate)` promotions). 78 of pager's tests pass unchanged; its `mod fixtures` integration tests (which need `btree`/`schema`/`record` together) stay in sqlite-rs until those extraction phases land.

### Fixed

- `sql-vfs`: added the missing `[lints.clippy]` baseline (matching sqlite-rs's own and `sql-record`'s/`sql-header`'s) -- its own `#[allow(clippy::unwrap_used, ...)]` annotations assumed this was already enforced.
- `sql-vfs`: `cargo test -p sql-vfs` alone never built its own `src/bin/lock_probe.rs` helper binary (`cargo test` doesn't build sibling `bin` targets automatically) -- the merged `#14` test suite only ever passed locally because `--bins` had been built manually first. `Makefile`'s `test` target now builds `lock_probe` explicitly first, matching sqlite-rs's own Makefile.

## [0.10.0] - 2026-09-03

### Added

- `sql-parser::column`: unary minus/plus (`-x`, `+x`) and `||` string concatenation (#34), growing column-rs's grammar toward DuckDB parity rather than SQLite's — `||` binds looser than `+`/`-`/`*`/`/` (DuckDB/Postgres precedence), deliberately not SQLite's own tighter-binding placement. `sql_expr` gains `BinOp::Concat`/`Expr::Neg`; `sql_vm::batch` gains `MapOp::Concat` (stringifies both operands) and `MapOp::Neg` (`Int`/`Float` negate, `Null` otherwise).

## [0.9.0] - 2026-09-03

### Added

- `sql-parser`: `sql_parser::row::grammar` (sqlite-rs's recursive-descent `Parser`), `row::error` (three-way `ParseOutcome`), and `row::printer` (AST pretty-printer) migrated in unchanged — completes `row`'s parser migration (#23). `row` now re-exports 14 `parse_*` functions and `ParseOutcome` at its module root, mirroring sqlite-rs's own `src/parser.rs`. All 82 of their original tests pass unchanged.
- `sql-vfs`: virtual filesystem abstraction (journal-mode `fcntl` locking, WAL `-shm` reader-mark/checkpoint/write-lock coordination via `pread`/`pwrite`), extracted verbatim from sqlite-rs's `src/vfs/*` (#14). `db-storage`'s separate, minimal, mmap-based `{Vfs, VfsFile}` is deliberately left as its own trait rather than unified with this one -- see ADR 0003 for why (sqlite-rs's own ADR-0001/ADR-0009 already rejected `mmap` for anything with concurrent-mutation exposure, which is `db-storage`'s entire reason to exist for its one read-only consumer). sqlite-rs's own `src/vfs/*` is untouched for now -- switching it over to depend on this crate is tracked separately in sqlite-rs's own repo.

## [0.8.0] - 2026-09-03

### Added

- `sql-parser`: `sql_parser::row::ast`, sqlite-rs's own AST (~15 DDL/DML/transaction/`PRAGMA` statement types) migrated in unchanged — second slice of `row`'s grammar migration (#23). Amends `ADR 0002`: `row` and `column` do not share one AST type after all (folding sqlite-rs's AST into `sql_expr::Query` would redesign an already-tested shape for no consumer that needs the two unified); they still share the Cargo-feature split and `sql_parser::Span`.

## [0.7.0] - 2026-09-03

### Added

- `sql-sys`: vendored POSIX syscall bindings (`fcntl` byte-range locking, `termios` raw mode), extracted verbatim from sqlite-rs's `src/sys/*` -- the lowest-level, dependency-free module in that crate's vendored-syscall layer (#11). db-core's sole `#![allow(unsafe_code)]` carve-out; every other workspace crate `#![forbid(unsafe_code)]`s. sqlite-rs's own `src/sys/*` is untouched for now -- switching it over to depend on this crate is tracked separately in sqlite-rs's own repo.

## [0.6.0] - 2026-09-03

### Added

- `sql-parser`: `sql_parser::row::tokenizer`, migrated unchanged from sqlite-rs's `src/parser/tokenizer.rs` — first real slice of `row`'s grammar migration (#23). Reuses `sql_parser::Span` rather than a second duplicate `Span` type. All 36 of its original tests pass unchanged.

## [0.5.0] - 2026-09-03

### Added

- `sql-parser`: split into `column`/`row` Cargo-feature-gated sections (`column` on by default), mirroring `sql-vm`'s `batch`/`row`/`stream` split (ADR 0001) — decided in `ADR 0002`. `sql_parser::column` holds column-rs's existing grammar (moved unchanged, re-exported at the crate root); `sql_parser::row` is a documented stub reserved for sqlite-rs's grammar migration (tracked separately, #23/#24).

## [0.4.1] - 2026-09-03

### Changed

- `sql-error` folded into `sql-parser` as a module (`sql_parser::span`, re-exported as `sql_parser::Span`); the crate had exactly one consumer, so its own `Cargo.toml`/workspace member was premature modularization (#8). No behavior change.

## [0.4.0] - 2026-09-03

### Added

- `sql-vm`: `Opcode::Window` for window functions (`ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, `SUM`/`AVG`/`COUNT OVER`), a 1:1 port of column-rs's private `compute_window` — partitions live rows, sorts each partition by `ORDER BY`, and writes one value per row (in original row order) into a `dst` register.

## [0.3.0] - 2026-09-03

### Added

- `sql-vm`: `VmError` now carries `opcode: &'static str` naming the instruction that failed (execution-time error context, matching sqlite-rs's `ExecError` pattern).
- `sql-vm`: `MAX_STEPS` bounded-execution guard (10M instructions), preventing pathological/buggy compiled programs from running indefinitely. `Vm::execute`/`Vm::run` fail with `VmError::StepLimitExceeded` once the limit is exceeded.
- `sql-vm`: `Opcode::name()` method returning each variant's runtime name, used as context in `VmError` messages.

### Changed

- `sql-vm`: `VmError` variants now struct-shaped to carry `opcode` and other context fields, improving error diagnostics.

## [0.2.0] - 2026-09-03

### Added

- `sql-vm`: `Opcode::HashBuild`/`Opcode::HashProbe` for equi-joins, backed by `sql-join::JoinHashTable`. Supports `INNER`/`LEFT`/`SEMI`/`ANTI`; NULL-safe join keys via a new `JoinKey` wrapper. `RIGHT`/`FULL`/`CROSS JOIN` and `Opcode::Window` remain out of scope (tracked separately).
- `sql-vm`: `Vm::clear_registers()`, for callers switching the live register set between a build-side and probe-side program run.

## [0.1.4] - 2026-09-03

### Added

- `sql-error`: `Span` (line/column/byte-offset), threaded through `sql-parser`'s `ParseError` so a consumer (REPL, IDE) can point at *where* parsing failed, not just read a message.
- ADR 0001: layered, synergetic architecture across db-core (`.openspec/adr/`).

## [0.1.3] - 2026-09-02

### Changed

- `sql-vm`: batch/row/stream executors gated behind Cargo features, so a consumer compiles only the one(s) it actually uses.

## [0.1.2] - 2026-09-02

### Added

- `sql-vm`: `BatchExecutor` (implemented); row/stream execution modes stubbed.
- `sql-join`: `JoinKind` and `should_emit` for equi-join semantics (inner/left/right/full/cross).
- `sql-parser`: `SELECT *`, table aliases, `CROSS`/`RIGHT`/`FULL JOIN`, `NOT`, `IS [NOT] NULL`.
- `sql-expr`: `JoinKind::{Right,Full,Cross}`, `SelectItem::Star`, `Expr::{Not,IsNull}`.
- `.openspec/` scaffolding (`adr/`, `specs/`).
- Unit tests for `sql-expr` AST types and `AggFunc::from_name`.

### Fixed

- README: db-core listed only 3 of its 4 workspace crates, omitting `sql-join`.

## [0.1.1] - 2026-09-02

### Added

- `sql-join`: `JoinHashTable`, a flat open-addressing multimap.

## [0.1.0] - 2026-09-02

### Added

- Initial workspace layout: `sql-types`, `sql-expr`, `sql-parser`.
- Makefile (`help`, `build`, `test`, `test-lib`, `lint`, `version`).
