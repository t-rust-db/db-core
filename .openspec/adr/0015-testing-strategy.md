# ADR 0015: db-core's testing strategy

## Status

Accepted. Resolves `#227`.

## Context

`db-core` is one of three crates with its own testing strategy
(`db-core`, `db-cli`, `db-storage`); the SQLite-compatible engine that
embeds them (`sqlite-rs`) has a fourth of its own. This ADR states what
`db-core` tests, at which tier, what gates a merge, and what this crate
deliberately leaves to the others. Nothing in `db-core`'s test layout
exists for another crate's benefit.

## Decision

### Tiers

1. **Unit tests** -- `#[cfg(test)]` modules inside each `src/` file,
   covering that file's own behaviour. Run by `make test-lib` and
   `make test`. This is where a decision's MC/DC vectors also live (tier
   3), next to the code.

2. **Public-API tests** -- `tests/unit/*.rs`, each a `[[test]]` target,
   exercising the crate as a consumer would: black-box compile-then-
   execute round trips through `codegen::row::dispatch` and `vm::row`
   against in-memory cursors (`codegen_roundtrip_test`,
   `codegen_cursor_factory_test`); one `*_public_api_test` per public
   surface (root modules, parser, `codegen::batch` and its emitter,
   `vm::row`, `vm::batch`, `vm::engine`/`vm::join`) plus
   `vm_row_hooks_test` for the storage hooks; totality at the
   nesting-depth cap (`nesting_depth_test`); and two policy checks that
   run as ordinary tests: `mcdc_discharge_test` (every tagged vector
   names a real obligation; obligation ids are unique across files) and
   `tests/version.rs` (`db_core::VERSION` equals `Cargo.toml`).

3. **MC/DC discharge** -- `make test-mcdc`. `cargo-mvl-mcdc` scans all
   of `src/` (no curated file list) into the committed snapshot
   `tests/mcdc/obligations.json`, then joins tests named
   `mcdc__<file-stem>_<line>__vN_<description>` to each multi-leaf
   decision. Every decision with two or more conditions must have its
   full vector set; a vector is an ordinary `#[test]` asserting
   observable behaviour, not a coverage stub. Because the tool runs a
   feature-less `cargo test`, every Cargo feature is on by default.

4. **Coverage** -- `make coverage` / `make check-coverage`
   (`cargo-llvm-cov`, `COVERAGE_MIN` = 80% line coverage over the library
   and `tests/unit`). A local floor, not a CI gate.

5. **Spikes** -- `tests/spike/`: throwaway experiments. Excluded from
   `make test` and from coverage; run only via `make test-spike`. A
   spike whose conclusion is recorded (in an ADR or an issue) is deleted.

6. **Performance report** -- `make perf`: micro-benchmarks under
   `benches/` for the parser (tokenize + parse), the planners
   (`codegen::row`/`codegen::batch` over an already-parsed AST) and a
   representative opcode per execution shape in both VMs, on the
   crate's own `std`-only harness (`benches/common`: warm-up, batched
   timed samples, min/median/p95 ns per call, JSON under
   `target/perf/`). Report only, never a gate; no benchmark asserts a
   number. `make perf-profile BENCH=<bench>` records the same binary
   under a sampling profiler (`tools/perf_profile.py`, Instruments on
   macOS) and ranks functions by self and inclusive time; the bench
   profile carries line tables so frames resolve to names without
   changing the measured codegen. `PERF_BUDGET_MS` lengthens a run for
   the profiler.

### Static gates

- **Lint** (`make lint`): `cargo clippy --all-targets --all-features -D
  warnings` with the production panic bar in `Cargo.toml`
  (`unwrap_used`, `expect_used`, `panic`, `unreachable`, `todo`,
  `unimplemented`, `indexing_slicing`, `arithmetic_side_effects`,
  `string_slice`, `cast_*` denied); `cargo fmt --check`; and the
  panic-allow policy (`tools/check_panic_allows.py`): no `#[allow]` of a
  panic lint anywhere in production `src/`. Test code has the opposite
  rule -- fail fast with `unwrap`/`expect`/`assert!` -- scoped by
  `clippy.toml`'s `allow-*-in-tests` and `lib.rs`'s `cfg_attr(test,
  allow(...))`, so inline test modules carry no allow header and each
  `tests/unit/*.rs` file carries the one canonical header.
- **Supply chain** (`make check-deny`): `cargo-deny` over licenses, bans
  and sources; the crate has zero third-party dependencies, runtime or
  dev.
- **Qualified subset** (`make check-mvl-limit`): `cargo-mvl-limit` over
  `src/` -- no `unsafe`, no `dyn`, no explicit lifetimes, allow-listed
  macros only -- minus the documented `dyn Cursor` boundary
  (`MVL_LIMIT_EXCLUDE`: `src/vm/row/{vm,cursor,cursor_factory,
  cursor_conformance}.rs`, ADR 0008). Adding a file to that list is an
  architecture decision, not a lint fix.

### What gates a merge

`make ci` runs, in the same order as `.github/workflows/ci.yml`: `lint`,
`check-deny`, `check-mvl-limit`, `test`. All four must pass. `test-mcdc`
and `check-coverage` are run locally before a PR that adds or moves
decisions; `test-mcdc` at 100% discharged is a merge requirement even
though it needs an external tool and is not in the workflow.

### What db-core does not test

- **Oracle parity against `sqlite3`** (corpus fixtures, sqllogictest,
  TCL-derived suites) -- the embedding engine's and the benchmark repo's
  job. `db-core` has no on-disk fixtures and never shells out to an
  oracle; `examples/oracle_check.rs` is a manual aid, not a suite.
- **Physical storage** (pages, WAL, b-tree, crash safety) -- `db-storage`.
- **Terminal and CLI behaviour** -- `db-cli`.
- **End-to-end performance** -- the benchmark repo. `db-core`'s own
  benchmarks (tier 6) isolate phases; they never assert a number.

## Consequences

- A change to `src/` that adds a multi-leaf decision is not complete
  until its vectors are in the same file and `make test-mcdc` is green;
  a change that shifts decision lines re-tags the affected vectors and
  commits the regenerated snapshot.
- A test layout convenient for another crate (a separate vector module,
  a drift check against a sibling tree) is not adopted here.
- Makefile comments point at this ADR for rationale instead of
  restating it.
