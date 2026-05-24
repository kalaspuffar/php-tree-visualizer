// Static-correctness sweeps over viz/*.html and viz/js/*.js.
//
//  9.1  No inline <script> blocks (every <script> has src).
//       No inline <style> blocks.
//  9.2  Every HTML carries the documented CSP meta tag.
//  9.3  viz/js/list.js never issues a write method (no 'POST',
//       'PUT', 'PATCH', 'DELETE' string literal).
//  9.4  viz/js/login.js and viz/js/list.js never log token-shaped
//       bytes via console.* (`token`, `password`, `body`).

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { assert_eq, assert_true, assert_contains, report_done } from "./lib/assert.js";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const repoRoot = path.resolve(__dirname, "..", "..");
const vizDir = path.join(repoRoot, "viz");

function read(file) {
    return fs.readFileSync(file, "utf-8");
}

function listHtmlFiles(dir) {
    return fs.readdirSync(dir)
        .filter((n) => n.endsWith(".html"))
        .map((n) => path.join(dir, n));
}

const htmlFiles = listHtmlFiles(vizDir).sort();
assert_true(htmlFiles.length >= 2, "found login.html + index.html");

// 9.1 — no inline script content, no inline style blocks.
const inlineScript = /<script(?:\s+[^>]*)?>\s*\S/;
const inlineScriptWithoutSrc = /<script\b(?![^>]*\bsrc=)/;
const inlineStyleBlock = /<style\b/;

for (const file of htmlFiles) {
    const html = read(file);
    const rel = path.relative(repoRoot, file);

    // Every <script> tag must have a `src` attribute. The regex
    // catches any <script> that doesn't.
    assert_eq(null, html.match(inlineScriptWithoutSrc), `${rel} has no inline <script>`);

    // No <style> block anywhere.
    assert_eq(null, html.match(inlineStyleBlock), `${rel} has no inline <style> block`);
}

// 9.2 — CSP meta on every HTML. Phase 6b adds style-src-attr.
const requiredDirectives = [
    "default-src 'self'",
    "script-src 'self'",
    "style-src 'self'",
    "style-src-attr 'unsafe-inline'",
    "connect-src 'self'",
    "object-src 'none'",
];
for (const file of htmlFiles) {
    const html = read(file);
    const rel = path.relative(repoRoot, file);

    assert_contains(html, "Content-Security-Policy", `${rel} carries CSP meta`);
    for (const directive of requiredDirectives) {
        assert_contains(html, directive, `${rel} CSP has ${directive}`);
    }
}

// 9.3 — read-only API surface. The entire read-only frontend (every
// file except login.js / api-client.js) must never issue write
// methods. Phase 5 covered list.js; Phase 6a added detail.js +
// virtualizer.js + tree-row.js; Phase 6b adds search.js,
// tooltip.js, keyboard.js.
const readOnlyClientFiles = [
    "list.js", "detail.js", "virtualizer.js", "tree-row.js",
    "search.js", "tooltip.js", "keyboard.js",
];
for (const fname of readOnlyClientFiles) {
    const src = read(path.join(vizDir, "js", fname));
    for (const method of ["POST", "PUT", "PATCH", "DELETE"]) {
        const needle = `'${method}'`;
        assert_eq(false, src.includes(needle),
            `${fname} does not contain literal ${needle}`);
        const dq = `"${method}"`;
        assert_eq(false, src.includes(dq),
            `${fname} does not contain literal ${dq}`);
    }
}

// 9.4 — no JS file logs secret-shaped values. Same heuristic as
// Phase 5, extended to detail.js / virtualizer.js / tree-row.js.
const consoleCall = /console\.\w+\s*\([^)]*\b(token|password|body|request)\b[^)]*\)/;

for (const fname of [
    "login.js", "list.js", "detail.js", "virtualizer.js", "tree-row.js",
    "search.js", "tooltip.js", "keyboard.js",
]) {
    const src = read(path.join(vizDir, "js", fname));
    assert_eq(null, src.match(consoleCall),
        `${fname} does not console.log token/password/body`);
}

// 9.4 (extension) — login.js does not write the token to
// localStorage or sessionStorage (Open Question 1: default is
// no client-side persistence).
const loginSrc = read(path.join(vizDir, "js", "login.js"));
assert_eq(false, loginSrc.includes("localStorage"),
    "login.js does not use localStorage");
assert_eq(false, loginSrc.includes("sessionStorage"),
    "login.js does not use sessionStorage");
assert_eq(false, loginSrc.includes("document.cookie"),
    "login.js does not touch document.cookie");

for (const fname of [
    "list.js", "detail.js", "virtualizer.js", "tree-row.js",
    "search.js", "tooltip.js", "keyboard.js",
]) {
    const src = read(path.join(vizDir, "js", fname));
    assert_eq(false, src.includes("document.cookie"),
        `${fname} does not touch document.cookie`);
    assert_eq(false, src.includes("localStorage"),
        `${fname} does not use localStorage`);
    assert_eq(false, src.includes("sessionStorage"),
        `${fname} does not use sessionStorage`);
}

report_done();
