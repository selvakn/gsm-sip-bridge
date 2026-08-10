CONFIG ?= config.toml
DOCKER_COMPOSE := docker compose -f docker/docker-compose.yml

.PHONY: build test test-scripts run clean lint format dev dev-gsm dev-sip \
        docker-build docker-build-internet docker-up docker-down docker-logs \
        coverage mutants mutants-full help

build: ## Compile all binaries (release mode)
	@cargo build --workspace --release

# Prefers cargo-nextest when installed: .config/nextest.toml sets a 20s
# per-test timeout, which `cargo test` has no equivalent for and which is what
# stops a serial-port or store-thread test that hangs from wedging the whole
# run. Falls back to `cargo test` so a bare checkout still works.
#
# `--no-fail-fast` because `cargo test` otherwise stops at the first failing
# *binary*, so the summary line reports only the tests that ran before it —
# which reads as a smaller suite passing rather than as a failure.
test: test-scripts ## Run the full test suite
	@if cargo nextest --version >/dev/null 2>&1; then \
		cargo nextest run --workspace --no-fail-fast; \
	else \
		echo "note: cargo-nextest not installed — falling back to cargo test (no per-test timeout)"; \
		cargo test --workspace --no-fail-fast; \
	fi

# Shell/integration tests that live outside cargo (e.g. the cellular-internet
# sidecar's readiness probe, specs/032). Hermetic — no hardware or network.
test-scripts: ## Run non-cargo shell integration tests
	@sh docker/cellular-internet/tests/probe_test.sh
	@sh docker/cellular-internet/tests/wds_lifecycle_test.sh

run: build ## Build and run the GSM-SIP bridge
	@cargo run --release --bin gsm-sip-bridge -- --config $(CONFIG)

clean: ## Remove all build artifacts
	@cargo clean

# The cargo-* guards below ask cargo whether the *subcommand* runs, not
# whether a `cargo-x` binary is on PATH. Cargo locates its subcommands in
# ~/.cargo/bin itself, which is frequently not on PATH (rustup shims, distro
# cargo). `command -v cargo-deny` therefore reported "not installed" on a
# machine where `cargo deny check` worked perfectly — silently skipping the
# dependency-policy gate, which is how a RUSTSEC advisory and a rejected
# license reached CI with a clean local `make lint`.
lint: ## Run formatting check, clippy, cargo-deny, shellcheck, and unsafe audit
	@cargo fmt --check
	@cargo clippy --workspace --all-targets -- -D warnings
	@if cargo deny --version >/dev/null 2>&1; then \
		cargo deny check; \
	else \
		echo "WARNING: cargo-deny not installed — dependency advisories and"; \
		echo "         licenses were NOT checked. CI does enforce them, so a"; \
		echo "         clean 'make lint' here can still fail there."; \
		echo "         Install: cargo install cargo-deny --locked"; \
	fi
	@if command -v shellcheck >/dev/null 2>&1; then \
		shellcheck -x docker/*.sh docker/cellular-internet/*.sh docker/cellular-internet/tests/*.sh; \
	else \
		echo "WARNING: shellcheck not installed — skipping shell lint"; \
	fi
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

docker-build-internet: ## Build the cellular-internet sidecar image (specs/032)
	@docker build -f docker/cellular-internet/Dockerfile -t gsm-sip-bridge-internet .

docker-up: ## Start all containers (bridge + monitoring stack)
	@$(DOCKER_COMPOSE) up -d

docker-down: ## Stop and remove all containers
	@$(DOCKER_COMPOSE) down

docker-logs: ## Tail logs from all containers
	@$(DOCKER_COMPOSE) logs -f

coverage: ## Generate code coverage report (requires cargo-llvm-cov)
	@cargo llvm-cov --workspace --lcov --output-path lcov.info
	@cargo llvm-cov report

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
