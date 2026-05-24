// Tests for viz/js/tree-row.js.
//
// Uses a tiny pure-JS DOM stub so the row builder can run under
// Node without jsdom. We only test the DOM shape (attributes, child
// structure, class names), not browser-rendered geometry.

import { buildTreeRow, _internals } from "../../viz/js/tree-row.js";
import {
    assert_eq,
    assert_true,
    assert_false,
    assert_contains,
    report_done,
} from "./lib/assert.js";

// ---- Minimal DOM shim ---------------------------------------------

class FakeNode {
    constructor(tag, namespace) {
        this.tagName = tag.toUpperCase();
        this.namespace = namespace;
        this.attributes = {};
        this.classList = new ClassList();
        this.children = [];
        this.textContent = "";
        this._title = "";
        this._tabIndex = 0;
        this._hidden = false;
    }
    setAttribute(name, value) {
        this.attributes[name] = String(value);
        // SVG elements set class via setAttribute; mirror to
        // classList so `className` / classList.contains() match.
        if (name === "class") {
            this.classList._set(value);
        }
    }
    getAttribute(name) { return this.attributes[name] ?? null; }
    hasAttribute(name) { return name in this.attributes; }
    appendChild(child) { this.children.push(child); child.parent = this; return child; }
    addEventListener(name, fn) {
        this._listeners = this._listeners || {};
        (this._listeners[name] = this._listeners[name] || []).push(fn);
    }
    fire(name, event) {
        const ls = this._listeners?.[name] || [];
        for (const fn of ls) fn(event);
    }
    get className() { return this.classList.toString(); }
    set className(v) { this.classList._set(v); }
    get title() { return this._title; }
    set title(v) { this._title = v; }
    get tabIndex() { return this._tabIndex; }
    set tabIndex(v) { this._tabIndex = v; }
    get hidden() { return this._hidden; }
    set hidden(v) { this._hidden = v; }
    get type() { return this.attributes.type; }
    set type(v) { this.attributes.type = v; }
    findOne(predicate) {
        for (const child of this.children) {
            if (predicate(child)) return child;
            const found = child.findOne(predicate);
            if (found) return found;
        }
        return null;
    }
    findAll(predicate) {
        const out = [];
        for (const child of this.children) {
            if (predicate(child)) out.push(child);
            out.push(...child.findAll(predicate));
        }
        return out;
    }
}
class ClassList {
    constructor() { this.value = ""; }
    _set(v) { this.value = String(v); }
    add(...names) { this.value = [this.value || "", ...names].filter(Boolean).join(" "); }
    contains(name) { return (this.value || "").split(/\s+/).includes(name); }
    toString() { return this.value || ""; }
}
globalThis.document = {
    createElement: (tag) => new FakeNode(tag, null),
    createElementNS: (ns, tag) => new FakeNode(tag, ns),
};

// ---- Tests ---------------------------------------------------------

const baseRow = {
    nodeId: 42,
    parentNodeId: 1,
    depth: 1,
    fqn: "Foo\\Bar::baz",
    file: "src/Foo/Bar.php",
    line: 88,
    kind: 1,
    count: 1_234,
    totalWallNs: 1_543_210_000,
    selfWallNs: 543_210_000,
    totalCpuUNs: 0,
    totalCpuSNs: 0,
    totalMemDeltaBytes: 1024,
    abnormalExitCount: 0,
    anomalyCount: 0,
    hasChildren: true,
    childrenLoaded: false,
    expanded: false,
    parentTotalWallNs: 3_000_000_000,
};

