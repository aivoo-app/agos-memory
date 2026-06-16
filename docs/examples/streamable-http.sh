#!/usr/bin/env bash
# =============================================================================
# agos-memory — MCP Streamable HTTP client via curl (issue 0003)
#
# Demonstrates the raw MCP wire protocol over the Streamable HTTP transport
# (rmcp 3.4). Useful as a reference for writing MCP clients in any language and
# as a smoke test that the server speaks stock MCP.
#
# Preconditions:
#   1. The server is running in HTTP mode (the `serve` default), e.g.
#        agos-memory serve --config /abs/path/agos-memory.toml
#      with `[server] bind = "127.0.0.1:8710"` and a token (>=16 chars) unless
#      the bind is loopback.
#   2. `jq` is on $PATH.
#
# The wire (SEP-2567), each line verified against rmcp 3.4:
#   - POST /mcp, `Content-Type: application/json`, and the client MUST accept
#     both response types: `Accept: application/json, text/event-stream`
#     (rmcp answers 406 without it).
#   - The response is an SSE stream: the JSON-RPC payload is the `data:` line
#     carrying an `id` (the stream is primed with an empty `data:` event).
#   - `initialize` opens a session; the id comes back in the `mcp-session-id`
#     response header, and every later request MUST send it as the
#     `mcp-session-id` request header. Without it rmcp treats the request as a
#     new session and answers `Unexpected message, expect initialize request`.
#   - After `initialize` the client MUST send the `notifications/initialized`
#     notification; requests may carry `mcp-protocol-version: 2025-03-26`.
#   - A tool call returns one text part (the fenced CLI rendering) plus
#     `structuredContent` — the machine-readable payload used here.
#
# The narrative reference is `docs/interfaces.md`; the Rust interop proof is
# `tests/mcp_stdio.rs` (stdio) and `tests/mcp_http.rs` (this transport).
# =============================================================================

set -euo pipefail

BASE="${BASE:-http://127.0.0.1:8710}"
TOKEN="${TOKEN:-your-token}"
MCP_URL="$BASE/mcp"

JSON=(-H 'Content-Type: application/json')
ACCEPT=(-H 'Accept: application/json, text/event-stream')
PROTO=(-H 'mcp-protocol-version: 2025-03-26')
AUTH=(-H "Authorization: Bearer $TOKEN")

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# The JSON-RPC payload out of one SSE response body: the non-empty `data:` lines
# are JSON values; the response is the one carrying an `id`.
sse_payload() {
    sed -n 's/^data: //p' "$1" | grep -vE '^[[:space:]]*$' |
        jq -cs 'map(select(type == "object" and has("id"))) | .[0]'
}

# POST one JSON-RPC message: headers -> "$WORK/<name>.headers", body ->
# "$WORK/<name>.sse". `${SESSION[@]+"${SESSION[@]}"}` keeps the call valid under
# `set -u` before the session exists (and on bash 3.2).
SESSION=()
post() {
    local name="$1" body="$2"
    curl -sS -D "$WORK/$name.headers" -o "$WORK/$name.sse" -X POST "$MCP_URL" \
        "${JSON[@]}" "${ACCEPT[@]}" "${PROTO[@]}" "${AUTH[@]}" \
        ${SESSION[@]+"${SESSION[@]}"} -d "$body"
}

echo "==> MCP endpoint: $MCP_URL"
echo

# ---------------------------------------------------------------------------
# 0. Public liveness probe
# ---------------------------------------------------------------------------
echo "--- GET /healthz (public, no auth) ---"
curl -sS "$BASE/healthz" | jq .
echo

# ---------------------------------------------------------------------------
# 1. Auth: no token -> 401 (a configured token gates every /mcp request)
# ---------------------------------------------------------------------------
echo "--- POST /mcp without a valid token (expect 401) ---"
curl -sS -o /dev/null -w '%{http_code}\n' -X POST "$MCP_URL" \
    "${JSON[@]}" "${ACCEPT[@]}" -H 'Authorization: Bearer wrong-token-value' \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize"}'
echo

# ---------------------------------------------------------------------------
# 2. initialize — opens the session, proves the server identity
# ---------------------------------------------------------------------------
echo "--- initialize ---"
post initialize '{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "protocolVersion": "2025-03-26",
    "capabilities": {},
    "clientInfo": {
      "name": "curl-reference-client",
      "version": "0.1.0"
    }
  }
}'
cat "$WORK/initialize.sse"
echo

