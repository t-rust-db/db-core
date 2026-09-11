# db-core (single crate: types, expr, parser, join, vm, codegen, emit)

.DEFAULT_GOAL := help

.PHONY: help check-sqlite-profile check-coverage-profile test test-lib test-spike build lint check-panic-allows check-deny check-mvl-limit coverage check-coverage ci perf perf-profile version

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

# Scanned file set for `test-mcdc`: all of `src/`, not a curated subset --
# no obligation is exempted by file selection (ADR 0015, tier 3).
MCDC_FILES := $(shell find src -name '*.rs')

mcdc-obligations: ## Regenerate the committed MC/DC obligations snapshot (tests/mcdc/obligations.json)
	@command -v cargo-mvl-mcdc >/dev/null 2>&1 || { \
		echo "cargo-mvl-mcdc not found — install with:"; \
		echo "  cargo install --git https://github.com/mvl-lang/mvl-rust rust-mcdc --bin cargo-mvl-mcdc"; \
		exit 1; \
	}
	@mkdir -p tests/mcdc
	@cargo-mvl-mcdc scan -o tests/mcdc/obligations.json $(MCDC_FILES)
	@echo "wrote tests/mcdc/obligations.json — commit it alongside the source change that shifted line numbers"

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

COVERAGE_MIN := 80

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
	cargo llvm-cov report
	cargo llvm-cov report --json --output-path target/llvm-cov.json

check-coverage: coverage ## Gate: fail if line coverage is below $(COVERAGE_MIN)%
	@python3 -c "import json, sys; \
	  p = json.load(open('target/llvm-cov.json'))['data'][0]['totals']['lines']['percent']; \
	  print(f'Line coverage: {p:.2f}% (threshold: $(COVERAGE_MIN)%)'); \
	  sys.exit(0 if p >= $(COVERAGE_MIN) else 1)"

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
FEATURES := storage-row storage-column storage-stream engine-row engine-column parser-row vm-row codegen-row vm-batch codegen-batch emit-batch

check-features: ## Each Cargo feature builds standalone (cargo check --no-default-features --features X)
	@for f in $(FEATURES); do \
		printf '  %-16s' "$$f"; \
		if out=$$(cargo check -q --no-default-features --features $$f 2>&1); then echo ok; \
		else echo FAIL; echo "$$out" | grep -E '^error' | head -5; exit 1; fi; \
	done

# The SQLite profile: the exact feature set sqlite-rs builds db-core with.
SQLITE_PROFILE := parser-row,vm-row,codegen-row,storage-row,engine-row

# ADR 0000 §Invariants (a)-(c): first-party-only closure, the named
# `unsafe` carve-outs and nothing else, no batch/column/stream file --
# all measured on what rustc actually compiles for the profile.
check-sqlite-profile: ## Charter gate: the SQLite profile is dependency-free, unsafe-confined, mode-isolated (tools/check_sqlite_profile.py)
	@python3 tools/check_sqlite_profile.py

# ADR 0000 §(g): the coverage floor as a claim about the safe-SQLite
# artifact -- instrumented over the profile, not --all-features. Too slow
# for every PR; .github/workflows/assurance.yml runs it weekly.
check-coverage-profile: ## Coverage floor over the SQLite profile (weekly assurance job)
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { \
		echo "cargo-llvm-cov not found — install with: cargo install cargo-llvm-cov --locked"; \
		exit 1; \
	}
	cargo llvm-cov clean --workspace
	cargo llvm-cov --locked --no-default-features --features $(SQLITE_PROFILE) --no-report --lib
	cargo llvm-cov report --json --output-path target/llvm-cov-profile.json
	@python3 -c "import json, sys; \
	  p = json.load(open('target/llvm-cov-profile.json'))['data'][0]['totals']['lines']['percent']; \
	  print(f'Line coverage (SQLite profile): {p:.2f}% (threshold: $(COVERAGE_MIN)%)'); \
	  sys.exit(0 if p >= $(COVERAGE_MIN) else 1)"

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
MVL_LIMIT_EXCLUDE := src/vm/row/vm.rs src/vm/row/cursor.rs src/vm/row/cursor_factory.rs src/vm/row/cursor_conformance.rs src/engine/row.rs src/engine/row/adapter.rs src/engine/column.rs src/storage/row/vfs.rs src/storage/row/vfs/page_source.rs src/storage/row/vfs/unix.rs src/storage/row/vfs/memory.rs src/storage/row/vfs/fcntl.rs src/storage/column/mmap.rs src/storage/column/parquet/* src/storage/column/parquet/compression/* src/storage/stream/*

check-mvl-limit: ## Qualified-subset gate (cargo-mvl-limit) over src/, minus the documented dyn boundary (MVL_LIMIT_EXCLUDE)
	@command -v cargo-mvl-limit >/dev/null 2>&1 || { \
		echo "cargo-mvl-limit not found — install with:"; \
		echo "  cargo install --git https://github.com/mvl-lang/mvl-rust rust-limit --bin cargo-mvl-limit --locked"; \
		exit 1; \
	}
	@cargo mvl-limit $$(find src -name '*.rs' $(foreach e,$(MVL_LIMIT_EXCLUDE),-not -path '$(e)') | sort) \
		&& echo "check-mvl-limit: all files in the qualified subset"

# === CI ===

ci: ## Run every CI gate locally, same order as .github/workflows/ci.yml
	$(MAKE) lint
	$(MAKE) check-deny
	$(MAKE) check-features
	$(MAKE) check-sqlite-profile
	$(MAKE) check-mcdc-fresh
	$(MAKE) check-mvl-limit
	$(MAKE) test
	@echo "all CI gates passed"

# === Performance ===

perf: ## Run the parser/codegen/vm_opcodes benchmarks (report only, not a CI gate; std-only harness, JSON under target/perf/ -- ADR 0015 tier 6)
	cargo bench --bench parser
	cargo bench --bench codegen
	cargo bench --bench vm_opcodes

BENCH ?= codegen

perf-profile: ## Sampling profile of one bench (BENCH=parser|codegen|vm_opcodes) via Instruments' Time Profiler; prints hot functions (macOS; ADR 0015 tier 6)
	@command -v xctrace >/dev/null 2>&1 || { echo "xctrace not found -- needs Xcode command line tools (macOS)"; exit 1; }
	python3 tools/perf_profile.py $(BENCH)

# === Release ===

version: ## Print the crate's current version
	@sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -1
