# db-core (single crate: types, expr, parser, join, vm, codegen, emit)

.DEFAULT_GOAL := help

.PHONY: help check-sqlite-profile check-stream-profile check-column-profile check-column-oracle gen-parquet-fixtures test test-lib test-spike build lint check-panic-allows check-deny check-mvl-limit coverage check-coverage ci perf perf-profile version fuzz-sql fuzz-diff fuzz-gen

help: ## Show this help
	@echo ""
	@awk 'BEGIN {FS = ":.*?## "} \
	  /^# === .* ===$$/  { sub(/^# === /, ""); sub(/ ===$$/, ""); printf "\n\033[33m%s\033[0m\n", $$0 } \
	  /^[a-zA-Z0-9_-]+:.*?## / { printf "  \033[36m%-24s\033[0m %s\n", $$1, $$2 }' \
	  $(MAKEFILE_LIST)
	@echo ""

# === Build ===

build: ## Build with all features
	cargo build --all-features

# === Test ===

# Every [[test]] target's name whose source is NOT under tests/spike/
# (naming any `--test` turns off cargo's target autodiscovery, which is
# exactly how tests/spike/ stays out of a run that lists these). Spikes
# are throwaway experiments (ADR 0015, tier 5): they must not run under
# the default test/coverage gates, nor count toward coverage -- use
# `make test-spike` to run them explicitly.
NON_SPIKE_TESTS := $(shell cargo metadata --no-deps --format-version 1 2>/dev/null \
	| python3 -c "import json,sys; \
	  pkg = next(p for p in json.load(sys.stdin)['packages'] if p['name'] == 'db-core'); \
	  print(' '.join('--test '+t['name'] for t in pkg['targets'] \
	    if 'test' in t['kind'] and '/tests/spike/' not in t['src_path']))")

test: ## Run db-core's test suite with all features (spikes excluded; see make test-spike)
	cargo test -p db-core --all-features --lib $(NON_SPIKE_TESTS)

test-lib: ## Just db-core's library unit tests (fastest inner loop)
	cargo test -p db-core --all-features --lib


SPIKE_TESTS := $(shell cargo metadata --no-deps --format-version 1 2>/dev/null \
	| python3 -c "import json,sys; \
	  pkg = next(p for p in json.load(sys.stdin)['packages'] if p['name'] == 'db-core'); \
	  print(' '.join('--test '+t['name'] for t in pkg['targets'] \
	    if 'test' in t['kind'] and '/tests/spike/' in t['src_path']))")

test-spike: ## Run only the throwaway experiments under tests/spike/
	cargo test -p db-core --all-features $(SPIKE_TESTS)

# === Fuzz (db-core#545 -- grammar-driven SQL fuzzing epic #543) ===
#
# `fuzz-sql` is the totality probe: generate N statements from
# src/parser/grammar.ebnf with catalog-aware terminals, run each through
# parser::row -> codegen::row -> vm::row (each stage under catch_unwind,
# TIMEOUT_MS per statement; STAGE=parse|codegen stops the chain early) and
# record only panics/hangs/corruption as findings
# under OUT (findings.jsonl + one .sql per hit). Typed rejections at any
# stage are the expected outcome and are counted, not reported. Exit 3
# when findings were recorded. REPLAY=<seed>:<index> re-runs one hit.
# Oracle comparison (#546) and column/stream targets (#547) come next.
# `fuzz-gen` is the generation-only view from #544.
TARGET ?= row
N ?= 1000
SEED ?= 1
MAX_DEPTH ?= 16
TIMEOUT_MS ?= 2000
OUT ?= target/fuzz
STAGE ?= vm
JOBS ?= 1
# The pinned oracle (parse.y / fixtures: 3.53.4). Homebrew's sqlite is
# preferred over the system one when present; ORACLE_BIN= overrides.
ORACLE_BIN ?= $(shell test -x /opt/homebrew/opt/sqlite/bin/sqlite3 && echo /opt/homebrew/opt/sqlite/bin/sqlite3 || echo sqlite3)

