# db-storage

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

build: ## Build the crate (all features)
	cargo build --all-features

# === Test ===

test: ## Run the full test suite (all features)
	@# Build lock_probe helper binary first -- cargo test doesn't build [[bin]]
	@# targets automatically, and row::pager's tests need it too, not just
	@# row::vfs's own (migrated from db-core's Makefile, db-core#39).
	cargo build --features row --bin lock_probe
	cargo test --all-features

test-lib: ## Just the library unit tests, all features (fastest inner loop)
	cargo test --all-features --lib

# === Gates ===

lint: ## Run clippy (deny warnings) and check formatting, all features
	cargo clippy --all-features --all-targets -- -D warnings
	cargo fmt -- --check

# === Release ===

version: ## Print the crate's current version (Cargo.toml [package].version)
	@sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -1
