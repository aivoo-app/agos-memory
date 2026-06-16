# AGOS Memory

Self-hosted agent memory manager: durable, tiered, citation-backed memory for
AI agents. One SQLite database per agent; MCP + HTTP interfaces; OpenAI-
compatible embeddings and LLMs via [agos-proxy].

Status: **v0.5.0 — Interfaces** (MCP stdio + Streamable HTTP, axum JSON API +
bearer auth, JSONL export/import, musl static build, Docker). Underlying core:
write path v0.2.0, recall v0.3.0, consolidation & forgetting v0.4.0 (all
shipped). See `docs/interfaces.md`, `docs/architecture.md`, and
`docs/roadmap.md`.

## Why

Agents that forget everything between sessions are expensive to use and
dangerous to trust. agos-memory gives an agent persistent, auditable memory:

- **Tiers** — working (current session), episodic (events), semantic
  (durable facts), procedural (trigger -> behavior rules).
- **Provenance & trust** — every memory records where it came from; untrusted
  memories are never injected by default.
- **Citations** — recalled memories link back to the turns they came from.
- **Forgetting** — deprecation by default; verified hard delete with
  tombstones and a leak test across every read path.
- **Cost ledger** — every LLM/embed call is recorded with estimated cost.

## Quick start

```sh
cargo build --release
./target/release/agos-memory init
./target/release/agos-memory status
./target/release/agos-memory doctor
```

`init` writes an `agos-memory.toml` scaffold and creates the database.
Point `[embed] base_url` at your agos-proxy instance.

## Serve

One process exposes both interfaces (MCP + JSON API) on one bind:

```sh
agos-memory serve                              # MCP at /mcp + JSON API at /api/v1
agos-memory serve --stdio                      # stdio MCP transport instead
AGOS_MEMORY_TOKEN=$(openssl rand -hex 24) \
  agos-memory serve --bind 0.0.0.0:8710        # non-loopback => token required
```

`GET /healthz` is the one public probe; everything else needs
`Authorization: Bearer <token>` when a token is configured. See
[docs/interfaces.md](docs/interfaces.md) for the tool/route/error reference and
[docs/integrations/](docs/integrations/) for Hermes and OpenClaw setup.

## Container

```sh
export AGOS_MEMORY_TOKEN="$(openssl rand -hex 24)"   # >= 16 chars, required
docker compose up --build                            # musl-static image on :8710
make docker-smoke                                    # build + /healthz 200 + auth matrix
```

The image runs the statically linked `x86_64-unknown-linux-musl` binary on
`alpine:3.20`; the database lives on the `agos_memory_data` volume.

## Design highlights

- **One process per database.** SQLite is single-writer; a second process is
  refused with a clear error instead of a cryptic `SQLITE_BUSY`.
- **Single-writer actor.** Blocking SQLite work runs on a dedicated writer
  thread; reads use a round-robin read pool. Nothing blocks async workers.
- **sqlite-vec** for vector search with in-scan hard filters (status, tier,
  trust) so excluded rows are never scored, plus FTS5 for keyword fallback.
- **Schema migrations** via `PRAGMA user_version`; databases from newer
  versions are refused with an upgrade message.
- **Deterministic tests** — fake clock, hash-based mock embedder, mock LLM.
  CI never needs network or provider access.

## Development

```sh
make check   # fmt + clippy -D warnings + tests
make help
```

Contributions welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).
Security issues: see [SECURITY.md](SECURITY.md).

## License

MIT — see [LICENSE](LICENSE).

[agos-proxy]: https://github.com/aivoo-app/agos-proxy
