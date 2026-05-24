<?php

declare(strict_types=1);

require_once __DIR__ . '/lib/assert.php';
require_once __DIR__ . '/lib/harness.php';
require_once __DIR__ . '/fixtures/make_config.php';
require_once __DIR__ . '/fixtures/make_index_sqlite.php';
require_once __DIR__ . '/../../api/internal/config.php';
require_once __DIR__ . '/../../api/internal/session.php';

$token = 'sv-tok-' . str_repeat('a', 32);
$salt  = 'sv-salt-' . str_repeat('b', 32);

$dataDir = sys_get_temp_dir() . '/phptv-sv-test-' . bin2hex(random_bytes(4));
mkdir($dataDir . '/traces', 0700, true);
$confPath = make_config([
    'auth' => ['token' => $token, 'session_salt' => $salt],
    'storage' => ['data_dir' => $dataDir],
]);

// 11.1 — v2 DB → 500 schema_version_mismatch with body redacted.
make_index_sqlite($dataDir . '/index.sqlite', [], 2);

$cookie = compute_session_value($token, $salt);
$resp = simulate_request(__DIR__ . '/../../api/traces.php', [
    'config_path' => $confPath,
    'method' => 'GET',
    'path' => '/api/traces',
    'cookies' => ['phptv_session' => $cookie],
]);

assert_eq(500, $resp['status'], 'v2 DB → 500');
assert_eq(
    ['error' => 'schema_version_mismatch'],
    json_decode($resp['body'], true),
    'documented error code in body'
);

// 11.3 — response body contains neither the path nor the observed version
assert_not_contains($resp['body'], $dataDir, 'path not in response body');
assert_not_contains($resp['body'], '/index.sqlite', 'filename not in response body');
assert_not_contains($resp['body'], 'user_version', 'pragma name not in response body');
assert_not_contains($resp['body'], '"2"', 'observed version (string) not in body');

// 11.2 — assert the operator-side message (via the sub-process's stderr
// from the syslog path) names the path and the observed version. The
// test harness's child registers __phptv_test_syslog_sink only when the
// parent passes a sink; we instead exercise it via a small fixture
// endpoint that mounts a custom sink and writes the captured line to
// stdout for inspection.
$resp = simulate_request(
    __DIR__ . '/fixtures/schema_version_endpoints/capture_sink.php',
    [
        'config_path' => $confPath,
        'method' => 'GET',
        'path' => '/api/traces',
        'cookies' => ['phptv_session' => $cookie],
    ]
);
assert_eq(500, $resp['status'], 'capture endpoint returns 500');
// The captured sink line is in the body in JSON form
$captured = json_decode($resp['body'], true);
assert_true(is_array($captured) && isset($captured['captured_line']), 'captured line present');
$line = (string) ($captured['captured_line'] ?? '');
assert_contains($line, '/index.sqlite', 'syslog mentions the path');
assert_contains($line, 'observed=2', 'syslog mentions observed version 2');
assert_contains($line, 'schema_version_mismatch', 'syslog tag identifies the failure');

// Cleanup
foreach (glob($dataDir . '/index.sqlite*') ?: [] as $f) {
    @unlink($f);
}
@rmdir($dataDir . '/traces');
@rmdir($dataDir);
@unlink($confPath);

report_done();
