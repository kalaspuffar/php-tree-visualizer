// Custom virtualizer for the call-tree row list.
//
// Renders only the rows in the scroll viewport + a small overscan
// (design D-2). Holds the full flat list of row descriptors in
// memory; the DOM holds at most (visible-rows + overscan*2) row
// elements regardless of total row count.
//
// The viewport is the scroll container. A spacer element inside it
// carries the full virtual height (totalRows * rowHeight); a rows
// container is absolutely positioned and shifted via
// transform: translateY(<offset>px). On scroll, we recompute the
// visible window and re-render only that range.
//
// API:
//   new Virtualizer({ viewport, spacer, rowsContainer, rowHeight, overscan, renderRow })
//   setRows(rows)
//   insertRowsAt(index, newRows)
//   removeRowsAt(index, count)
//   scrollToIndex(i)
//   getRows()                   — exposed for tests + the orchestrator
//
// `renderRow(rowDescriptor, index): Node` is the caller-supplied
// builder. The orchestrator wires it to viz/js/tree-row.js's
// buildTreeRow.

/* eslint-env browser */

export class Virtualizer {
    /**
     * @param {object} cfg
     * @param {HTMLElement} cfg.viewport
     * @param {HTMLElement} cfg.spacer
     * @param {HTMLElement} cfg.rowsContainer
     * @param {number} cfg.rowHeight     px per row
     * @param {number} cfg.overscan      extra rows above + below
     * @param {(row: any, index: number) => Node} cfg.renderRow
     */
    constructor(cfg) {
        this.viewport      = cfg.viewport;
        this.spacer        = cfg.spacer;
        this.rowsContainer = cfg.rowsContainer;
        this.rowHeight     = cfg.rowHeight;
        this.overscan      = cfg.overscan ?? 4;
        this.renderRow     = cfg.renderRow;

        this.rows = [];
        this.iFirst = 0;
        this.iLast = 0;

        // Bind so addEventListener removes cleanly if the caller
        // ever asks us to detach.
        this._onScroll = this._onScroll.bind(this);
        this._onResize = this._onResize.bind(this);

        this.viewport.addEventListener("scroll", this._onScroll, { passive: true });
        if (typeof window !== "undefined") {
            window.addEventListener("resize", this._onResize);
        }
    }

    /**
     * Replace the full flat row list.
     *
     * @param {any[]} rows
     */
    setRows(rows) {
        this.rows = rows.slice();
        this._applyHeight();
        this._renderWindow();
    }

    /**
     * Splice `newRows` into the flat list at `index`. Used by lazy
     * expansion (after the parent row).
     *
     * @param {number} index
     * @param {any[]} newRows
     */
    insertRowsAt(index, newRows) {
        if (!Array.isArray(newRows) || newRows.length === 0) return;
        this.rows.splice(index, 0, ...newRows);
        this._applyHeight();
        this._renderWindow();
    }

    /**
     * Remove `count` rows starting at `index`. Used by collapse.
     *
     * @param {number} index
     * @param {number} count
     */
    removeRowsAt(index, count) {
        if (count <= 0) return;
        this.rows.splice(index, count);
        this._applyHeight();
        this._renderWindow();
    }

    /**
     * Scroll so the row at `index` is positioned ~64 px below the
     * viewport top. Used by polish-slice search nav; ship the method
     * now so 6b doesn't add API.
     */
    scrollToIndex(index) {
        const target = Math.max(0, index * this.rowHeight - 64);
        this.viewport.scrollTop = target;
    }

    /**
     * Find the flat-list index of a row by predicate. Convenience
     * for the orchestrator (e.g., to splice children after a
     * parent node).
     *
     * @param {(row: any) => boolean} predicate
     * @returns {number} -1 if not found
     */
    findIndex(predicate) {
        return this.rows.findIndex(predicate);
    }

    /**
     * @returns {any[]} the live flat list reference (mutations are
     *                  intentionally not exposed via a defensive
     *                  copy — the orchestrator does splice via the
     *                  methods above).
     */
    getRows() {
        return this.rows;
    }

    // ---- internals -------------------------------------------------

    _applyHeight() {
        if (this.spacer) {
            this.spacer.style.height = `${this.rows.length * this.rowHeight}px`;
        }
    }

    _onScroll() {
        this._renderWindow();
    }

    _onResize() {
        this._renderWindow();
    }

    _computeWindow() {
        const scrollTop = this.viewport.scrollTop;
        const viewportH = this.viewport.clientHeight;
        const firstVisible = Math.floor(scrollTop / this.rowHeight);
        const visibleCount = Math.ceil(viewportH / this.rowHeight);

        const iFirst = Math.max(0, firstVisible - this.overscan);
        const iLast  = Math.min(
            this.rows.length,
            firstVisible + visibleCount + this.overscan
        );
        return [iFirst, iLast];
    }

    _renderWindow() {
        const [iFirst, iLast] = this._computeWindow();
        this.iFirst = iFirst;
        this.iLast  = iLast;

        // Wipe + repopulate. At <100 visible rows, re-rendering the
        // window per scroll is cheaper than tracking diffs. If
        // measurements ever show otherwise we add a row pool.
        while (this.rowsContainer.firstChild) {
            this.rowsContainer.removeChild(this.rowsContainer.firstChild);
        }

        for (let i = iFirst; i < iLast; i++) {
            const node = this.renderRow(this.rows[i], i);
            if (node) this.rowsContainer.appendChild(node);
        }

        // Shift the rendered window down by iFirst rows.
        this.rowsContainer.style.transform =
            `translateY(${iFirst * this.rowHeight}px)`;
    }
}

// Exported for test access.
export const _internals = {};
