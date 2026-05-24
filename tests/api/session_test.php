<?php

declare(strict_types=1);

require_once __DIR__ . '/lib/assert.php';
require_once __DIR__ . '/fixtures/make_config.php';
require_once __DIR__ . '/../../api/internal/config.php';
require_once __DIR__ . '/../../api/internal/response.php';
require_once __DIR__ . '/../../api/internal/session.php';

// --- 6.1 / 6.5 round-trip ----------------------------------------

$token = 'abcdef-token-' . str_repeat('a', 32);
$salt = 'pepper-' . str_repeat('z', 36);

$cookie = compute_session_value($token, $salt);
assert_true(strlen($cookie) > 30, 'cookie is non-trivial length');
assert_eq(0, preg_match('/[^A-Za-z0-9_-]/', $cookie), 'cookie is base64url (no /+= chars)');

// Same inputs → same output (deterministic)
assert_eq(
    $cookie,
    compute_session_value($token, $salt),
    'derivation is deterministic'
);

// --- 6.6 rotated salt invalidates ---------------------------------

$rotated = compute_session_value($token, 'pepper-' . str_repeat('Q', 36));
assert_true($cookie !== $rotated, 'different salt → different cookie');

// --- 6.7 tampered-by-one-byte -------------------------------------

$tampered = $cookie;
// Flip one base64url alphabet character (we can't just `++` since the
// byte must remain in the alphabet).
$tampered[0] = ($tampered[0] === 'A') ? 'B' : 'A';
assert_true(
    $cookie !== $tampered,
    'tampered cookie differs from the canonical one'
);

// require_session over the harness: build a config and exercise the
// helper via the simulate_request harness. require_session calls
// json_error which exit()s; we need a child process.

require_once __DIR__ . '/lib/harness.php';

$confPath = make_config([
    'auth' => ['token' => $token, 'session_salt' => $salt],
]);
$goodCookie = compute_session_value($token, $salt);

// 6.5 round-trip via the endpoint shim
$ok = simulate_request(
    __DIR__ . '/fixtures/session_endpoints/require_session_endpoint.php',
    [
        'config_path' => $confPath,
        'cookies' => [PHPTV_COOKIE_NAME => $goodCookie],
    ]
);
assert_eq(204, $ok['status'], 'valid session passes through');

// 6.8 absence -----------------------------------------------------

$absent = simulate_request(
    __DIR__ . '/fixtures/session_endpoints/require_session_endpoint.php',
    ['config_path' => $confPath]
);
assert_eq(401, $absent['status'], 'missing cookie -> 401');
assert_eq(
    ['error' => 'unauthorized'],
    json_decode($absent['body'], true),
    'unauthorized body shape'
);

// 6.7 tampered ----------------------------------------------------

$tamp = simulate_request(
    __DIR__ . '/fixtures/session_endpoints/require_session_endpoint.php',
    [
        'config_path' => $confPath,
        'cookies' => [PHPTV_COOKIE_NAME => $tampered],
    ]
);
assert_eq(401, $tamp['status'], 'tampered cookie -> 401');

// 6.6 rotated salt invalidates prior cookies via the endpoint ------

$rotatedConfPath = make_config([
    'auth' => ['token' => $token, 'session_salt' => 'newsalt-' . str_repeat('Q', 36)],
]);
$stale = simulate_request(
    __DIR__ . '/fixtures/session_endpoints/require_session_endpoint.php',
    [
        'config_path' => $rotatedConfPath,
        'cookies' => [PHPTV_COOKIE_NAME => $goodCookie],
    ]
);
assert_eq(401, $stale['status'], 'rotated salt invalidates prior cookie');
@unlink($rotatedConfPath);
@unlink($confPath);

// --- Issue / clear cookie headers --------------------------------

$capturedHeaders = [];
$GLOBALS['__phptv_test_emit_header'] = static function (string $line) use (&$capturedHeaders): void {
    $capturedHeaders[] = $line;
};

// Need a config loaded so issue_session_cookie can consult [server].tls
Config::forgetCache();
Config::load(make_config(['server' => ['tls' => true]]));
issue_session_cookie('VALUE-A');
assert_eq(1, count($capturedHeaders), 'issue emits one header');
$cookieLine = $capturedHeaders[0];
assert_contains($cookieLine, 'phptv_session=VALUE-A', 'cookie name + value');
assert_contains($cookieLine, 'HttpOnly', 'HttpOnly attr');
assert_contains($cookieLine, 'SameSite=Lax', 'SameSite=Lax');
assert_contains($cookieLine, 'Path=/', 'Path=/');
assert_contains($cookieLine, 'Secure', 'Secure when TLS=true');

$capturedHeaders = [];
Config::forgetCache();
Config::load(make_config(['server' => ['tls' => false]]));
issue_session_cookie('VALUE-B');
assert_not_contains($capturedHeaders[0], 'Secure', 'Secure absent when TLS=false');

$capturedHeaders = [];
clear_session_cookie();
$clearLine = $capturedHeaders[0];
assert_contains($clearLine, 'phptv_session=;', 'clear sets empty value');
assert_contains($clearLine, 'Max-Age=0', 'clear has Max-Age=0');

report_done();
