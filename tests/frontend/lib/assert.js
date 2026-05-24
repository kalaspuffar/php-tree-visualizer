// Minimal assertion helpers for the Node-based frontend tests.
//
// Each test file imports these, exercises pure functions from the
// viz/js modules, and ends with report_done() — same shape as
// tests/api/lib/assert.php.
//
// On failure, prints a diagnostic and increments the failure
// counter. report_done() emits the `## phptv-test-summary {…}`
// line the runner parses, mirroring the PHP harness.

const state = {
    total: 0,
    failed: 0,
};

function caller() {
    const stack = new Error().stack || "";
    const lines = stack.split("\n");
    // 0: Error, 1: caller(), 2: assert_*, 3: test code.
    const frame = lines[3] || lines[2] || "";
    const m = frame.match(/\(?([^()]+?):(\d+):\d+\)?$/);
    if (!m) return "<unknown>";
    const file = m[1].split("/").pop();
    return `${file}:${m[2]}`;
}

function fmt(v) {
    if (typeof v === "string") return JSON.stringify(v);
    try {
        return JSON.stringify(v);
    } catch {
        return String(v);
    }
}

export function assert_eq(expected, actual, label = "") {
    state.total++;
    const equal =
        expected === actual ||
        (expected !== expected && actual !== actual) || // NaN
        deepEqual(expected, actual);
    if (equal) return;
    state.failed++;
    process.stdout.write(
        `    ✗ assert_eq ${label} at ${caller()}\n` +
        `        expected: ${fmt(expected)}\n` +
        `        actual:   ${fmt(actual)}\n`
    );
}

export function assert_true(cond, label = "") {
    state.total++;
    if (cond === true) return;
    state.failed++;
    process.stdout.write(`    ✗ assert_true ${label} at ${caller()}\n`);
}

export function assert_false(cond, label = "") {
    assert_true(!cond, label);
}

export function assert_contains(haystack, needle, label = "") {
    state.total++;
    if (typeof haystack === "string" && haystack.includes(needle)) return;
    state.failed++;
    process.stdout.write(
        `    ✗ assert_contains ${label} at ${caller()}\n` +
        `        needle: ${fmt(needle)}\n` +
        `        haystack: ${fmt((haystack ?? "").toString().slice(0, 400))}\n`
    );
}

export async function assert_throws(expectedClass, fn, label = "") {
    state.total++;
    try {
        const r = fn();
        if (r && typeof r.then === "function") {
            await r;
        }
    } catch (e) {
        if (expectedClass === null || e instanceof expectedClass) {
            return e;
        }
        state.failed++;
        process.stdout.write(
            `    ✗ assert_throws ${label} at ${caller()}\n` +
            `        expected class: ${expectedClass?.name ?? expectedClass}\n` +
            `        got class:      ${e?.constructor?.name ?? typeof e}\n` +
            `        message:        ${e?.message ?? ""}\n`
        );
        return e;
    }
    state.failed++;
    process.stdout.write(
        `    ✗ assert_throws ${label} at ${caller()}\n` +
        `        expected an exception of ${expectedClass?.name ?? "<any>"}\n` +
        `        got: no throw\n`
    );
    return null;
}

function deepEqual(a, b) {
    if (a === b) return true;
    if (typeof a !== "object" || typeof b !== "object") return false;
    if (a === null || b === null) return false;
    if (Array.isArray(a) !== Array.isArray(b)) return false;
    if (Array.isArray(a)) {
        if (a.length !== b.length) return false;
        for (let i = 0; i < a.length; i++) {
            if (!deepEqual(a[i], b[i])) return false;
        }
        return true;
    }
    const aKeys = Object.keys(a);
    const bKeys = Object.keys(b);
    if (aKeys.length !== bKeys.length) return false;
    for (const k of aKeys) {
        if (!deepEqual(a[k], b[k])) return false;
    }
    return true;
}

export function report_done() {
    process.stdout.write(
        `## phptv-test-summary ${JSON.stringify(state)}\n`
    );
    if (state.failed > 0) {
        process.exit(1);
    }
}
