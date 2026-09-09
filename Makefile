# db-core (single crate: types, expr, parser, join, vm, codegen, emit)

.DEFAULT_GOAL := help

.PHONY: help test test-lib test-spike build lint check-panic-allows check-deny check-mvl-limit coverage check-coverage ci perf perf-profile version

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
# `src/storage/*` (ADR 0016, #288): the absorbed db-storage code was never
# under this gate -- explicit lifetimes (`'a`/`'m` on the Parquet reader,
# page sources, VFS traits) and the two audited `unsafe` carve-outs
# (`column::mmap`, `row::vfs::fcntl`). Excluded as a unit, tracked as a
# worklist in db-core#289; `src/storage.rs` itself stays in the scan.
MVL_LIMIT_EXCLUDE := src/vm/row/vm.rs src/vm/row/cursor.rs src/vm/row/cursor_factory.rs src/vm/row/cursor_conformance.rs src/storage/*

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
