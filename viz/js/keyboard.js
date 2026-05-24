// Tree keyboard navigation — pure handler logic.
//
//   nextFocusIndex({ rows, currentIndex, key, shiftKey, ctrlKey,
//                    metaKey, altKey })
//      -> { newIndex, expand, collapse, consumed }
//
// Returns the action the caller should take. Detail.js wraps this:
// applies focus to the new index, triggers expand/collapse, calls
// preventDefault when consumed. Pure so the keyboard map is unit-
// testable without a real DOM or virtualizer.
//
// Row shape (what nextFocusIndex reads):
//   { hasChildren: boolean, expanded: boolean, indentDepthForUi: number }

const PAGE_STEP = 10;

/**
 * @param {object} ctx
 * @param {Array<{
 *   hasChildren: boolean, expanded: boolean, indentDepthForUi: number
 * }>} ctx.rows
 * @param {number} ctx.currentIndex
 * @param {string} ctx.key
 * @param {boolean} [ctx.shiftKey]
 * @param {boolean} [ctx.ctrlKey]
 * @param {boolean} [ctx.metaKey]
 * @param {boolean} [ctx.altKey]
 * @returns {{ newIndex: number, expand: boolean, collapse: boolean, consumed: boolean }}
 */
export function nextFocusIndex(ctx) {
    const { rows, currentIndex, key } = ctx;
    if (ctx.ctrlKey || ctx.metaKey || ctx.altKey) {
        return result(currentIndex, false);
    }
    if (rows.length === 0) {
        return result(0, false);
    }

    const safeIndex =
        currentIndex < 0 ? 0
            : currentIndex >= rows.length ? rows.length - 1
                : currentIndex;
    const current = rows[safeIndex];

    switch (key) {
        case "ArrowDown":
            return result(Math.min(safeIndex + 1, rows.length - 1), true);
        case "ArrowUp":
            return result(Math.max(safeIndex - 1, 0), true);
        case "PageDown":
            return result(Math.min(safeIndex + PAGE_STEP, rows.length - 1), true);
        case "PageUp":
            return result(Math.max(safeIndex - PAGE_STEP, 0), true);
        case "Home":
            return result(0, true);
        case "End":
            return result(rows.length - 1, true);
        case "ArrowRight":
            if (current.hasChildren && !current.expanded) {
                return { newIndex: safeIndex, expand: true, collapse: false, consumed: true };
            }
            if (current.expanded) {
                // Focus first child = the very next row (parents
                // appear before children in the flat list).
                return result(Math.min(safeIndex + 1, rows.length - 1), true);
            }
            // Leaf — no-op but consume.
            return result(safeIndex, true);
        case "ArrowLeft":
            if (current.expanded) {
                return { newIndex: safeIndex, expand: false, collapse: true, consumed: true };
            }
            // Walk upward to find the parent: the most recent
            // earlier row with strictly shallower depth.
            for (let i = safeIndex - 1; i >= 0; i--) {
                if (rows[i].indentDepthForUi < current.indentDepthForUi) {
                    return result(i, true);
                }
            }
            // No parent (we're a top-level row). No-op but consume.
            return result(safeIndex, true);
        case "Enter":
            // Reserved per spec §3.3.8.2 — consume to suppress
            // any default behavior (like submitting an enclosing
            // form, which we don't have but which the consumer
            // should still defend against).
            return result(safeIndex, true);
        default:
            return result(safeIndex, false);
    }
}

function result(newIndex, consumed) {
    return { newIndex, expand: false, collapse: false, consumed };
}
