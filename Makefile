# db-core (single crate: types, expr, parser, join, vm, codegen)

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

# === Gates ===

lint: ## Run clippy (deny warnings) and check formatting
	cargo clippy --all-targets --all-features -- -D warnings
	cargo fmt --all -- --check

# === Release ===

version: ## Print the crate's current version
	@sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -1