fuzz-sql: ## Totality-fuzz TARGET=row: N statements from grammar.ebnf through parse/codegen/vm (STAGE=parse|codegen|vm, JOBS=n, SEED=, MAX_DEPTH=, TIMEOUT_MS=, OUT=, REPLAY=seed:idx)
	TARGET=$(TARGET) N=$(N) SEED=$(SEED) MAX_DEPTH=$(MAX_DEPTH) TIMEOUT_MS=$(TIMEOUT_MS) OUT=$(OUT) STAGE=$(STAGE) JOBS=$(JOBS) cargo run -q -p fuzz-run --bin run

fuzz-diff: ## Differential-fuzz TARGET=row against the pinned sqlite3 oracle: fuzz-sql knobs + ORACLE_BIN=, REDUCE=0 (exit 4 on mismatches, 3 on panics)
	TARGET=$(TARGET) N=$(N) SEED=$(SEED) MAX_DEPTH=$(MAX_DEPTH) TIMEOUT_MS=$(TIMEOUT_MS) OUT=$(OUT) STAGE=$(STAGE) JOBS=$(JOBS) ORACLE=1 ORACLE_BIN=$(ORACLE_BIN) cargo run -q -p fuzz-run --bin run

fuzz-gen: ## Generate N statements for TARGET=row|column|stream from grammar.ebnf without running them (SEED=, MAX_DEPTH=)
	TARGET=$(TARGET) N=$(N) SEED=$(SEED) MAX_DEPTH=$(MAX_DEPTH) cargo run -q -p fuzz-gen --bin gen

# Scanned file set for `test-mcdc`: all of `src/`, not a curated subset --
# no obligation is exempted by file selection (ADR 0015, tier 3).
MCDC_FILES := $(shell find src -name '*.rs' | LC_ALL=C sort)

mcdc-obligations: ## Regenerate the committed MC/DC obligations snapshot (tests/mcdc/obligations.json)
	@command -v cargo-mvl-mcdc >/dev/null 2>&1 || { \
		echo "cargo-mvl-mcdc not found — install with:"; \
		echo "  cargo install --git https://github.com/mvl-lang/mvl-rust rust-mcdc --bin cargo-mvl-mcdc"; \
		exit 1; \
	}
	@mkdir -p tests/mcdc
	@cargo-mvl-mcdc scan -o tests/mcdc/obligations.json $(MCDC_FILES)
	@echo "wrote tests/mcdc/obligations.json — commit it alongside the source change that added or edited a decision"

# The committed snapshot must match the source: `unit_mcdc_discharge` only
# checks that tagged tests name ids that exist, so a stale snapshot passes
# until the next regeneration surfaces every collision at once (0.78.1's
# main had two). Regenerate to a scratch file and compare byte-for-byte.
check-mcdc-fresh: ## tests/mcdc/obligations.json matches a fresh scan of src/
	@cargo-mvl-mcdc scan -o target/obligations.fresh.json $(MCDC_FILES) \
		&& cmp -s target/obligations.fresh.json tests/mcdc/obligations.json \
		&& echo "check-mcdc-fresh: snapshot is current" \
		|| { echo "check-mcdc-fresh: tests/mcdc/obligations.json is stale -- run make mcdc-obligations and commit it"; exit 1; }

test-mcdc: mcdc-obligations ## MC/DC dashboard for all of src/; fails if any multi-leaf obligation is undischarged (VERBOSE=1 for per-obligation detail)
	# `harvest` re-runs `cargo test` itself (it has no `--features` flag
	# of its own) and joins on tagged test names regardless of overall
	# suite pass/fail (per-test outcome, not exit status) -- the tagged
	# tests are ordinary #[test] fns already run under `make test`; this
	# target is an additional coverage *view*, not a separate test run.
	# Every feature is `default` (db-core#111) specifically so this bare
	# `cargo test` still reaches every scanned module (vm-row,
	# codegen-row, ...), not just what a curated file list would imply.
	cargo-mvl-mcdc harvest --obligations=tests/mcdc/obligations.json --run-dir=. 2>/dev/null \
		| python3 tools/mcdc_report.py $(if $(filter 1,$(VERBOSE)),--verbose,)

