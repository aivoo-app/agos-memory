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
#   make bench         # strict release perf gate: recall p95 < 150 ms @10k (NOT MET is a failure)
#   make bench-report  # release 10k measurement, report threshold result without asserting
#   make bench-100k    # strict release perf gate: recall p95 < 300 ms @100k (NOT MET is a failure)
#   make bench-100k-report # release 100k measurement, report threshold result without asserting
#   make build         # release build
#   make build-musl    # static release build for x86_64-unknown-linux-musl
#                        (requires musl-gcc; see docs/operations/runbook.md)
#   make gate-v0.1.0   # milestone gate: check
#   make gate-v0.1.1   # v0.1.1 milestone gate: fmt check + clippy + tests
#   make gate-v0.2.0   # v0.2.0 milestone gate: check + release build + CLI smoke
#   make gate-v0.5.0   # v0.5.0 Interfaces gate: check + yaml-guard + release + smoke + smoke-serve + eval + eval-summarize
#   make gate-v0.6.0   # v0.6.0 proof gate: structural checks + report-only 10k/100k measurements + soak
#   make soak          # release mixed-traffic soak; AGOS_SOAK_SECS controls duration
#   make smoke-serve   # spawn serve: stdio tools/call roundtrip + HTTP /healthz
#   make docker-build  # build the container image (also proves the musl build)
#   make docker-smoke  # run the image: /healthz 200 + auth matrix (mirrors CI)
#   make yaml-guard    # fail if a workflow is invalid YAML (silently disables CI)
#   make ci            # check (what CI runs)
# v0.6.0 release proof is split into strict performance targets and a structural
# report-only closeout. The strict targets remain the source of truth for a
# pass/fail latency claim.

SHELL := /bin/bash
.DEFAULT_GOAL := help
.PHONY: help check fmt lint test test-fast build build-musl bench bench-report bench-100k bench-100k-report bench-consolidate eval eval-summarize gate-v0.1.0 gate-v0.1.1 gate-v0.2.0 gate-v0.3.0 gate-v0.4.0 gate-v0.5.0 gate-v0.6.0 yaml-guard ci smoke smoke-serve restore-drill soak docker-build docker-smoke

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

restore-drill: ## Execute the snapshot → replace → reopen backup/restore drill
	cargo test --test restore_drill -- --nocapture --test-threads=1

soak: ## Release mixed-traffic soak test (ignored test; default 60s, AGOS_SOAK_SECS overrides)
	AGOS_SOAK_SECS=$${AGOS_SOAK_SECS:-60} cargo test --release --test soak -- --ignored --nocapture --test-threads=1

check: yaml-guard fmt lint test ## Full local gate (fmt + clippy + tests + workflow YAML)

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

docker-build: ## Build the container image (0007); also proves the musl static build (0006)
	docker build -t agos-memory:local .

docker-smoke: docker-build ## Run the image and assert /healthz 200 + the auth matrix (mirrors CI)
	@set -euo pipefail; \
	NAME=agos-smoke; \
	PORT=$$(( 14000 + RANDOM % 20000 )); \
	TOK=smoke-token-0123456789; \
	cleanup() { docker rm -f $$NAME >/dev/null 2>&1 || true; }; \
	trap cleanup EXIT; \
	cleanup; \
	docker run -d --name $$NAME \
	  -e AGOS_MEMORY_BIND=0.0.0.0:8710 \
	  -e AGOS_MEMORY_TOKEN=$$TOK \
	  -p 127.0.0.1:$$PORT:8710 agos-memory:local >/dev/null; \
	CODE=000; \
	for i in $$(seq 1 60); do \
	  CODE=$$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:$$PORT/healthz || echo 000); \
	  [ "$$CODE" = 200 ] && break; \
	  sleep 1; \
	done; \
	[ "$$CODE" = 200 ] || { echo "docker-smoke: /healthz failed (code=$$CODE):"; docker logs $$NAME; exit 1; }; \
	echo "docker-smoke: /healthz 200 OK"; \
	A=$$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:$$PORT/api/v1/status); \
	B=$$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $$TOK" http://127.0.0.1:$$PORT/api/v1/status); \
	[ "$$A" = 401 ] || { echo "docker-smoke: expected 401 without token, got $$A"; docker logs $$NAME; exit 1; }; \
	[ "$$B" = 200 ] || { echo "docker-smoke: expected 200 with token, got $$B"; docker logs $$NAME; exit 1; }; \
	echo "docker-smoke: auth matrix OK (401 without token, 200 with)"

