#!/usr/bin/env bash
#
# bin/reset-data.sh — reset the collector's data state to clean/empty.
#
# For test/eval loops on limited hardware. Instead of re-running
# install-debian.sh, this wipes every trace and rebuilds an empty,
# correctly-permissioned data layout, then restarts the collector so
# it recreates a fresh index.sqlite. It does NOT rebuild the binary,
# rewrite the config, touch the vhost, or reinstall the service unit —
# it only resets DATA.
#
# It deletes ONLY the collector's known data artifacts under the
# configured data_dir:
#   - index.sqlite, index.sqlite-wal, index.sqlite-shm
#   - traces/   (raw <key>.raw/ batches + per-trace <key>.sqlite)
#   - tmp/      (partial uploads)
# It never `rm -rf`s the data_dir itself, so a mis-set data_dir cannot
# cause collateral damage.
#
# This is the explicit-empty-state path; it does not invoke the
# retention sweeper.
#
# Usage:
#   sudo bin/reset-data.sh [--config PATH] [--no-restart] [--yes]
#
#   --config PATH   collector.toml to read [storage].data_dir from.
#                   Default: $PHPTV_CONFIG or /etc/php-tree-viz/collector.toml
#   --no-restart    Do not touch the systemd service; just reset the
#                   data + perms. (Use when you run the collector
#                   manually rather than via systemd.)
#   --yes, -y       Skip the confirmation prompt (for scripted loops).
#   --help          Show this help.

set -euo pipefail

# ---- defaults / args ------------------------------------------------

CONFIG_FILE="${PHPTV_CONFIG:-/etc/php-tree-viz/collector.toml}"
SERVICE="php-tree-viz-collector"
RESTART=1
ASSUME_YES=0

usage() {
    sed -n '2,/^set -euo/ { /^set -euo/d; s/^# \{0,1\}//; p }' "$0"
    exit "${1:-0}"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --config)      CONFIG_FILE="${2:?--config needs a path}"; shift 2 ;;
        --config=*)    CONFIG_FILE="${1#--config=}"; shift ;;
        --no-restart)  RESTART=0; shift ;;
        --yes|-y)      ASSUME_YES=1; shift ;;
        --help|-h)     usage 0 ;;
        *) printf 'error: unrecognised argument %q\n\n' "$1" >&2; usage 1 ;;
    esac
done

