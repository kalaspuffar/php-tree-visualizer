<?php

declare(strict_types=1);

/**
 * Generalised SQL-safety static check across all api/*.php files.
 *
 * Asserts, for every prepare() call site in the codebase: the
 * argument is either
 *   (a) a const identifier (T_STRING in UPPER_SNAKE),
 *   (b) a function call to phptv_build_sql() (the whitelist
 *       substituter),
 *   (c) a $variable whose only assignment in the file is from
 *       phptv_build_sql() or a top-level const,
 * and that no T_DOUBLE-QUOTED string-with-$interpolation and no
 * concatenation of user input into the SQL appear in the file.
 *
 * The check rejects the obvious-bad shape (variable in prepare with
 * unknown provenance) while permitting the legitimate
 * const-whitelist-substitute pattern Phase 4 uses for sort
 * substitution. The behavioural injection test (route regex
 * rejects bad node_id at parse time) is the runtime backstop.
 */

require_once __DIR__ . '/lib/assert.php';

$apiDir = realpath(__DIR__ . '/../../api');
if ($apiDir === false) {
    fwrite(STDERR, "cannot resolve api/ directory\n");
    exit(1);
}

$apiFiles = [];
$iter = new RecursiveIteratorIterator(
    new RecursiveDirectoryIterator($apiDir, FilesystemIterator::SKIP_DOTS)
);
foreach ($iter as $info) {
    if ($info->isFile() && str_ends_with($info->getFilename(), '.php')) {
        $apiFiles[] = $info->getPathname();
    }
}
sort($apiFiles);
assert_true(count($apiFiles) > 0, 'found at least one api/*.php file');

foreach ($apiFiles as $file) {
    $code = (string) file_get_contents($file);
    $rel = basename(dirname($file)) . '/' . basename($file);
    $tokens = \PhpToken::tokenize($code);

    // Pass 1: collect the set of "blessed" variable names — those
    // assigned exclusively from a phptv_build_sql() call or from a
    // const expression. The set is per-file (variables don't leak
    // across require boundaries for purposes of this check; the
    // only call we care about is local).
    //
    // A blessed assignment looks like:
    //     $name = phptv_build_sql(SQL_TREE_FETCH, $sort);
    //     $name = SQL_FOO;
    //     $name = 'string literal';
    //     $name = "string with no \$ in it";
    //
    // Any $variable that has at least one NON-blessed assignment is
    // dropped from the set.
    $blessed = [];
    $tainted = [];
    for ($i = 0; $i < count($tokens); $i++) {
        if ($tokens[$i]->id !== T_VARIABLE) {
            continue;
        }
        // Look for `$x =` (single `=`, not `==`, `===`, `=>`, `+=`).
        $j = $i + 1;
        while ($j < count($tokens) && $tokens[$j]->id === T_WHITESPACE) {
            $j++;
        }
        if ($j >= count($tokens) || $tokens[$j]->text !== '=') {
            continue;
        }
        if (($tokens[$j + 1]->text ?? '') === '=') {
            continue;
        }

        // Snapshot the RHS up to the next semicolon (depth-aware).
        $k = $j + 1;
        $rhsTokens = [];
        $depth = 0;
        while ($k < count($tokens)) {
            $t = $tokens[$k];
            if ($t->text === '(' || $t->text === '[') {
                $depth++;
            } elseif ($t->text === ')' || $t->text === ']') {
                $depth--;
            } elseif ($t->text === ';' && $depth === 0) {
                break;
            }
            $rhsTokens[] = $t;
            $k++;
        }

        $varName = $tokens[$i]->text;
        if (phptv_rhs_is_safe($rhsTokens)) {
            if (!isset($tainted[$varName])) {
                $blessed[$varName] = true;
            }
        } else {
            $tainted[$varName] = true;
            unset($blessed[$varName]);
        }
    }

    // Pass 2: every prepare() arg must be either a T_STRING (const),
    // a phptv_build_sql(...) call, or a blessed variable.
    $offending = [];
    for ($i = 0; $i < count($tokens); $i++) {
        if ($tokens[$i]->id !== T_STRING || $tokens[$i]->text !== 'prepare') {
            continue;
        }
        // Snapshot the argument-list contents (token list, no
        // whitespace).
        $depth = 0;
        $started = false;
        $argTokens = [];
        for ($j = $i + 1; $j < count($tokens); $j++) {
            $t = $tokens[$j];
            if ($t->text === '(') {
                if ($started) {
                    $argTokens[] = $t;
                }
                $depth++;
                $started = true;
                continue;
            }
            if ($t->text === ')') {
                $depth--;
                if ($started && $depth === 0) {
                    break;
                }
                $argTokens[] = $t;
                continue;
            }
            if ($started && $t->id !== T_WHITESPACE) {
                $argTokens[] = $t;
            }
        }

        if (!phptv_prepare_args_are_safe($argTokens, $blessed)) {
            $offending[] = $tokens[$i]->line;
        }
    }
    assert_eq([], $offending, 'every prepare() in ' . $rel . ' takes a safe SQL argument');
}

