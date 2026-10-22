ROOT := $(shell cd $(dir $(lastword $(MAKEFILE_LIST)))/../../.. && pwd)
TEST_01 := 01_zstd_decoder

.PHONY: help
help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

.PHONY: check
check: ## Run the correctness test (debug, fast, no perf numbers)
	cd $(ROOT) && cargo test --features storage-column --test $(TEST_01)

.PHONY: run
run: ## Run the perf spike against ruzstd (release, prints timings)
	cd $(ROOT) && cargo test --release --features storage-column --test $(TEST_01) -- --ignored --nocapture
