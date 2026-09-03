# Changelog

All notable changes to db-core. Format follows [Keep a Changelog](https://keepachangelog.com/), versioning follows [SemVer](https://semver.org/). Pre-1.0: minor bumps may break the public API.

**Versioning policy:** all workspace crates (`sql-types`, `sql-expr`, `sql-parser`, `sql-join`, `sql-vm`, `sql-sys`) version together, one tag per release.

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
