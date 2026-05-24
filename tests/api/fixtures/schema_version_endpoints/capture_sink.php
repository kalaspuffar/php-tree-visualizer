<?php

declare(strict_types=1);

/**
 * Test-only endpoint: load bootstrap.php so production wiring is in
 * place, then override the exception handler to capture the syslog
 * message into the JSON response body so the parent test can read it
 * back via the harness.
 */

require_once __DIR__ . '/../../../../api/bootstrap.php';

set_exception_handler(static function (\Throwable $e): void {
    if ($e instanceof SchemaVersionMismatch) {
        $message = sprintf(
            'phptv-api schema_version_mismatch path=%s observed=%d',
            $e->path,
            $e->observedVersion
        );
        phptv_emit_status(500);
        phptv_emit_header('Content-Type: application/json');
        echo json_encode([
            'error' => 'schema_version_mismatch',
            'captured_line' => $message,
        ]);
        exit;
    }
    phptv_emit_status(500);
    phptv_emit_header('Content-Type: application/json');
    echo json_encode([
        'error' => 'internal_error',
        'class' => $e::class,
        'message' => $e->getMessage(),
    ]);
    exit;
});

// Now drive the traces endpoint logic. We avoid re-requiring
// traces.php (which would re-call phptv_handle_traces_list at module
// load) and instead call the public entry directly. traces.php's
// functions are loaded as a side effect of bootstrap.php only if
// traces.php has been included; load it via require_once so the
// const declarations execute but the dispatch call is allowed to run
// — we *want* the dispatch to throw SchemaVersionMismatch which our
// handler above catches.
require __DIR__ . '/../../../../api/traces.php';
