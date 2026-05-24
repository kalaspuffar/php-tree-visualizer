<?php

declare(strict_types=1);

/**
 * Routing tests for api/trace.php.
 *
 * Drives the dispatcher with a wide variety of REQUEST_URI shapes and
 * asserts each one lands in the correct handler — or returns 404
 * cleanly. Builds a minimal fixture (one per-trace DB + one index
 * row) so legal routes actually produce 200, not crash on missing
 * data.
 */

require_once __DIR__ . '/lib/assert.php';
require_once __DIR__ . '/lib/harness.php';
require_once __DIR__ . '/fixtures/make_config.php';
require_once __DIR__ . '/fixtures/make_index_sqlite.php';
require_once __DIR__ . '/fixtures/make_trace_sqlite.php';
require_once __DIR__ . '/../../api/internal/config.php';
require_once __DIR__ . '/../../api/internal/response.php';
require_once __DIR__ . '/../../api/internal/session.php';

$key   = str_repeat('a', 32);
$token = 'route-tok-' . str_repeat('a', 32);
$salt  = 'route-salt-' . str_repeat('b', 32);

$dataDir = sys_get_temp_dir() . '/phptv-route-' . bin2hex(random_bytes(4));
mkdir($dataDir . '/traces', 0700, true);
$confPath = make_config([
    'auth'    => ['token' => $token, 'session_salt' => $salt],
    'storage' => ['data_dir' => $dataDir],
]);

make_index_sqlite($dataDir . '/index.sqlite', [
    ['trace_key' => $key],
]);
make_trace_sqlite($dataDir . '/traces/' . $key . '.sqlite', [
    'trace_key' => $key,
], [
    1 => ['fqn' => 'main'],
], [
    ['parent_node_id' => 1, 'fn_id' => 1, 'depth' => 1, 'total_wall_ns' => 100],
]);

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

// --- Legal shapes route to a handler (status != 404) -----------------

$ok = $req('/api/traces/' . $key);
assert_eq(200, $ok['status'], 'metadata path: GET /api/traces/<key> → 200');

$ok = $req('/api/traces/' . $key . '/tree');
assert_eq(200, $ok['status'], 'tree path: GET /api/traces/<key>/tree → 200');

$ok = $req('/api/traces/' . $key . '/tree/1/children');
assert_eq(200, $ok['status'], 'children path: GET .../tree/1/children → 200');

$ok = $req('/api/traces/' . $key . '/tree/2/children');
assert_eq(200, $ok['status'], 'children path with leaf node → 200');

// --- 404 shapes ------------------------------------------------------

foreach (
    [
        '/api/traces/' . strtoupper($key)     => 'uppercase key',
        '/api/traces/' . substr($key, 0, 31)  => 'short key',
        '/api/traces/' . $key . 'a'           => 'long key',
        '/api/traces/g' . substr($key, 1)     => 'non-hex char',
        '/api/traces/' . $key . '/'           => 'trailing slash on key',
        '/api/traces/' . $key . '/nope'       => 'unknown sub-resource',
        '/api/traces/' . $key . '/tree/'      => 'trailing slash on tree',
        '/api/traces/' . $key . '/tree/0/children'    => 'node_id zero',
        '/api/traces/' . $key . '/tree/01/children'   => 'leading-zero node_id',
        '/api/traces/' . $key . '/tree/-1/children'   => 'negative node_id',
        '/api/traces/' . $key . '/tree/abc/children'  => 'non-digit node_id',
        '/api/traces/' . $key . '/tree/1/grandchildren' => 'wrong leaf word',
        '/api/traces/../../etc/passwd'        => 'traversal in key',
        '/api/traces/' . $key . '/tree/1/children/extra' => 'trailing extra segment',
    ] as $path => $label
) {
    $r = $req($path);
    assert_eq(404, $r['status'], "404: {$label} ({$path})");
    assert_eq(['error' => 'not_found'], json_decode($r['body'], true), "404 body: {$label}");
}

// --- Wrong method ----------------------------------------------------

foreach (['POST', 'PUT', 'DELETE', 'PATCH'] as $method) {
    $r = simulate_request($endpoint, [
        'config_path' => $confPath,
        'method'      => $method,
        'path'        => '/api/traces/' . $key,
        'cookies'     => ['phptv_session' => $cookie],
    ]);
    assert_eq(405, $r['status'], "{$method} /api/traces/<key> → 405");
}

// --- Auth required on every shape ------------------------------------

foreach (
    [
        '/api/traces/' . $key,
        '/api/traces/' . $key . '/tree',
        '/api/traces/' . $key . '/tree/1/children',
    ] as $path
) {
    $noCookie = simulate_request($endpoint, [
        'config_path' => $confPath,
        'method'      => 'GET',
        'path'        => $path,
    ]);
    assert_eq(401, $noCookie['status'], "no cookie on {$path} → 401");

    $bad = simulate_request($endpoint, [
        'config_path' => $confPath,
        'method'      => 'GET',
        'path'        => $path,
        'cookies'     => ['phptv_session' => 'tampered_value'],
    ]);
    assert_eq(401, $bad['status'], "bad cookie on {$path} → 401");
}

// Cleanup
foreach (glob($dataDir . '/index.sqlite*') ?: [] as $f) @unlink($f);
foreach (glob($dataDir . '/traces/*.sqlite*') ?: [] as $f) @unlink($f);
@rmdir($dataDir . '/traces');
@rmdir($dataDir);
@unlink($confPath);

report_done();
