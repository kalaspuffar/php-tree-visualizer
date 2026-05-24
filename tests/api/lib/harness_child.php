<?php

declare(strict_types=1);

/**
 * Child-process entry point for simulate_request().
 *
 * Receives a serialized payload describing one HTTP request, sets up
 * $_SERVER / $_COOKIE / $_GET / php://input, requires the endpoint
 * file (which `exit`s via json_success/json_error), captures
 * everything that came out, and writes a serialized response to the
 * out_file the parent will read back.
 *
 * A shutdown function ensures the response is written even if the
 * endpoint dies with a fatal — the parent gets enough to diagnose.
 */

if ($argc < 2) {
    fwrite(STDERR, "harness_child.php expects a payload file path\n");
    exit(2);
}

$payloadFile = $argv[1];
$raw = file_get_contents($payloadFile);
if ($raw === false) {
    fwrite(STDERR, "could not read payload file: {$payloadFile}\n");
    exit(2);
}
$payload = @unserialize($raw);
if (!is_array($payload)) {
    fwrite(STDERR, "payload not unserializable\n");
    exit(2);
}

$method   = (string) ($payload['method'] ?? 'GET');
$path     = (string) ($payload['path'] ?? '/');
$query    = (string) ($payload['query'] ?? '');
$headers  = (array)  ($payload['headers'] ?? []);
$cookies  = (array)  ($payload['cookies'] ?? []);
$body     = (string) ($payload['body'] ?? '');
$endpoint = (string) ($payload['endpoint'] ?? '');
$outFile  = (string) ($payload['out_file'] ?? '');

if ($outFile === '' || $endpoint === '') {
    fwrite(STDERR, "payload missing endpoint or out_file\n");
    exit(2);
}

// Build $_SERVER.
$_SERVER['REQUEST_METHOD'] = strtoupper($method);
$_SERVER['REQUEST_URI']    = $path . ($query !== '' ? '?' . $query : '');
$_SERVER['QUERY_STRING']   = $query;
$_SERVER['HTTP_HOST']      = $headers['Host'] ?? 'localhost';
$_SERVER['SERVER_PROTOCOL']= 'HTTP/1.1';
$_SERVER['HTTPS']          = 'off';

foreach ($headers as $name => $value) {
    $name = strtoupper(str_replace('-', '_', (string) $name));
    if ($name === 'CONTENT_TYPE' || $name === 'CONTENT_LENGTH') {
        $_SERVER[$name] = (string) $value;
    } else {
        $_SERVER['HTTP_' . $name] = (string) $value;
    }
}

$_COOKIE = [];
foreach ($cookies as $k => $v) {
    $_COOKIE[(string) $k] = (string) $v;
}

$_GET = [];
parse_str($query, $_GET);

// Stand in for php://input. The endpoint reads it via the
// `phptv_read_raw_body()` helper in bootstrap.php, which we wire to
// a global so the harness can inject without touching the stream wrapper.
$GLOBALS['__phptv_test_input_body'] = $body;

// Capture stdout (the endpoint echoes the JSON body) and headers
// (which the endpoint emits via header()).
$capturedHeaders = [];
$capturedStatus = 200;

// Wire global hooks the production helpers consult when present.
// This keeps the production code testable WITHOUT putting an `if (TEST)`
// branch into it — the production helpers just call these functions if
// they are defined.
$GLOBALS['__phptv_test_emit_header'] = static function (string $line) use (&$capturedHeaders): void {
    $capturedHeaders[] = $line;
};
$GLOBALS['__phptv_test_emit_status'] = static function (int $status) use (&$capturedStatus): void {
    $capturedStatus = $status;
};

ob_start();

$writeResponse = static function () use (&$capturedHeaders, &$capturedStatus, $outFile): void {
    $body = ob_get_clean();
    if ($body === false) {
        $body = '';
    }
    file_put_contents($outFile, serialize([
        'status' => $capturedStatus,
        'headers' => $capturedHeaders,
        'body' => $body,
    ]));
};

register_shutdown_function($writeResponse);

require $endpoint;
