// Node test runner — discovers *_test.js under tests/frontend/ and
// runs each in a child Node process so a fatal in one test can't
// kill the run. Mirrors tests/api/run.php.
//
// Invoke from the repo root: `node tests/frontend/run.js`.
// Exit 0 on full pass, 1 on any failure.

import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const rootDir = path.resolve(__dirname, "..", "..");

function collectTestFiles(dir) {
    const found = [];
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
        const full = path.join(dir, entry.name);
        if (entry.isDirectory()) {
            found.push(...collectTestFiles(full));
        } else if (entry.isFile() && entry.name.endsWith("_test.js")) {
            found.push(full);
        }
    }
    return found;
}

const files = collectTestFiles(__dirname).sort();

if (files.length === 0) {
    process.stderr.write("no test files found under tests/frontend/\n");
    process.exit(1);
}

let totalAssertions = 0;
let failedAssertions = 0;
const failedFiles = [];
const startedAt = Date.now();

for (const file of files) {
    const rel = path.relative(rootDir, file);
    process.stdout.write(`→ ${rel}\n`);

    const res = spawnSync(process.execPath, [file], {
        cwd: rootDir,
        encoding: "utf-8",
    });

    const lines = (res.stdout || "").split(/\r?\n/);
    const remaining = [];
    let summary = { total: 0, failed: 0 };
    for (const line of lines) {
        if (line.startsWith("## phptv-test-summary ")) {
            try {
                summary = { ...summary, ...JSON.parse(line.slice("## phptv-test-summary ".length)) };
            } catch {
                /* keep defaults */
            }
        } else {
            remaining.push(line);
        }
    }
    totalAssertions += summary.total || 0;
    failedAssertions += summary.failed || 0;

    const body = remaining.join("\n");
    if (body.trim().length > 0) {
        process.stdout.write(body);
        if (!body.endsWith("\n")) process.stdout.write("\n");
    }

    const passed = res.status === 0 && (summary.failed || 0) === 0;
    if (passed) {
        process.stdout.write(`  ✓ ${summary.total} assertion(s)\n`);
    } else {
        process.stdout.write(`  ✗ failed (exit=${res.status})\n`);
        if (res.stderr) {
            for (const l of res.stderr.split(/\r?\n/)) {
                if (l.length) process.stdout.write(`    │ ${l}\n`);
            }
        }
        failedFiles.push(rel);
    }
}

const elapsedMs = Date.now() - startedAt;
process.stdout.write("\n");
if (failedFiles.length === 0) {
    process.stdout.write(
        `PASS — ${totalAssertions} assertion(s) across ${files.length} file(s) in ${elapsedMs} ms\n`
    );
    process.exit(0);
}
process.stdout.write(`FAIL — ${failedFiles.length} file(s) failed:\n`);
for (const f of failedFiles) process.stdout.write(`  - ${f}\n`);
process.stdout.write(
    `(${failedAssertions} failed assertion(s) of ${totalAssertions} across ${files.length} file(s) in ${elapsedMs} ms)\n`
);
process.exit(1);
