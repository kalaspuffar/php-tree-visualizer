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
#   bash bin/validate-proxy-configs.sh                   # both
#   bash bin/validate-proxy-configs.sh --proxy=apache    # apache only
#   bash bin/validate-proxy-configs.sh --proxy=nginx     # nginx only
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
# The script lives in bin/, so the repo is one level up. Resolving
# from the script's own path (not $PWD) means `bash
# /abs/path/bin/validate-proxy-configs.sh` works from any cwd.

REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"

APACHE_TEMPLATE="$REPO_ROOT/etc/apache-validate.conf.in"
NGINX_TEMPLATE="$REPO_ROOT/etc/nginx-validate.conf.in"

# ---- Tempfile / tempdir bookkeeping ---------------------------------
#
# Generated wrappers and nginx's helper tempdir get pushed onto
# TMPFILES / TMPDIRS; a single EXIT trap cleans them up on script
# exit (success or failure). The trap fires regardless of how the
# script ends.

TMPFILES=()
TMPDIRS=()
cleanup_tmpfiles() {
    # Snapshot the exit code BEFORE doing anything else — `set +e`
    # below would otherwise let later commands stomp $? before we
    # have a chance to record it. Disable set -e for the body so a
    # surprise rm/test failure can't turn a clean run into a 1.
    local entry_rc=$?
    set +e
    echo ">>> trap firing, captured exit rc=${entry_rc}" >&2
    local f
    for f in "${TMPFILES[@]:-}"; do
        [ -n "$f" ] && rm -f "$f"
    done
    local d
    for d in "${TMPDIRS[@]:-}"; do
        [ -n "$d" ] && [ -d "$d" ] && rm -rf "$d"
    done
    echo ">>> trap done, exiting with rc=${entry_rc}" >&2
    # Re-exit with the snapshotted code. Without this, certain
    # bash versions let the trap's own last-command rc override
    # what the main body returned.
    exit "$entry_rc"
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

generate_nginx_wrapper_dir() {
    # nginx resolves the relative `include fastcgi_params;` directive
    # in etc/nginx-example.conf against a base directory that, in
    # practice, ends up being the directory containing the -c file
    # (the -p flag is supposed to set this but is overridden in
    # current nginx versions). We can't change the example fragment
    # — operators rely on `include fastcgi_params;` being relative
    # because that's the canonical Debian/Ubuntu shape. So we put
    # both the wrapper AND fastcgi_params in the same tempdir, and
    # nginx finds the include next to the wrapper.
    local out_dir
    out_dir="$(mktemp -d --suffix=.phptv-nginx-validate)"
    TMPDIRS+=("$out_dir")
    sed "s|__REPO_ROOT__|${REPO_ROOT}|g" "$NGINX_TEMPLATE" > "$out_dir/nginx.conf"
    if [ -f /etc/nginx/fastcgi_params ]; then
        cp /etc/nginx/fastcgi_params "$out_dir/fastcgi_params"
    else
        echo "warning: /etc/nginx/fastcgi_params not present — nginx -t will fail on the relative include" >&2
    fi
    printf '%s' "$out_dir"
}

# ---- Individual configtests -----------------------------------------

# Capture a configtest invocation under explicit `set +e` brackets.
# Bash's `set -e` interaction with `var=$(cmd)` assignments is
# version-dependent — defensively suppressing it around the
# capture means a non-zero exit always lands in $rc instead of
# silently killing the script. We restore -e immediately after.
capture_configtest() {
    local _output_var="$1"
    local _rc_var="$2"
    shift 2
    set +e
    local _output
    _output="$("$@" 2>&1)"
    local _rc=$?
    set -e
    printf -v "$_output_var" '%s' "$_output"
    printf -v "$_rc_var" '%s' "$_rc"
}

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

    echo ">>> running ${apache_bin} -t -f ${wrapper}" >&2
    local output rc
    capture_configtest output rc "$apache_bin" -t -f "$wrapper"
    echo ">>> apache2 -t exited rc=${rc}" >&2
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
    local nginx_dir
    nginx_dir="$(generate_nginx_wrapper_dir)"
    local wrapper="$nginx_dir/nginx.conf"

    # `-p <tempdir>` is the prefix nginx uses for relative paths;
    # the wrapper file and a copy of fastcgi_params both live in
    # that tempdir, so the example fragment's
    # `include fastcgi_params;` resolves cleanly.
    echo ">>> running nginx -t -c ${wrapper} -p ${nginx_dir}" >&2
    local output rc
    capture_configtest output rc nginx -t -c "$wrapper" -p "$nginx_dir"
    echo ">>> nginx -t exited rc=${rc}" >&2
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

# Run with set +e so a failed configtest goes to the diagnostic
# block (it already does — but suspending set -e here makes the
# control flow explicit and prevents future surprises where set
# -e + && interactions silently abort the script).
set +e
case "$PROXY" in
    apache) run_apache_configtest ;;
    nginx)  run_nginx_configtest ;;
    both)   run_apache_configtest && run_nginx_configtest ;;
esac
final_rc=$?
echo ">>> main body done, final_rc=${final_rc}" >&2
exit "$final_rc"
