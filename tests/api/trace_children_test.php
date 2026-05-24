<?php

declare(strict_types=1);

/**
 * Tests for GET /api/traces/{key}/tree/{node_id}/children.
 *
 *  - children_loaded is ALWAYS false (§5.7 contract: UI lazy-expands).
 *  - Leaf parent → empty array, status 200.
 *  - Non-existent node_id (well-shaped int) → empty array, NOT 404.
 *  - limit/offset paging with bounds.
 *  - Sort whitelist same as the tree endpoint.
 *  - SQL injection attempt → caught by the route regex (404).
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
$token = 'children-tok-' . str_repeat('a', 32);
$salt  = 'children-salt-' . str_repeat('b', 32);

$dataDir = sys_get_temp_dir() . '/phptv-children-' . bin2hex(random_bytes(4));
mkdir($dataDir . '/traces', 0700, true);
$confPath = make_config([
    'auth'    => ['token' => $token, 'session_salt' => $salt],
    'storage' => ['data_dir' => $dataDir],
]);
make_index_sqlite($dataDir . '/index.sqlite', [['trace_key' => $key]]);

// Build a node with 5 children + one leaf parent. Five-child fanout
// lets the limit/offset test return a slice with a known order.
//
//  1 <root>
//  ├── 2 a()   (5 direct children)
//  │    ├── 4 c1   total=500 count=1
//  │    ├── 5 c2   total=400 count=2
//  │    ├── 6 c3   total=300 count=3
//  │    ├── 7 c4   total=200 count=4
//  │    └── 8 c5   total=100 count=5
//  └── 3 leaf()  (no children)

$dict = [
    1  => ['fqn' => 'a'],
    2  => ['fqn' => 'leaf'],
    11 => ['fqn' => 'c1'],
    12 => ['fqn' => 'c2'],
    13 => ['fqn' => 'c3'],
    14 => ['fqn' => 'c4'],
    15 => ['fqn' => 'c5'],
];

$nodes = [
    ['node_id' => 2, 'parent_node_id' => 1, 'fn_id' => 1, 'depth' => 1,
     'total_wall_ns' => 5000, 'call_count' => 1],
    ['node_id' => 3, 'parent_node_id' => 1, 'fn_id' => 2, 'depth' => 1,
     'total_wall_ns' => 1000, 'call_count' => 1],
    ['node_id' => 4, 'parent_node_id' => 2, 'fn_id' => 11, 'depth' => 2,
     'total_wall_ns' => 500, 'call_count' => 1],
    ['node_id' => 5, 'parent_node_id' => 2, 'fn_id' => 12, 'depth' => 2,
     'total_wall_ns' => 400, 'call_count' => 2],
    ['node_id' => 6, 'parent_node_id' => 2, 'fn_id' => 13, 'depth' => 2,
     'total_wall_ns' => 300, 'call_count' => 3],
    ['node_id' => 7, 'parent_node_id' => 2, 'fn_id' => 14, 'depth' => 2,
     'total_wall_ns' => 200, 'call_count' => 4],
    ['node_id' => 8, 'parent_node_id' => 2, 'fn_id' => 15, 'depth' => 2,
     'total_wall_ns' => 100, 'call_count' => 5],
];

make_trace_sqlite(
    $dataDir . '/traces/' . $key . '.sqlite',
    ['trace_key' => $key], $dict, $nodes
);

$cookie = compute_session_value($token, $salt);
$endpoint = __DIR__ . '/../../api/trace.php';
$req = function (int $nodeId, array $query = []) use ($endpoint, $confPath, $cookie, $key): array {
    return simulate_request($endpoint, [
        'config_path' => $confPath,
        'method'      => 'GET',
        'path'        => '/api/traces/' . $key . '/tree/' . $nodeId . '/children',
        'query'       => $query,
        'cookies'     => ['phptv_session' => $cookie],
    ]);
};

// --- 5.3 parent with 5 children -----------------------------------

$resp = $req(2);
assert_eq(200, $resp['status'], 'parent with 5 → 200');
$body = json_decode($resp['body'], true);
assert_eq(5, count($body['nodes']), '5 children returned');

// All children carry children_loaded=false (§5.7 contract)
foreach ($body['nodes'] as $n) {
    assert_eq(false, $n['children_loaded'], 'children endpoint sets children_loaded=false');
    // Also: every child here is a leaf, so has_children=false
    assert_eq(false, $n['has_children'], 'no grandchildren in fixture');
}

// Default sort is total_wall_desc → 500, 400, 300, 200, 100
$totals = array_column($body['nodes'], 'total_wall_ns');
assert_eq([500, 400, 300, 200, 100], $totals, 'default sort = total_wall_desc');

// --- 5.4 leaf parent → empty -------------------------------------

$resp = $req(3);
assert_eq(200, $resp['status'], 'leaf parent → 200');
$body = json_decode($resp['body'], true);
assert_eq([], $body['nodes'], 'leaf parent yields empty children');

// --- 5.5 non-existent node_id (shaped like a positive int) → empty

$resp = $req(99999);
assert_eq(200, $resp['status'], 'non-existent node_id → 200 (D-8)');
assert_eq([], json_decode($resp['body'], true)['nodes'], 'empty array body');

// --- 5.6 node_id shape: 0, leading zero, non-digit, negative → 404
// (the route regex rejects these before any handler runs)

foreach (
    [
        '/api/traces/' . $key . '/tree/0/children'    => 'node_id=0',
        '/api/traces/' . $key . '/tree/01/children'   => 'leading-zero',
        '/api/traces/' . $key . '/tree/abc/children'  => 'non-digit',
        '/api/traces/' . $key . '/tree/-1/children'   => 'negative',
    ] as $path => $label
) {
    $r = simulate_request($endpoint, [
        'config_path' => $confPath,
        'method'      => 'GET',
        'path'        => $path,
        'cookies'     => ['phptv_session' => $cookie],
    ]);
    assert_eq(404, $r['status'], "shape-rejected: {$label}");
}

// --- 5.7 limit / offset / sort -----------------------------------

// limit=2 of 5 → first two (500, 400)
$resp = $req(2, ['limit' => '2']);
$body = json_decode($resp['body'], true);
assert_eq(2, count($body['nodes']), 'limit=2 returns 2');
assert_eq([500, 400], array_column($body['nodes'], 'total_wall_ns'), 'limit=2 keeps order');

// limit=2, offset=1 → second two (400, 300)
$resp = $req(2, ['limit' => '2', 'offset' => '1']);
$body = json_decode($resp['body'], true);
assert_eq([400, 300], array_column($body['nodes'], 'total_wall_ns'), 'limit=2 offset=1');

// limit=2, offset=3 → last two (200, 100)
$resp = $req(2, ['limit' => '2', 'offset' => '3']);
$body = json_decode($resp['body'], true);
assert_eq([200, 100], array_column($body['nodes'], 'total_wall_ns'), 'limit=2 offset=3');

// offset past the end → empty
$resp = $req(2, ['limit' => '10', 'offset' => '10']);
assert_eq([], json_decode($resp['body'], true)['nodes'], 'offset past end → empty');

// limit=1000 is allowed (the max)
$resp = $req(2, ['limit' => '1000']);
assert_eq(200, $resp['status'], 'limit=1000 → 200');

// limit=1001 rejected
$resp = $req(2, ['limit' => '1001']);
assert_eq(400, $resp['status'], 'limit=1001 → 400');

// Other sorts produce different orders.
$resp = $req(2, ['sort' => 'count_desc']);
$body = json_decode($resp['body'], true);
$counts = array_column($body['nodes'], 'count');
assert_eq([5, 4, 3, 2, 1], $counts, 'sort=count_desc');

$resp = $req(2, ['sort' => 'fqn_asc']);
$body = json_decode($resp['body'], true);
$fqns = array_column($body['nodes'], 'fqn');
assert_eq(['c1', 'c2', 'c3', 'c4', 'c5'], $fqns, 'sort=fqn_asc');

// --- 5.8 validation ----------------------------------------------

foreach (
    [
        ['limit' => '0'],
        ['limit' => '1001'],
        ['limit' => 'abc'],
        ['limit' => '-1'],
        ['offset' => '-1'],
        ['offset' => 'abc'],
        ['sort' => 'unknown'],
        ['sort' => 'total_wall_asc'],
    ] as $bad
) {
    $resp = $req(2, $bad);
    assert_eq(400, $resp['status'], 'bad param → 400: ' . http_build_query($bad));
}

// --- 5.9 404 paths -----------------------------------------------

$resp = simulate_request($endpoint, [
    'config_path' => $confPath,
    'method'      => 'GET',
    'path'        => '/api/traces/' . str_repeat('e', 32) . '/tree/1/children',
    'cookies'     => ['phptv_session' => $cookie],
]);
assert_eq(404, $resp['status'], 'missing trace → 404');

// Cleanup
foreach (glob($dataDir . '/index.sqlite*') ?: [] as $f) @unlink($f);
foreach (glob($dataDir . '/traces/*.sqlite*') ?: [] as $f) @unlink($f);
@rmdir($dataDir . '/traces');
@rmdir($dataDir);
@unlink($confPath);

report_done();
