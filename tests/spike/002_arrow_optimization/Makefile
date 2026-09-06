ROOT := $(shell cd $(dir $(lastword $(MAKEFILE_LIST)))/../../.. && pwd)
TEST_01 := 01_groupreduce_key_hashing
TEST_02 := 02_groupreduce_key_hashing_adjusted

.PHONY: help
help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

.PHONY: check
check: ## Run both correctness tests (debug, fast, no perf numbers)
	cd $(ROOT) && cargo test --test $(TEST_01) --test $(TEST_02)

.PHONY: check-01
check-01: ## Run only the round-1 correctness test (debug)
	cd $(ROOT) && cargo test --test $(TEST_01)

.PHONY: check-02
check-02: ## Run only the round-2 (adjusted) correctness test (debug)
	cd $(ROOT) && cargo test --test $(TEST_02)

.PHONY: run-01
run-01: ## Run the round-1 perf spike (release, prints timings)
	cd $(ROOT) && cargo test --release --test $(TEST_01) -- --ignored --nocapture

.PHONY: run-02
run-02: ## Run the round-2 (adjusted, high-cardinality re-check) perf spike (release)
	cd $(ROOT) && cargo test --release --test $(TEST_02) -- --ignored --nocapture

.PHONY: run
run: run-01 run-02 ## Run both perf spikes (release)
