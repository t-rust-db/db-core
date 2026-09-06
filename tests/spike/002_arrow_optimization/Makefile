ROOT := $(shell cd $(dir $(lastword $(MAKEFILE_LIST)))/../../.. && pwd)
TEST := 01_groupreduce_key_hashing

.PHONY: help
help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

.PHONY: check
check: ## Run the correctness test (debug, fast, no perf numbers)
	cd $(ROOT) && cargo test --test $(TEST)

.PHONY: run
run: ## Run the perf spike (release, prints timings)
	cd $(ROOT) && cargo test --release --test $(TEST) -- --ignored --nocapture
