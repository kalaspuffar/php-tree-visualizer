<?php

declare(strict_types=1);

/**
 * Read-only SQLite opener with the DR-5 schema-version gate.
 *
 *   open_index_db_ro(): PDO
 *   open_trace_db_ro(string $trace_key): PDO
 *
 * Both:
 *  - Resolve the path from [storage].data_dir in the loaded Config.
 *  - Construct PDO with SQLITE_OPEN_READONLY (INV-8); never with
 *    SQLITE_OPEN_READWRITE or SQLITE_OPEN_CREATE.
 *  - Verify PRAGMA user_version == 1 (DR-5). Mismatch throws
 *    SchemaVersionMismatch; the top-level handler in response.php
 *    maps that to 500 schema_version_mismatch.
 *
 * INV-8: nothing in this file issues a write. SQLite's read-only mode
 * is the belt; the "no INSERT/UPDATE/DELETE/REPLACE in api/" grep in
 * tests/api/static_check_test.php is the suspenders.
 */

require_once __DIR__ . '/config.php';

class SchemaVersionMismatch extends \RuntimeException
{
    public string $path;
    public int $observedVersion;

    public function __construct(string $path, int $observedVersion)
    {
        parent::__construct(
            "schema version mismatch: {$path} reports user_version={$observedVersion}, want 1"
        );
        $this->path = $path;
        $this->observedVersion = $observedVersion;
    }
}

class InvalidTraceKey extends \RuntimeException
{
}

/**
 * Open <data_dir>/index.sqlite read-only and verify user_version == 1.
 */
function open_index_db_ro(): \PDO
{
    $dataDir = phptv_resolve_data_dir();
    $path = rtrim($dataDir, '/') . '/index.sqlite';
    return phptv_open_sqlite_ro($path);
}

/**
 * Open <data_dir>/traces/<trace_key>.sqlite read-only. Validates the
 * trace key shape (32 lowercase hex chars) before touching the path so
 * traversal payloads never reach the filesystem.
 */
function open_trace_db_ro(string $trace_key): \PDO
{
    if (!phptv_is_valid_trace_key($trace_key)) {
        throw new InvalidTraceKey(
            'trace_key must be 32 lowercase hex characters'
        );
    }
    $dataDir = phptv_resolve_data_dir();
    $path = rtrim($dataDir, '/') . '/traces/' . $trace_key . '.sqlite';
    return phptv_open_sqlite_ro($path);
}

/**
 * Test seam: callers can override the data_dir via the
 * PHPTV_DATA_DIR_OVERRIDE env var (used by tests so they can point at
 * fixtures without rewriting the production config path).
 */
function phptv_resolve_data_dir(): string
{
    $override = getenv('PHPTV_DATA_DIR_OVERRIDE');
    if (is_string($override) && $override !== '') {
        return $override;
    }
    return Config::load()->getString('storage', 'data_dir');
}

function phptv_is_valid_trace_key(string $candidate): bool
{
    return (bool) preg_match('/^[0-9a-f]{32}$/', $candidate);
}

/**
 * Internal: open a SQLite path read-only and verify the user_version.
 * Centralised so both helpers above stay consistent.
 */
function phptv_open_sqlite_ro(string $path): \PDO
{
    if (!is_file($path)) {
        // Map "file missing" to a SchemaVersionMismatch-shaped 500 is
        // wrong (the file IS missing, not the wrong shape). Throwing
        // the bare RuntimeException lets the top-level handler land it
        // on 500 internal_error. The operator log line will name the
        // path; the response body will not.
        throw new \RuntimeException("sqlite file not found: {$path}");
    }
    $pdo = new \PDO(
        'sqlite:' . $path,
        null,
        null,
        [
            \PDO::ATTR_ERRMODE            => \PDO::ERRMODE_EXCEPTION,
            \PDO::ATTR_DEFAULT_FETCH_MODE => \PDO::FETCH_ASSOC,
            \PDO::SQLITE_ATTR_OPEN_FLAGS  => \PDO::SQLITE_OPEN_READONLY,
        ]
    );

    $stmt = $pdo->query('PRAGMA user_version');
    if ($stmt === false) {
        throw new \RuntimeException("could not query user_version: {$path}");
    }
    $row = $stmt->fetch(\PDO::FETCH_NUM);
    $version = is_array($row) ? (int) $row[0] : -1;
    if ($version !== 1) {
        throw new SchemaVersionMismatch($path, $version);
    }
    return $pdo;
}
