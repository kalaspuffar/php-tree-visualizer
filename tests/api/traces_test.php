<?php

declare(strict_types=1);

require_once __DIR__ . '/lib/assert.php';
require_once __DIR__ . '/lib/harness.php';
require_once __DIR__ . '/fixtures/make_config.php';
require_once __DIR__ . '/fixtures/make_index_sqlite.php';
require_once __DIR__ . '/../../api/internal/config.php';
require_once __DIR__ . '/../../api/internal/response.php';
require_once __DIR__ . '/../../api/internal/session.php';

$token = 'list-test-tok-' . str_repeat('a', 32);
$salt  = 'list-test-salt-' . str_repeat('b', 32);

$dataDir = sys_get_temp_dir() . '/phptv-traces-test-' . bin2hex(random_bytes(4));
mkdir($dataDir . '/traces', 0700, true);
$confPath = make_config([
    'auth' => ['token' => $token, 'session_salt' => $salt],
    'storage' => ['data_dir' => $dataDir],
]);

// Three rows with predictable start_time_ns so DESC order is
// row-2 newest, row-1 middle, row-0 oldest.
$rows = [
    [
        'trace_key' => str_repeat('1', 32),
        'uri_or_script' => '/srv/app/index.php',
        'start_time_ns' => 1_700_000_000_000_000_000,
        'call_count' => 100,
        'cpu_snapshot_available' => 1,
    ],
    [
        'trace_key' => str_repeat('2', 32),
        'uri_or_script' => '/srv/app/bin/run-tests.php',
        'start_time_ns' => 1_700_000_001_000_000_000,
        'call_count' => 200,
        'dropped_records' => 42,
        'cpu_snapshot_available' => 0,
    ],
    [
        'trace_key' => str_repeat('3', 32),
        'uri_or_script' => '/srv/app/cron/run-nightly.php',
        'start_time_ns' => 1_700_000_002_000_000_000,
        'call_count' => 300,
        'state' => 'active',
        'cpu_snapshot_available' => 1,
    ],
];
make_index_sqlite($dataDir . '/index.sqlite', $rows);

$tracesEndpoint = __DIR__ . '/../../api/traces.php';
$validCookie = compute_session_value($token, $salt);
$reqOpts = function (array $overrides = []) use ($confPath, $validCookie): array {
    return array_replace_recursive([
        'config_path' => $confPath,
        'method' => 'GET',
        'path' => '/api/traces',
        'cookies' => ['phptv_session' => $validCookie],
    ], $overrides);
};

// 9.14 — no cookie → 401
$resp = simulate_request($tracesEndpoint, [
    'config_path' => $confPath,
    'method' => 'GET',
    'path' => '/api/traces',
]);
assert_eq(401, $resp['status'], 'no cookie → 401');

// 9.15 — tampered cookie → 401
$resp = simulate_request($tracesEndpoint, $reqOpts([
    'cookies' => ['phptv_session' => 'tampered_value'],
]));
assert_eq(401, $resp['status'], 'tampered cookie → 401');

// 9.8 — three rows, no q → all three in DESC order
$resp = simulate_request($tracesEndpoint, $reqOpts());
assert_eq(200, $resp['status'], 'happy path → 200');
$body = json_decode($resp['body'], true);
assert_eq(3, $body['total'], 'total = 3');
assert_eq(false, $body['has_more'], 'has_more = false');
assert_eq(3, count($body['items']), 'three items returned');
assert_eq(str_repeat('3', 32), $body['items'][0]['trace_key'], 'newest first');
assert_eq(str_repeat('2', 32), $body['items'][1]['trace_key'], 'middle second');
assert_eq(str_repeat('1', 32), $body['items'][2]['trace_key'], 'oldest last');