COVERAGE_MIN := 85

# ADR-0000 (g): the coverage floor accounts for all of db-core, and every
# exception is named here with a justification. There are none: #570's
# unwired hash-agg spike, the only previous entry, was deleted (#407)
# rather than exempted. `^$` matches nothing.
COVERAGE_EXCLUDE_REGEX := ^$$

coverage: ## Line coverage report over the library + tests/unit (cargo-llvm-cov); spikes excluded
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { \
		echo "cargo-llvm-cov not found — install with: cargo install cargo-llvm-cov --locked"; \
		echo "  (also needs: rustup component add llvm-tools-preview)"; \
		exit 1; \
	}
	# Ported from sqlite-rs. Same test set `make test` runs -- `--lib`
	# plus NON_SPIKE_TESTS -- under cargo-llvm-cov instrumentation
	# instead of plain `cargo test`.
	cargo llvm-cov clean --workspace
	cargo llvm-cov --locked --all-features --no-report --lib $(NON_SPIKE_TESTS)
	cargo llvm-cov report --ignore-filename-regex '$(COVERAGE_EXCLUDE_REGEX)'
	cargo llvm-cov report --ignore-filename-regex '$(COVERAGE_EXCLUDE_REGEX)' --json --output-path target/llvm-cov.json

# db-core#407: one 85% floor over all of db-core, checked two ways from
# the same `--all-features` run -- the crate-wide total (as before) and,
# new, every individual file. A crate-wide-only floor lets thin files
# hide behind well-covered ones; this doesn't move if even one file
# regresses under the line, and lists every offender in one run rather
# than the next PR discovering the next file.
check-coverage: coverage ## Gate: fail if line coverage is below $(COVERAGE_MIN)% crate-wide or in any one file
	@python3 -c "import json, os, sys; \
	  root = os.getcwd() + '/'; \
	  d = json.load(open('target/llvm-cov.json'))['data'][0]; \
	  total = d['totals']['lines']['percent']; \
	  print(f'Line coverage: {total:.2f}% (threshold: $(COVERAGE_MIN)%)'); \
	  below = [(f['summary']['lines']['percent'], f['filename'].removeprefix(root)) for f in d['files'] if f['summary']['lines']['percent'] < $(COVERAGE_MIN)]; \
	  below.sort(); \
	  [print(f'  {pct:6.2f}%  {name}') for pct, name in below]; \
	  print(f'{len(below)} file(s) below $(COVERAGE_MIN)%' if below else 'every file is at or above $(COVERAGE_MIN)%'); \
	  sys.exit(0 if total >= $(COVERAGE_MIN) and not below else 1)"

# === Gates ===

lint: ## Run clippy (deny warnings), check formatting, and the panic-allow policy gate
	cargo clippy --all-targets --all-features -- -D warnings
	cargo fmt --all -- --check
	$(MAKE) check-panic-allows

# The panic lints in Cargo.toml are a *production* rule (ADR 0015):
# production returns typed errors, tests fail fast. clippy.toml's
# `allow-*-in-tests` + lib.rs's `cfg_attr(test, allow(...))` scope the
# lints so test code needs no per-module `#[allow]`; this gate then
# states the policy directly -- no `#[allow(clippy::{unwrap_used,
# expect_used,panic,unreachable,todo,unimplemented})]` anywhere in
# production `src/`. Its EXEMPT list is db-core#231's worklist.
check-panic-allows: ## Policy gate: no panic-lint allows in production src/ (see tools/check_panic_allows.py)
	@python3 tools/check_panic_allows.py

# Every feature must build on its own (with its declared implications and
# nothing else). `--all-features` hides a missing implication; a consumer
# enabling one feature finds it. 0.78.0 shipped `storage-row` without
# `parser-row` this way.
FEATURES := storage-row storage-column storage-stream engine-row engine-column engine-stream parser-row vm-row codegen-row vm-batch codegen-batch emit-batch

