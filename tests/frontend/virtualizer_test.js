// Tests for viz/js/virtualizer.js.
//
// Uses a minimal viewport/spacer/rows-container stub that mimics the
// few DOM properties the virtualizer reads: clientHeight, scrollTop,
// firstChild/removeChild/appendChild, style. We drive scroll +
// resize events through the registered listeners directly.

import { Virtualizer } from "../../viz/js/virtualizer.js";
import {
    assert_eq,
    assert_true,
    report_done,
} from "./lib/assert.js";

class FakeStyle {
    constructor() { this.height = ""; this.transform = ""; }
}

class FakeElement {
    constructor() {
        this.style = new FakeStyle();
        this.children = [];
        this._listeners = {};
        this.clientHeight = 600;
        this.scrollTop = 0;
    }
    get firstChild() { return this.children[0] ?? null; }
    appendChild(node) { this.children.push(node); return node; }
    removeChild(node) {
        const idx = this.children.indexOf(node);
        if (idx >= 0) this.children.splice(idx, 1);
        return node;
    }
    addEventListener(name, fn) {
        (this._listeners[name] = this._listeners[name] || []).push(fn);
    }
    fire(name) {
        const ls = this._listeners[name] || [];
        for (const fn of ls) fn();
    }
}

const ROW_HEIGHT = 28;

function makeRows(n) {
    return Array.from({ length: n }, (_, i) => ({
        id: i + 1,
        label: `row-${i + 1}`,
    }));
}

function renderRow(row) {
    // The virtualizer just appends; we don't need real DOM.
    return { row };
}

// Stub a window.addEventListener so the resize handler register
// succeeds without a real DOM.
globalThis.window = globalThis.window || { addEventListener: () => {} };

// ---- setRows: visible window respects viewport + overscan ----------

{
    const viewport = new FakeElement();   // clientHeight=600 → 21 rows visible at 28 px
    const spacer = new FakeElement();
    const rowsContainer = new FakeElement();
    const v = new Virtualizer({
        viewport, spacer, rowsContainer,
        rowHeight: ROW_HEIGHT,
        overscan: 4,
        renderRow,
    });

    v.setRows(makeRows(100));

    // Spacer height = 100 * 28 = 2800.
    assert_eq("2800px", spacer.style.height, "spacer height after setRows");

    // Visible window from scrollTop=0:
    //   firstVisible = 0
    //   visibleCount = ceil(600/28) = 22
    //   iFirst = 0 - 4 → 0 (clamped)
    //   iLast  = 0 + 22 + 4 = 26
    assert_eq(26, rowsContainer.children.length, "26 rows rendered at top");
    assert_eq("translateY(0px)", rowsContainer.style.transform, "no transform offset at top");

    // First rendered row carries row #1.
    assert_eq(1, rowsContainer.children[0].row.id, "first row is #1");
    assert_eq(26, rowsContainer.children[25].row.id, "last row is #26");
}

// ---- scroll mid-list ------------------------------------------------

{
    const viewport = new FakeElement();
    const spacer = new FakeElement();
    const rowsContainer = new FakeElement();
    const v = new Virtualizer({
        viewport, spacer, rowsContainer,
        rowHeight: ROW_HEIGHT,
        overscan: 4,
        renderRow,
    });
    v.setRows(makeRows(100));

    viewport.scrollTop = 280;       // 10 rows down
    viewport.fire("scroll");

    // firstVisible = 10; iFirst = 10 - 4 = 6; iLast = 10 + 22 + 4 = 36.
    assert_eq(30, rowsContainer.children.length, "30 rows rendered after scroll");
    assert_eq(7,  rowsContainer.children[0].row.id, "first rendered row is #7 (1-indexed)");
    assert_eq("translateY(168px)", rowsContainer.style.transform, "transform shifts by iFirst*rowHeight");
}

// ---- insertRowsAt: spacer + visible window update ------------------

