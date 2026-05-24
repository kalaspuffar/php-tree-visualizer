<?php

declare(strict_types=1);

require_once __DIR__ . '/lib/assert.php';
require_once __DIR__ . '/lib/harness.php';
require_once __DIR__ . '/fixtures/make_config.php';
require_once __DIR__ . '/fixtures/make_index_sqlite.php';
require_once __DIR__ . '/../../api/internal/config.php';
require_once __DIR__ . '/../../api/internal/session.php';

// --- 12.2 Static source checks ---------------------------------------

$apiDir = realpath(__DIR__ . '/../../api');
$apiFiles = [];
$it = new RecursiveIteratorIterator(new RecursiveDirectoryIterator($apiDir));
foreach ($it as $info) {
    if ($info->isFile() && str_ends_with($info->getFilename(), '.php')) {
        $apiFiles[] = $info->getPathname();
    }
}

// Forbid the obvious leak shapes anywhere in api/.
$forbidden = [
    '$_SERVER[\'HTTP_AUTHORIZATION\']',
    '$_SERVER["HTTP_AUTHORIZATION"]',
    'getallheaders',
    'var_dump',
    'print_r($_SERVER',
    'error_log($_SERVER',
    'error_log($_COOKIE',
];

foreach ($apiFiles as $f) {
    $code = strip_php_comments((string) file_get_contents($f));
    foreach ($forbidden as $needle) {
        assert_true(
            !str_contains($code, $needle),
            'api file ' . basename($f) . ' must not contain ' . $needle
        );
    }
}

// Forbid write SQL anywhere in api/ (INV-8 suspenders).
$writePatterns = [
    '/\\bINSERT\\b/i',
    '/\\bUPDATE\\b/i',
    '/\\bDELETE\\b/i',
    '/\\bREPLACE\\s+INTO\\b/i',
    '/\\bCREATE\\s+TABLE\\b/i',
    '/\\bDROP\\s+TABLE\\b/i',
    '/\\bALTER\\s+TABLE\\b/i',
    '/\\bmode=rw\\b/',
];
foreach ($apiFiles as $f) {
    $code = strip_php_comments((string) file_get_contents($f));
    foreach ($writePatterns as $pattern) {
        assert_eq(
            0,
            preg_match($pattern, $code),
            'api file ' . basename($f) . ' must not contain pattern ' . $pattern
        );
    }
}

// Forbid PDO::SQLITE_OPEN_READWRITE / CREATE.
foreach ($apiFiles as $f) {
    $code = strip_php_comments((string) file_get_contents($f));
    assert_true(
        !str_contains($code, 'SQLITE_OPEN_READWRITE'),
        'api file ' . basename($f) . ' must not pass SQLITE_OPEN_READWRITE'
    );
    assert_true(
        !str_contains($code, 'SQLITE_OPEN_CREATE'),
        'api file ' . basename($f) . ' must not pass SQLITE_OPEN_CREATE'
    );
}

// --- 12.1 Behavioral hygiene: token sentinel never leaks -------------

$tokenSentinel = 'SENTINEL-TOKEN-' . str_repeat('Q', 48);
$saltSentinel  = 'SENTINEL-SALT-' . str_repeat('R', 48);
$dataDir = sys_get_temp_dir() . '/phptv-hyg-' . bin2hex(random_bytes(4));
mkdir($dataDir . '/traces', 0700, true);
$confPath = make_config([
    'auth' => ['token' => $tokenSentinel, 'session_salt' => $saltSentinel],
    'storage' => ['data_dir' => $dataDir],
]);
make_index_sqlite($dataDir . '/index.sqlite', [
    ['trace_key' => str_repeat('a', 32)],
]);

// Login with the sentinel token, list traces with the sentinel cookie.
$login = simulate_request(__DIR__ . '/../../api/auth.php', [
    'config_path' => $confPath,
    'method' => 'POST',
    'path' => '/api/auth',
    'headers' => ['Content-Type' => 'application/json'],
    'body' => json_encode(['token' => $tokenSentinel]),
]);
assert_eq(204, $login['status'], 'login succeeds (precondition)');

$cookie = compute_session_value($tokenSentinel, $saltSentinel);
$list = simulate_request(__DIR__ . '/../../api/traces.php', [
    'config_path' => $confPath,
    'method' => 'GET',
    'path' => '/api/traces',
    'cookies' => ['phptv_session' => $cookie],
]);
assert_eq(200, $list['status'], 'list succeeds (precondition)');

// Token sentinel must not appear in any captured output (stderr,
// stdout body, or any header line) from the production endpoints.
foreach ([$login, $list] as $resp) {
    assert_not_contains($resp['stderr'], $tokenSentinel, 'token not in stderr');
    assert_not_contains($resp['body'], $tokenSentinel, 'token not in body');
    foreach ($resp['headers'] as $h) {
        assert_not_contains($h, $tokenSentinel, 'token not in header');
    }
    assert_not_contains($resp['stderr'], $saltSentinel, 'salt not in stderr');
    assert_not_contains($resp['body'], $saltSentinel, 'salt not in body');
}

// Cleanup
foreach (glob($dataDir . '/index.sqlite*') ?: [] as $f) {
    @unlink($f);
}
@rmdir($dataDir . '/traces');
@rmdir($dataDir);
@unlink($confPath);

report_done();

function strip_php_comments(string $php): string
{
    $tokens = \PhpToken::tokenize($php);
    $out = '';
    foreach ($tokens as $t) {
        if ($t->id === T_COMMENT || $t->id === T_DOC_COMMENT) {
            continue;
        }
        $out .= $t->text;
    }
    return $out;
}
