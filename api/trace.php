<?php

declare(strict_types=1);

/**
 * Trace-detail endpoints (Phase 4 / SPECIFICATION.md §10.4).
 *
 *   GET /api/traces/{key}
 *   GET /api/traces/{key}/tree
 *   GET /api/traces/{key}/tree/{node_id}/children
 *
 * The proxy routes /api/traces/* (everything except the exact
 * /api/traces handled by traces.php) to this file. Dispatch below
 * picks between the three handlers on REQUEST_URI's path; anything
 * outside the three shapes -> 404 not_found.
 */

require_once __DIR__ . '/bootstrap.php';
require_once __DIR__ . '/internal/tree.php';

$requestPath = parse_url(
    (string) ($_SERVER['REQUEST_URI'] ?? '/'),
    PHP_URL_PATH
) ?? '/';

if (preg_match('#^/api/traces/([0-9a-f]{32})$#', $requestPath, $m)) {
    phptv_handle_trace_meta($m[1]);
}
if (preg_match('#^/api/traces/([0-9a-f]{32})/tree$#', $requestPath, $m)) {
    phptv_handle_trace_tree($m[1], $_GET);
}
if (preg_match('#^/api/traces/([0-9a-f]{32})/tree/([1-9][0-9]*)/children$#', $requestPath, $m)) {
    phptv_handle_trace_children($m[1], (int) $m[2], $_GET);
}

json_error(404, 'not_found');
