#!/usr/bin/env bash
# ==============================================================================
# AGOS Memory — top-level development & CI makefile.
#
# Quick reference:
#   make help          # this help
#   make check         # fmt + clippy + test (gate before every commit)
#   make fmt           # cargo fmt --all
#   make lint          # cargo clippy --all-targets -- -D warnings
#   make test          # cargo test --all-targets (benches run informational in debug)
#   make test-fast     # cargo test --lib --tests (no benches/doc tests)
#   make bench         # release perf gate: recall p95 < 150 ms (AGOS_BENCH_VECTORS=10000 for @10k)
#   make build         # release build
#   make gate-v0.1.0   # milestone gate: check + plan-guard
#   make gate-v0.1.1   # v0.1.1 milestone gate: fmt check + clippy + tests + plan-guard
#   make gate-v0.2.0   # v0.2.0 milestone gate: check + plan-guard + release build + CLI smoke
#   make plan-guard    # fail if plan/ or .clinerules are tracked by git
#   make ci            # plan-guard + check (what CI runs)
# ==============================================================================

SHELL := /bin/bash
.DEFAULT_GOAL := help
.PHONY: help check fmt lint test test-fast build bench eval gate-v0.1.0 gate-v0.1.1 gate-v0.2.0 gate-v0.3.0 gate-v0.4.0 plan-guard ci smoke

help: ## Show this help
	@grep -E '^[a-zA-Z0-9_.-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-14s\033[0m %s\n", $$1, $$2}'

fmt: ## Format all code
	cargo fmt --all

lint: ## Clippy with warnings as errors
	cargo clippy --all-targets -- -D warnings

test: ## Run all tests (benches self-skip their perf assertion in debug; see `bench`)
	cargo test --all-targets

test-fast: ## Run lib + integration tests only (skips benches; fastest full-signal run)
	cargo test --lib --tests

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

eval: ## Offline eval gate — hermetic (hash embedder): precision ≥0.90, recall ≥0.95, MRR ≥0.80, 0 leaks
	@set -euo pipefail; \
	DIR=$$(mktemp -d); \
	DB=$$DIR/eval.db; \
	printf "db_path = '%s'\nagent_id = 'eval'\n\n[embed]\nprovider = 'hash'\n" "$$DB" > $$DIR/agos-memory.toml; \
	cargo run --quiet -- --config $$DIR/agos-memory.toml --db $$DB eval \
		--file fixtures/eval_cases.jsonl \
		--min-precision 0.90 --min-recall 0.95 --min-mrr 0.80 --json; \
	rm -rf $$DIR

bench: ## Release perf gate: recall p95 < 150 ms (@10k: AGOS_BENCH_VECTORS=10000)
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

gate-v0.4.0: ## v0.4.0 milestone gate (fmt + clippy -D + tests + plan-guard + release + smoke + eval)
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	$(MAKE) plan-guard
	cargo build --release
	$(MAKE) smoke
	$(MAKE) eval
	@echo "gate-v0.4.0: perf bench is the separate release gate — run \`make bench\`"

ci: plan-guard check ## CI pipeline (offline; no provider access needed)
