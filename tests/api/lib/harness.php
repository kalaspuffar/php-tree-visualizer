<?php

declare(strict_types=1);

/**
 * In-process request simulator.
 *
 * `simulate_request($endpointFile, $opts)` sets up the $_SERVER /
 * $_COOKIE / $_GET / php://input environment for the endpoint, then
 * runs it in a child PHP process so json_success/json_error's `exit`
 * doesn't terminate the test runner. The child writes a serialized
 * response (status code + headers + body) to a temp file which the
 * parent reads back.
 *
 * Why a child process: the production code uses `exit` after writing
 * a response. We could refactor to `return` but the spec is `exit`
 * (the safer terminal). A subprocess is the cleanest containment.
 */

require_once __DIR__ . '/assert.php';

/**
 * @param array{
 *   method?: string,
 *   path?: string,
 *   query?: string|array<string,string>,
 *   headers?: array<string,string>,
 *   cookies?: array<string,string>,
 *   body?: string,
 *   config_path?: string,
 *   data_dir?: string,
 *   env?: array<string,string>,
 * } $opts
 * @return array{status:int, headers:array<int,string>, body:string, stderr:string, exit_code:int}
 */
function simulate_request(string $endpointFile, array $opts = []): array
{
    $method = $opts['method'] ?? 'GET';
    $path = $opts['path'] ?? '/';
    $query = $opts['query'] ?? '';
    if (is_array($query)) {
        $query = http_build_query($query);
    }
    $headers = $opts['headers'] ?? [];
    $cookies = $opts['cookies'] ?? [];
    $body = $opts['body'] ?? '';
    $env = $opts['env'] ?? [];
    if (isset($opts['config_path'])) {
        $env['PHPTV_CONFIG'] = $opts['config_path'];
    }
    if (isset($opts['data_dir'])) {
        $env['PHPTV_DATA_DIR_OVERRIDE'] = $opts['data_dir'];
    }

    $outFile = tempnam(sys_get_temp_dir(), 'phptv_resp_');
    if ($outFile === false) {
        throw new \RuntimeException('tempnam failed');
    }

    $payload = [
        'endpoint' => $endpointFile,
        'method' => $method,
        'path' => $path,
        'query' => $query,
        'headers' => $headers,
        'cookies' => $cookies,
        'body' => $body,
        'out_file' => $outFile,
    ];

    $payloadFile = tempnam(sys_get_temp_dir(), 'phptv_in_');
    if ($payloadFile === false) {
        @unlink($outFile);
        throw new \RuntimeException('tempnam failed');
    }
    file_put_contents($payloadFile, serialize($payload));

    $runner = __DIR__ . '/harness_child.php';
    $command = escapeshellcmd(PHP_BINARY)
        . ' -d error_reporting=E_ALL'
        . ' -d display_errors=stderr'
        . ' ' . escapeshellarg($runner)
        . ' ' . escapeshellarg($payloadFile);

    $envBlock = [];
    foreach ($env as $k => $v) {
        $envBlock[$k] = (string) $v;
    }
    // Inherit current env unless overridden.
    foreach (getenv() as $k => $v) {
        if (!isset($envBlock[$k])) {
            $envBlock[$k] = (string) $v;
        }
    }

    $process = proc_open(
        $command,
        [
            1 => ['pipe', 'w'],
            2 => ['pipe', 'w'],
        ],
        $pipes,
        null,
        $envBlock
    );
    if (!is_resource($process)) {
        @unlink($outFile);
        @unlink($payloadFile);
        throw new \RuntimeException('proc_open failed');
    }
    $childStdout = stream_get_contents($pipes[1]) ?: '';
    $childStderr = stream_get_contents($pipes[2]) ?: '';
    fclose($pipes[1]);
    fclose($pipes[2]);
    $exitCode = proc_close($process);

    $captured = @file_get_contents($outFile);
    @unlink($outFile);
    @unlink($payloadFile);

    if ($captured === false || $captured === '') {
        return [
            'status' => 0,
            'headers' => [],
            'body' => $childStdout,
            'stderr' => $childStderr,
            'exit_code' => $exitCode,
        ];
    }

    $decoded = @unserialize($captured);
    if (!is_array($decoded)) {
        return [
            'status' => 0,
            'headers' => [],
            'body' => $childStdout,
            'stderr' => $childStderr,
            'exit_code' => $exitCode,
        ];
    }

    return [
        'status' => (int) ($decoded['status'] ?? 0),
        'headers' => array_values($decoded['headers'] ?? []),
        'body' => (string) ($decoded['body'] ?? ''),
        'stderr' => $childStderr,
        'exit_code' => $exitCode,
    ];
}

/**
 * Return the value of the first header whose name (case-insensitive)
 * matches $name, or null if not present.
 *
 * @param array<int,string> $headers
 */
function header_value(array $headers, string $name): ?string
{
    $needle = strtolower($name) . ':';
    foreach ($headers as $line) {
        if (stripos($line, $needle) === 0) {
            return ltrim(substr($line, strlen($needle)));
        }
    }
    return null;
}

/**
 * Decode the JSON body if Content-Type is application/json; else null.
 *
 * @return array<string,mixed>|null
 */
function json_body(array $response): ?array
{
    $ct = header_value($response['headers'], 'Content-Type') ?? '';
    if (stripos($ct, 'application/json') === false) {
        return null;
    }
    $decoded = json_decode($response['body'], true);
    return is_array($decoded) ? $decoded : null;
}