yaml-guard: ## Fail if a GitHub workflow is not valid YAML (a broken one silently disables all CI)
	@python3 scripts/check-workflows.py

gate-v0.1.0: ## v0.1.0 milestone gate
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets

gate-v0.1.1: ## v0.1.1 milestone gate (same gates, named for the release)
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets

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

smoke-serve: ## Serve smoke (0009): MCP stdio tools/call roundtrip + HTTP /healthz 200 (throwaway, offline)
	@set -euo pipefail; \
	DIR=$$(mktemp -d); \
	DB=$$DIR/smoke-serve.db; \
	SRV=""; \
	cleanup() { \
	  [ -n "$$SRV" ] && kill $$SRV 2>/dev/null || true; \
	  pkill -f "$$DIR/agos-memory.toml" 2>/dev/null || true; \
	  rm -rf $$DIR; \
	}; \
	trap cleanup EXIT; \
	printf "db_path = '%s'\nagent_id = 'default'\n\n[embed]\nprovider = 'none'\n" "$$DB" > $$DIR/agos-memory.toml; \
	CFGF="--config $$DIR/agos-memory.toml"; \
	cargo run --quiet -- $$CFGF init >/dev/null || { echo "smoke-serve: init failed"; exit 1; }; \
	{ printf '%s\n%s\n%s\n' \
	    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"smoke","version":"0.0.0"}}}' \
	    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
	    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"remember","arguments":{"text":"smoke-serve stdio roundtrip"}}}'; \
	  sleep 2; \
	} | cargo run --quiet -- $$CFGF serve --stdio > $$DIR/stdio.out 2> $$DIR/stdio.err \
	  || { echo "smoke-serve: stdio server failed:"; cat $$DIR/stdio.err; exit 1; }; \
	grep -q '"public_id"' $$DIR/stdio.out \
	  || { echo "smoke-serve: stdio tools/call remember failed:"; cat $$DIR/stdio.out; cat $$DIR/stdio.err; exit 1; }; \
	echo "smoke-serve: stdio tools/call remember OK"; \
	PORT=$$(( 12000 + RANDOM % 20000 )); \
	cargo run --quiet -- $$CFGF serve --bind 127.0.0.1:$$PORT > $$DIR/http.log 2>&1 & SRV=$$!; \
	CODE=000; \
	for i in $$(seq 1 80); do \
	  CODE=$$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:$$PORT/healthz 2>/dev/null || echo 000); \
	  [ "$$CODE" = 200 ] && break; \
	  kill -0 $$SRV 2>/dev/null || break; \
	  sleep 0.25; \
	done; \
	[ "$$CODE" = 200 ] || { echo "smoke-serve: HTTP /healthz failed (code=$$CODE):"; cat $$DIR/http.log; exit 1; }; \
	echo "smoke-serve: HTTP /healthz 200 OK"

gate-v0.2.0: ## v0.2.0 milestone gate (write path: fmt + clippy + tests + build + smoke)
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	cargo build --release
	$(MAKE) smoke

