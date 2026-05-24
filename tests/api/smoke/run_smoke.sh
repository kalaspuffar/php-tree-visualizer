#!/usr/bin/env bash
#
# tests/api/smoke/run_smoke.sh — end-to-end smoke against the PHP
# built-in dev server. Maps SPECIFICATION.md §10.3 acceptance:
#   1) POST /api/auth with the configured token returns 204 + cookie.
#   2) GET /api/traces with the cookie returns 200 + §5.4 JSON.
#   3) No occurrence of the configured token in any captured log.
#
# Usage: bash tests/api/smoke/run_smoke.sh
# Exits 0 on success, non-zero otherwise.

set -euo pipefail

cd "$(dirname "$0")/../../.."

TMP="$(mktemp -d -t phptv-smoke.XXXXXX)"
trap 'rm -rf "$TMP"; if [[ -n "${SERVER_PID:-}" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then kill "$SERVER_PID" 2>/dev/null || true; fi' EXIT

TOKEN="SMOKE-TOKEN-$(head -c 24 /dev/urandom | base64 | tr -d '=' | tr '/+' '_-')"
SALT="SMOKE-SALT-$(head -c 24 /dev/urandom | base64 | tr -d '=' | tr '/+' '_-')"
DATA_DIR="$TMP/data"
CONFIG_PATH="$TMP/collector.toml"
SERVER_LOG="$TMP/server.log"
JAR="$TMP/cookie-jar.txt"

mkdir -p "$DATA_DIR/traces"

cat >"$CONFIG_PATH" <<EOF
[server]
bind = "127.0.0.1:8088"
max_body_bytes = 67108864
queue_capacity = 256
tls = false

[auth]
token = "$TOKEN"
session_salt = "$SALT"

[storage]
data_dir = "$DATA_DIR"
retention_days = 30

[finalize]
idle_seconds = 30
tick_seconds = 5
EOF

# Build the fixture index.sqlite via the test helper.
php -r '
require "tests/api/fixtures/make_index_sqlite.php";
make_index_sqlite($argv[1], [
    ["trace_key" => str_repeat("1", 32), "uri_or_script" => "/srv/app/index.php", "start_time_ns" => 1700000000000000000],
    ["trace_key" => str_repeat("2", 32), "uri_or_script" => "/srv/app/bin/run-tests.php", "start_time_ns" => 1700000001000000000],
    ["trace_key" => str_repeat("3", 32), "uri_or_script" => "/srv/app/cron/run-nightly.php", "start_time_ns" => 1700000002000000000, "state" => "active"],
]);
' "$DATA_DIR/index.sqlite"

# Pick a free port.
PORT="$(php -r '$s = stream_socket_server("tcp://127.0.0.1:0"); $n = stream_socket_get_name($s, false); echo (int) substr($n, strrpos($n, ":") + 1); fclose($s);')"

# Spawn `php -S` with the router. PHPTV_CONFIG points the endpoints at
# our fixture config.
PHPTV_CONFIG="$CONFIG_PATH" php \
    -d error_log="$SERVER_LOG" \
    -S "127.0.0.1:$PORT" \
    tests/api/smoke/router.php \
    >"$SERVER_LOG.out" 2>"$SERVER_LOG.err" &
SERVER_PID=$!

# Wait for the server to listen.
for _ in $(seq 1 50); do
    if curl -fso /dev/null "http://127.0.0.1:$PORT/api/internal/anything" 2>/dev/null \
        || curl -so /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/api/internal/anything" 2>/dev/null | grep -q '^404$'; then
        break
    fi
    sleep 0.05
done

echo "--- POST /api/auth ---"
LOGIN_HEADERS="$TMP/login_headers.txt"
LOGIN_STATUS=$(curl -sS -o /dev/null -w '%{http_code}' -D "$LOGIN_HEADERS" \
    -c "$JAR" -X POST \
    -H 'Content-Type: application/json' \
    -d "{\"token\":\"$TOKEN\"}" \
    "http://127.0.0.1:$PORT/api/auth")

if [[ "$LOGIN_STATUS" != "204" ]]; then
    echo "FAIL: expected 204 from /api/auth, got $LOGIN_STATUS"
    cat "$LOGIN_HEADERS"
    exit 1
fi
if ! grep -qi '^set-cookie: phptv_session=' "$LOGIN_HEADERS"; then
    echo "FAIL: no Set-Cookie phptv_session in login response"
    cat "$LOGIN_HEADERS"
    exit 1
fi
echo "  ok: 204 + Set-Cookie present"

echo "--- GET /api/traces ---"
LIST_BODY="$TMP/list_body.json"
LIST_STATUS=$(curl -sS -o "$LIST_BODY" -w '%{http_code}' \
    -b "$JAR" \
    "http://127.0.0.1:$PORT/api/traces")

if [[ "$LIST_STATUS" != "200" ]]; then
    echo "FAIL: expected 200 from /api/traces, got $LIST_STATUS"
    cat "$LIST_BODY"
    exit 1
fi
TOTAL=$(php -r 'echo json_decode(file_get_contents($argv[1]), true)["total"] ?? "missing";' "$LIST_BODY")
if [[ "$TOTAL" != "3" ]]; then
    echo "FAIL: expected total=3, got $TOTAL"
    cat "$LIST_BODY"
    exit 1
fi
echo "  ok: 200 + total=3"

echo "--- GET /api/traces?q=run- ---"
FILTERED_BODY="$TMP/filtered_body.json"
curl -sS -o "$FILTERED_BODY" -w '' -b "$JAR" "http://127.0.0.1:$PORT/api/traces?q=run-"
FILTERED_TOTAL=$(php -r 'echo json_decode(file_get_contents($argv[1]), true)["total"] ?? "missing";' "$FILTERED_BODY")
if [[ "$FILTERED_TOTAL" != "2" ]]; then
    echo "FAIL: expected filtered total=2, got $FILTERED_TOTAL"
    cat "$FILTERED_BODY"
    exit 1
fi
echo "  ok: filter matched 2 rows"

echo "--- POST /api/auth (wrong token) ---"
WRONG_STATUS=$(curl -sS -o /dev/null -w '%{http_code}' \
    -X POST -H 'Content-Type: application/json' \
    -d '{"token":"wrong"}' \
    "http://127.0.0.1:$PORT/api/auth")
if [[ "$WRONG_STATUS" != "401" ]]; then
    echo "FAIL: expected 401 on wrong token, got $WRONG_STATUS"
    exit 1
fi
echo "  ok: 401 on wrong token"

# 14.2 — token must NOT appear in any captured log.
LOG_HITS=0
for f in "$SERVER_LOG" "$SERVER_LOG.out" "$SERVER_LOG.err"; do
    [[ -f "$f" ]] || continue
    if grep -F "$TOKEN" "$f" >/dev/null; then
        LOG_HITS=$((LOG_HITS + 1))
        echo "FAIL: token appears in $f"
        grep -F "$TOKEN" "$f"
    fi
    if grep -F "$SALT" "$f" >/dev/null; then
        LOG_HITS=$((LOG_HITS + 1))
        echo "FAIL: salt appears in $f"
    fi
done
if [[ "$LOG_HITS" -ne 0 ]]; then
    echo "FAIL: secrets leaked to logs ($LOG_HITS hits)"
    exit 1
fi
echo "  ok: zero token/salt hits across server logs"

echo
echo "SMOKE PASS"
