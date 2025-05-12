# AGOS Memory

Self-hosted agent memory manager: durable, tiered, citation-backed memory for
AI agents. One SQLite database per agent; MCP + HTTP interfaces; OpenAI-
compatible embeddings and LLMs via [agos-proxy].

Status: **v0.1.1 — Foundation + gate integrity** (storage, migrations,
sqlite-vec, single-writer store, verified backups, full test suites, CI).
The write path (extraction pipeline) lands in v0.2.0; recall in v0.3.0;
MCP/HTTP in v0.5.0. See `docs/architecture.md` and `docs/roadmap.md`.

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
