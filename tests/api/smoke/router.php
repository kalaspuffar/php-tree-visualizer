<?php

declare(strict_types=1);

/**
 * Router for `php -S` — mirrors the proxy rules in etc/nginx-example.conf
 * so the end-to-end smoke test can drive real HTTP against the endpoints.
 *
 *   /api/internal/*       → 404
 *   /api/auth             → api/auth.php
 *   /api/auth/logout      → api/auth.php
 *   /api/traces           → api/traces.php  (exact path)
 *   /api/traces/*         → 404 (Phase 4 file does not exist yet)
 *   anything else         → 404
 */

$apiDir = realpath(__DIR__ . '/../../../api');
if ($apiDir === false) {
    http_response_code(500);
    echo "router cannot locate api/ directory\n";
    return true;
}

$path = parse_url((string) $_SERVER['REQUEST_URI'], PHP_URL_PATH) ?? '/';

if (str_starts_with($path, '/api/internal/')) {
    http_response_code(404);
    header('Content-Type: application/json');
    echo '{"error":"not_found"}';
    return true;
}

if ($path === '/api/auth' || $path === '/api/auth/logout') {
    require $apiDir . '/auth.php';
    return true;
}

if ($path === '/api/traces') {
    require $apiDir . '/traces.php';
    return true;
}

http_response_code(404);
header('Content-Type: application/json');
echo '{"error":"not_found"}';
return true;
