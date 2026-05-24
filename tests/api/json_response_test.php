<?php

declare(strict_types=1);

require_once __DIR__ . '/lib/assert.php';
require_once __DIR__ . '/lib/harness.php';
require_once __DIR__ . '/fixtures/make_config.php';

$confPath = make_config();

$success = simulate_request(
    __DIR__ . '/fixtures/echo_json_endpoints/json_success_endpoint.php',
    ['config_path' => $confPath]
);
assert_eq(200, $success['status'], 'json_success emits the requested status');
assert_eq(
    'application/json',
    header_value($success['headers'], 'Content-Type'),
    'Content-Type is application/json'
);
$decoded = json_decode($success['body'], true);
assert_eq(['hello' => 'world', 'count' => 3], $decoded, 'success body round-trips');

$err = simulate_request(
    __DIR__ . '/fixtures/echo_json_endpoints/json_error_endpoint.php',
    ['config_path' => $confPath]
);
assert_eq(400, $err['status'], 'json_error emits the requested status');
$errBody = json_decode($err['body'], true);
assert_eq(['error' => 'bad_request'], $errBody, 'error body shape matches');

@unlink($confPath);
report_done();
