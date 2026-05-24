<?php

declare(strict_types=1);

require_once __DIR__ . '/lib/assert.php';
require_once __DIR__ . '/lib/harness.php';
require_once __DIR__ . '/fixtures/make_config.php';
require_once __DIR__ . '/../../api/internal/config.php';
require_once __DIR__ . '/../../api/internal/response.php';
require_once __DIR__ . '/../../api/internal/session.php';

$token = 'good-token-' . str_repeat('a', 40);
$salt  = 'good-salt-' . str_repeat('b', 40);
$confPath = make_config([
    'auth' => ['token' => $token, 'session_salt' => $salt],
]);

$authEndpoint = __DIR__ . '/../../api/auth.php';

// 8.4 — correct token → 204 + Set-Cookie present, cookie value
//                       matches the recomputed HMAC
$resp = simulate_request($authEndpoint, [
    'config_path' => $confPath,
    'method' => 'POST',
    'path' => '/api/auth',
    'headers' => ['Content-Type' => 'application/json'],
    'body' => json_encode(['token' => $token]),
]);
assert_eq(204, $resp['status'], 'correct token → 204');
$setCookie = header_value($resp['headers'], 'Set-Cookie');
assert_true($setCookie !== null, 'Set-Cookie header present');
assert_contains($setCookie ?? '', 'phptv_session=', 'cookie name present');
assert_contains($setCookie ?? '', 'HttpOnly', 'HttpOnly attr present');
assert_contains($setCookie ?? '', 'SameSite=Lax', 'SameSite=Lax present');
assert_contains($setCookie ?? '', 'Path=/', 'Path=/ present');
$expected = compute_session_value($token, $salt);
assert_contains($setCookie ?? '', 'phptv_session=' . $expected, 'cookie value matches HMAC');

// 8.5 — wrong token → 401, no Set-Cookie
$resp = simulate_request($authEndpoint, [
    'config_path' => $confPath,
    'method' => 'POST',
    'path' => '/api/auth',
    'headers' => ['Content-Type' => 'application/json'],
    'body' => json_encode(['token' => 'bogus']),
]);
assert_eq(401, $resp['status'], 'wrong token → 401');
assert_eq(['error' => 'unauthorized'], json_decode($resp['body'], true), 'unauthorized body');
assert_eq(null, header_value($resp['headers'], 'Set-Cookie'), 'no Set-Cookie on bad token');
// And the token value is NOT echoed back in any header or body
assert_not_contains($resp['body'], $token, 'token NEVER in body');
foreach ($resp['headers'] as $h) {
    assert_not_contains($h, $token, 'token NEVER in any header line');
}

// 8.6 — missing body → 400
$resp = simulate_request($authEndpoint, [
    'config_path' => $confPath,
    'method' => 'POST',
    'path' => '/api/auth',
    'headers' => ['Content-Type' => 'application/json'],
    'body' => '',
]);
assert_eq(400, $resp['status'], 'empty body → 400');

// missing token field → 400
$resp = simulate_request($authEndpoint, [
    'config_path' => $confPath,
    'method' => 'POST',
    'path' => '/api/auth',
    'headers' => ['Content-Type' => 'application/json'],
    'body' => '{"not_token":"foo"}',
]);
assert_eq(400, $resp['status'], 'missing token field → 400');

// non-JSON body → 400
$resp = simulate_request($authEndpoint, [
    'config_path' => $confPath,
    'method' => 'POST',
    'path' => '/api/auth',
    'headers' => ['Content-Type' => 'application/json'],
    'body' => 'not json {',
]);
assert_eq(400, $resp['status'], 'malformed JSON → 400');

// 8.7 — wrong content-type → 415
$resp = simulate_request($authEndpoint, [
    'config_path' => $confPath,
    'method' => 'POST',
    'path' => '/api/auth',
    'headers' => ['Content-Type' => 'text/plain'],
    'body' => '{"token":"x"}',
]);
assert_eq(415, $resp['status'], 'wrong content-type → 415');

// 8.8 — GET → 405
$resp = simulate_request($authEndpoint, [
    'config_path' => $confPath,
    'method' => 'GET',
    'path' => '/api/auth',
]);
assert_eq(405, $resp['status'], 'GET /api/auth → 405');

// 8.9 — POST /api/auth/logout always 204, with and without cookie
$resp = simulate_request($authEndpoint, [
    'config_path' => $confPath,
    'method' => 'POST',
    'path' => '/api/auth/logout',
]);
assert_eq(204, $resp['status'], 'logout without cookie → 204');
$setCookie = header_value($resp['headers'], 'Set-Cookie');
assert_contains($setCookie ?? '', 'phptv_session=;', 'logout clears cookie');
assert_contains($setCookie ?? '', 'Max-Age=0', 'logout has Max-Age=0');

$resp = simulate_request($authEndpoint, [
    'config_path' => $confPath,
    'method' => 'POST',
    'path' => '/api/auth/logout',
    'cookies' => ['phptv_session' => $expected],
]);
assert_eq(204, $resp['status'], 'logout with valid cookie → 204');

// Unknown path → 404
$resp = simulate_request($authEndpoint, [
    'config_path' => $confPath,
    'method' => 'POST',
    'path' => '/api/auth/something_else',
]);
assert_eq(404, $resp['status'], 'unknown path → 404');

// Constant-time compare: source uses hash_equals, never == or ===
// over $token bytes. Static check the source.
$authSrc = file_get_contents(__DIR__ . '/../../api/auth.php');
assert_contains($authSrc ?: '', 'hash_equals', 'auth.php uses hash_equals');

@unlink($confPath);
report_done();
