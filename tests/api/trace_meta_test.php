<?php

declare(strict_types=1);

/**
 * Tests for GET /api/traces/{key} (the metadata endpoint).
 *
 * Covers the §5.5 happy path, the 404 cases (no row, no file), the
 * field types, and the special "anomaly_count is sourced from
 * index.sqlite, not recomputed" decision (task 3.2).
 */

require_once __DIR__ . '/lib/assert.php';
require_once __DIR__ . '/lib/harness.php';
require_once __DIR__ . '/fixtures/make_config.php';
require_once __DIR__ . '/fixtures/make_index_sqlite.php';
require_once __DIR__ . '/fixtures/make_trace_sqlite.php';
require_once __DIR__ . '/../../api/internal/config.php';
require_once __DIR__ . '/../../api/internal/response.php';
require_once __DIR__ . '/../../api/internal/session.php';

$key   = str_repeat('1', 32);
$token = 'meta-tok-' . str_repeat('a', 32);
$salt  = 'meta-salt-' . str_repeat('b', 32);

$dataDir = sys_get_temp_dir() . '/phptv-meta-' . bin2hex(random_bytes(4));
mkdir($dataDir . '/traces', 0700, true);
$confPath = make_config([
    'auth'    => ['token' => $token, 'session_salt' => $salt],
    'storage' => ['data_dir' => $dataDir],
]);

