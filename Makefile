BUILD_DIR := build
GSM_BINARY := $(BUILD_DIR)/audio-echo
SIP_BINARY := $(BUILD_DIR)/sip-echo

.PHONY: build test run run-sip clean lint help

build: ## Compile both audio-echo and sip-echo binaries
	@cmake -B $(BUILD_DIR) -DCMAKE_BUILD_TYPE=Release -S .
	@cmake --build $(BUILD_DIR) --parallel

test: ## Run the full integration test suite
	@cmake -B $(BUILD_DIR) -DCMAKE_BUILD_TYPE=Debug -S .
	@cmake --build $(BUILD_DIR) --parallel
	@cd $(BUILD_DIR) && ctest --output-on-failure

run: build ## Build and run GSM audio-echo with auto-detection
	@$(GSM_BINARY)

run-sip: build ## Build and run SIP echo server with config.ini
	@$(SIP_BINARY) --config config.ini --verbose

clean: ## Remove all build artifacts
	@rm -rf $(BUILD_DIR)

lint: ## Run static analysis on all source files
	@cppcheck --enable=all --std=c++17 --suppress=missingIncludeSystem \
		--error-exitcode=1 -I src src/ 2>&1 | grep -v "^Checking" || true
	@echo "Lint complete."

help: ## Show all available targets
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