// Regular row, collapsed, with children.
{
    const li = buildTreeRow(baseRow, {
        indentDepthForUi: 1, posInSet: 2, setSize: 3,
    });
    assert_eq("LI", li.tagName, "<li>");
    assert_eq("treeitem", li.getAttribute("role"), "role=treeitem");
    assert_eq("42", li.getAttribute("data-node-id"), "data-node-id");
    assert_eq("2", li.getAttribute("aria-level"), "aria-level = 1 + indentDepthForUi");
    assert_eq("false", li.getAttribute("aria-expanded"), "aria-expanded=false (collapsed)");
    assert_eq("2", li.getAttribute("aria-posinset"), "aria-posinset");
    assert_eq("3", li.getAttribute("aria-setsize"), "aria-setsize");
    assert_true(li.classList.contains("tree-row--indent-1"), "indent bucket 1");
    assert_true(li.classList.contains("tree-row"), "tree-row class");
    assert_false(li.classList.contains("tree-row--unresolved"), "not unresolved");
    assert_eq("Foo\\Bar::baz", li.title, "title carries full fqn");

    // Function cell: indent + chevron (right) + fqn + file:line + anomaly slot
    const fnCell = li.children[0];
    const chevron = fnCell.findOne((c) => c.className === "tree-row__chevron");
    assert_true(chevron !== null, "chevron present");
    assert_false(!!chevron.hidden, "chevron visible for has_children=true");
    assert_eq("Expand children", chevron.getAttribute("aria-label"), "chevron aria-label collapsed");

    const fqn = fnCell.findOne((c) => c.className === "tree-row__fqn");
    assert_eq("Foo\\Bar::baz", fqn.textContent, "fqn text");

    const fileLine = fnCell.findOne((c) => c.className === "tree-row__file-line");
    assert_eq("src/Foo/Bar.php:88", fileLine.textContent, "file:line");

    // No badge for kind=1.
    assert_eq(null, fnCell.findOne((c) => c.className === "fn-badge--int"), "no [int] badge");

    // Anomaly slot present but empty.
    const slot = fnCell.findOne((c) => c.className === "tree-row__anomaly-slot");
    assert_true(slot !== null, "anomaly slot exists");
    assert_eq(0, slot.children.length, "anomaly slot empty for zero counts");

    // Numeric cells: count, total, self, %parent, mem.
    assert_contains(li.children[1].textContent, "1,234", "count formatted");
    assert_contains(li.children[2].textContent, "1.54 s", "total wall");
    assert_contains(li.children[3].textContent, "543 ms", "self wall");

    // %parent: 1.543e9 / 3e9 = ~51.4 → 51.4% → 50 bucket
    const pctCell = li.children[4];
    const fill = pctCell.findOne((c) => (c.className || "").includes("hot-bar__fill"));
    assert_true(
        fill.className.includes("hot-bar__fill--50"),
        "hot-bar bucket 50 for ~51.4%"
    );
    const pctLabel = pctCell.findOne((c) => c.className && c.className.includes("tree-row__pct-text"));
    assert_eq("51%", pctLabel.textContent, "percent label");

    // Mem: +1.00 KB, positive tone.
    const memCell = li.children[5];
    assert_eq("+1.00 KB", memCell.textContent, "mem text");
    assert_false(memCell.classList.contains("tree-row__cell--danger"), "positive tone (no danger)");
}

// Leaf row: no children → chevron hidden, no aria-expanded.
{
    const li = buildTreeRow(
        { ...baseRow, nodeId: 7, hasChildren: false, count: 1 },
        { indentDepthForUi: 2 }
    );
    assert_false(li.hasAttribute("aria-expanded"), "leaf has no aria-expanded");
    const chevron = li.findOne((c) => c.className === "tree-row__chevron");
    assert_true(chevron.hidden === true, "leaf chevron hidden");
}

// Expanded row.
{
    const li = buildTreeRow(
        { ...baseRow, expanded: true, childrenLoaded: true },
        { indentDepthForUi: 0 }
    );
    assert_eq("true", li.getAttribute("aria-expanded"), "expanded -> aria-expanded=true");
    const chevron = li.findOne((c) => c.className === "tree-row__chevron");
    const use = chevron.findOne((c) => c.tagName === "USE");
    assert_contains(use.getAttribute("href"), "icon-chevron-down", "chevron-down icon");
}

// Loading state.
{
    const li = buildTreeRow(
        { ...baseRow, expanded: true, loadingChildren: true },
        { indentDepthForUi: 0 }
    );
    assert_true(li.classList.contains("tree-row--loading"), "loading class");
    const use = li.findOne((c) => c.tagName === "USE");
    assert_contains(use.getAttribute("href"), "icon-loader", "loader icon");
}

// Load-error state.
{
    const li = buildTreeRow(
        { ...baseRow, expanded: true, loadError: true },
        { indentDepthForUi: 0 }
    );
    assert_true(li.classList.contains("tree-row--load-error"), "load-error class");
    const chevron = li.findOne((c) => c.className === "tree-row__chevron");
    assert_eq("Retry loading children", chevron.getAttribute("aria-label"), "chevron aria-label retry");
    assert_contains(chevron.title, "Could not load children", "chevron title retry copy");
}

// Internal function (kind=3).
{
    const li = buildTreeRow(
        { ...baseRow, kind: 3, fqn: "array_map", file: "", line: 0,
          hasChildren: false },
        { indentDepthForUi: 1 }
    );
    const fnCell = li.children[0];
    const badge = fnCell.findOne((c) => c.className === "fn-badge--int");
    assert_true(badge !== null, "internal-function badge present");
    assert_eq("[int]", badge.textContent, "[int] text");
    assert_eq("Internal function (PHP core)", badge.title, "badge title");
    // file:line cell suppressed for internal functions.
    assert_eq(null, fnCell.findOne((c) => c.className === "tree-row__file-line"),
        "file:line cell suppressed for internal");
    assert_eq("Internal function (PHP core)", li.title, "row title is internal-function copy");
}

// Closure (kind=2): redundant file:line.
{
    const li = buildTreeRow(
        { ...baseRow, kind: 2, fqn: "closure:src/X.php:42",
          file: "src/X.php", line: 42 },
        { indentDepthForUi: 1 }
    );
    const fnCell = li.children[0];
    const fqn = fnCell.findOne((c) => c.className === "tree-row__fqn");
    assert_eq("closure:src/X.php:42", fqn.textContent, "fqn carries closure name");
    const fileLine = fnCell.findOne((c) => c.className === "tree-row__file-line");
    assert_eq("src/X.php:42", fileLine.textContent, "redundant file:line preserved");
}

