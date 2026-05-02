BUILD_DIR := build
BRIDGE_BINARY := $(BUILD_DIR)/gsm-sip-bridge
GSM_ECHO_BINARY := $(BUILD_DIR)/audio-echo
SIP_ECHO_BINARY := $(BUILD_DIR)/sip-echo

.PHONY: build test run run-gsm-echo run-sip-echo docker clean lint help

build: ## Compile all binaries
	@cmake -B $(BUILD_DIR) -DCMAKE_BUILD_TYPE=Release -S .
	@cmake --build $(BUILD_DIR) --parallel

test: ## Run the full integration test suite
	@cmake -B $(BUILD_DIR) -DCMAKE_BUILD_TYPE=Debug -S .
	@cmake --build $(BUILD_DIR) --parallel
	@cd $(BUILD_DIR) && ctest --output-on-failure

run: build ## Build and run the GSM-SIP bridge
	@$(BRIDGE_BINARY) --config config.ini

run-gsm-echo: build ## [Debug] Echo GSM audio back to caller (no SIP)
	@$(GSM_ECHO_BINARY)

run-sip-echo: build ## [Debug] Echo SIP audio back to caller (no GSM)
	@$(SIP_ECHO_BINARY) --config config.ini --verbose

docker: ## Build and run via Docker Compose
	@docker compose up --build -d
	@docker compose logs -f

clean: ## Remove all build artifacts
	@rm -rf $(BUILD_DIR)

lint: ## Run static analysis on all source files
	@cppcheck --enable=all --std=c++17 --suppress=missingIncludeSystem \
		--error-exitcode=1 -I src src/ 2>&1 | grep -v "^Checking" || true
	@echo "Lint complete."

help: ## Show all available targets
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'
