#!/usr/bin/env bash
#
# tests/api/smoke/run_browser_smoke.sh — browser-flow smoke that
# exercises the static `/viz/*` files alongside the JSON API. Goes
# through:
#
#   1) GET  /viz/login.html              -> 200, contains the CSP meta
#                                           tag and a <script type="module">
#   2) GET  /viz/assets/styles.css       -> 200, CSS body
#   3) GET  /viz/assets/icons.svg        -> 200, image/svg+xml
#   4) GET  /viz/js/login.js             -> 200, JS body
#   5) GET  /viz/js/list.js              -> 200, JS body
#   6) POST /api/auth                    -> 204, cookie issued
#   7) GET  /viz/index.html with cookie  -> 200 (static; cookie not
#                                           consulted, but the JS
#                                           inside will fetch /api/traces)
#   8) GET  /api/traces                  -> 200, total=3
#   9) Zero hits on the configured token / salt in any server log.
#
# Doesn't drive an actual browser (no headless Chromium). Confirms
# the static assets serve correctly through the proxy-emulating
# router so the JS modules are reachable; the in-browser behaviour
# itself is covered by the Node unit tests in tests/frontend/.

set -euo pipefail

cd "$(dirname "$0")/../../.."

TMP="$(mktemp -d -t phptv-browser-smoke.XXXXXX)"
trap 'rm -rf "$TMP"; if [[ -n "${SERVER_PID:-}" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then kill "$SERVER_PID" 2>/dev/null || true; fi' EXIT

TOKEN="BROWSER-SMOKE-TOKEN-$(head -c 24 /dev/urandom | base64 | tr -d '=' | tr '/+' '_-')"
SALT="BROWSER-SMOKE-SALT-$(head -c 24 /dev/urandom | base64 | tr -d '=' | tr '/+' '_-')"
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

# Build the fixture index.sqlite.
php -r '
require "tests/api/fixtures/make_index_sqlite.php";
make_index_sqlite($argv[1], [
    ["trace_key" => str_repeat("1", 32), "uri_or_script" => "/srv/app/index.php", "start_time_ns" => 1700000000000000000],
    ["trace_key" => str_repeat("2", 32), "uri_or_script" => "/srv/app/bin/run-tests.php", "start_time_ns" => 1700000001000000000],
    ["trace_key" => str_repeat("3", 32), "uri_or_script" => "/srv/app/cron/run-nightly.php", "start_time_ns" => 1700000002000000000, "state" => "active"],
]);
' "$DATA_DIR/index.sqlite"

PORT="$(php -r '$s = stream_socket_server("tcp://127.0.0.1:0"); $n = stream_socket_get_name($s, false); echo (int) substr($n, strrpos($n, ":") + 1); fclose($s);')"

PHPTV_CONFIG="$CONFIG_PATH" php \
    -d error_log="$SERVER_LOG" \
    -S "127.0.0.1:$PORT" \
    tests/api/smoke/router.php \
    >"$SERVER_LOG.out" 2>"$SERVER_LOG.err" &
SERVER_PID=$!

for _ in $(seq 1 50); do
    if curl -so /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/api/internal/anything" 2>/dev/null | grep -q '^404$'; then
        break
    fi
    sleep 0.05
done

ok() { echo "  ok: $*"; }
fail() { echo "FAIL: $*"; exit 1; }

# 1) login.html serves and carries CSP + module script.
LOGIN_HTML="$TMP/login.html"
S=$(curl -sS -o "$LOGIN_HTML" -w '%{http_code}' "http://127.0.0.1:$PORT/viz/login.html")
[[ "$S" == "200" ]]              || fail "/viz/login.html status $S"
grep -q 'Content-Security-Policy' "$LOGIN_HTML"   || fail "no CSP meta"
grep -q '<script type="module"'    "$LOGIN_HTML"   || fail "no module script"
grep -q 'id="login-form"'          "$LOGIN_HTML"   || fail "no login form"
ok "login.html serves with CSP + module script + form"