// Abnormal exits → warn icon.
{
    const li = buildTreeRow(
        { ...baseRow, abnormalExitCount: 3 },
        { indentDepthForUi: 0 }
    );
    const slot = li.findOne((c) => c.className === "tree-row__anomaly-slot");
    const icon = slot.findOne((c) => c.className && c.className.includes("anomaly-icon--warn"));
    assert_true(icon !== null, "abnormal-exit icon present");
    const titleEl = icon.findOne((c) => c.tagName === "TITLE");
    assert_contains(titleEl.textContent, "3 call(s) exited abnormally", "abnormal-exit tooltip");
}

// Data anomalies → danger icon.
{
    const li = buildTreeRow(
        { ...baseRow, anomalyCount: 2 },
        { indentDepthForUi: 0 }
    );
    const slot = li.findOne((c) => c.className === "tree-row__anomaly-slot");
    const icon = slot.findOne((c) => c.className && c.className.includes("anomaly-icon--danger"));
    assert_true(icon !== null, "data-anomaly icon present");
}

// Both anomaly kinds.
{
    const li = buildTreeRow(
        { ...baseRow, abnormalExitCount: 1, anomalyCount: 1 },
        { indentDepthForUi: 0 }
    );
    const slot = li.findOne((c) => c.className === "tree-row__anomaly-slot");
    assert_eq(2, slot.children.length, "two anomaly icons stacked");
}

// Unresolved-fn (D-12).
{
    const li = buildTreeRow(
        {
            ...baseRow,
            fqn: "unresolved fn_id 17",
            hasChildren: false,
        },
        { indentDepthForUi: 1 }
    );
    assert_true(li.classList.contains("tree-row--unresolved"), "unresolved class");
}

// Mem-delta tones.
{
    // 2.5 binary-MB = 2.5 * 1024² = 2_621_440 bytes.
    const liNeg = buildTreeRow(
        { ...baseRow, totalMemDeltaBytes: -2_621_440, hasChildren: false },
        { indentDepthForUi: 0 }
    );
    const memNeg = liNeg.children[5];
    assert_eq("-2.50 MB", memNeg.textContent, "negative mem text");
    assert_true(memNeg.classList.contains("tree-row__cell--danger"), "negative tone class");

    const liZero = buildTreeRow(
        { ...baseRow, totalMemDeltaBytes: 0, hasChildren: false },
        { indentDepthForUi: 0 }
    );
    const memZero = liZero.children[5];
    assert_eq("±0 B", memZero.textContent, "zero mem text");
    assert_true(memZero.classList.contains("tree-row__cell--zero"), "zero tone class");
}

// Hot-path 0/50/100%.
{
    function pct(ownNs, parentNs) {
        const li = buildTreeRow(
            { ...baseRow, totalWallNs: ownNs, parentTotalWallNs: parentNs,
              hasChildren: false },
            { indentDepthForUi: 0 }
        );
        const fill = li.findOne(
            (c) => (c.className || "").includes("hot-bar__fill")
        );
        return fill.className;
    }
    assert_contains(pct(0, 1_000_000_000),           "hot-bar__fill--0",   "0%");
    assert_contains(pct(500_000_000, 1_000_000_000), "hot-bar__fill--50",  "50%");
    assert_contains(pct(1_000_000_000, 1_000_000_000), "hot-bar__fill--100", "100%");
    // > 100% (defensive) → clamps to 100% bucket
    assert_contains(pct(2_000_000_000, 1_000_000_000), "hot-bar__fill--100", "clamps at 100%");
}

// Internals export
assert_eq(0,   _internals.pctBucket(0),     "pctBucket 0");
assert_eq(5,   _internals.pctBucket(4),     "pctBucket round to 5 (from 4)");
// Math.round(47/5) = 9 (banker's rounding to nearest); 9*5 = 45.
// 47 is closer to 45 than to 50.
assert_eq(45,  _internals.pctBucket(47),    "pctBucket round 47 -> 45");
assert_eq(50,  _internals.pctBucket(48),    "pctBucket round 48 -> 50");
assert_eq(100, _internals.pctBucket(99),    "pctBucket clamp to 100");
assert_eq(0,   _internals.clampIndentBucket(-1), "clamp negative indent to 0");
assert_eq(20,  _internals.clampIndentBucket(25), "clamp deep indent to 20");
assert_eq(0,   _internals.computePctOfParent({ totalWallNs: 100, parentTotalWallNs: 0 }),
    "zero parent total -> 0% bucket");

// Chevron click invokes the callback.
{
    let received = null;
    const li = buildTreeRow(baseRow, {
        onChevronClick: (row) => { received = row.nodeId; },
        indentDepthForUi: 1,
    });
    const chevron = li.findOne((c) => c.className === "tree-row__chevron");
    chevron.fire("click", { stopPropagation: () => {} });
    assert_eq(42, received, "chevron click delivers row");
}

report_done();
