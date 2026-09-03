# Changelog

All notable changes to db-core. Format follows [Keep a Changelog](https://keepachangelog.com/), versioning follows [SemVer](https://semver.org/). Pre-1.0: minor bumps may break the public API.

**Versioning policy:** all workspace crates (`sql-types`, `sql-expr`, `sql-parser`, `sql-error`, `sql-join`, `sql-vm`) version together, one tag per release.

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
