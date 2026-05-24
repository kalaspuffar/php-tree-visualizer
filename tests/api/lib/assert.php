<?php

declare(strict_types=1);

/**
 * Minimal assertion helpers used by every *_test.php file.
 *
 * Each assertion appends to a process-local counter pair. The test
 * file ends with `report_done()` which emits the
 * `## phptv-test-summary {...}` line the runner parses.
 *
 * On failure, prints a diagnostic to stdout AND throws an
 * AssertionError so any uncaught throw also fails the file.
 */

if (!isset($GLOBALS['__phptv_assert_total'])) {
    $GLOBALS['__phptv_assert_total'] = 0;
    $GLOBALS['__phptv_assert_failed'] = 0;
}

function assert_eq(mixed $expected, mixed $actual, string $label = ''): void
{
    $GLOBALS['__phptv_assert_total']++;
    if ($expected === $actual) {
        return;
    }
    $GLOBALS['__phptv_assert_failed']++;
    $where = caller_location();
    $exp = var_export($expected, true);
    $got = var_export($actual, true);
    fwrite(
        STDOUT,
        "    ✗ assert_eq {$label} at {$where}\n"
        . "        expected: {$exp}\n"
        . "        actual:   {$got}\n"
    );
}

function assert_true(bool $cond, string $label = ''): void
{
    $GLOBALS['__phptv_assert_total']++;
    if ($cond === true) {
        return;
    }
    $GLOBALS['__phptv_assert_failed']++;
    $where = caller_location();
    fwrite(STDOUT, "    ✗ assert_true {$label} at {$where}\n");
}

function assert_false(bool $cond, string $label = ''): void
{
    assert_true(!$cond, $label);
}

function assert_contains(string $haystack, string $needle, string $label = ''): void
{
    $GLOBALS['__phptv_assert_total']++;
    if (str_contains($haystack, $needle)) {
        return;
    }
    $GLOBALS['__phptv_assert_failed']++;
    $where = caller_location();
    fwrite(
        STDOUT,
        "    ✗ assert_contains {$label} at {$where}\n"
        . "        needle: " . var_export($needle, true) . "\n"
        . "        haystack: " . substr(var_export($haystack, true), 0, 400) . "\n"
    );
}

function assert_not_contains(string $haystack, string $needle, string $label = ''): void
{
    $GLOBALS['__phptv_assert_total']++;
    if (!str_contains($haystack, $needle)) {
        return;
    }
    $GLOBALS['__phptv_assert_failed']++;
    $where = caller_location();
    fwrite(
        STDOUT,
        "    ✗ assert_not_contains {$label} at {$where}\n"
        . "        needle: " . var_export($needle, true) . "\n"
        . "        haystack: " . substr(var_export($haystack, true), 0, 400) . "\n"
    );
}

/**
 * Assert that $callable throws something matching $expectedClass
 * (or any Throwable if null). Returns the caught exception so tests
 * can inspect it further.
 */
function assert_throws(?string $expectedClass, callable $callable, string $label = ''): ?\Throwable
{
    $GLOBALS['__phptv_assert_total']++;
    try {
        $callable();
    } catch (\Throwable $t) {
        if ($expectedClass === null || $t instanceof $expectedClass) {
            return $t;
        }
        $GLOBALS['__phptv_assert_failed']++;
        $where = caller_location();
        $actualClass = $t::class;
        fwrite(
            STDOUT,
            "    ✗ assert_throws {$label} at {$where}\n"
            . "        expected class: {$expectedClass}\n"
            . "        got class:      {$actualClass}\n"
            . "        message:        {$t->getMessage()}\n"
        );
        return $t;
    }
    $GLOBALS['__phptv_assert_failed']++;
    $where = caller_location();
    fwrite(
        STDOUT,
        "    ✗ assert_throws {$label} at {$where}\n"
        . "        expected an exception of " . ($expectedClass ?? '<any>') . "\n"
        . "        got: no throw\n"
    );
    return null;
}

function caller_location(): string
{
    $bt = debug_backtrace(DEBUG_BACKTRACE_IGNORE_ARGS, 3);
    $frame = $bt[2] ?? $bt[1] ?? ['file' => '?', 'line' => 0];
    $file = $frame['file'] ?? '?';
    $line = $frame['line'] ?? 0;
    return basename($file) . ':' . $line;
}

function report_done(): void
{
    $total = $GLOBALS['__phptv_assert_total'] ?? 0;
    $failed = $GLOBALS['__phptv_assert_failed'] ?? 0;
    fwrite(STDOUT, "## phptv-test-summary " . json_encode([
        'total' => $total,
        'failed' => $failed,
    ]) . "\n");
    if ($failed > 0) {
        exit(1);
    }
}
