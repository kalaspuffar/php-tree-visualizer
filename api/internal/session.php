<?php

declare(strict_types=1);

/**
 * Session derivation + cookie plumbing.
 *
 * Session value = base64url( hmac_sha256(salt, "phptv:v1:" . token) )
 *
 * The "phptv:v1:" prefix namespaces this derivation so a future v2
 * scheme (UA-binding, longer salt, whatever) can land without
 * colliding with old cookies. Verification recomputes and uses
 * hash_equals (constant-time).
 *
 * SPEC ↔ COMMENTS divergence on UA fingerprint: COMMENTS.md wins (no
 * UA in the input). Open Question 1 in design.md tracks this.
 *
 *   compute_session_value(string $token, string $salt): string
 *   issue_session_cookie(string $value): void
 *   clear_session_cookie(): void
 *   require_session(): void   — 401s on absence/mismatch
 */

require_once __DIR__ . '/config.php';
require_once __DIR__ . '/response.php';

const PHPTV_COOKIE_NAME = 'phptv_session';
const PHPTV_HMAC_PREFIX = 'phptv:v1:';

function compute_session_value(string $token, string $salt): string
{
    $raw = hash_hmac('sha256', PHPTV_HMAC_PREFIX . $token, $salt, true);
    return phptv_base64url_encode($raw);
}

/**
 * RFC-4648 §5 base64url (no padding).
 */
function phptv_base64url_encode(string $raw): string
{
    return rtrim(strtr(base64_encode($raw), '+/', '-_'), '=');
}

/**
 * Issue the session cookie. Emits Set-Cookie via the response shim so
 * the test harness sees an identical line. `Secure` is added when
 * `[server].tls = true` in the TOML.
 */
function issue_session_cookie(string $value): void
{
    $config = Config::load();
    $secure = $config->getBool('server', 'tls', false);

    $parts = [
        PHPTV_COOKIE_NAME . '=' . $value,
        'Path=/',
        'HttpOnly',
        'SameSite=Lax',
    ];
    if ($secure) {
        $parts[] = 'Secure';
    }
    phptv_emit_set_cookie(implode('; ', $parts));
}

function clear_session_cookie(): void
{
    phptv_emit_set_cookie(
        PHPTV_COOKIE_NAME . '=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax'
    );
}

/**
 * Enforce a valid session on the current request. Writes 401 and
 * exits on absence or mismatch — never returns false; callers can
 * assume control returns only on a verified session.
 */
function require_session(): void
{
    $cookie = $_COOKIE[PHPTV_COOKIE_NAME] ?? null;
    if (!is_string($cookie) || $cookie === '') {
        json_error(401, 'unauthorized');
    }
    $config = Config::load();
    $expected = compute_session_value(
        $config->getString('auth', 'token'),
        $config->getString('auth', 'session_salt')
    );
    if (!hash_equals($expected, $cookie)) {
        json_error(401, 'unauthorized');
    }
}
