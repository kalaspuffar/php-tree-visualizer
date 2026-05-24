<?php

declare(strict_types=1);

/**
 * Per-trace schema-version gate end-to-end test.
 *
 * Builds a per-trace `<key>.sqlite` with `PRAGMA user_version = 2`
 * and hits each of the three trace-detail endpoints. Every one MUST
 * return 500 with body `{"error":"schema_version_mismatch"}`, no
 * path or version in the response body.
 *
 * Phase 3 already covered the index.sqlite gate (see
 * tests/api/schema_version_test.php). This test is the per-trace
 * peer.
 */

require_once __DIR__ . '/lib/assert.php';
require_once __DIR__ . '/lib/harness.php';
require_once __DIR__ . '/fixtures/make_config.php';
require_once __DIR__ . '/fixtures/make_index_sqlite.php';
require_once __DIR__ . '/fixtures/make_trace_sqlite.php';
require_once __DIR__ . '/../../api/internal/config.php';
require_once __DIR__ . '/../../api/internal/session.php';

$key   = str_repeat('a', 32);
$token = 'sv2-tok-' . str_repeat('a', 32);
$salt  = 'sv2-salt-' . str_repeat('b', 32);

$dataDir = sys_get_temp_dir() . '/phptv-sv2-' . bin2hex(random_bytes(4));
mkdir($dataDir . '/traces', 0700, true);
$confPath = make_config([
    'auth'    => ['token' => $token, 'session_salt' => $salt],
    'storage' => ['data_dir' => $dataDir],
]);

// index.sqlite is v1 (so we don't trip THAT gate); per-trace is v2.
make_index_sqlite($dataDir . '/index.sqlite', [['trace_key' => $key]]);
make_trace_sqlite(
    $dataDir . '/traces/' . $key . '.sqlite',
    ['trace_key' => $key],
    [1 => ['fqn' => 'main']],
    [['parent_node_id' => 1, 'fn_id' => 1, 'depth' => 1]],
    [],
    2     // user_version = 2 → trips DR-5
);

$cookie = compute_session_value($token, $salt);
$endpoint = __DIR__ . '/../../api/trace.php';
$req = function (string $path) use ($endpoint, $confPath, $cookie): array {
    return simulate_request($endpoint, [
        'config_path' => $confPath,
        'method'      => 'GET',
        'path'        => $path,
        'cookies'     => ['phptv_session' => $cookie],
    ]);
};

foreach (
    [
        '/api/traces/' . $key,
        '/api/traces/' . $key . '/tree',
        '/api/traces/' . $key . '/tree/1/children',
    ] as $path
) {
    $resp = $req($path);
    assert_eq(500, $resp['status'], "v2 DB → 500 on {$path}");
    assert_eq(
        ['error' => 'schema_version_mismatch'],
        json_decode($resp['body'], true),
        "documented error body on {$path}"
    );
    // Response body MUST NOT leak the path or the observed version.
    assert_not_contains($resp['body'], $dataDir, "path not in body on {$path}");
    assert_not_contains($resp['body'], '/index.sqlite', "no .sqlite in body on {$path}");
    assert_not_contains($resp['body'], '"2"', "no version in body on {$path}");
    assert_not_contains($resp['body'], 'user_version', "no pragma name in body on {$path}");
}

// Cleanup
foreach (glob($dataDir . '/index.sqlite*') ?: [] as $f) @unlink($f);
foreach (glob($dataDir . '/traces/*.sqlite*') ?: [] as $f) @unlink($f);
@rmdir($dataDir . '/traces');
@rmdir($dataDir);
@unlink($confPath);

report_done();
