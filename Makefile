# db-core (single crate: types, expr, parser, join, vm, codegen, emit)

.DEFAULT_GOAL := help

.PHONY: help test test-lib build lint version

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

test: ## Run the full test suite with all features
	cargo test --all-features

test-lib: ## Just the library unit tests (fastest inner loop)
	cargo test --all-features --lib

# Scanned file set for `test-mcdc` (db-core#111): all of `src/`, not a
# curated subset -- no obligation is exempted by file selection.
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

# === Gates ===

lint: ## Run clippy (deny warnings) and check formatting
	cargo clippy --all-targets --all-features -- -D warnings
	cargo fmt --all -- --check

# === Release ===

version: ## Print the crate's current version
	@sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -1
