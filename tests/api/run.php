<?php

declare(strict_types=1);

/**
 * Test runner — discovers *_test.php under tests/api/ and runs each in
 * a child PHP process so a fatal in one test doesn't kill the run.
 *
 * Invoke from the repo root: `php tests/api/run.php`.
 * Exits 0 on full pass, 1 on any failure.
 *
 * Test files are plain PHP that call `assert_eq`, `assert_true`,
 * `assert_throws`, and `report_done()` at the end. The helpers live in
 * tests/api/lib/assert.php.
 */

$rootDir = realpath(__DIR__ . '/..');
if ($rootDir === false) {
    fwrite(STDERR, "cannot resolve tests directory\n");
    exit(1);
}

$testFiles = collect_test_files(__DIR__);
if ($testFiles === []) {
    fwrite(STDERR, "no test files found under tests/api/\n");
    exit(1);
}

sort($testFiles);

$totalFiles = count($testFiles);
$failedFiles = [];
$totalAssertions = 0;
$failedAssertions = 0;

$startedAt = microtime(true);

foreach ($testFiles as $file) {
    $relativePath = substr($file, strlen($rootDir) + 1);
    fwrite(STDOUT, "→ {$relativePath}\n");

    $command = escapeshellcmd(PHP_BINARY)
        . ' -d error_reporting=E_ALL'
        . ' -d display_errors=stderr'
        . ' ' . escapeshellarg($file);

    $process = proc_open(
        $command,
        [
            1 => ['pipe', 'w'],
            2 => ['pipe', 'w'],
        ],
        $pipes,
        $rootDir
    );

    if (!is_resource($process)) {
        fwrite(STDERR, "  ✗ could not spawn child process\n");
        $failedFiles[] = $relativePath;
        continue;
    }

    $stdout = stream_get_contents($pipes[1]) ?: '';
    $stderr = stream_get_contents($pipes[2]) ?: '';
    fclose($pipes[1]);
    fclose($pipes[2]);
    $exitCode = proc_close($process);

    [$summary, $printed] = parse_summary($stdout);
    $totalAssertions += $summary['total'] ?? 0;
    $failedAssertions += $summary['failed'] ?? 0;

    if ($printed !== '') {
        fwrite(STDOUT, $printed);
    }

    $passed = $exitCode === 0 && ($summary['failed'] ?? 1) === 0;
    if ($passed) {
        $count = $summary['total'] ?? 0;
        fwrite(STDOUT, "  ✓ {$count} assertion(s)\n");
    } else {
        fwrite(STDOUT, "  ✗ failed (exit={$exitCode})\n");
        if ($stderr !== '') {
            fwrite(STDOUT, indent_block($stderr, '    │ '));
        }
        $failedFiles[] = $relativePath;
    }
}

$elapsedMs = (int) round((microtime(true) - $startedAt) * 1000);

fwrite(STDOUT, "\n");
if ($failedFiles === []) {
    fwrite(
        STDOUT,
        "PASS — {$totalAssertions} assertion(s) across {$totalFiles} file(s) "
        . "in {$elapsedMs} ms\n"
    );
    exit(0);
}

fwrite(STDOUT, "FAIL — " . count($failedFiles) . " file(s) failed:\n");
foreach ($failedFiles as $f) {
    fwrite(STDOUT, "  - {$f}\n");
}
fwrite(
    STDOUT,
    "({$failedAssertions} failed assertion(s) of {$totalAssertions} "
    . "across {$totalFiles} file(s) in {$elapsedMs} ms)\n"
);
exit(1);

/**
 * Recursively collect `*_test.php` files under $dir.
 *
 * @return list<string>
 */
function collect_test_files(string $dir): array
{
    $found = [];
    $iterator = new RecursiveIteratorIterator(
        new RecursiveDirectoryIterator($dir, FilesystemIterator::SKIP_DOTS)
    );
    foreach ($iterator as $info) {
        if ($info->isFile() && str_ends_with($info->getFilename(), '_test.php')) {
            $found[] = $info->getPathname();
        }
    }
    return $found;
}

/**
 * Strip a single `## phptv-test-summary {json}` line from stdout (the
 * one report_done() emits) and return [$summary, $remaining_stdout].
 *
 * @return array{0: array{total:int,failed:int}|array<string,int>, 1: string}
 */
function parse_summary(string $stdout): array
{
    $lines = preg_split('/\R/', $stdout) ?: [];
    $remaining = [];
    $summary = ['total' => 0, 'failed' => 0];
    foreach ($lines as $line) {
        if (str_starts_with($line, '## phptv-test-summary ')) {
            $json = substr($line, strlen('## phptv-test-summary '));
            $decoded = json_decode($json, true);
            if (is_array($decoded)) {
                $summary = $decoded + $summary;
            }
            continue;
        }
        $remaining[] = $line;
    }
    $body = implode("\n", $remaining);
    if ($body !== '' && !str_ends_with($body, "\n")) {
        $body .= "\n";
    }
    return [$summary, $body];
}

/**
 * Prefix every line of a block with $prefix. Empty trailing line is
 * preserved.
 */
function indent_block(string $text, string $prefix): string
{
    $lines = preg_split('/\R/', $text) ?: [];
    return implode("\n", array_map(fn($l) => $prefix . $l, $lines));
}