check-features: ## Each Cargo feature builds standalone (cargo check --no-default-features --features X)
	@for f in $(FEATURES); do \
		printf '  %-16s' "$$f"; \
		if out=$$(cargo check -q --no-default-features --features $$f 2>&1); then echo ok; \
		else echo FAIL; echo "$$out" | grep -E '^error' | head -5; exit 1; fi; \
	done

# ADR 0000 §Invariants (a)-(c): first-party-only closure, the named
# `unsafe` carve-outs and nothing else, no batch/column/stream file --
# all measured on what rustc actually compiles for the profile.
check-sqlite-profile: ## Charter gate: the SQLite profile is dependency-free, unsafe-confined, mode-isolated (tools/check_sqlite_profile.py)
	@python3 tools/check_sqlite_profile.py sqlite

# The stream profile: the log engine's execution mode (db-core#403). ADR
# 0000 §Invariants (a)/(b) measured here too -- no mode-isolation check,
# since the stream profile legitimately compiles vm-batch/codegen-batch.
check-stream-profile: ## Charter gate: the stream profile is dependency-free and unsafe-confined (tools/check_sqlite_profile.py)
	@python3 tools/check_sqlite_profile.py stream

# The column profile: the Parquet analytics mode (db-core#405). ADR 0000
# §The column profile: not first-party-only by design (memmap2/ruzstd),
# so only invariant (b) -- the unsafe carve-out -- is checked here.
check-column-profile: ## Charter gate: the column profile carries exactly its named unsafe carve-out (tools/check_sqlite_profile.py)
	@python3 tools/check_sqlite_profile.py column

# ADR 0000 §(f), db-core#406: the column half of oracle parity -- the
# column engine's query results against DuckDB reading the same Parquet
# fixtures. Deliberately local-only, not a CI gate and not in the weekly
# assurance workflow: unlike sqlite3 (a system package `apt install`s),
# a DuckDB binary in CI is a new external dependency with its own supply
# chain, and this charter's whole ethos is minimizing exactly that. Run by
# hand before/after a change to `storage::column` or `vm::batch`'s
# aggregate merge path; needs `duckdb` on PATH (`brew install duckdb` or
# https://duckdb.org/docs/installation).
check-column-oracle: ## Local-only: column engine query results agree with DuckDB over Parquet fixtures (tools/check_column_oracle.sh)
	@tools/check_column_oracle.sh

gen-parquet-fixtures: ## Regenerate the codec-variant Parquet fixtures check-column-oracle reads (tools/gen_parquet_fixtures.sh)
	@tools/gen_parquet_fixtures.sh

check-deny: ## Supply-chain policy: license/ban/source checks (see deny.toml)
	@command -v cargo-deny >/dev/null 2>&1 || { \
		echo "cargo-deny not found — install with: cargo install cargo-deny --locked"; \
		exit 1; \
	}
	cargo deny check

