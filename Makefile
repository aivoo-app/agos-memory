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
#   make plan-guard    # fail if plan/ or .clinerules are tracked by git
#   make ci            # plan-guard + check (what CI runs)
# ==============================================================================

SHELL := /bin/bash
.DEFAULT_GOAL := help
.PHONY: help check fmt lint test build gate-v0.1.0 plan-guard ci

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

plan-guard: ## Fail if plan/ or local-only files are tracked by git
	bash scripts/ci/check-plan-not-tracked.sh

gate-v0.1.0: ## v0.1.0 milestone gate
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	bash scripts/ci/check-plan-not-tracked.sh

ci: plan-guard check ## CI pipeline (offline; no provider access needed)
