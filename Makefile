# db-core (workspace: sql-types, sql-expr, sql-parser, ...)

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

build: ## Build every crate in the workspace
	cargo build --workspace

# === Test ===

test: ## Run the full test suite across the workspace
	@# Build lock_probe helper binary first -- cargo test doesn't build [[bin]]/src/bin
	@# targets automatically, and sql-pager's tests need it too (dev-dependency on
	@# sql-vfs's "test-util" feature), not just sql-vfs's own.
	cargo build -p sql-vfs --bin lock_probe
	cargo test --workspace

test-lib: ## Just the library unit tests across the workspace (fastest inner loop)
	cargo test --workspace --lib

# === Gates ===

lint: ## Run clippy (deny warnings) and check formatting across the workspace
	cargo clippy --workspace --all-targets -- -D warnings
	cargo fmt --all -- --check

# === Release ===

version: ## Print each workspace member's current version
	@for f in */Cargo.toml; do \
	  name=$$(sed -n 's/^name *= *"\([^"]*\)".*/\1/p' $$f | head -1); \
	  ver=$$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' $$f | head -1); \
	  printf "%-16s %s\n" "$$name" "$$ver"; \
	done
