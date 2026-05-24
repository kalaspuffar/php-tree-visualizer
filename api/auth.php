<?php

declare(strict_types=1);

/**
 * Authentication endpoints.
 *
 *   POST /api/auth         token -> phptv_session cookie
 *   POST /api/auth/logout  always 204, always clears the cookie
 *
 * Both paths are routed to this file by the reverse proxy; the
 * dispatch below picks between them on REQUEST_URI's path.
 */

require_once __DIR__ . '/bootstrap.php';

$requestPath = parse_url(
    (string) ($_SERVER['REQUEST_URI'] ?? '/'),
    PHP_URL_PATH
) ?? '/';

switch ($requestPath) {
    case '/api/auth':
        phptv_handle_login();
        break;
    case '/api/auth/logout':
        phptv_handle_logout();
        break;
    default:
        json_error(404, 'not_found');
}

function phptv_handle_login(): void
{
    dispatch_method('POST');
    $body = read_json_body();

    $submittedToken = $body['token'] ?? null;
    if (!is_string($submittedToken) || $submittedToken === '') {
        json_error(400, 'bad_request');
    }

    $config = Config::load();
    $configuredToken = $config->getString('auth', 'token');
    if (!hash_equals($configuredToken, $submittedToken)) {
        // Important: do NOT include the submitted token in any log
        // or response — INV-2. The error code is enough.
        json_error(401, 'unauthorized');
    }

    $sessionValue = compute_session_value(
        $configuredToken,
        $config->getString('auth', 'session_salt')
    );
    issue_session_cookie($sessionValue);
    phptv_emit_status(204);
    exit;
}

function phptv_handle_logout(): void
{
    dispatch_method('POST');
    // No require_session: clearing an already-invalid cookie is
    // harmless and a stale cookie shouldn't soft-lock the user out
    // of logging out. (Open Question 2 in design.md.)
    clear_session_cookie();
    phptv_emit_status(204);
    exit;
}