// Field type assertions
$item = $body['items'][0];
foreach (
    [
        'trace_key', 'trace_id', 'host', 'start_time', 'sapi',
        'uri_or_script', 'state',
    ] as $stringField
) {
    assert_true(is_string($item[$stringField]), "field {$stringField} is string");
}
foreach (
    ['pid', 'call_count', 'total_wall_ns', 'dropped_records', 'anomaly_count']
    as $intField
) {
    assert_true(is_int($item[$intField]), "field {$intField} is int");
}
assert_true(is_bool($item['cpu_snapshot_available']), 'cpu_snapshot_available is bool');
assert_true(str_ends_with($item['start_time'], 'Z'), 'start_time is RFC-3339 UTC');
assert_true(
    (bool) preg_match('/\.\d{9}Z$/', $item['start_time']),
    'start_time has nine fractional digits'
);

// 9.9 — q=run- filters
$resp = simulate_request($tracesEndpoint, $reqOpts([
    'query' => ['q' => 'run-'],
]));
$body = json_decode($resp['body'], true);
assert_eq(2, $body['total'], 'q=run- → 2 matches');
$keys = array_column($body['items'], 'trace_key');
assert_true(in_array(str_repeat('2', 32), $keys, true), 'run-tests row included');
assert_true(in_array(str_repeat('3', 32), $keys, true), 'run-nightly row included');
assert_true(!in_array(str_repeat('1', 32), $keys, true), 'index.php row excluded');

// 9.9 (continued) — case-insensitive
$resp = simulate_request($tracesEndpoint, $reqOpts([
    'query' => ['q' => 'INDEX'],
]));
$body = json_decode($resp['body'], true);
assert_eq(1, $body['total'], 'case-insensitive q=INDEX matches /index.php');
assert_eq(str_repeat('1', 32), $body['items'][0]['trace_key'], 'matched row is /index.php');

// 9.10 — wildcard chars are literal
make_index_sqlite($dataDir . '/index.sqlite', array_merge($rows, [
    [
        'trace_key' => str_repeat('4', 32),
        'uri_or_script' => '/srv/app/coupons/10%_off.php',
        'start_time_ns' => 1_700_000_003_000_000_000,
    ],
]));
$resp = simulate_request($tracesEndpoint, $reqOpts([
    'query' => ['q' => '10%_off'],
]));
$body = json_decode($resp['body'], true);
assert_eq(1, $body['total'], 'q=10%_off matches only the literal substring');
assert_eq(str_repeat('4', 32), $body['items'][0]['trace_key'], 'matched the coupons row');

// Negative case: a row whose URI has "10" or "off" but not the
// literal "10%_off" must NOT match.
$resp = simulate_request($tracesEndpoint, $reqOpts([
    'query' => ['q' => 'tests.10%_off'],
]));
$body = json_decode($resp['body'], true);
assert_eq(0, $body['total'], 'wildcard-shaped q does not over-match');

// Restore the 3-row fixture for subsequent tests
make_index_sqlite($dataDir . '/index.sqlite', $rows);

// 9.11 — limit=2 returns 2 items, has_more=true, total=3
$resp = simulate_request($tracesEndpoint, $reqOpts([
    'query' => ['limit' => '2'],
]));
$body = json_decode($resp['body'], true);
assert_eq(3, $body['total'], 'total still 3');
assert_eq(true, $body['has_more'], 'has_more true when paginated');
assert_eq(2, count($body['items']), 'two items returned');

// 9.12 — offset=2 skips two
$resp = simulate_request($tracesEndpoint, $reqOpts([
    'query' => ['offset' => '2'],
]));
$body = json_decode($resp['body'], true);
assert_eq(1, count($body['items']), 'one item after offset=2');
assert_eq(str_repeat('1', 32), $body['items'][0]['trace_key'], 'last row by DESC order');
assert_eq(false, $body['has_more'], 'has_more false at the end');

// 9.13 — non-conforming params each → 400
foreach (
    [
        ['limit' => '501'],
        ['limit' => 'abc'],
        ['limit' => '0'],
        ['offset' => '-1'],
        ['offset' => 'abc'],
        ['sort' => 'unknown'],
    ] as $badQuery
) {
    $resp = simulate_request($tracesEndpoint, $reqOpts(['query' => $badQuery]));
    assert_eq(400, $resp['status'], 'bad params -> 400 for: ' . http_build_query($badQuery));
    assert_eq(
        ['error' => 'bad_request'],
        json_decode($resp['body'], true),
        'bad_request body for: ' . http_build_query($badQuery)
    );
}

