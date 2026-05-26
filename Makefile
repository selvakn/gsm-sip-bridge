CONFIG ?= config.toml
DOCKER_COMPOSE := docker compose -f docker/docker-compose.yml

.PHONY: build test run clean lint format dev dev-gsm dev-sip \
        docker-build docker-up docker-down docker-logs \
        coverage mutants mutants-full help

build: ## Compile all binaries (release mode)
	@cargo build --workspace --release

test: ## Run the full test suite
	@cargo test --workspace

run: build ## Build and run the GSM-SIP bridge
	@cargo run --release --bin gsm-sip-bridge -- --config $(CONFIG)

clean: ## Remove all build artifacts
	@cargo clean

lint: ## Run formatting check, clippy, cargo-deny, and unsafe audit
	@cargo fmt --check
	@cargo clippy -p gsm-sip-bridge -p pjsua-safe -- -D warnings
	@if command -v cargo-deny >/dev/null 2>&1; then cargo deny check; fi
	@bash tools/count-unsafe.sh

format: ## Auto-format all Rust source files
	@cargo fmt

dev: ## Run in debug mode with verbose logging
	@RUST_LOG=debug,gsm_sip_bridge=trace cargo run --bin gsm-sip-bridge -- --config $(CONFIG) --verbose

dev-gsm: ## [Debug] Run GSM-only audio loopback
	@cargo run --bin gsm-echo

dev-sip: ## [Debug] Run SIP-only audio loopback
	@cargo run --bin sip-echo -- --config $(CONFIG) --verbose

docker-build: ## Build the production Docker image
	@$(DOCKER_COMPOSE) build

docker-up: ## Start all containers (bridge + monitoring stack)
	@$(DOCKER_COMPOSE) up -d

docker-down: ## Stop and remove all containers
	@$(DOCKER_COMPOSE) down

docker-logs: ## Tail logs from all containers
	@$(DOCKER_COMPOSE) logs -f

coverage: ## Generate code coverage report (requires cargo-llvm-cov)
	@cargo llvm-cov --workspace --lcov --output-path lcov.info
	@cargo llvm-cov report --workspace

mutants: ## Mutation test core logic (store, AT parser, control protocol) — fast, no hardware needed
	@cargo mutants \
	  --package gsm-sip-bridge \
	  --re 'store/schema|store/slots|control/protocol|modules/at_commander' \
	  --timeout 30 \
	  --jobs 2 \
	  --output mutants-out/

mutants-full: ## Mutation test all non-hardware modules (slower, includes config + modules/mod.rs)
	@cargo mutants \
	  --package gsm-sip-bridge \
	  --timeout 45 \
	  --jobs 2 \
	  --output mutants-out/

help: ## Show all available targets
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'
