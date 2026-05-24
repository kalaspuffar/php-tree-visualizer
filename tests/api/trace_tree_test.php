<?php

declare(strict_types=1);

/**
 * Tests for GET /api/traces/{key}/tree (recursive tree fetch).
 *
 *  - Depth defaults to 2 and is clamped to 1..4.
 *  - Sort whitelist is exactly the five §5.6 values.
 *  - Parents appear before children in the response array.
 *  - self_wall_ns = max(0, total - children_total).
 *  - has_children / children_loaded match D-5.
 *  - anomaly_count per node comes from the per-trace `anomalies`
 *    table joined in.
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
$token = 'tree-tok-' . str_repeat('a', 32);
$salt  = 'tree-salt-' . str_repeat('b', 32);

$dataDir = sys_get_temp_dir() . '/phptv-tree-' . bin2hex(random_bytes(4));
mkdir($dataDir . '/traces', 0700, true);
$confPath = make_config([
    'auth'    => ['token' => $token, 'session_salt' => $salt],
    'storage' => ['data_dir' => $dataDir],
]);
make_index_sqlite($dataDir . '/index.sqlite', [['trace_key' => $key]]);

// Build a 4-deep tree (root + 4 levels) with multiple branches per
// level so depth clamping + sort + paging are all distinguishable.
//
// Tree shape (node_id : fqn @ depth_in_tree → total_wall_ns):
//
//   1 <root>                  0
//   ├── 2 a()                 1  total=1000, children=600   self=400
//   │    ├── 4 a.x()          2  total=400  children=0      self=400
//   │    └── 5 a.y()          2  total=200  children=100    self=100
//   │         └── 6 a.y.q()   3  total=100  children=80     self=20
//   │              └── 7 a.y.q.r() 4 total=80 children=0    self=80
//   └── 3 b()                 1  total=500  children=300    self=200
//        └── 8 b.x()          2  total=300  children=0      self=300

$dict = [
    1 => ['fqn' => 'a'],
    2 => ['fqn' => 'b'],
    3 => ['fqn' => 'a.x'],
    4 => ['fqn' => 'a.y'],
    5 => ['fqn' => 'a.y.q'],
    6 => ['fqn' => 'a.y.q.r'],
    7 => ['fqn' => 'b.x'],
];

$nodes = [
    ['node_id' => 2, 'parent_node_id' => 1, 'fn_id' => 1, 'depth' => 1,
     'total_wall_ns' => 1000, 'call_count' => 1,
     'total_cpu_u_ns' => 500, 'total_cpu_s_ns' => 100,
     'total_mem_delta_bytes' => 1024],
    ['node_id' => 3, 'parent_node_id' => 1, 'fn_id' => 2, 'depth' => 1,
     'total_wall_ns' => 500, 'call_count' => 2,
     'total_cpu_u_ns' => 200, 'total_cpu_s_ns' => 50,
     'total_mem_delta_bytes' => 512],
    ['node_id' => 4, 'parent_node_id' => 2, 'fn_id' => 3, 'depth' => 2,
     'total_wall_ns' => 400, 'call_count' => 3],
    ['node_id' => 5, 'parent_node_id' => 2, 'fn_id' => 4, 'depth' => 2,
     'total_wall_ns' => 200, 'call_count' => 1],
    ['node_id' => 6, 'parent_node_id' => 5, 'fn_id' => 5, 'depth' => 3,
     'total_wall_ns' => 100, 'call_count' => 1],
    ['node_id' => 7, 'parent_node_id' => 6, 'fn_id' => 6, 'depth' => 4,
     'total_wall_ns' => 80, 'call_count' => 1],
    ['node_id' => 8, 'parent_node_id' => 3, 'fn_id' => 7, 'depth' => 2,
     'total_wall_ns' => 300, 'call_count' => 1],
];

$anomalies = [
    ['node_id' => 5, 'kind' => 'inverted_time'],
    ['node_id' => 5, 'kind' => 'inverted_time'],
    ['node_id' => 7, 'kind' => 'unresolved_fn'],
];

make_trace_sqlite(
    $dataDir . '/traces/' . $key . '.sqlite',
    ['trace_key' => $key],
    $dict,
    $nodes,
    $anomalies
);

$cookie = compute_session_value($token, $salt);
$endpoint = __DIR__ . '/../../api/trace.php';
$req = function (array $query = []) use ($endpoint, $confPath, $cookie, $key): array {
    return simulate_request($endpoint, [
        'config_path' => $confPath,
        'method'      => 'GET',
        'path'        => '/api/traces/' . $key . '/tree',
        'query'       => $query,
        'cookies'     => ['phptv_session' => $cookie],
    ]);
};

// --- 4.5 / 4.10 default depth=2 returns root + 2 levels --------------

$resp = $req();
assert_eq(200, $resp['status'], 'default depth → 200');
$body = json_decode($resp['body'], true);
assert_eq(1, $body['root_node_id'], 'root_node_id = 1');

$ids = array_column($body['nodes'], 'node_id');
// depth=2 means root (depth 0) + 2 levels = node_ids 1,2,3 (level 1) and 4,5,8 (level 2).
// Nodes 6 and 7 (deeper) excluded.
sort($ids);
assert_eq([1, 2, 3, 4, 5, 8], $ids, 'depth=2 yields root + 2 levels');

$byId = [];
foreach ($body['nodes'] as $n) {
    $byId[$n['node_id']] = $n;
}

// 4.10 has_children + children_loaded matrix:
//  - leaf (4): has_children false, children_loaded true
//  - interior above boundary (2): has_children true, children_loaded true
//  - interior at boundary (5): has_children true, children_loaded false
assert_eq(false, $byId[4]['has_children'], 'leaf has_children false');
assert_eq(true,  $byId[4]['children_loaded'], 'leaf children_loaded true');
assert_eq(true,  $byId[2]['has_children'], 'interior above boundary has_children true');
assert_eq(true,  $byId[2]['children_loaded'], 'interior above boundary children_loaded true');
assert_eq(true,  $byId[5]['has_children'], 'boundary node has_children true');
assert_eq(false, $byId[5]['children_loaded'], 'boundary node children_loaded false');

// 4.9 self_wall_ns = total - children_total
assert_eq(400, $byId[2]['self_wall_ns'], 'node 2 self_wall = 400');
assert_eq(200, $byId[3]['self_wall_ns'], 'node 3 self_wall = 200');
assert_eq(400, $byId[4]['self_wall_ns'], 'leaf self_wall = total');
assert_eq(100, $byId[5]['self_wall_ns'], 'node 5 self_wall = 100');
assert_eq(300, $byId[8]['self_wall_ns'], 'node 8 self_wall = 300');
// Root has 1500 = 1000 + 500
assert_eq(0,   $byId[1]['self_wall_ns'], 'root self_wall = 0 (no fixture override)');
assert_eq(1500,$byId[1]['total_wall_ns'], 'root total = sum of children');

// 4.6 parents-before-children invariant
$seen = [];
foreach ($body['nodes'] as $n) {
    if ($n['parent_node_id'] !== null) {
        assert_true(
            isset($seen[$n['parent_node_id']]),
            'parent_node_id ' . $n['parent_node_id'] . ' must appear before node ' . $n['node_id']
        );
    }
    $seen[$n['node_id']] = true;
}

// 4.11 anomaly_count per node — fixture has 2 for node 5, 0 elsewhere
assert_eq(2, $byId[5]['anomaly_count'], 'node 5 anomaly_count = 2');
assert_eq(0, $byId[4]['anomaly_count'], 'node 4 anomaly_count = 0');

// Field types
foreach ($byId as $n) {
    foreach (['node_id', 'depth', 'count', 'total_wall_ns', 'self_wall_ns',
              'total_cpu_u_ns', 'total_cpu_s_ns', 'total_mem_delta_bytes',
              'abnormal_exit_count', 'anomaly_count', 'kind', 'line'] as $f) {
        assert_true(is_int($n[$f]), "{$f} is int on node {$n['node_id']}");
    }
    foreach (['fqn', 'file'] as $f) {
        assert_true(is_string($n[$f]), "{$f} is string");
    }
    foreach (['has_children', 'children_loaded'] as $f) {
        assert_true(is_bool($n[$f]), "{$f} is bool");
    }
}
assert_true($byId[1]['parent_node_id'] === null, 'root parent_node_id is null');
assert_true(is_int($byId[2]['parent_node_id']), 'non-root parent_node_id is int');

// --- 4.5 (continued) depth=3 includes node 6; depth=4 includes node 7

$resp = $req(['depth' => '3']);
$body = json_decode($resp['body'], true);
$ids = array_column($body['nodes'], 'node_id'); sort($ids);
assert_eq([1, 2, 3, 4, 5, 6, 8], $ids, 'depth=3 includes node 6');

$resp = $req(['depth' => '4']);
$body = json_decode($resp['body'], true);
$ids = array_column($body['nodes'], 'node_id'); sort($ids);
assert_eq([1, 2, 3, 4, 5, 6, 7, 8], $ids, 'depth=4 includes node 7');

$resp = $req(['depth' => '1']);
$body = json_decode($resp['body'], true);
$ids = array_column($body['nodes'], 'node_id'); sort($ids);
assert_eq([1, 2, 3], $ids, 'depth=1 yields root + 1 level');

// --- 4.7 sort whitelist ----------------------------------------------

// Set up: at level 1 (nodes 2 and 3 under root) the sorts produce:
//   total_wall_desc:  2 (1000), 3 (500)
//   self_wall_desc:   2 (400),  3 (200)
//   count_desc:       3 (2),    2 (1)
//   mem_delta_desc:   2 (1024), 3 (512)
//   fqn_asc:          2 ('a'),  3 ('b')

foreach (
    [
        'total_wall_desc' => [2, 3],
        'self_wall_desc'  => [2, 3],
        'count_desc'      => [3, 2],
        'mem_delta_desc'  => [2, 3],
        'fqn_asc'         => [2, 3],
    ] as $sort => $expectedLevel1Order
) {
    $resp = $req(['depth' => '1', 'sort' => $sort]);
    $body = json_decode($resp['body'], true);
    // Strip root, keep level-1 nodes in response order.
    $level1 = [];
    foreach ($body['nodes'] as $n) {
        if ($n['node_id'] !== 1) {
            $level1[] = $n['node_id'];
        }
    }
    assert_eq(
        $expectedLevel1Order,
        $level1,
        'sort=' . $sort . ' produces expected ordering of level-1 nodes'
    );
}

// --- 4.8 validation -----------------------------------------------

foreach (
    [
        ['depth' => '0'],
        ['depth' => '5'],
        ['depth' => 'abc'],
        ['depth' => '-1'],
        ['sort' => 'unknown'],
        ['sort' => 'total_wall_asc'],
    ] as $bad
) {
    $resp = $req($bad);
    assert_eq(400, $resp['status'], 'bad param → 400: ' . http_build_query($bad));
    assert_eq(['error' => 'bad_request'], json_decode($resp['body'], true), 'body shape');
}

// --- 4.12 404 paths -----------------------------------------------

$resp = simulate_request($endpoint, [
    'config_path' => $confPath,
    'method'      => 'GET',
    'path'        => '/api/traces/' . str_repeat('e', 32) . '/tree',
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
