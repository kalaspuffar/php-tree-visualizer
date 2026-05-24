<?php

declare(strict_types=1);

/**
 * Shared substrate for every PHP API endpoint.
 *
 * Every endpoint file requires this first. It loads the TOML config,
 * registers the top-level exception handler, and exposes the helpers
 * endpoints need:
 *   - dispatch_method(string $expectedMethod): void
 *   - read_json_body(): array
 *   - require_session(): void
 *   - json_success(int $status, array $data): never
 *   - json_error(int $status, string $code, ?string $detail = null): never
 *   - open_index_db_ro(): PDO
 *   - open_trace_db_ro(string $trace_key): PDO
 *
 * INV-8: opens SQLite read-only. INV-2: never logs token/cookie content.
 * DR-5: refuses any database whose PRAGMA user_version != 1.
 */

require_once __DIR__ . '/internal/config.php';
require_once __DIR__ . '/internal/response.php';
require_once __DIR__ . '/internal/storage.php';
require_once __DIR__ . '/internal/session.php';

// Eagerly load the config so the redaction sentinels are available
// to log_internal_error from the first request handler that throws.
// Wrapped in a try because a missing config file is a 500-class event
// the top-level handler still needs to produce a clean response for.
try {
    Config::load();
} catch (\Throwable $configFailure) {
    // We register the handler first so even this failure produces a
    // well-formed JSON 500 (without leaking the path).
    set_exception_handler('phptv_handle_uncaught_exception');
    throw $configFailure;
}

set_exception_handler('phptv_handle_uncaught_exception');

/**
 * Reject any HTTP method other than the one this endpoint expects.
 * Writes 405 and exits on mismatch.
 */
function dispatch_method(string $expectedMethod): void
{
    $actual = strtoupper((string) ($_SERVER['REQUEST_METHOD'] ?? ''));
    if ($actual !== strtoupper($expectedMethod)) {
        json_error(405, 'method_not_allowed');
    }
}

/**
 * Read the request body, enforce Content-Type: application/json, and
 * decode. 415 on wrong content type, 400 on malformed JSON.
 *
 * @return array<string, mixed>
 */
function read_json_body(): array
{
    $contentType = (string) ($_SERVER['CONTENT_TYPE'] ?? $_SERVER['HTTP_CONTENT_TYPE'] ?? '');
    // Strip a possible parameter like "; charset=utf-8".
    $mime = trim(strtolower(explode(';', $contentType, 2)[0]));
    if ($mime !== 'application/json') {
        json_error(415, 'unsupported_media_type');
    }
    $raw = phptv_read_raw_body();
    if ($raw === '') {
        json_error(400, 'bad_request');
    }
    try {
        $decoded = json_decode($raw, true, 32, JSON_THROW_ON_ERROR);
    } catch (\JsonException) {
        json_error(400, 'bad_request');
    }
    if (!is_array($decoded)) {
        json_error(400, 'bad_request');
    }
    return $decoded;
}
