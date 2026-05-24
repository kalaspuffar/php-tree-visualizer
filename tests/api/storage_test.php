<?php

declare(strict_types=1);

require_once __DIR__ . '/lib/assert.php';
require_once __DIR__ . '/fixtures/make_index_sqlite.php';
require_once __DIR__ . '/fixtures/make_config.php';
require_once __DIR__ . '/../../api/internal/storage.php';

$dataDir = sys_get_temp_dir() . '/phptv-storage-test-' . bin2hex(random_bytes(4));
mkdir($dataDir . '/traces', 0700, true);

$confPath = make_config([
    'storage' => ['data_dir' => $dataDir],
]);
Config::forgetCache();
Config::load($confPath);

// --- 5.5 v1 DB opens, returns PDO with the expected flags ---------

make_index_sqlite($dataDir . '/index.sqlite', [
    ['trace_key' => str_repeat('a', 32)],
]);
$pdo = open_index_db_ro();
assert_true($pdo instanceof \PDO, 'open_index_db_ro returns a PDO');
assert_eq(
    \PDO::FETCH_ASSOC,
    $pdo->getAttribute(\PDO::ATTR_DEFAULT_FETCH_MODE),
    'default fetch mode is FETCH_ASSOC'
);

// PDO::SQLITE_ATTR_OPEN_FLAGS is a constructor-only attribute (not
// readable via getAttribute), so RO mode is verified two ways:
//   1. Behaviorally — writes raise (assertion below).
//   2. Statically — by source grep — verifying that the production
//      *code* (comments excluded) passes SQLITE_OPEN_READONLY and not
//      SQLITE_OPEN_READWRITE or SQLITE_OPEN_CREATE.
$storageCode = strip_php_comments(
    (string) file_get_contents(__DIR__ . '/../../api/internal/storage.php')
);
assert_true(
    str_contains($storageCode, 'SQLITE_OPEN_READONLY'),
    'production code mentions SQLITE_OPEN_READONLY'
);
assert_true(
    !str_contains($storageCode, 'SQLITE_OPEN_READWRITE'),
    'production code does NOT mention SQLITE_OPEN_READWRITE'
);
assert_true(
    !str_contains($storageCode, 'SQLITE_OPEN_CREATE'),
    'production code does NOT mention SQLITE_OPEN_CREATE'
);

/**
 * Strip comments + whitespace via PHP's tokenizer so static greps
 * see code, not documentation.
 */
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

// Reads succeed
$count = (int) $pdo->query('SELECT COUNT(*) FROM traces')->fetchColumn();
assert_eq(1, $count, 'fixture row is visible');

// Writes fail (suspenders for SQLite layer)
$writeAttempt = null;
try {
    $pdo->exec(
        "INSERT INTO traces (trace_key, trace_id, host, pid, start_time_ns, sapi,"
        . " uri_or_script, first_batch_at_ns, last_batch_at_ns) VALUES"
        . " ('" . str_repeat('b', 32) . "', '00000000-0000-0000-0000-000000000000',"
        . " 'h', 1, 1, 'cli', '/x', 1, 1)"
    );
} catch (\Throwable $t) {
    $writeAttempt = $t;
}
assert_true($writeAttempt !== null, 'INSERT against the RO PDO throws');

// --- 5.6 v2 DB raises SchemaVersionMismatch -----------------------

$badDataDir = sys_get_temp_dir() . '/phptv-storage-bad-' . bin2hex(random_bytes(4));
mkdir($badDataDir . '/traces', 0700, true);
make_index_sqlite($badDataDir . '/index.sqlite', [], 2);

$badConf = make_config(['storage' => ['data_dir' => $badDataDir]]);
Config::forgetCache();
Config::load($badConf);

$err = assert_throws(
    SchemaVersionMismatch::class,
    fn() => open_index_db_ro(),
    'v2 DB throws SchemaVersionMismatch'
);
if ($err instanceof SchemaVersionMismatch) {
    assert_eq(2, $err->observedVersion, 'observed version is 2');
    assert_contains($err->path, '/index.sqlite', 'path is reported');
}

// --- 5.7 path traversal in trace_key ------------------------------

Config::forgetCache();
Config::load($confPath);

assert_throws(
    InvalidTraceKey::class,
    fn() => open_trace_db_ro('../../etc/passwd'),
    'traversal rejected'
);
assert_throws(
    InvalidTraceKey::class,
    fn() => open_trace_db_ro(''),
    'empty rejected'
);
assert_throws(
    InvalidTraceKey::class,
    fn() => open_trace_db_ro('AABBCCDD00112233445566778899AABB'),
    'uppercase rejected (must be lowercase hex)'
);
assert_throws(
    InvalidTraceKey::class,
    fn() => open_trace_db_ro('aabb'),
    'too short rejected'
);
$valid = str_repeat('a', 32);
// File doesn't exist, but key validation must pass first; we expect a
// RuntimeException about the missing file, not InvalidTraceKey.
$err = assert_throws(
    \RuntimeException::class,
    fn() => open_trace_db_ro($valid),
    'valid key passes shape check'
);
if ($err !== null) {
    assert_true(!($err instanceof InvalidTraceKey), 'thrown class is not InvalidTraceKey');
}

// Cleanup
foreach (
    glob($dataDir . '/index.sqlite*') ?: [] as $f
) {
    @unlink($f);
}
@rmdir($dataDir . '/traces');
@rmdir($dataDir);
foreach (
    glob($badDataDir . '/index.sqlite*') ?: [] as $f
) {
    @unlink($f);
}
@rmdir($badDataDir . '/traces');
@rmdir($badDataDir);
@unlink($confPath);
@unlink($badConf);

report_done();