eval: ## Offline eval gate: absolute floors + committed baseline drift check (one percentage point)
	@set -euo pipefail; \
	DIR=$$(mktemp -d); \
	DB=$$DIR/eval.db; \
	BASELINE=fixtures/eval_baseline.json; \
	COMMIT=$$(git rev-parse --short HEAD); \
	trap 'rm -rf $$DIR' EXIT; \
	printf "db_path = '%s'\nagent_id = 'eval'\n\n[embed]\nprovider = 'hash'\n" "$$DB" > $$DIR/agos-memory.toml; \
	AGOS_EVAL_RELEASE=v0.6.0 AGOS_EVAL_GIT_COMMIT=$$COMMIT \
	cargo run --quiet -- --config $$DIR/agos-memory.toml --db $$DB eval \
		--file fixtures/eval_cases.jsonl \
		--baseline $$BASELINE \
		--min-precision 0.90 --min-recall 0.95 --min-mrr 0.80 --json

eval-summarize: ## Offline summarize-quality gate — mean ROUGE-L ≥ 0.85 over fixtures/summarize_cases.jsonl (hermetic: MockChat)
	cargo test --test summarize_quality -- --nocapture

bench: ## Strict release perf gate: recall p95 < 150 ms @10k (NOT MET is a failure)
	AGOS_BENCH_VECTORS=10000 AGOS_BENCH_REPORT_ONLY=0 cargo bench --bench recall_bench

bench-report: ## Release 10k perf measurement; reports the threshold result without asserting
	AGOS_BENCH_VECTORS=10000 AGOS_BENCH_REPORT_ONLY=1 cargo bench --bench recall_bench

bench-100k: ## Strict release perf gate: recall p95 < 300 ms @100k (NOT MET is a failure)
	AGOS_BENCH_VECTORS=100000 AGOS_BENCH_SAMPLES=20 AGOS_BENCH_REPORT_ONLY=0 AGOS_BENCH_ASSERT=1 cargo bench --bench recall_bench

bench-100k-report: ## Release 100k perf measurement; reports the threshold result without asserting
	AGOS_BENCH_VECTORS=100000 AGOS_BENCH_SAMPLES=20 AGOS_BENCH_REPORT_ONLY=1 cargo bench --bench recall_bench

bench-consolidate: ## Release consolidation bench (0049): summarize quality/speed, dedup, TTL reaper
	cargo bench --bench consolidate_bench

gate-v0.3.0: ## v0.3.0 milestone gate (fmt + clippy -D + tests + build + smoke + eval + bench)
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	cargo build --release
	$(MAKE) smoke
	$(MAKE) eval
	$(MAKE) bench

gate-v0.4.0: ## v0.4.0 milestone gate (fmt + clippy -D + tests + release + smoke + eval + eval-summarize)
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	cargo build --release
	$(MAKE) smoke
	$(MAKE) eval
	$(MAKE) eval-summarize
	@echo "gate-v0.4.0: perf benches are separate release gates — run \`make bench\` and \`make bench-consolidate\`"

gate-v0.5.0: ## v0.5.0 Interfaces gate: fmt + clippy -D + tests + yaml-guard + release + smoke + smoke-serve + eval + eval-summarize
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	$(MAKE) yaml-guard
	cargo build --release
	$(MAKE) smoke
	$(MAKE) smoke-serve
	$(MAKE) eval
	$(MAKE) eval-summarize
	@echo "gate-v0.5.0: OK — perf benches are separate release gates (make bench / make bench-consolidate)"

gate-v0.6.0: ## v0.6.0 proof gate: structural checks + measured performance reports + soak
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	$(MAKE) yaml-guard
	cargo build --release
	$(MAKE) smoke
	$(MAKE) smoke-serve
	$(MAKE) eval
	$(MAKE) eval-summarize
	$(MAKE) bench-report
	$(MAKE) bench-100k-report
	$(MAKE) soak
	@echo "gate-v0.6.0: structural gate OK; strict performance thresholds remain make bench / make bench-100k and are NOT MET on the published reference host"

ci: check ## CI pipeline (offline; no provider access needed)