SESSION_ID=$(grep -i '^mcp-session-id:' "$WORK/initialize.headers" |
    tr -d '\r' | awk '{print $2}')
[ -n "$SESSION_ID" ] || { echo "ERROR: no mcp-session-id header" >&2; exit 1; }
SESSION=(-H "mcp-session-id: $SESSION_ID")

INIT_JSON=$(sse_payload "$WORK/initialize.sse")
SERVER_NAME=$(echo "$INIT_JSON" | jq -r '.result.serverInfo.name')
SERVER_VERSION=$(echo "$INIT_JSON" | jq -r '.result.serverInfo.version')
if [ "$SERVER_NAME" != "agos-memory" ]; then
    echo "ERROR: expected serverInfo.name = 'agos-memory', got: $SERVER_NAME" >&2
    exit 1
fi
echo "==> session: $SESSION_ID"
echo "==> server identity confirmed: $SERVER_NAME $SERVER_VERSION"
echo

# ---------------------------------------------------------------------------
# 3. notifications/initialized — required by the MCP lifecycle
# ---------------------------------------------------------------------------
echo "--- notifications/initialized (expect 202) ---"
CODE=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$MCP_URL" \
    "${JSON[@]}" "${ACCEPT[@]}" "${PROTO[@]}" "${AUTH[@]}" "${SESSION[@]}" \
    -d '{"jsonrpc":"2.0","method":"notifications/initialized"}')
echo "==> http $CODE"
echo

# ---------------------------------------------------------------------------
# 4. tools/list — the six-tool surface
# ---------------------------------------------------------------------------
echo "--- tools/list ---"
post tools_list '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
sse_payload "$WORK/tools_list.sse" | jq -c '.result.tools[] | {name, description}'
echo

# ---------------------------------------------------------------------------
# 5. tools/call: remember — capture the new memory's public id
# ---------------------------------------------------------------------------
echo "--- tools/call: remember ---"
post remember '{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "tools/call",
  "params": {
    "name": "remember",
    "arguments": {
      "text": "The agent prefers Rust for systems programming.",
      "source_kind": "user",
      "confidence": 0.9
    }
  }
}'
sse_payload "$WORK/remember.sse" | jq .

MEM_ID=$(sse_payload "$WORK/remember.sse" |
    jq -r '.result.structuredContent.public_id')
[ -n "$MEM_ID" ] && [ "$MEM_ID" != "null" ] ||
    { echo "ERROR: remember did not return a public_id" >&2; exit 1; }
echo "==> remembered: $MEM_ID"
echo

# ---------------------------------------------------------------------------
# 6. tools/call: recall — the stored fact comes back (episodic needs opt-in)
# ---------------------------------------------------------------------------
echo "--- tools/call: recall ---"
post recall '{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "tools/call",
  "params": {
    "name": "recall",
    "arguments": {
      "text": "Rust systems programming",
      "k": 3,
      "include_episodic": true
    }
  }
}'
sse_payload "$WORK/recall.sse" | jq -r '.result.content[0].text'

if sse_payload "$WORK/recall.sse" |
    jq -e '.result.content[0].text | test("Rust")' > /dev/null; then
    echo "==> recall returned the stored fact"
else
    echo "NOTE: recall response did not contain the expected text" >&2
fi
echo

# ---------------------------------------------------------------------------
# 7. tools/call: explain — provenance drill-down by public id
# ---------------------------------------------------------------------------
echo "--- tools/call: explain ---"
post explain "{
  \"jsonrpc\": \"2.0\",
  \"id\": 5,
  \"method\": \"tools/call\",
  \"params\": {\"name\": \"explain\", \"arguments\": {\"id\": \"$MEM_ID\"}}
}"
sse_payload "$WORK/explain.sse" |
    jq '{public_id: .result.structuredContent.public_id,
         source_kind: .result.structuredContent.source_kind,
         ref_count: .result.structuredContent.ref_count}'
echo

# ---------------------------------------------------------------------------
# 8. tools/call: status — schema version, counts per status, index cache
# ---------------------------------------------------------------------------
echo "--- tools/call: status ---"
post status '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"status","arguments":{}}}'
sse_payload "$WORK/status.sse" | jq '.result.structuredContent'
echo

echo "==> MCP roundtrip complete"
