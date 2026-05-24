<?php

declare(strict_types=1);

/**
 * JSON response helpers + RFC-3339 nanosecond formatter +
 * secret-redacting error logger + top-level exception handler.
 *
 * All HTTP emission goes through the small `phptv_emit_*` shims
 * defined below so the test harness can capture them in-process
 * without the production code knowing about tests.
 */

require_once __DIR__ . '/config.php';

/**
 * Emit an HTTP status code. In production this calls
 * http_response_code(); in tests a hook captures it.
 */
function phptv_emit_status(int $status): void
{
    if (isset($GLOBALS['__phptv_test_emit_status'])) {
        ($GLOBALS['__phptv_test_emit_status'])($status);
        return;
    }
    http_response_code($status);
}

/**
 * Emit a single response header line ("Name: value"). Same hook story.
 */
function phptv_emit_header(string $line): void
{
    if (isset($GLOBALS['__phptv_test_emit_header'])) {
        ($GLOBALS['__phptv_test_emit_header'])($line);
        return;
    }
    header($line, true);
}

/**
 * Emit a `Set-Cookie:` line. Built as a string rather than via
 * `setcookie()` so the test harness sees it identically to production
 * and so we control attribute order.
 */
function phptv_emit_set_cookie(string $cookie): void
{
    phptv_emit_header('Set-Cookie: ' . $cookie);
}

/**
 * Read the raw request body. Hook lets the harness inject without
 * touching the php://input stream wrapper.
 */
function phptv_read_raw_body(): string
{
    if (isset($GLOBALS['__phptv_test_input_body'])) {
        return (string) $GLOBALS['__phptv_test_input_body'];
    }
    $raw = file_get_contents('php://input');
    return $raw === false ? '' : $raw;
}

/**
 * Write a JSON success response and exit. Never returns.
 *
 * @param array<string,mixed>|list<mixed> $data
 */
function json_success(int $status, array $data): never
{
    $body = json_encode($data, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES);
    phptv_emit_status($status);
    phptv_emit_header('Content-Type: application/json');
    echo $body;
    exit;
}

/**
 * Write a JSON error response and exit. Never returns.
 */
function json_error(int $status, string $code, ?string $detail = null): never
{
    $payload = ['error' => $code];
    if ($detail !== null) {
        $payload['detail'] = $detail;
    }
    $body = json_encode($payload, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES);
    phptv_emit_status($status);
    phptv_emit_header('Content-Type: application/json');
    echo $body;
    exit;
}

/**
 * Format CLOCK_REALTIME nanoseconds-since-epoch as
 * RFC-3339 UTC with nine fractional digits and a trailing Z.
 *
 * INV-3: ns is CLOCK_REALTIME; never mix with CLOCK_MONOTONIC.
 */
function format_rfc3339_ns(int $ns): string
{
    if ($ns < 0) {
        // Pre-epoch values aren't expected; keep the formatter total
        // by emitting an explicit invalid marker rather than producing
        // a malformed string.
        return '0000-00-00T00:00:00.000000000Z';
    }
    $sec = intdiv($ns, 1_000_000_000);
    $frac = $ns % 1_000_000_000;
    return gmdate('Y-m-d\\TH:i:s', $sec) . sprintf('.%09dZ', $frac);
}

/**
 * Log an internal error to syslog, scrubbing any byte-sequence that
 * matches the configured token or session salt.
 *
 * Tests stub `__phptv_test_syslog_sink` to capture the formatted line
 * for inspection.
 */
function log_internal_error(\Throwable $e): void
{
    $sentinels = [];
    $config = Config::cached();
    if ($config !== null) {
        $sentinels = $config->logRedactionSentinels();
    }
    if (isset($_COOKIE['phptv_session']) && is_string($_COOKIE['phptv_session'])) {
        $sentinels[] = $_COOKIE['phptv_session'];
    }

    $line = sprintf(
        'phptv-api error: %s: %s at %s:%d',
        $e::class,
        $e->getMessage(),
        $e->getFile(),
        $e->getLine()
    );
    $line = phptv_redact($line, $sentinels);

    if (isset($GLOBALS['__phptv_test_syslog_sink'])) {
        ($GLOBALS['__phptv_test_syslog_sink'])($line);
        return;
    }
    // openlog/syslog/closelog is the operator-friendly path. error_log
    // is the fallback for environments without syslog wired in.
    if (function_exists('openlog')) {
        openlog('phptv-api', LOG_PID | LOG_NDELAY, LOG_USER);
        syslog(LOG_ERR, $line);
        closelog();
    } else {
        error_log($line);
    }
}

/**
 * Replace every occurrence of every sentinel in $text with "[REDACTED]".
 * Empty sentinels are ignored (would match the empty string between
 * every byte).
 *
 * @param list<string> $sentinels
 */
function phptv_redact(string $text, array $sentinels): string
{
    foreach ($sentinels as $s) {
        if ($s === '') {
            continue;
        }
        $text = str_replace($s, '[REDACTED]', $text);
    }
    return $text;
}

/**
 * Top-level exception handler. Wired by bootstrap.php.
 *
 * Maps known exception classes to their documented JSON shapes; falls
 * through to 500 internal_error for everything else.
 */
function phptv_handle_uncaught_exception(\Throwable $e): void
{
    // SchemaVersionMismatch is defined in storage.php; class_exists is
    // the trick to avoid load-order coupling between this file and
    // storage.php.
    if (class_exists('SchemaVersionMismatch', false) && $e instanceof SchemaVersionMismatch) {
        $message = sprintf(
            'phptv-api schema_version_mismatch path=%s observed=%d',
            $e->path,
            $e->observedVersion
        );
        if (isset($GLOBALS['__phptv_test_syslog_sink'])) {
            ($GLOBALS['__phptv_test_syslog_sink'])($message);
        } elseif (function_exists('openlog')) {
            openlog('phptv-api', LOG_PID | LOG_NDELAY, LOG_USER);
            syslog(LOG_ERR, $message);
            closelog();
        } else {
            error_log($message);
        }
        json_error(500, 'schema_version_mismatch');
    }

    log_internal_error($e);
    json_error(500, 'internal_error');
}