# cargo-mvl-limit is not published to crates.io; install from source at a
# pinned rev (see .github/workflows/ci.yml for the version this repo gates
# on). This is the "qualified subset" gate: it flags language features
# (explicit lifetimes, dyn dispatch, non-allowlisted macros, ...) outside
# the subset this codebase holds itself to.
#
# The designated `dyn` boundary is exempt, same convention as sqlite-rs's
# `src/vfs.rs`: `vm/row`'s Cursor/Transaction/CursorFactory/SchemaStorage
# are ADR 0008's storage-agnostic extension point, implemented by
# *downstream* crates (sqlite-rs over its own b-tree) that db-core cannot
# name at compile time -- an open implementor set generics can't express.
# The exclude list names the files that actually hold a `dyn` over that
# boundary: the Cursor and CursorFactory trait files, `vm.rs` (which owns
# the boxed cursors) and the `cursor_conformance.rs` harness that checks
# implementors through `&dyn Cursor`. `transaction.rs` and
# `schema_storage.rs` define boundary traits too, but contain no `dyn`
# themselves and so pass the gate unexempted. Everything above the
# boundary stays in the qualified subset. Adding a file here is an
# architecture decision (ADR 0008, ADR 0015), not a lint fix.
#
# Storage boundary files (ADR 0016 §Gates, #289): the VFS traits and their
# implementors (`vfs.rs`, `page_source.rs`, `unix.rs`, `memory.rs`) are an
# open-implementor `dyn` boundary exactly like `vm/row/cursor*.rs`; the
# Parquet reader (`parquet/*`) is a zero-copy reader over the mmap and
# carries the lifetimes that implies -- designated a boundary rather than
# rewritten to own its buffer; `fcntl.rs` and `mmap.rs` hold the two
# audited `unsafe` carve-outs. Everything else under `src/storage/row` and
# `src/storage/column` is in the qualified subset. `src/storage/stream/*`
# stays excluded while #304/#305 build it (lifted with #305).
#
# `src/engine/row.rs`, `src/engine/row/adapter.rs` and `src/engine/column.rs`
# (ADR 0017; column.rs holds `ParquetFile<'a>`/`RowGroupSegment<'a, 'm>`): the
# implementors of that same ADR 0008 boundary -- they hand `Box<dyn
# CursorFactory>`/`Box<dyn Transaction>`/`Box<dyn SchemaStorage>` to the Vm
# and hold `Rc<dyn PageSource>`. Same exemption, same reason, as vm.rs.
#
# `src/json_path.rs` (#307): `JsonValue<'a>` is a zero-copy parser --
# every variant borrows string spans from the source line rather than
# allocating, which is the whole point (`storage::stream::json`'s
# ingest-time cost budget, ADR 0018) -- so it carries the same explicit-
# lifetime shape as `ParquetFile<'a>` above, moved here from the already-
# excluded `src/storage/stream/*` so `functions::json_extract` can share
# it without a second parser.
MVL_LIMIT_EXCLUDE := src/vm/row/vm.rs src/vm/row/cursor.rs src/vm/row/cursor_factory.rs src/vm/row/cursor_conformance.rs src/engine/row.rs src/engine/row/adapter.rs src/engine/column.rs src/storage/row/vfs.rs src/storage/row/vfs/page_source.rs src/storage/row/vfs/unix.rs src/storage/row/vfs/memory.rs src/storage/row/vfs/fcntl.rs src/storage/column/mmap.rs src/storage/column/parquet/* src/storage/column/parquet/compression/* src/storage/stream/* src/engine/stream.rs src/json_path.rs

check-mvl-limit: ## Qualified-subset gate (cargo-mvl-limit) over src/, minus the documented dyn boundary (MVL_LIMIT_EXCLUDE)
	@command -v cargo-mvl-limit >/dev/null 2>&1 || { \
		echo "cargo-mvl-limit not found — install with:"; \
		echo "  cargo install --git https://github.com/mvl-lang/mvl-rust rust-limit --bin cargo-mvl-limit --locked"; \
		exit 1; \
	}
	@cargo mvl-limit $$(find src -name '*.rs' $(foreach e,$(MVL_LIMIT_EXCLUDE),-not -path '$(e)') | sort) \
		&& echo "check-mvl-limit: all files in the qualified subset"

# db-core#360: `tests/public_api.md` is a committed snapshot of the crate's
# public API (`cargo public-api`, `-ss` to omit blanket/auto-trait impls --
# otherwise every type's `Send`/`Sync`/`Into`/`From` derivations dominate
# the diff). `#![warn(missing_docs)]` already proves every `pub` item is
# documented; this proves the *set* of `pub` items doesn't grow without a
# reviewed diff, the same "committed evidence, regenerable on demand" split
# `tests/mcdc/obligations.json` uses above. Needs nightly (rustdoc JSON).
#
# CI (`ubuntu-latest`, x86_64-unknown-linux-gnu) is the authoritative
# environment for this snapshot. `src/storage/row/vfs/fcntl.rs::fsync` is
# `cfg(target_os = "macos")`-only (#652), so rendering for the *host*
# target adds that one line spuriously on a macOS machine (and would drop
# a Linux-only item the same way on Linux). Pinning `--target
# x86_64-unknown-linux-gnu` here makes `make public-api` and `make
# check-public-api-fresh` produce CI's exact output from any host --
# `rustup target add x86_64-unknown-linux-gnu` once if it's missing
# (rustdoc JSON generation only needs the target's std metadata, not a
# linker, so this works without cross-compilation tooling).
define RENDER_PUBLIC_API
	echo "# db-core public API"; \
	echo; \
	echo "Generated by \`make public-api\` (\`cargo public-api\`, \`-ss\` to omit blanket"; \
	echo "and auto-trait impls -- otherwise every public type's \`Send\`/\`Sync\`/\`Into\`/"; \
	echo "\`From\` auto-derivations dominate the diff). Regenerate with \`make"; \
	echo "public-api\` whenever a change adds, removes, or changes the signature of a"; \
	echo "\`pub\` item; \`make check-public-api-fresh\` (part of \`make ci\`) fails the"; \
	echo "build if this file is stale, so a public API change without a regenerated"; \
	echo "snapshot is a build failure, not a silent widening. Rendered for"; \
	echo "\`x86_64-unknown-linux-gnu\` (CI's target) regardless of host, so"; \
	echo "\`fcntl::fsync\` (macOS-only, #652) is consistently absent."; \
	echo; \
	echo '```text'; \
	cargo +nightly public-api --target x86_64-unknown-linux-gnu --all-features -ss 2>/dev/null; \
	echo '```'
endef

public-api: ## Regenerate the committed public API snapshot (tests/public_api.md)
	@command -v cargo-public-api >/dev/null 2>&1 || { \
		echo "cargo-public-api not found — install with: cargo install cargo-public-api --locked"; \
		exit 1; \
	}
	@rustup target add x86_64-unknown-linux-gnu >/dev/null 2>&1 || true
	@{ $(RENDER_PUBLIC_API); } > tests/public_api.md
	@echo "wrote tests/public_api.md — commit it alongside the source change that widened or narrowed the public API"

check-public-api-fresh: ## tests/public_api.md matches a fresh cargo-public-api scan
	@command -v cargo-public-api >/dev/null 2>&1 || { \
		echo "cargo-public-api not found — install with: cargo install cargo-public-api --locked"; \
		exit 1; \
	}
	@rustup target add x86_64-unknown-linux-gnu >/dev/null 2>&1 || true
	@mkdir -p target
	@{ $(RENDER_PUBLIC_API); } > target/public_api.fresh.md
	@cmp -s target/public_api.fresh.md tests/public_api.md \
		&& echo "check-public-api-fresh: snapshot is current" \
		|| { echo "check-public-api-fresh: tests/public_api.md is stale -- run make public-api and commit it"; diff -u tests/public_api.md target/public_api.fresh.md || true; exit 1; }

# === CI ===

ci: ## Run every CI gate locally, same order as .github/workflows/ci.yml
	$(MAKE) lint
	$(MAKE) check-deny
	$(MAKE) check-features
	$(MAKE) check-sqlite-profile
	$(MAKE) check-stream-profile
	$(MAKE) check-column-profile
	$(MAKE) check-mcdc-fresh
	$(MAKE) check-mvl-limit
	$(MAKE) check-public-api-fresh
	$(MAKE) test
	$(MAKE) check-coverage
	@echo "all CI gates passed"

# === Performance ===

perf: ## Run the parser/codegen/vm_opcodes benchmarks (report only, not a CI gate; std-only harness, JSON under target/perf/ -- ADR 0015 tier 6)
	cargo bench --bench parser
	cargo bench --bench codegen
	cargo bench --bench vm_opcodes
	cargo bench --bench stream_materialize
	cargo bench --bench cross_mode_lookup
	cargo bench --bench wal_commit

BENCH ?= codegen

perf-profile: ## Sampling profile of one bench (BENCH=parser|codegen|vm_opcodes) via Instruments' Time Profiler; prints hot functions (macOS; ADR 0015 tier 6)
	@command -v xctrace >/dev/null 2>&1 || { echo "xctrace not found -- needs Xcode command line tools (macOS)"; exit 1; }
	python3 tools/perf_profile.py $(BENCH)

# === Release ===

version: ## Print the crate's current version
	@sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -1
