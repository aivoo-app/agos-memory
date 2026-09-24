#!/usr/bin/env bash
# =============================================================================
# agos-memory — JSON API recipes (issue 0003)
#
# Every command below runs verbatim against a live `serve` (HTTP) instance.
# `BASE` and `TOKEN` are the only things you change. The recipe doubles as the
# doc-verification harness for `docs/api/openapi.yaml`: run it end to end and
# every documented route is exercised once.
#
# Preconditions:
#   1. The server is running, e.g.
#        agos-memory serve --config /abs/path/agos-memory.toml
#      with `[server] bind = "127.0.0.1:8710"` (the default) and, for anything
#      other than a loopback bind, a token of at least 16 characters:
#        [server]
#        bind = "127.0.0.1:8710"
#        token = "a-long-random-value"
#      Keep `db_path` absolute: a relative path resolves against the *process*
#      working directory, so a client that launches the server from another cwd
#      would open a different database.
#   2. `jq` is on $PATH (output formatting only — the API is pure JSON).
#
# Auth: `/healthz` is public; every `/api/v1` route requires
#       `Authorization: Bearer <token>` when a token is configured. Loopback
#       binds without a token are allowed for local development (D38).
#
# The normative route/schema reference is `docs/api/openapi.yaml`; the
# narrative reference is `docs/interfaces.md`.
# =============================================================================

set -euo pipefail

BASE="${BASE:-http://127.0.0.1:8710}"
TOKEN="${TOKEN:-your-token}"

# Header sets are *arrays*, not strings: `AUTH="-H 'Authorization: Bearer ...'"`
# would be word-split into separate arguments and every request would 401.
JSON=(-H 'Content-Type: application/json')
AUTH=(-H "Authorization: Bearer $TOKEN")

# One request: `api <method> <path> [json-body]`.
api() {
    local method="$1" path="$2" body="${3:-}"
    if [ -n "$body" ]; then
        curl -sS -X "$method" "$BASE$path" "${JSON[@]}" "${AUTH[@]}" -d "$body"
    else
        curl -sS -X "$method" "$BASE$path" "${AUTH[@]}"
    fi
}

echo "==> base URL: $BASE"
echo

# ---------------------------------------------------------------------------
# 0. Liveness — public, no auth, no store access
# ---------------------------------------------------------------------------
echo "--- GET /healthz (public) ---"
curl -sS "$BASE/healthz" | jq .
echo

# ---------------------------------------------------------------------------
# 1. Auth: the same route without the token is refused
# ---------------------------------------------------------------------------
echo "--- GET /api/v1/status without a token (expect 401) ---"
curl -sS -o /dev/null -w '%{http_code}\n' "$BASE/api/v1/status"
echo

# ---------------------------------------------------------------------------
# 2. POST /api/v1/remember
# ---------------------------------------------------------------------------
echo "--- POST /api/v1/remember (explicit fields) ---"
REMEMBERED=$(api POST /api/v1/remember '{
      "text": "The project uses Rust and SQLite with sqlite-vec for embeddings.",
      "tier": "semantic",
      "kind": "fact",
      "source_kind": "user",
      "confidence": 0.95
    }')
echo "$REMEMBERED" | jq .

# The id is the handle for every later call, so capture it rather than pasting
# a placeholder — the recipe stays runnable end to end.
MEM_ID=$(echo "$REMEMBERED" | jq -r '.public_id')
echo "==> remembered: $MEM_ID"
echo

# Pin/unpin are idempotent and do not change trust or status.
echo "--- POST /api/v1/pin/{id} ---"
api POST "/api/v1/pin/$MEM_ID" | jq .
echo
echo "--- POST /api/v1/unpin/{id} ---"
api POST "/api/v1/unpin/$MEM_ID" | jq .
echo

echo "--- POST /api/v1/remember (defaults: tier=episodic, kind=fact) ---"
api POST /api/v1/remember '{
      "text": "Prefers dark mode across all tools.",
      "source_kind": "agent"
    }' | jq .
echo

# ---------------------------------------------------------------------------
# 3. POST /api/v1/recall
# ---------------------------------------------------------------------------
echo "--- POST /api/v1/recall (hybrid search) ---"
api POST /api/v1/recall '{
      "text": "Rust SQLite embeddings",
      "k": 5,
      "include_episodic": true
    }' | jq '{tokens_used, no_hit, degraded, hits: [.hits[] | {public_id, tier, score, tokens}]}'
echo

echo "--- POST /api/v1/recall (token budget + untrusted opt-in) ---"
api POST /api/v1/recall '{
      "text": "prefers dark mode",
      "k": 3,
      "budget_tokens": 2048,
      "include_untrusted": true
    }' | jq '{tokens_used, hits: [.hits[] | .public_id]}'
echo

# ---------------------------------------------------------------------------
# 4. GET /api/v1/explain/{id}
# ---------------------------------------------------------------------------
# The id is a *path* parameter (router and OpenAPI spec agree; there is no
# `?id=` query form).
echo "--- GET /api/v1/explain/{id} ---"
api GET "/api/v1/explain/$MEM_ID" | jq '{public_id, source_kind, source_ref, ref_count}'
echo

# ---------------------------------------------------------------------------
# 5. POST /api/v1/summarize
# ---------------------------------------------------------------------------
# Summarization runs through the configured chat model (`[llm]`). Without a
# reachable endpoint the route answers `{"error": "...", "code": "LLM"}` with
# HTTP 502 — the failure is reported, never hidden.
echo "--- POST /api/v1/summarize (by id) ---"
api POST /api/v1/summarize "{\"id\": \"$MEM_ID\"}" | jq .
echo

echo "--- POST /api/v1/summarize (whole store, tier filter) ---"
api POST /api/v1/summarize '{"tier": "episodic"}' | jq '{scope, count}'
echo

# ---------------------------------------------------------------------------
# 6. POST /api/v1/forget/{id} — soft, restore, rollback, hard
# ---------------------------------------------------------------------------
echo "--- POST /api/v1/forget/{id} (soft: deprecate, recoverable) ---"
api POST "/api/v1/forget/$MEM_ID" '{"action": "soft", "reason": "no longer accurate"}' | jq .
echo

echo "--- POST /api/v1/forget/{id} (restore) ---"
api POST "/api/v1/forget/$MEM_ID" '{"action": "restore"}' | jq .
echo

# `to_version` must exist: a freshly written memory has only version 1.
echo "--- POST /api/v1/forget/{id} (rollback to version 1) ---"
api POST "/api/v1/forget/$MEM_ID" '{"action": "rollback", "to_version": 1}' | jq .
echo

# Irreversible: deletes the row with its links/versions/vectors, verifies zero
# survivors, VACUUMs, and writes a tombstone.
echo "--- POST /api/v1/forget/{id} (hard: purge + tombstone) ---"
api POST "/api/v1/forget/$MEM_ID" '{"action": "hard", "reason": "privacy removal"}' | jq .
echo

# ---------------------------------------------------------------------------
# 7. GET /api/v1/status
# ---------------------------------------------------------------------------
echo "--- GET /api/v1/status (counts per status + index cache) ---"
api GET /api/v1/status | jq .
echo

echo "==> done"
