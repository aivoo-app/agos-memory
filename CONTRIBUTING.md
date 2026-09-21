# Contributing to agos-memory

Thanks for helping. This document mirrors the agos-proxy contribution guide;
read it once and you can contribute to both.

## Ground rules

- Rust only; keep new dependencies minimal and justify each one.
- The `main` branch is locked; work happens on version branches (e.g.
  `v0.1.0`) or issue branches, opened as PRs from there.

## Getting started

```sh
git clone https://github.com/aivoo-app/agos-memory
cd agos-memory
cargo build
make check   # fmt + clippy -D warnings + tests
```

## Workflow

1. Branch from the current version branch; use descriptive names
   (`feat/recall-pack`, `fix/lock-reclaim`).
2. Keep changes small and focused; one concern per PR.
3. Tests are expected for new behavior and for changed behavior.
4. Docs live with code: changing behavior without updating the relevant doc
   (`README`, `docs/architecture`, `docs/adr/*`, ...) may get the PR returned.
5. Schema changes require a migration and an "upgrade from older store" test.

## Commit style

- Conventional commits: `feat:`, `fix:`, `docs:`, `test:`, `refactor:`.
- Imperative subject lines; keep commits atomic.

## Testing

- Unit tests live inline in the modules they cover.
- Integration tests live in `tests/`.
- Tests must not depend on network, real LLMs, or real embedding providers —
  use the mock providers in `src/llm` and `src/embed`.

## Security-sensitive changes

Auth, tokens, exposure (bind address), deletion, and provenance/trust logic
get extra review. Mention it in the PR description.

Do not open a public issue for a security vulnerability — see `SECURITY.md`.

## Releases

Cut from `main` by a maintainer: bump `Cargo.toml` + `VERSION`, update
`CHANGELOG.md`, tag, build, smoke-test, regenerate docs, push tag.
