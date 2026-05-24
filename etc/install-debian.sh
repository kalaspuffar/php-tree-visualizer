#!/bin/bash
# shellcheck disable=SC2317  # the __PROBE_BASE64__ marker + blob at the
# bottom of the file are data, not code — read via sed $0 from inside the
# smoke-test section. shellcheck flags them as unreachable, which they
# are by design.
#
# install-debian.sh — one-shot installer for the php-tree-visualizer
# collector + PHP API + static frontend on a freshly-imaged Debian 13
# host. Tested on Debian 13 (Trixie). Operators on other distros: read
# this script and adapt; the apt + a2enmod incantations are
# distro-specific, the rest is shape-portable.
#
# Usage:
#   sudo bash etc/install-debian.sh <public-hostname> [data-dir-owner]
#
# Arguments:
#   <public-hostname>    The DNS name the vhost will serve, e.g.
#                        visualizer.example.org. The vhost's
#                        ServerName is set from this.
#   [data-dir-owner]     Optional, defaults to `www-data`. The user
#                        that will own /etc/php-tree-viz/collector.toml
#                        and /var/lib/php-tree-viz/.
#
# Idempotency contract (re-running this script on a working install
# is a no-op):
#   - apt packages already installed are skipped (apt handles dedupe).
#   - Apache modules already enabled are skipped (a2enmod -q is a no-op).
#   - The data directory, config directory, and webroot are created
#     with `mkdir -p` (no-op if present).
#   - /etc/php-tree-viz/collector.toml is NEVER overwritten if it
#     exists — re-running preserves the existing token + salt and
#     does NOT invalidate active sessions.
#   - The systemd unit and Apache vhost are overwritten from the
#     tracked templates on every run. Hand-edits to either get
#     reverted with a one-line notice. The tracked files at
#     etc/php-tree-viz-collector.service.example and
#     etc/apache-example.conf are the canonical source.
#   - The smoke test runs unconditionally on every invocation.
#
# Exit codes:
#   0     install succeeded and the smoke test passed.
#   2     usage error (missing/bad arguments).
#   1     install or smoke test failed (diagnostic on stderr).

set -euo pipefail

# ---------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------

usage() {
    echo "usage: sudo bash $0 <public-hostname> [data-dir-owner]" >&2
    echo "" >&2
    echo "  <public-hostname>    DNS name served by the vhost (required)" >&2
    echo "  [data-dir-owner]     user owning data + config (default: www-data)" >&2
    exit 2
}

if [ "$#" -lt 1 ] || [ -z "${1-}" ]; then
    usage
fi
HOSTNAME_ARG="$1"
DATA_OWNER="${2-www-data}"

# Resolve repo root from the script's location (the script lives in
# etc/, so the repo is one level up).
REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"

# Hardcoded conventions. Operators wanting non-default paths should
# read + adapt this script rather than parameterising further.
COLLECTOR_BIN="/usr/local/bin/php-tree-viz-collector"
CONFIG_DIR="/etc/php-tree-viz"
CONFIG_FILE="$CONFIG_DIR/collector.toml"
DATA_DIR="/var/lib/php-tree-viz"
WEBROOT="/var/www/php-tree-viz"
UNIT_PATH="/etc/systemd/system/php-tree-viz-collector.service"
VHOST_PATH="/etc/apache2/sites-available/${HOSTNAME_ARG}.conf"