// 9.13 — sort=start_time_desc explicitly accepted
$resp = simulate_request($tracesEndpoint, $reqOpts([
    'query' => ['sort' => 'start_time_desc'],
]));
assert_eq(200, $resp['status'], 'sort=start_time_desc accepted explicitly');

// 9.16 — SQL injection attempt → harmless
$resp = simulate_request($tracesEndpoint, $reqOpts([
    'query' => ['q' => "' OR 1=1 --"],
]));
$body = json_decode($resp['body'], true);
assert_eq(0, $body['total'], 'injection attempt matches zero rows');
// DB unchanged: re-query without filter still returns the same three rows
$resp = simulate_request($tracesEndpoint, $reqOpts());
$body = json_decode($resp['body'], true);
assert_eq(3, $body['total'], 'database row count unchanged after injection attempt');

// SQL builder uses bound params. We assert this by tokenizing
// traces.php and checking that every string passed to
// PDO::prepare()'s SQL argument is constant (no $variable
// interpolation). bindValue calls are not flagged — variables in
// parameter values are the whole point of binding.
$tracesCode = (string) file_get_contents(__DIR__ . '/../../api/traces.php');
$tokens = \PhpToken::tokenize($tracesCode);
$sqlBuilderHasVariable = false;
for ($i = 0; $i < count($tokens); $i++) {
    if ($tokens[$i]->id !== T_STRING || $tokens[$i]->text !== 'prepare') {
        continue;
    }
    // Look ahead for the matching paren-group's contents.
    $depth = 0;
    $started = false;
    for ($j = $i + 1; $j < count($tokens); $j++) {
        $t = $tokens[$j];
        if ($t->text === '(') {
            $depth++;
            $started = true;
            continue;
        }
        if ($t->text === ')') {
            $depth--;
            if ($started && $depth === 0) {
                break;
            }
            continue;
        }
        if ($started && $t->id === T_VARIABLE) {
            $sqlBuilderHasVariable = true;
            break;
        }
    }
}
assert_false(
    $sqlBuilderHasVariable,
    'no $variable appears inside PDO::prepare(...)'
);
assert_contains($tracesCode, 'bindValue', 'uses bindValue for params');

// Empty-DB case
make_index_sqlite($dataDir . '/index.sqlite', []);
$resp = simulate_request($tracesEndpoint, $reqOpts());
$body = json_decode($resp['body'], true);
assert_eq(0, $body['total'], 'empty DB → total 0');
assert_eq(false, $body['has_more'], 'empty DB → has_more false');
assert_eq([], $body['items'], 'empty DB → items []');

// Wrong method → 405
$resp = simulate_request($tracesEndpoint, $reqOpts(['method' => 'POST']));
assert_eq(405, $resp['status'], 'POST /api/traces → 405');

// cpu_snapshot_available: 0 -> false, 1 -> true (verify via filter)
make_index_sqlite($dataDir . '/index.sqlite', $rows);
$resp = simulate_request($tracesEndpoint, $reqOpts());
$body = json_decode($resp['body'], true);
$byKey = [];
foreach ($body['items'] as $it) {
    $byKey[$it['trace_key']] = $it;
}
assert_eq(true,  $byKey[str_repeat('3', 32)]['cpu_snapshot_available'], 'cpu_snap=1 → true');
assert_eq(false, $byKey[str_repeat('2', 32)]['cpu_snapshot_available'], 'cpu_snap=0 → false');
assert_eq(42, $byKey[str_repeat('2', 32)]['dropped_records'], 'dropped_records visible');
assert_eq('active', $byKey[str_repeat('3', 32)]['state'], 'state column visible');

// Cleanup
foreach (
    glob($dataDir . '/index.sqlite*') ?: [] as $f
) {
    @unlink($f);
}
@rmdir($dataDir . '/traces');
@rmdir($dataDir);
@unlink($confPath);

report_done();
