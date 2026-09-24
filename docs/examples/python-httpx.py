#!/usr/bin/env python3
"""agos-memory — JSON API client via httpx (issue 0003)

Small, dependency-light example of the agos-memory JSON API. Run it against a
live `serve` (HTTP) instance:

    pip install httpx
    AGOS_BASE=http://127.0.0.1:8710 AGOS_TOKEN=your-token \\
        python3 docs/examples/python-httpx.py

Prerequisites:
  * the server is running (`agos-memory serve --config /abs/path/agos-memory.toml`);
  * `AGOS_TOKEN` matches `[server] token` (a loopback bind without a token
    accepts any value — the header is simply ignored);
  * `[llm]` points at a reachable chat endpoint for the summarize step.

The normative route/schema reference is `docs/api/openapi.yaml`; the curl
equivalent of this script is `docs/examples/remember-recall.sh`.
"""

from __future__ import annotations

import os
import sys

import httpx

BASE = os.environ.get("AGOS_BASE", "http://127.0.0.1:8710")
TOKEN = os.environ.get("AGOS_TOKEN", "your-token")
HEADERS = {"Authorization": f"Bearer {TOKEN}"}


def request(
    method: str,
    path: str,
    json_body: dict | None = None,
    *,
    timeout: float = 10.0,
) -> httpx.Response:
    """Issue one JSON request and return the raw response (no status check)."""
    kwargs: dict = {
        "method": method,
        "url": f"{BASE}{path}",
        "headers": HEADERS,
        "timeout": timeout,
    }
    if json_body is not None:
        kwargs["json"] = json_body
    with httpx.Client() as client:
        return client.request(**kwargs)


def ok(
    method: str,
    path: str,
    json_body: dict | None = None,
    *,
    timeout: float = 10.0,
) -> dict:
    """Like [`request`], but require 2xx and return the parsed body.

    Failed calls answer `{"error": "...", "code": "..."}` with a matching HTTP
    status, so `raise_for_status()` reports the taxonomy code to the caller.
    """
    resp = request(method, path, json_body, timeout=timeout)
    resp.raise_for_status()
    return resp.json()


def main() -> int:
    print(f"==> base URL: {BASE}")
    print()

    # 0. Liveness (public, no auth)
    print("--- GET /healthz (public) ---")
    health = ok("GET", "/healthz")
    print(health)
    assert health.get("status") == "ok", health
    print()

    # 1. Remember (explicit fields)
    print("--- POST /api/v1/remember ---")
    remembered = ok(
        "POST",
        "/api/v1/remember",
        {
            "text": "The agent prefers Rust for systems programming.",
            "tier": "semantic",
            "kind": "fact",
            "source_kind": "user",
            "confidence": 0.9,
        },
    )
    print(remembered)
    mem_id = remembered["public_id"]
    assert mem_id, remembered
    assert remembered["trust"] == "trusted", remembered
    print(f"==> remembered: {mem_id}")
    print()

    # 2. Recall — episodic tier is opt-in (D26), so ask for it explicitly.
    print("--- POST /api/v1/recall ---")
    recalled = ok(
        "POST",
        "/api/v1/recall",
        {
            "text": "Rust systems programming",
            "k": 3,
            "include_episodic": True,
        },
    )
    hits = recalled.get("hits", [])
    print(
        f"==> {len(hits)} hit(s), {recalled.get('tokens_used', 0)} tokens, "
        f"degraded={recalled.get('degraded')}"
    )
    for hit in hits:
        print(f"    {hit['public_id']}  tier={hit['tier']}  score={hit['score']:.4f}")
    print()

    # 3. Explain — the id is a *path* parameter, not a query parameter.
    print("--- GET /api/v1/explain/{id} ---")
    explained = ok("GET", f"/api/v1/explain/{mem_id}")
    print(explained)
    assert explained["public_id"] == mem_id, explained
    print()

    # 4. Status — counts grouped by memory status + index cache stats.
    print("--- GET /api/v1/status ---")
    status = ok("GET", "/api/v1/status")
    print(status)
    print(f"==> schema v{status['schema_version']}, {len(status['counts'])} status bucket(s)")
    print()

    # 5. Pin/unpin — retrieval priority only; trust and status stay unchanged.
    print("--- POST /api/v1/pin/{id} ---")
    pinned = ok("POST", f"/api/v1/pin/{mem_id}")
    print(pinned)
    assert pinned["pinned"] is True, pinned
    print("--- POST /api/v1/unpin/{id} ---")
    unpinned = ok("POST", f"/api/v1/unpin/{mem_id}")
    print(unpinned)
    assert unpinned["pinned"] is False, unpinned
    print()

    # 6. Forget (soft deprecate, then restore)
    print("--- POST /api/v1/forget/{id} (soft) ---")
    softened = ok(
        "POST",
        f"/api/v1/forget/{mem_id}",
        {"action": "soft", "reason": "demo"},
    )
    print(softened)
    assert softened["action"] == "soft", softened
    print()

    print("--- POST /api/v1/forget/{id} (restore) ---")
    restored = ok("POST", f"/api/v1/forget/{mem_id}", {"action": "restore"})
    print(restored)
    assert restored["action"] == "restore", restored
    print()

    # 7. Summarize — runs through the configured chat model (`[llm]`). Without a
    #    reachable endpoint the route answers 502 `{"error": ..., "code": "LLM"}`,
    #    which is reported here instead of crashing the example.
    print("--- POST /api/v1/summarize (by id) ---")
    resp = request("POST", "/api/v1/summarize", {"id": mem_id}, timeout=30.0)
    if resp.status_code == httpx.codes.OK:
        print(resp.json())
    else:
        print(f"==> not summarized (http {resp.status_code}): {resp.json()}")
    print()

    print("==> done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
