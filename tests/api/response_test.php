<?php

declare(strict_types=1);

require_once __DIR__ . '/lib/assert.php';
require_once __DIR__ . '/fixtures/make_config.php';

// We need json_success/json_error to NOT exit during these tests so we
// can run several assertions in one file. Wrap the exit by catching the
// fatal-effect via a custom shutdown? Easier: declare PHP_EXIT_SHIM
// before loading response.php and use a thrown exception instead.
// Simplest path: drive json_success/json_error through the harness for
// the cases that exit; test the helpers that don't exit (format_rfc3339_ns,
// phptv_redact, log_internal_error) directly here.

require_once __DIR__ . '/../../api/internal/response.php';

// --- 10.1 / 10.2 / 10.3 / 10.4 format_rfc3339_ns -------------------

assert_eq(
    '2025-05-23T11:33:20.123456789Z',
    format_rfc3339_ns(1748000000123456789),
    'known timestamp formats with nanosecond precision'
);
assert_eq(
    '2001-09-09T01:46:40.000000000Z',
    format_rfc3339_ns(1_000_000_000_000_000_000),
    'integer second has nine-zero fractional pad'
);
assert_eq(
    '1970-01-01T00:00:00.000000005Z',
    format_rfc3339_ns(5),
    'tiny ns pads on the left'
);
assert_eq(
    '1970-01-01T00:00:00.000000000Z',
    format_rfc3339_ns(0),
    'zero is the epoch'
);
assert_true(
    str_ends_with(format_rfc3339_ns(123), 'Z'),
    'always ends with Z'
);
// Exactly 9 fractional digits, always.
$samples = [
    format_rfc3339_ns(0),
    format_rfc3339_ns(1),
    format_rfc3339_ns(1748000000123456789),
    format_rfc3339_ns(123_456_789),
];
foreach ($samples as $s) {
    assert_true(
        (bool) preg_match('/\.\d{9}Z$/', $s),
        'has exactly 9 fractional digits: ' . $s
    );
}

// --- 4.6 log_internal_error redacts the configured token ----------

$confPath = make_config([
    'auth' => [
        'token'        => 'SENTINEL-TOKEN-XYZZY-' . str_repeat('a', 24),
        'session_salt' => 'SENTINEL-SALT-PLUGH-' . str_repeat('b', 24),
    ],
]);
Config::forgetCache();
Config::load($confPath);

$capturedLines = [];
$GLOBALS['__phptv_test_syslog_sink'] = static function (string $line) use (&$capturedLines): void {
    $capturedLines[] = $line;
};

$_COOKIE['phptv_session'] = 'COOKIE-VALUE-OPAQUE-ZYXWV';

try {
    throw new \RuntimeException(
        'something exploded with token SENTINEL-TOKEN-XYZZY-' . str_repeat('a', 24)
        . ' and salt SENTINEL-SALT-PLUGH-' . str_repeat('b', 24)
        . ' and cookie COOKIE-VALUE-OPAQUE-ZYXWV'
    );
} catch (\Throwable $t) {
    log_internal_error($t);
}

assert_eq(1, count($capturedLines), 'one syslog line emitted');
$line = $capturedLines[0];
assert_not_contains($line, 'SENTINEL-TOKEN-XYZZY', 'token not in log line');
assert_not_contains($line, 'SENTINEL-SALT-PLUGH', 'salt not in log line');
assert_not_contains($line, 'COOKIE-VALUE-OPAQUE', 'cookie not in log line');
assert_contains($line, '[REDACTED]', 'redaction marker present');
assert_contains($line, 'RuntimeException', 'exception class present');
assert_contains($line, 'something exploded', 'context message present (after redaction)');

// phptv_redact is also exercised directly for empty-sentinel safety.
assert_eq(
    'plain message',
    phptv_redact('plain message', ['']),
    'empty sentinel is a no-op'
);
assert_eq(
    'a[REDACTED]b',
    phptv_redact('axyzb', ['xyz']),
    'simple sentinel replacement'
);

@unlink($confPath);
Config::forgetCache();

report_done();