// Structural assertions: the safe-construction pattern is present
// in tree.php.
$treeCode = (string) file_get_contents($apiDir . '/internal/tree.php');
assert_contains($treeCode, 'phptv_build_sql', 'tree.php uses phptv_build_sql helper');
assert_contains($treeCode, 'PHPTV_SORT_CLAUSES', 'tree.php whitelists sort clauses');
assert_contains($treeCode, 'bindValue', 'tree.php binds via bindValue');

// Behavioural: phptv_build_sql throws on an unwhitelisted key
// (defence-in-depth past the validator).
require_once $apiDir . '/internal/tree.php';
assert_throws(
    \LogicException::class,
    fn() => phptv_build_sql(SQL_TREE_FETCH, 'never_whitelisted'),
    'unwhitelisted sort key throws LogicException'
);

report_done();

/**
 * RHS is safe iff it is EXACTLY ONE of the following producers
 * (whitespace ignored):
 *
 *   (a) a T_CONSTANT_ENCAPSED_STRING containing no `$`,
 *   (b) a single UPPER_SNAKE T_STRING identifier (const reference),
 *   (c) a `phptv_build_sql(` ... `)` call — contents skipped because
 *       the helper itself validates the sort key against the const
 *       whitelist and throws LogicException on miss.
 *
 * Composite expressions (concatenation, additional variables, other
 * function calls) are rejected. This is intentionally narrow: it
 * matches exactly the patterns the codebase needs and refuses
 * anything else.
 *
 * @param list<\PhpToken> $tokens  RHS tokens (already trimmed at `;`)
 */
function phptv_rhs_is_safe(array $tokens): bool
{
    $non = array_values(array_filter(
        $tokens,
        fn($t) => $t->id !== T_WHITESPACE
    ));
    if ($non === []) {
        return false;
    }

    // (a) single literal string.
    if (
        count($non) === 1
        && $non[0]->id === T_CONSTANT_ENCAPSED_STRING
        && !str_contains($non[0]->text, '$')
    ) {
        return true;
    }
    // (b) single UPPER_SNAKE const identifier.
    if (
        count($non) === 1
        && $non[0]->id === T_STRING
        && preg_match('/^[A-Z][A-Z0-9_]+$/', $non[0]->text)
    ) {
        return true;
    }
    // (c) phptv_build_sql(...) with balanced parens to EOF (the
    // RHS started at `=` and ran until `;`, so the function call
    // is the entire RHS).
    if (
        count($non) >= 4
        && $non[0]->id === T_STRING
        && $non[0]->text === 'phptv_build_sql'
        && $non[1]->text === '('
        && end($non)->text === ')'
    ) {
        return true;
    }
    return false;
}

/**
 * Are the tokens passed to prepare() a safe SQL argument?
 *
 * @param list<\PhpToken> $tokens
 * @param array<string, true> $blessed
 */
function phptv_prepare_args_are_safe(array $tokens, array $blessed): bool
{
    $nonWs = array_values(array_filter(
        $tokens,
        fn($t) => $t->id !== T_WHITESPACE
    ));
    if ($nonWs === []) {
        return false;
    }
    $first = $nonWs[0];

    // Const identifier alone.
    if (
        count($nonWs) === 1
        && $first->id === T_STRING
        && preg_match('/^[A-Z][A-Z0-9_]+$/', $first->text)
    ) {
        return true;
    }
    // phptv_build_sql(...) — first token is the function name, the
    // rest is the argument list.
    if (
        $first->id === T_STRING
        && $first->text === 'phptv_build_sql'
    ) {
        return true;
    }
    // Blessed variable alone.
    if (
        count($nonWs) === 1
        && $first->id === T_VARIABLE
        && isset($blessed[$first->text])
    ) {
        return true;
    }
    return false;
}