step() { printf '\n==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
fail() { printf '\nERROR: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------
# 1. apt packages
# ---------------------------------------------------------------------

step "Installing apt packages"
DEBIAN_FRONTEND=noninteractive apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
    apache2 \
    php8.4-fpm \
    php8.4-sqlite3 \
    libclang-dev \
    curl \
    openssl

# ---------------------------------------------------------------------
# 2. Apache modules + global PHP-FPM handler
# ---------------------------------------------------------------------

step "Enabling Apache modules and the global PHP-FPM conf"
a2enmod -q proxy proxy_fcgi proxy_http rewrite headers setenvif
a2enconf -q php8.4-fpm
systemctl reload apache2
systemctl reload php8.4-fpm

# ---------------------------------------------------------------------
# 3. Build + install the collector binary
# ---------------------------------------------------------------------

step "Building the collector (cargo build --release)"
RELEASE_BIN="$REPO_ROOT/target/release/php-tree-viz-collector"
if [ ! -x "$RELEASE_BIN" ] || [ -n "$(find "$REPO_ROOT/crates" "$REPO_ROOT/Cargo.toml" "$REPO_ROOT/Cargo.lock" -newer "$RELEASE_BIN" -type f -print -quit 2>/dev/null)" ]; then
    note "compiling — this can take several minutes on a fresh box"
    # cargo runs as the repo owner (not root) so its target/ dir
    # belongs to the user, not root. Switch to the repo owner if
    # we're root.
    OWNER="$(stat -c '%U' "$REPO_ROOT")"
    if [ "$(id -u)" -eq 0 ] && [ "$OWNER" != "root" ]; then
        sudo -u "$OWNER" -H bash -c "cd '$REPO_ROOT' && cargo build --release --quiet"
    else
        (cd "$REPO_ROOT" && cargo build --release --quiet)
    fi
else
    note "binary already built and up to date"
fi
install -o root -g root -m 0755 "$RELEASE_BIN" "$COLLECTOR_BIN"

# ---------------------------------------------------------------------
# 4. Config directory + secrets (generate on first run, preserve on re-run)
# ---------------------------------------------------------------------

step "Deploying $CONFIG_FILE"
mkdir -p "$CONFIG_DIR"
if [ -f "$CONFIG_FILE" ]; then
    note "config exists — preserving secrets"
else
    note "generating fresh 40-char token + salt"
    TOKEN="$(openssl rand -base64 33 | tr -d +/= | head -c 40)"
    SALT="$(openssl rand -base64 33 | tr -d +/= | head -c 40)"
    sed -e "s|REPLACE_ME_TOO|$SALT|" -e "s|REPLACE_ME|$TOKEN|" \
        "$REPO_ROOT/etc/collector.toml.example" > "$CONFIG_FILE"
fi
chown "$DATA_OWNER:www-data" "$CONFIG_FILE"
chmod 0640 "$CONFIG_FILE"

# ---------------------------------------------------------------------
# 5. Data directory + webroot
# ---------------------------------------------------------------------

step "Creating $DATA_DIR (2770 setgid) and populating $WEBROOT"
mkdir -p "$DATA_DIR"
chown "$DATA_OWNER:www-data" "$DATA_DIR"
chmod 2770 "$DATA_DIR"

mkdir -p "$WEBROOT"
cp -a "$REPO_ROOT/api"  "$WEBROOT/"
cp -a "$REPO_ROOT/viz"  "$WEBROOT/"
chown -R "$DATA_OWNER:www-data" "$WEBROOT"

# ---------------------------------------------------------------------
# 6. systemd unit
# ---------------------------------------------------------------------

step "Installing the systemd unit"
SRC_UNIT="$REPO_ROOT/etc/php-tree-viz-collector.service.example"
if [ -f "$UNIT_PATH" ] && ! cmp -s "$SRC_UNIT" "$UNIT_PATH"; then
    note "overwriting hand-edited unit (the tracked template is source of truth)"
fi
install -o root -g root -m 0644 "$SRC_UNIT" "$UNIT_PATH"
systemctl daemon-reload
systemctl enable --now php-tree-viz-collector

# ---------------------------------------------------------------------
# 7. Apache vhost
# ---------------------------------------------------------------------

step "Installing the Apache vhost at $VHOST_PATH"
if [ -f "$VHOST_PATH" ]; then
    note "overwriting any prior vhost — the tracked apache-example.conf is source of truth"
fi
{
    echo "# Generated by install-debian.sh for $HOSTNAME_ARG."
    echo "# Source: etc/apache-example.conf in the php-tree-visualizer repo."
    echo "<VirtualHost *:80>"
    echo "    ServerName $HOSTNAME_ARG"
    echo ""
    sed 's/^/    /' "$REPO_ROOT/etc/apache-example.conf"
    echo "</VirtualHost>"
} > "$VHOST_PATH"
chmod 0644 "$VHOST_PATH"
chown root:root "$VHOST_PATH"

a2dissite -q 000-default >/dev/null 2>&1 || true
a2ensite -q "$HOSTNAME_ARG"
apache2ctl configtest 2>&1 | tail -1
systemctl reload apache2

# ---------------------------------------------------------------------
# 8. Smoke test
# ---------------------------------------------------------------------

step "Smoke test — POST probe, wait for finalize, list traces"

# Verify both services are active.
systemctl is-active --quiet php-tree-viz-collector || fail "collector is not active"
systemctl is-active --quiet apache2 || fail "apache2 is not active"

# Read the bearer token from the config (group-readable via www-data;
# this script runs as root so it can read regardless).
TOKEN="$(awk -F'"' '/^token/ {print $2; exit}' "$CONFIG_FILE")"
[ -n "$TOKEN" ] || fail "could not read auth.token from $CONFIG_FILE"

# Decode the embedded probe body. The blob is at the bottom of this
# script after the marker. See section "Probe blob" below for how it
# was generated.
PROBE="$(mktemp)"
trap 'rm -f "$PROBE"' EXIT
sed -n '/^__PROBE_BASE64__$/,$ p' "$0" | tail -n +2 | base64 -d > "$PROBE"
[ "$(wc -c < "$PROBE")" = 474 ] || fail "probe body has wrong size — script corruption?"

# 8a. POST the probe through the Apache vhost.
note "POST http://${HOSTNAME_ARG}/ingest/v1 — expect 200"
HTTP_CODE="$(curl -s -o /dev/null -w '%{http_code}' \
    -X POST "http://${HOSTNAME_ARG}/ingest/v1" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/vnd.php-analyze.v1+msgpack" \
    --data-binary "@$PROBE")"
[ "$HTTP_CODE" = 200 ] || fail "POST /ingest/v1 returned HTTP $HTTP_CODE (expected 200)"

# 8b. Log in to the PHP API.
COOKIE="$(mktemp)"
trap 'rm -f "$PROBE" "$COOKIE"' EXIT
HTTP_CODE="$(curl -s -o /dev/null -w '%{http_code}' -c "$COOKIE" \
    -X POST "http://${HOSTNAME_ARG}/api/auth" \
    -H 'Content-Type: application/json' \
    -d "{\"token\":\"$TOKEN\"}")"
