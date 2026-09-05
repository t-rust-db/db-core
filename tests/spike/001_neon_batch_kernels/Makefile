ROOT := $(shell cd $(dir $(lastword $(MAKEFILE_LIST)))/../../.. && pwd)
TEST_01 := 01_neon_batch_kernels
TEST_02 := 02_neon_adjusted
TEST_03 := 03_batch_specific
TEST_04 := 04_adjusted

.PHONY: help
help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

.PHONY: check
check: ## Run all four fast correctness tests (debug, no perf numbers)
	cd $(ROOT) && cargo test --test $(TEST_01) --test $(TEST_02) --test $(TEST_03) --test $(TEST_04)

.PHONY: check-01
check-01: ## Run only the round-1 correctness test (debug)
	cd $(ROOT) && cargo test --test $(TEST_01)

.PHONY: check-02
check-02: ## Run only the round-2 (adjusted) correctness test (debug)
	cd $(ROOT) && cargo test --test $(TEST_02)

.PHONY: check-03
check-03: ## Run only the round-3 (batch-specific) correctness test (debug)
	cd $(ROOT) && cargo test --test $(TEST_03)

.PHONY: check-04
check-04: ## Run only the round-4 (adjusted ladder) correctness test (debug)
	cd $(ROOT) && cargo test --test $(TEST_04)

.PHONY: run-01
run-01: ## Run the round-1 NEON perf spike (release, prints timings)
	cd $(ROOT) && cargo test --release --test $(TEST_01) -- --ignored --nocapture

.PHONY: run-02
run-02: ## Run the round-2 (adjusted) NEON perf spike (release, prints timings + GB/s)
	cd $(ROOT) && cargo test --release --test $(TEST_02) -- --ignored --nocapture

.PHONY: run-03
run-03: ## Run the round-3 real vm::batch (Vm/Program/run_parallel) perf spike (release)
	cd $(ROOT) && cargo test --release --test $(TEST_03) -- --ignored --nocapture

.PHONY: run-04
run-04: ## Run the round-4 adjusted ladder (Load/+Map/+Emit increments, ns/row) perf spike (release)
	cd $(ROOT) && cargo test --release --test $(TEST_04) -- --ignored --nocapture

.PHONY: run
run: run-01 run-02 run-03 run-04 ## Run all four perf spikes (release)

.PHONY: asm-01
asm-01: ## Build release and count NEON vector instructions in the round-1 test binary (needs otool, macOS only)
	cd $(ROOT) && cargo build --release --tests
	@bin=$$(find $(ROOT)/target/release/deps -maxdepth 1 -type f -perm +111 -name '$(TEST_01)-*' | head -1); \
	echo "binary: $$bin"; \
	otool -tV "$$bin" | grep -cE '\.(16b|8h|4s|2d)' || true

.PHONY: asm-02
asm-02: ## Build release and count NEON vector instructions in the round-2 test binary (needs otool, macOS only)
	cd $(ROOT) && cargo build --release --tests
	@bin=$$(find $(ROOT)/target/release/deps -maxdepth 1 -type f -perm +111 -name '$(TEST_02)-*' | head -1); \
	echo "binary: $$bin"; \
	otool -tV "$$bin" | grep -cE '\.(16b|8h|4s|2d)' || true

.PHONY: asm
asm: asm-01 asm-02 ## Count NEON vector instructions in the round-1/round-2 test binaries
