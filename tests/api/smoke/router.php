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
 *   /api/traces/*         → api/trace.php   (Phase 4)
 *   /viz/*                → static file from viz/  (Phase 5)
 *   anything else         → 404
 */

$repoRoot = realpath(__DIR__ . '/../../..');
$apiDir = realpath(__DIR__ . '/../../../api');
$vizDir = realpath(__DIR__ . '/../../../viz');
if ($apiDir === false || $repoRoot === false) {
    http_response_code(500);
    echo "router cannot locate api/ directory\n";
    return true;
}

$path = parse_url((string) $_SERVER['REQUEST_URI'], PHP_URL_PATH) ?? '/';

// /viz/* — serve static files. The built-in server already serves
// files that exist at the document root; we only fall into this
// branch when `php -S` is rooted at this router (so static-file
// auto-serving is opt-in via a `false` return). Resolve the path
// against viz/, refuse traversal, and let php -S serve via `return
// false` if the file exists.
if (str_starts_with($path, '/viz/') && $vizDir !== false) {
    $relative = substr($path, strlen('/viz/'));
    $candidate = $vizDir . '/' . $relative;
    $real = realpath($candidate);
    if ($real !== false && str_starts_with($real, $vizDir) && is_file($real)) {
        // Set a content-type that matches the extension; php -S's
        // built-in MIME table is not exposed to a router script, so
        // we route the common static types ourselves.
        $ext = strtolower(pathinfo($real, PATHINFO_EXTENSION));
        $types = [
            'html' => 'text/html; charset=utf-8',
            'css'  => 'text/css; charset=utf-8',
            'js'   => 'text/javascript; charset=utf-8',
            'svg'  => 'image/svg+xml',
            'json' => 'application/json',
        ];
        if (isset($types[$ext])) {
            header('Content-Type: ' . $types[$ext]);
        }
        readfile($real);
        return true;
    }
    http_response_code(404);
    return true;
}

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

if (str_starts_with($path, '/api/traces/')) {
    require $apiDir . '/trace.php';
    return true;
}

http_response_code(404);
header('Content-Type: application/json');
echo '{"error":"not_found"}';
return true;