step() { printf '\n==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
warn() { printf '    WARNING: %s\n' "$*" >&2; }
fail() { printf '\nERROR: %s\n' "$*" >&2; exit 1; }

# ---- 1. resolve data_dir from the config ----------------------------

[ -f "$CONFIG_FILE" ] || fail "config not found: $CONFIG_FILE (pass --config or set PHPTV_CONFIG)"

# Parse [storage].data_dir. Tracks the current TOML section so a
# data_dir key under a different table can't be picked up by accident;
# strips surrounding quotes and any trailing comment.
DATA_DIR="$(awk '
    /^[[:space:]]*#/ { next }
    /^[[:space:]]*\[[^]]*\]/ { section = $0; next }
    section ~ /\[storage\]/ && /^[[:space:]]*data_dir[[:space:]]*=/ {
        line = $0
        sub(/^[^=]*=[[:space:]]*/, "", line)   # drop "data_dir ="
        sub(/[[:space:]]*#.*/, "", line)        # drop trailing comment
        gsub(/^"|"$/, "", line)                  # strip surrounding quotes
        gsub(/[[:space:]]+$/, "", line)          # trailing whitespace
        print line
        exit
    }
' "$CONFIG_FILE")"

[ -n "$DATA_DIR" ] || fail "could not read [storage].data_dir from $CONFIG_FILE"

# ---- 2. sanity-check the path before we delete anything -------------

case "$DATA_DIR" in
    /*) : ;;
    *)  fail "data_dir is not an absolute path: $DATA_DIR" ;;
esac
case "$DATA_DIR" in
    / | /bin | /boot | /dev | /etc | /home | /lib | /lib64 | /proc | /root | \
    /run | /sbin | /srv | /sys | /tmp | /usr | /var | /var/lib | /var/www)
        fail "refusing to operate on a system path: data_dir=$DATA_DIR" ;;
esac

# ---- 3. determine owner:group to (re)apply --------------------------

if [ -d "$DATA_DIR" ]; then
    OWNER="$(stat -c '%U' "$DATA_DIR")"
    GROUP="$(stat -c '%G' "$DATA_DIR")"
    # stat prints UNKNOWN when there's no name for the id; fall back so
    # chown below still gets something valid.
    [ "$OWNER" != "UNKNOWN" ] || OWNER="www-data"
    [ "$GROUP" != "UNKNOWN" ] || GROUP="www-data"
else
    OWNER="www-data"
    GROUP="www-data"
fi

# ---- 4. decide whether we'll manage the systemd service -------------

have_unit() { command -v systemctl >/dev/null 2>&1 && systemctl cat "$SERVICE" >/dev/null 2>&1; }

MANAGED=0
if [ "$RESTART" = 1 ]; then
    if have_unit; then
        MANAGED=1
    else
        warn "no systemd unit for $SERVICE (or systemctl unavailable) — skipping service control; (re)start the collector yourself afterwards"
    fi
fi

# ---- 5. confirm -----------------------------------------------------

step "Reset collector data state"
note "config:    $CONFIG_FILE"
note "data_dir:  $DATA_DIR"
note "owner:grp: $OWNER:$GROUP"
note "service:   $([ "$MANAGED" = 1 ] && echo "stop+restart $SERVICE" || echo "left untouched")"
note "to delete: index.sqlite{,-wal,-shm}, traces/, tmp/  (data_dir itself is kept)"

if [ "$ASSUME_YES" != 1 ]; then
    if [ ! -t 0 ]; then
        fail "refusing to run non-interactively without --yes"
    fi
    printf '\n    Wipe all traces under %s? [y/N] ' "$DATA_DIR"
    read -r reply
    case "$reply" in
        y|Y|yes|YES) : ;;
        *) fail "aborted by user" ;;
    esac
fi

# ---- 6. require root only where it's actually needed ----------------
#
# Root is required to control the service or to chown to an owner we
# are not. A developer running the collector manually as the data
# owner can reset without sudo via --no-restart.

if [ "$(id -u)" != 0 ]; then
    if [ "$MANAGED" = 1 ]; then
        fail "service control needs root — re-run with sudo, or pass --no-restart"
    fi
    if [ "$(id -un)" != "$OWNER" ]; then
        fail "chown to $OWNER:$GROUP needs root — re-run with sudo, or run as $OWNER"
    fi
fi

# ---- 7. stop the collector (if we're managing it) -------------------

if [ "$MANAGED" = 1 ]; then
    step "Stopping $SERVICE"
    systemctl stop "$SERVICE"
    note "stopped"
fi

# ---- 8. wipe the known data artifacts -------------------------------

step "Wiping trace data under $DATA_DIR"
rm -f  "$DATA_DIR/index.sqlite" "$DATA_DIR/index.sqlite-wal" "$DATA_DIR/index.sqlite-shm"
rm -rf "$DATA_DIR/traces" "$DATA_DIR/tmp"
note "removed index.sqlite (+wal/shm), traces/, tmp/"

# ---- 9. ensure the empty layout with the right perms ----------------
#
# data_dir, traces/, and tmp/ are recreated 2770 (setgid) so files the
# collector writes inherit the shared group and the PHP API (same
# group) can read them — the SPEC §3.5 perm model. The collector also
# (re)asserts traces/ and tmp/ at startup; creating them here keeps the
# layout valid even in --no-restart mode.

step "Recreating empty data layout ($OWNER:$GROUP, mode 2770)"
mkdir -p "$DATA_DIR" "$DATA_DIR/traces" "$DATA_DIR/tmp"
chown "$OWNER:$GROUP" "$DATA_DIR" "$DATA_DIR/traces" "$DATA_DIR/tmp"
chmod 2770 "$DATA_DIR" "$DATA_DIR/traces" "$DATA_DIR/tmp"
note "data_dir, traces/, tmp/ present and group-accessible"

# ---- 10. restart + verify --------------------------------------------

if [ "$MANAGED" = 1 ]; then
    step "Starting $SERVICE"
    systemctl start "$SERVICE"
    # The collector creates a fresh index.sqlite at startup; wait briefly.
    for _ in $(seq 1 20); do
        [ -f "$DATA_DIR/index.sqlite" ] && break
        sleep 0.5
    done
    if systemctl is-active --quiet "$SERVICE"; then
        note "started"
    else
        warn "$SERVICE did not become active — check: journalctl -u $SERVICE"
    fi
fi

step "Result"
if [ -f "$DATA_DIR/index.sqlite" ]; then
    # Best-effort verification via PHP (read-only so we don't create
    # root-owned -wal/-shm sidecars). sqlite3(1) is often absent.
    if command -v php >/dev/null 2>&1; then
        # Single-quoted on purpose: $argv/$db/$n are PHP variables, not
        # shell — the data_dir is passed as the trailing argv[1].
        # shellcheck disable=SC2016
        php -r '
            $p = $argv[1] . "/index.sqlite";
            try {
                $db = new PDO("sqlite:" . $p, null, null, [
                    PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION,
                    PDO::SQLITE_ATTR_OPEN_FLAGS => PDO::SQLITE_OPEN_READONLY,
                ]);
                $uv = $db->query("PRAGMA user_version")->fetchColumn();
                $n  = $db->query("SELECT COUNT(*) FROM traces")->fetchColumn();
                fwrite(STDOUT, "    index.sqlite present: user_version=$uv, traces=$n\n");
                if ((int)$n !== 0) { fwrite(STDERR, "    WARNING: expected 0 traces after reset\n"); }
            } catch (Throwable $e) {
                fwrite(STDERR, "    (verify skipped: " . $e->getMessage() . ")\n");
            }
        ' "$DATA_DIR" || true
    else
        note "index.sqlite present (install sqlite3/php to verify row count)"
    fi
else
    note "index.sqlite not present yet — it is created when the collector starts"
    [ "$MANAGED" = 1 ] || note "start the collector to create it"
fi

echo ""
echo "clean data state ready at $DATA_DIR"
