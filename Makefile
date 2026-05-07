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
#   make build-musl    # static release build for x86_64-unknown-linux-musl
#                        (requires musl-gcc; see docs/operations/runbook.md)
#   make gate-v0.1.0   # milestone gate: check + plan-guard
#   make gate-v0.1.1   # v0.1.1 milestone gate: fmt check + clippy + tests + plan-guard
#   make gate-v0.2.0   # v0.2.0 milestone gate: check + plan-guard + release build + CLI smoke
#   make plan-guard    # fail if plan/ or .clinerules are tracked by git
#   make ci            # plan-guard + check (what CI runs)
# ==============================================================================

SHELL := /bin/bash
.DEFAULT_GOAL := help
.PHONY: help check fmt lint test test-fast build build-musl bench bench-consolidate eval eval-summarize gate-v0.1.0 gate-v0.1.1 gate-v0.2.0 gate-v0.3.0 gate-v0.4.0 plan-guard ci smoke

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

# Static musl release build (0006). Two one-time prerequisites, both checked:
#   1. `rustup target add x86_64-unknown-linux-musl`
#   2. `musl-gcc` (musl-tools: apt 'musl-tools' / pacman 'musl'), which the
#      vendored sqlite-vec + rusqlite C code link against. Without it, `cc-rs`
#      aborts looking for `x86_64-linux-musl-gcc`. The `[profile.release]`
#      (`lto`, `strip`) is reused as-is. Result must be statically linked.
build-musl: ## Static release build for x86_64-unknown-linux-musl (0006)
	@rustup target list --installed | grep -qx 'x86_64-unknown-linux-musl' || { echo "build-musl: 'rustup target add x86_64-unknown-linux-musl' first"; exit 1; }
	@command -v x86_64-linux-musl-gcc >/dev/null || command -v musl-gcc >/dev/null || { echo "build-musl: musl-gcc not found — install musl-tools (apt: musl-tools, pacman: musl), see docs/operations/runbook.md"; exit 1; }
	cargo build --release --target x86_64-unknown-linux-musl
	@echo "musl binary: target/release/x86_64-unknown-linux-musl/agos-memory"
	@if ldd target/release/x86_64-unknown-linux-musl/agos-memory 2>&1 | grep -q 'not a dynamic executable'; then echo "ldd: statically linked (OK)"; else echo "ldd: WARNING — not fully static"; exit 1; fi

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

eval-summarize: ## Offline summarize-quality gate — mean ROUGE-L ≥ 0.85 over fixtures/summarize_cases.jsonl (hermetic: MockChat)
	cargo test --test summarize_quality -- --nocapture

bench: ## Release perf gate: recall p95 < 150 ms (@10k: AGOS_BENCH_VECTORS=10000)
	cargo bench --bench recall_bench

bench-consolidate: ## Release consolidation bench (0049): summarize quality/speed, dedup, TTL reaper
	cargo bench --bench consolidate_bench

gate-v0.3.0: ## v0.3.0 milestone gate (fmt + clippy -D + tests + plan-guard + build + smoke + eval + bench)
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	$(MAKE) plan-guard
	cargo build --release
	$(MAKE) smoke
	$(MAKE) eval
	$(MAKE) bench

gate-v0.4.0: ## v0.4.0 milestone gate (fmt + clippy -D + tests + plan-guard + release + smoke + eval + eval-summarize)
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	$(MAKE) plan-guard
	cargo build --release
	$(MAKE) smoke
	$(MAKE) eval
	$(MAKE) eval-summarize
	@echo "gate-v0.4.0: perf benches are separate release gates — run \`make bench\` and \`make bench-consolidate\`"

ci: plan-guard check ## CI pipeline (offline; no provider access needed)