[ "$HTTP_CODE" = 204 ] || fail "POST /api/auth returned HTTP $HTTP_CODE (expected 204)"

# 8c. Poll /api/traces for up to 60s.
note "polling /api/traces for the new trace (≤60s)"
DEADLINE=$(( $(date +%s) + 60 ))
TRACE_KEY=""
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    BODY="$(curl -s -b "$COOKIE" "http://${HOSTNAME_ARG}/api/traces")"
    if echo "$BODY" | grep -q '"trace_key"'; then
        TRACE_KEY="$(echo "$BODY" | sed -n 's/.*"trace_key":"\([0-9a-f]*\)".*/\1/p' | head -n 1)"
        break
    fi
    sleep 2
done
[ -n "$TRACE_KEY" ] || fail "no trace appeared in /api/traces within 60s"

echo ""
echo "installed and verified: trace_key=$TRACE_KEY"
echo ""
echo "Next:"
echo "  - browse http://${HOSTNAME_ARG}/viz/login.html"
echo "  - sign in with the token from $CONFIG_FILE"
echo "  - point your php-analyze extension at http://127.0.0.1:8088/ingest/v1"
echo "    (or via the public vhost at http://${HOSTNAME_ARG}/ingest/v1)"
echo "  - for TLS: sudo apt-get install -y certbot python3-certbot-apache &&"
echo "             sudo certbot --apache -d ${HOSTNAME_ARG}"

exit 0

# ---------------------------------------------------------------------
# Probe blob
# ---------------------------------------------------------------------
#
# Base64-encoded 474-byte v1 MessagePack batch. Equivalent to:
#
#   build_test_batch_with_chain("verify-host", 4242, 1700000000000000000)
#
# from crates/php-tree-viz-collector/tests/support/batch.rs. Decoded by
# the smoke-test section above via `base64 -d` (base64 is in coreutils
# on every Debian install; `xxd` is not, hence the choice).
#
# To regenerate (if the wire format changes):
#   1. Use the synth helper in tests/support/batch.rs or the
#      /tmp/probe-gen one-off the verification used.
#   2. base64 -w 0 /tmp/phptv-probe.msgpack
#   3. Paste the single-line output after the __PROBE_BASE64__ marker.
#
__PROBE_BASE64__
g6RtZXRhiK5zY2hlbWFfdmVyc2lvbgGodHJhY2VfaWTZJDAwMDAwMDAwLTAwMDAtMDAwMC0wMDAwLTAwMDAwMDAwMDAwMKRob3N0q3ZlcmlmeS1ob3N0o3BpZM0QkqpzdGFydF90aW1lzxeXnP42KgAApHNhcGmjY2xprXVyaV9vcl9zY3JpcHSvL3RtcC92ZXJpZnkucGhwr2Ryb3BwZWRfcmVjb3JkcwCkZGljdJKFpWZuX2lkAaNmcW6oQXBwXG1haW6kZmlsZa8vdG1wL3ZlcmlmeS5waHCkbGluZQGka2luZACFpWZuX2lkAqNmcW6pQXBwXGNoaWxkpGZpbGWvL3RtcC92ZXJpZnkucGhwpGxpbmUKpGtpbmQApWNhbGxzkounY2FsbF9pZAGmcGFyZW50AqJmbgKlZGVwdGgCpHRfaW5kpXRfb3V0zJalY3B1X3UFpWNwdV9zAqZtZW1faW4Ap21lbV9vdXTNBACtYWJub3JtYWxfZXhpdMKLp2NhbGxfaWQCpnBhcmVudACiZm4BpWRlcHRoAaR0X2luAKV0X291dMzIpWNwdV91FKVjcHVfcwWmbWVtX2luAKdtZW1fb3V0zRAArWFibm9ybWFsX2V4aXTC