// Index row mirrors what index.sqlite carries; anomaly_count is the
// only column the metadata endpoint reads from the index.
make_index_sqlite($dataDir . '/index.sqlite', [
    [
        'trace_key'              => $key,
        'trace_id'               => '00000000-0000-0000-0000-000000000000',
        'host'                   => 'dev-1',
        'pid'                    => 4321,
        'start_time_ns'          => 1748000000123456789,
        'sapi'                   => 'fpm-fcgi',
        'uri_or_script'          => '/srv/app/index.php',
        'state'                  => 'finalized',
        'dropped_records'        => 7,
        'anomaly_count'          => 3,
        'cpu_snapshot_available' => 1,
    ],
]);
// Per-trace file with matching trace_meta. The collector keeps the
// two in lockstep at idle-finalize; we mirror that here. The
// per-trace anomalies table has rows too, used to verify that the
// endpoint reads the count from the index (not recomputed).
make_trace_sqlite(
    $dataDir . '/traces/' . $key . '.sqlite',
    [
        'trace_key'              => $key,
        'trace_id'               => '00000000-0000-0000-0000-000000000000',
        'host'                   => 'dev-1',
        'pid'                    => 4321,
        'start_time_ns'          => 1748000000123456789,
        'sapi'                   => 'fpm-fcgi',
        'uri_or_script'          => '/srv/app/index.php',
        'state'                  => 'finalized',
        'dropped_records'        => 7,
        'cpu_snapshot_available' => 1,
    ],
    [1 => ['fqn' => 'main']],
    [
        ['parent_node_id' => 1, 'fn_id' => 1, 'depth' => 1,
         'total_wall_ns' => 1_000_000, 'call_count' => 1],
    ],
    [
        ['node_id' => 2, 'kind' => 'inverted_time', 'detail' => 't_in=5,t_out=3'],
        // Deliberately seed FIVE rows here so we can prove the
        // endpoint reports 3 (from the index), not 5 (from the
        // per-trace anomalies table).
        ['node_id' => 2, 'kind' => 'inverted_time', 'detail' => 't_in=6,t_out=4'],
        ['node_id' => 2, 'kind' => 'inverted_time', 'detail' => 't_in=7,t_out=5'],
        ['node_id' => 2, 'kind' => 'inverted_time', 'detail' => 't_in=8,t_out=6'],
        ['node_id' => 2, 'kind' => 'inverted_time', 'detail' => 't_in=9,t_out=7'],
    ]
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

// --- 3.3 happy path -----------------------------------------------

$resp = $req('/api/traces/' . $key);
assert_eq(200, $resp['status'], 'known trace → 200');
$body = json_decode($resp['body'], true);

assert_eq($key, $body['trace_key'], 'trace_key');
assert_eq('00000000-0000-0000-0000-000000000000', $body['trace_id'], 'trace_id');
assert_eq('dev-1', $body['host'], 'host');
assert_eq(4321, $body['pid'], 'pid');
assert_eq('fpm-fcgi', $body['sapi'], 'sapi');
assert_eq('/srv/app/index.php', $body['uri_or_script'], 'uri_or_script');
assert_eq('finalized', $body['state'], 'state');
assert_eq(7, $body['dropped_records'], 'dropped_records');

// 3.2 — anomaly_count comes from index.sqlite (3), not from the
//       per-trace table (which has 5 rows). The discrepancy in the
//       fixture is intentional.
assert_eq(3, $body['anomaly_count'], 'anomaly_count comes from index (3, not 5)');

assert_eq(true, $body['cpu_snapshot_available'], 'cpu_snapshot_available coerced to bool');
assert_eq(1, $body['root_node_id'], 'root_node_id constant = 1');

// RFC-3339 nanosecond timestamp.
assert_eq(
    '2025-05-23T11:33:20.123456789Z',
    $body['start_time'],
    'start_time RFC-3339 ns'
);
assert_true(is_int($body['pid']), 'pid is int');
assert_true(is_bool($body['cpu_snapshot_available']), 'cpu_snapshot_available is bool');
assert_true(is_int($body['root_node_id']), 'root_node_id is int');

// --- 3.4 unknown key -> 404 ---------------------------------------

$resp = $req('/api/traces/' . str_repeat('e', 32));
assert_eq(404, $resp['status'], 'unknown key → 404');
assert_eq(['error' => 'not_found'], json_decode($resp['body'], true), '404 body');

// --- 3.5 index row exists but per-trace file missing -> 404 -------

$ghostKey = str_repeat('c', 32);
$pdo = new PDO('sqlite:' . $dataDir . '/index.sqlite');
$pdo->exec("INSERT INTO traces (trace_key, trace_id, host, pid, start_time_ns,"
    . " sapi, uri_or_script, state, first_batch_at_ns, last_batch_at_ns,"
    . " batch_count, call_count, total_wall_ns, dropped_records, anomaly_count,"
    . " cpu_snapshot_available) VALUES ('"
    . $ghostKey . "', '00000000-0000-0000-0000-000000000000', 'h', 1, 1, 'cli',"
    . " '/x', 'finalized', 1, 1, 1, 1, 1, 0, 0, 1)");
$pdo = null;
// No <ghostKey>.sqlite file exists.
$resp = $req('/api/traces/' . $ghostKey);
assert_eq(404, $resp['status'], 'index row without file → 404 (D-7)');
assert_eq(['error' => 'not_found'], json_decode($resp['body'], true), '404 body when file missing');

// --- 3.6/3.7 covered in trace_routing_test (auth + method) --------

// Extra: when the per-trace file's trace_meta uses cpu_snapshot_available=0,
// the response's bool is false.
$key2 = str_repeat('2', 32);
make_trace_sqlite(
    $dataDir . '/traces/' . $key2 . '.sqlite',
    ['trace_key' => $key2, 'cpu_snapshot_available' => 0],
    [1 => ['fqn' => 'main']],
    [['parent_node_id' => 1, 'fn_id' => 1, 'depth' => 1]]
);
$pdo = new PDO('sqlite:' . $dataDir . '/index.sqlite');
$pdo->exec("INSERT INTO traces (trace_key, trace_id, host, pid, start_time_ns,"
    . " sapi, uri_or_script, state, first_batch_at_ns, last_batch_at_ns,"
    . " batch_count, call_count, total_wall_ns, dropped_records, anomaly_count,"
    . " cpu_snapshot_available) VALUES ('"
    . $key2 . "', '00000000-0000-0000-0000-000000000000', 'h', 1, 1, 'cli',"
    . " '/x', 'finalized', 1, 1, 1, 1, 1, 0, 0, 0)");
$pdo = null;
$resp = $req('/api/traces/' . $key2);
assert_eq(false, json_decode($resp['body'], true)['cpu_snapshot_available'],
    'cpu_snapshot_available=0 → false');

// Cleanup
foreach (glob($dataDir . '/index.sqlite*') ?: [] as $f) @unlink($f);
foreach (glob($dataDir . '/traces/*.sqlite*') ?: [] as $f) @unlink($f);
@rmdir($dataDir . '/traces');
@rmdir($dataDir);
@unlink($confPath);

report_done();