{
    const viewport = new FakeElement();
    const spacer = new FakeElement();
    const rowsContainer = new FakeElement();
    const v = new Virtualizer({
        viewport, spacer, rowsContainer,
        rowHeight: ROW_HEIGHT,
        overscan: 4,
        renderRow,
    });
    v.setRows(makeRows(100));

    v.insertRowsAt(5, [{ id: 999, label: "inserted" }, { id: 1000, label: "inserted2" }]);

    assert_eq(102, v.getRows().length, "row count grew by 2");
    assert_eq("2856px", spacer.style.height, "spacer reflects 102 rows");
    // Still at top → first rendered row IDs are 1, 2, 3, 4, 5, 999, 1000, 6, …
    assert_eq(999,  rowsContainer.children[5].row.id, "inserted row spliced at index 5");
    assert_eq(1000, rowsContainer.children[6].row.id, "second inserted row");
    assert_eq(6,    rowsContainer.children[7].row.id, "row 6 follows the inserted block");
}

// ---- removeRowsAt: shrink the spacer + rerender --------------------

{
    const viewport = new FakeElement();
    const spacer = new FakeElement();
    const rowsContainer = new FakeElement();
    const v = new Virtualizer({
        viewport, spacer, rowsContainer,
        rowHeight: ROW_HEIGHT,
        overscan: 4,
        renderRow,
    });
    v.setRows(makeRows(100));

    v.removeRowsAt(10, 5);
    assert_eq(95, v.getRows().length, "row count shrank by 5");
    assert_eq("2660px", spacer.style.height, "spacer reflects 95 rows");
}

// ---- scrollToIndex sets scrollTop ----------------------------------

{
    const viewport = new FakeElement();
    const spacer = new FakeElement();
    const rowsContainer = new FakeElement();
    const v = new Virtualizer({
        viewport, spacer, rowsContainer,
        rowHeight: ROW_HEIGHT,
        overscan: 4,
        renderRow,
    });
    v.setRows(makeRows(100));

    v.scrollToIndex(50);
    // Target = 50 * 28 - 64 = 1400 - 64 = 1336.
    assert_eq(1336, viewport.scrollTop, "scrollToIndex sets scrollTop with 64px lead");

    v.scrollToIndex(0);
    assert_eq(0, viewport.scrollTop, "scrollToIndex(0) clamps to 0 (negative target clamped)");

    v.scrollToIndex(1);
    // Target = 1 * 28 - 64 = -36 → clamped to 0.
    assert_eq(0, viewport.scrollTop, "scrollToIndex(1) clamps to 0");
}

// ---- findIndex: convenience for orchestrator splices ---------------

{
    const viewport = new FakeElement();
    const spacer = new FakeElement();
    const rowsContainer = new FakeElement();
    const v = new Virtualizer({
        viewport, spacer, rowsContainer,
        rowHeight: ROW_HEIGHT,
        overscan: 4,
        renderRow,
    });
    v.setRows(makeRows(50));
    assert_eq(24, v.findIndex((r) => r.id === 25), "findIndex returns the 0-based index");
    assert_eq(-1, v.findIndex((r) => r.id === 9999), "findIndex returns -1 when absent");
}

// ---- resize event re-renders --------------------------------------

{
    const viewport = new FakeElement();
    const spacer = new FakeElement();
    const rowsContainer = new FakeElement();
    let resizeListener = null;
    globalThis.window = {
        addEventListener: (name, fn) => {
            if (name === "resize") resizeListener = fn;
        },
    };
    const v = new Virtualizer({
        viewport, spacer, rowsContainer,
        rowHeight: ROW_HEIGHT,
        overscan: 4,
        renderRow,
    });
    v.setRows(makeRows(100));
    const initialCount = rowsContainer.children.length;

    // Halve the viewport — fewer rows fit.
    viewport.clientHeight = 280;
    assert_true(typeof resizeListener === "function", "resize listener registered");
    resizeListener();

    // visibleCount = ceil(280/28) = 10; iLast = 0 + 10 + 4 = 14.
    assert_eq(14, rowsContainer.children.length, "fewer rows rendered after shrink");
    assert_true(rowsContainer.children.length < initialCount, "rendered count shrank");
}

// ---- empty rows ----------------------------------------------------

{
    const viewport = new FakeElement();
    const spacer = new FakeElement();
    const rowsContainer = new FakeElement();
    const v = new Virtualizer({
        viewport, spacer, rowsContainer,
        rowHeight: ROW_HEIGHT,
        overscan: 4,
        renderRow,
    });
    v.setRows([]);
    assert_eq("0px", spacer.style.height, "empty list -> 0px spacer");
    assert_eq(0, rowsContainer.children.length, "no rows rendered");
}

report_done();