# 2) styles.css serves with CSS content-type.
H=$(curl -sS -o /dev/null -w '%{content_type}' "http://127.0.0.1:$PORT/viz/assets/styles.css")
[[ "$H" == text/css* ]]          || fail "styles.css content-type $H"
S=$(curl -sS -o "$TMP/styles.css" -w '%{http_code}' "http://127.0.0.1:$PORT/viz/assets/styles.css")
[[ "$S" == "200" ]]              || fail "styles.css status $S"
grep -q -- '--accent:' "$TMP/styles.css" || fail "styles.css doesn't carry the token"
ok "styles.css serves as text/css and carries design tokens"

# 3) icons.svg serves with image/svg+xml.
H=$(curl -sS -o "$TMP/icons.svg" -w '%{content_type}' "http://127.0.0.1:$PORT/viz/assets/icons.svg")
[[ "$H" == image/svg+xml* ]]     || fail "icons.svg content-type $H"
grep -q 'id="icon-search"' "$TMP/icons.svg" || fail "icons.svg doesn't contain icon-search symbol"
ok "icons.svg serves as image/svg+xml with the documented symbols"

# 4-5) login.js and list.js serve.
for path in /viz/js/login.js /viz/js/list.js /viz/js/api-client.js /viz/js/time.js /viz/js/debounce.js; do
    S=$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT$path")
    [[ "$S" == "200" ]] || fail "$path status $S"
done
ok "all JS modules serve over HTTP"

# 6) login the API.
LOGIN_STATUS=$(curl -sS -o /dev/null -w '%{http_code}' \
    -c "$JAR" -X POST \
    -H 'Content-Type: application/json' \
    -d "{\"token\":\"$TOKEN\"}" \
    "http://127.0.0.1:$PORT/api/auth")
[[ "$LOGIN_STATUS" == "204" ]] || fail "login status $LOGIN_STATUS"
# curl's Netscape-format jar stores HttpOnly cookies prefixed with
# "#HttpOnly_" on tab-separated lines; match the field name only.
grep -q 'phptv_session' "$JAR"  || fail "no session cookie in jar"
ok "POST /api/auth → 204 + cookie"

# 7) index.html serves regardless of cookie state (it's a static page).
S=$(curl -sS -o "$TMP/index.html" -w '%{http_code}' "http://127.0.0.1:$PORT/viz/index.html")
[[ "$S" == "200" ]]              || fail "/viz/index.html status $S"
grep -q 'id="filter-input"'   "$TMP/index.html" || fail "no filter input"
grep -q 'class="skeleton-row"' "$TMP/index.html" || fail "no skeleton rows in initial HTML"
ok "index.html serves with skeleton rows + filter input"

# 8) API call the page would make.
LIST_BODY="$TMP/list.json"
S=$(curl -sS -o "$LIST_BODY" -w '%{http_code}' -b "$JAR" "http://127.0.0.1:$PORT/api/traces")
[[ "$S" == "200" ]] || fail "/api/traces status $S"
T=$(php -r 'echo json_decode(file_get_contents($argv[1]), true)["total"] ?? "missing";' "$LIST_BODY")
[[ "$T" == "3" ]] || fail "expected total=3, got $T"
ok "GET /api/traces (with cookie) → 200 + total=3"

# 9) Zero token/salt hits across logs.
HITS=0
for f in "$SERVER_LOG" "$SERVER_LOG.out" "$SERVER_LOG.err"; do
    [[ -f "$f" ]] || continue
    grep -F "$TOKEN" "$f" && HITS=$((HITS+1)) || true
    grep -F "$SALT"  "$f" && HITS=$((HITS+1)) || true
done
# Also confirm the served HTML never echoed the token.
for f in "$LOGIN_HTML" "$TMP/index.html" "$LIST_BODY"; do
    [[ -f "$f" ]] || continue
    if grep -F "$TOKEN" "$f"; then
        HITS=$((HITS+1))
        echo "FAIL: token in $f"
    fi
done
[[ "$HITS" == "0" ]] || fail "secrets leaked ($HITS hits)"
ok "zero token/salt hits across server logs and served HTML"

echo
echo "BROWSER SMOKE PASS"
