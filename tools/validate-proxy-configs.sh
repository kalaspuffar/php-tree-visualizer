#!/bin/bash
#
# validate-proxy-configs.sh — syntax-check the proxy example
# fragments shipped under etc/.
#
# Why this script exists: etc/apache-example.conf and
# etc/nginx-example.conf are VirtualHost / server-block
# *fragments*, not standalone configs. They can't be passed
# directly to `apache2ctl -t` or `nginx -t`. The two
# etc/*-validate.conf.in templates supply the scaffolding;
# this script substitutes the repo root into each, runs the
# proxy's native configtest, and exits 0 iff both pass.
#
# Used by:
#   - .github/workflows/ci.yml (the `proxy-configs` job).
#   - Operators editing either example file who want a fast
#     local check before pushing.
#
# Usage:
#   bash tools/validate-proxy-configs.sh                   # both
#   bash tools/validate-proxy-configs.sh --proxy=apache    # apache only
#   bash tools/validate-proxy-configs.sh --proxy=nginx     # nginx only
#
# Exit codes:
#   0   selected configtest(s) passed.
#   2   usage error (bad --proxy value, extra positional, etc.).
#   1   at least one configtest failed; stderr names which.
#
# Requirements on the invoking host:
#   --proxy=apache : `apache2ctl` on PATH (Debian/Ubuntu:
#                    `apt-get install apache2`).
#   --proxy=nginx  : `nginx` on PATH (Debian/Ubuntu:
#                    `apt-get install nginx-light`).
#   --proxy=both   : both of the above.
#
# This script is deliberately small and free of optional
# dependencies. It runs on any POSIX bash with `sed` and
# `mktemp`.

set -euo pipefail

# ---- Arg parsing -----------------------------------------------------

PROXY="both"
for arg in "$@"; do
    case "$arg" in
        --proxy=apache|--proxy=nginx|--proxy=both)
            PROXY="${arg#--proxy=}"
            ;;
        --proxy=*)
            echo "error: unrecognised --proxy value '${arg#--proxy=}' (must be apache, nginx, or both)" >&2
            exit 2
            ;;
        -h|--help)
            sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "error: unexpected argument '$arg'" >&2
            echo "usage: bash $0 [--proxy=apache|nginx|both]" >&2
            exit 2
            ;;
    esac
done

# ---- Resolve REPO_ROOT from the script's own location ----------------
#
# The script lives in tools/, so the repo is one level up. Resolving
# from the script's own path (not $PWD) means `bash
# /abs/path/tools/validate-proxy-configs.sh` works from any cwd.

REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"

APACHE_TEMPLATE="$REPO_ROOT/etc/apache-validate.conf.in"
NGINX_TEMPLATE="$REPO_ROOT/etc/nginx-validate.conf.in"

# ---- Tempfile bookkeeping -------------------------------------------
#
# Each generated wrapper gets pushed onto TMPFILES; a single EXIT trap
# unlinks them on script exit (success or failure). The trap fires
# regardless of how the script ends.

TMPFILES=()
cleanup_tmpfiles() {
    local f
    for f in "${TMPFILES[@]:-}"; do
        [ -n "$f" ] && rm -f "$f"
    done
}
trap cleanup_tmpfiles EXIT

generate_wrapper() {
    # $1 = template path, $2 = label (for the tempfile suffix).
    local template="$1"
    local label="$2"
    local out
    out="$(mktemp --suffix=".phptv-${label}-validate.conf")"
    TMPFILES+=("$out")
    sed "s|__REPO_ROOT__|${REPO_ROOT}|g" "$template" > "$out"
    printf '%s' "$out"
}

# ---- Individual configtests -----------------------------------------

run_apache_configtest() {
    # We call the apache2 binary directly, not the apache2ctl wrapper,
    # because Debian's apache2ctl unconditionally runs
    # `mkdir -p $APACHE_RUN_DIR` (defaulting to /var/run/apache2)
    # on every invocation — including `-t`. That fails as a non-root
    # user (the CI runner, or any operator running this script
    # locally without prior sudo). The apache2 binary's `-t -f` does
    # the same syntax check without the runtime-dir setup.
    local apache_bin=""
    if command -v apache2 >/dev/null 2>&1; then
        apache_bin=apache2
    elif [ -x /usr/sbin/apache2 ]; then
        apache_bin=/usr/sbin/apache2
    else
        echo "error: apache2 binary not found — install apache2 (apt-get install apache2)" >&2
        return 1
    fi
    local wrapper
    wrapper="$(generate_wrapper "$APACHE_TEMPLATE" apache)"

    # `apache2 -t -f <file>` writes "Syntax OK" to stderr on success
    # (Apache convention). We capture both streams and inspect them.
    local output rc
    output="$("$apache_bin" -t -f "$wrapper" 2>&1)"
    rc=$?
    if [ "$rc" -ne 0 ] || ! grep -q 'Syntax OK' <<<"$output"; then
        echo "error: apache2 -t FAILED against etc/apache-example.conf" >&2
        echo "------ apache2 -t output ------" >&2
        echo "$output" >&2
        echo "-------------------------------" >&2
        return 1
    fi
    echo "ok: apache2 -t passed against etc/apache-example.conf"
}

run_nginx_configtest() {
    if ! command -v nginx >/dev/null 2>&1; then
        echo "error: nginx not on PATH — install nginx-light (apt-get install nginx-light)" >&2
        return 1
    fi
    local wrapper
    wrapper="$(generate_wrapper "$NGINX_TEMPLATE" nginx)"

    # `nginx -t -c <file> -p /tmp` runs the configtest with `/tmp` as
    # the prefix, so any default-relative paths in nginx land in a
    # writable location. nginx writes its configtest banner to stderr.
    local output rc
    output="$(nginx -t -c "$wrapper" -p /tmp 2>&1)"
    rc=$?
    if [ "$rc" -ne 0 ] || ! grep -q 'test is successful' <<<"$output"; then
        echo "error: nginx -t FAILED against etc/nginx-example.conf" >&2
        echo "------ nginx -t output ------" >&2
        echo "$output" >&2
        echo "-----------------------------" >&2
        return 1
    fi
    echo "ok: nginx -t passed against etc/nginx-example.conf"
}

# ---- Dispatch --------------------------------------------------------

case "$PROXY" in
    apache) run_apache_configtest ;;
    nginx)  run_nginx_configtest ;;
    both)   run_apache_configtest && run_nginx_configtest ;;
esac
