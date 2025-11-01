#!/usr/bin/env bash
# ==============================================================================
# AGOS Memory — top-level development & CI makefile.
#
# Quick reference:
#   make help          # this help
#   make check         # fmt + clippy + test (gate before every commit)
#   make fmt           # cargo fmt --all
#   make lint          # cargo clippy --all-targets -- -D warnings
#   make test          # cargo test --all-targets
#   make build         # release build
#   make gate-v0.1.0   # milestone gate: check + plan-guard
#   make gate-v0.1.1   # v0.1.1 milestone gate: fmt check + clippy + tests + plan-guard
#   make gate-v0.2.0   # v0.2.0 milestone gate: check + plan-guard + release build + CLI smoke
#   make plan-guard    # fail if plan/ or .clinerules are tracked by git
#   make ci            # plan-guard + check (what CI runs)
# ==============================================================================

SHELL := /bin/bash
.DEFAULT_GOAL := help
.PHONY: help check fmt lint test build gate-v0.1.0 gate-v0.1.1 gate-v0.2.0 plan-guard ci smoke

help: ## Show this help
	@grep -E '^[a-zA-Z0-9_.-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-14s\033[0m %s\n", $$1, $$2}'

fmt: ## Format all code
	cargo fmt --all

lint: ## Clippy with warnings as errors
	cargo clippy --all-targets -- -D warnings

test: ## Run all tests
	cargo test --all-targets

check: fmt lint test ## Full local gate (fmt + clippy + tests)

build: ## Release build
	cargo build --release

plan-guard: ## Fail if plan/ local notes are tracked by git
	@if git ls-files plan/ | grep -v '^plan/.gitignore$$' | grep -q .; then echo "plan-guard: FAIL — plan/ files are tracked (keep plan/ git-ignored)"; git ls-files plan/ | grep -v '^plan/.gitignore$$'; exit 1; fi
	@echo "plan-guard: OK — plan/ is untracked"

gate-v0.1.0: ## v0.1.0 milestone gate
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	$(MAKE) plan-guard

gate-v0.1.1: ## v0.1.1 milestone gate (same gates, named for the release)
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	$(MAKE) plan-guard

smoke: ## CLI smoke test on a throwaway database (init/session/remember/status/doctor)
	@set -euo pipefail; \
	DIR=$$(mktemp -d); \
	DB=$$DIR/smoke.db; \
	printf "db_path = 'smoke.db'\nagent_id = 'default'\n\n[embed]\nprovider = 'none'\n" > $$DIR/agos-memory.toml; \
	CFG="--config $$DIR/agos-memory.toml --db $$DB"; \
	cargo run --quiet -- $$CFG init >/dev/null; \
	cargo run --quiet -- $$CFG status | grep -q 'schema:'; \
	cargo run --quiet -- $$CFG doctor | grep -q 'doctor done'; \
	cargo run --quiet -- $$CFG session open | grep -q 'session:'; \
	cargo run --quiet -- $$CFG session append --role user --content 'I prefer Rust for CLI tools.' | grep -q 'turn:'; \
	cargo run --quiet -- $$CFG remember --text 'Prefers Rust for CLI tools.' | grep -q 'memory:'; \
	cargo run --quiet -- $$CFG status | grep -q 'memories\[active\]: 1'; \
	rm -rf $$DIR; \
	echo "smoke: OK (offline: provider = 'none')"

gate-v0.2.0: ## v0.2.0 milestone gate (write path: fmt + clippy + tests + plan-guard + build + smoke)
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	$(MAKE) plan-guard
	cargo build --release
	$(MAKE) smoke

eval: ## Run offline eval gate (precision ≥0.90, recall ≥0.95, MRR ≥0.80, 0 leaks)
	@set -euo pipefail; \
	DIR=$$(mktemp -d); \
	DB=$$DIR/eval.db; \
	cp fixtures/eval_cases.jsonl $$DIR/; \
	CFG="--config agos-memory.toml --db $$DB"; \
	cargo run --quiet -- $$CFG eval --file $$DIR/eval_cases.jsonl --min-precision 0.90 --min-recall 0.95 --min-mrr 0.80 --json 2>&1 | tail -20; \
	rm -rf $$DIR

bench: ## Run recall performance benchmark (p95 < 150ms)
	cargo bench --bench recall_bench

gate-v0.3.0: ## v0.3.0 milestone gate (fmt + clippy -D + tests + plan-guard + build + smoke + eval + bench)
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	$(MAKE) plan-guard
	cargo build --release
	$(MAKE) smoke
	$(MAKE) eval
	$(MAKE) bench

gate-v0.4.0: ## v0.4.0 milestone gate (fmt + clippy -D + tests + plan-guard + build + smoke)
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	$(MAKE) plan-guard
	cargo build --release
	$(MAKE) smoke

ci: plan-guard check ## CI pipeline (offline; no provider access needed)
