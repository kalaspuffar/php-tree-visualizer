// Orchestrator for /viz/trace.html.
//
// Phase 6a wired: metadata fetch, tree fetch, lazy expand/collapse,
// copy-key, 404 + error states.
// Phase 6b adds: sort UI (column headers + dropdown), in-tree
// search (input + prev/next + match highlight + `/` shortcut),
// keyboard navigation in the tree, AbortController on lazy expand,
// delegated tooltip wiring for the high-traffic icons.

/* eslint-env browser */

import { apiFetch, ApiNetworkError } from "./api-client.js";
import { Virtualizer } from "./virtualizer.js";
import { buildTreeRow } from "./tree-row.js";
import { formatDuration, formatCount } from "./time.js";
import { debounce } from "./debounce.js";
import { findMatches } from "./search.js";
import { nextFocusIndex } from "./keyboard.js";
import { wireTooltip, wireTooltipDelegated } from "./tooltip.js";

const ROW_HEIGHT = 28;
const OVERSCAN = 6;
const ICON_HREF = "/viz/assets/icons.svg";
const HEX32 = /^[0-9a-f]{32}$/;

const SORT_BY_COL = {
    fqn:        "fqn_asc",
    count:      "count_desc",
    total_wall: "total_wall_desc",
    self_wall:  "self_wall_desc",
    // %Parent has no direct API sort; proxy to self_wall_desc.
    // Documented in design.md D-2.
    pct_parent: "self_wall_desc",
    mem_delta:  "mem_delta_desc",
};

const SORT_LABELS = {
    total_wall_desc: { col: "total_wall", direction: "desc", asc: false },
    self_wall_desc:  { col: "self_wall",  direction: "desc", asc: false },
    count_desc:      { col: "count",      direction: "desc", asc: false },
    mem_delta_desc:  { col: "mem_delta",  direction: "desc", asc: false },
    fqn_asc:         { col: "fqn",        direction: "asc",  asc: true  },
};

// ---- module-local state -------------------------------------------

const els = {};
let virtualizer = null;
let currentKey = null;
let currentSort = "total_wall_desc";
let traceMeta = null;

// Lazy-expand: per-node controllers + token guards (6a + 6b combined).
const expandTokens = new Map();
const expandControllers = new Map();

// In-tree search state.
let currentQuery = "";
let currentMatches = [];
let currentMatchIndex = -1;

// Keyboard nav state.
let focusedRowIndex = -1;  // -1 means "no row has the class"

// ---- entry --------------------------------------------------------

function $(id) { return document.getElementById(id); }

document.addEventListener("DOMContentLoaded", () => {
    els.uri              = $("trace-uri");
    els.keyShort         = $("trace-key-short");
    els.copyBtn          = $("copy-key");
    els.copyTooltip      = $("copy-tooltip");
    els.metadataStrip    = $("metadata-strip");
    els.bannerZone       = $("banner-zone");
    els.errorBanner      = $("error-banner");
    els.treeWrapper      = $("tree-wrapper");
    els.treeViewport     = $("tree-viewport");
    els.treeSpacer       = $("tree-spacer");
    els.treeRows         = $("tree-rows");
    els.treeEmpty        = $("tree-empty");
    els.treeEmptyTitle   = els.treeEmpty
        ? els.treeEmpty.querySelector(".tree-empty__title")
        : null;
    els.treeEmptyBody    = els.treeEmpty
        ? els.treeEmpty.querySelector(".tree-empty__body")
        : null;
    els.copyModal        = $("copy-modal");
    els.copyModalInput   = $("copy-modal-input");
    els.copyModalClose   = $("copy-modal-close");

    // 6b chrome
    els.search           = $("tree-search-input");
    els.searchClear      = $("tree-search-clear");
    els.searchCount      = $("tree-search-count");
    els.searchPrev       = $("tree-search-prev");
    els.searchNext       = $("tree-search-next");
    els.sortTrigger      = $("sort-trigger");
    els.sortMenu         = $("sort-menu");
    els.columnHeader     = document.querySelector(".column-header");

    main();
});

async function main() {
    const params = new URLSearchParams(location.search);
    const key = params.get("key");

    if (!key || !HEX32.test(key)) {
        showEmptyState();
        return;
    }
    currentKey = key;

    els.keyShort.textContent = truncateKey(key);
    wireCopyButton();
    wireModal();
    wireSortChrome();
    wireSearchChrome();
    wireSlashShortcut();
    wireBeforeUnload();
    renderActiveColumn();

    try {
        const [meta, tree] = await Promise.all([
            apiFetch(`/api/traces/${encodeURIComponent(key)}`),
            apiFetch(
                `/api/traces/${encodeURIComponent(key)}/tree` +
                `?depth=2&sort=${currentSort}`
            ),
        ]);
        traceMeta = meta;
        hydrateHeader(meta);
        hydrateTree(meta, tree);
    } catch (err) {
        if (err instanceof ApiNetworkError && err.status === 404) {
            showEmptyState();
            return;
        }
        if (err instanceof ApiNetworkError) {
            showErrorBanner(`Couldn't load this trace — the API returned ${err.status}.`);
            return;
        }
        showErrorBanner("Couldn't load this trace.");
    }
}

// ---- Header hydration ---------------------------------------------

function hydrateHeader(meta) {
    els.uri.textContent = meta.uri_or_script;
    els.uri.title = meta.uri_or_script;
    els.keyShort.textContent = truncateKey(meta.trace_key);

    renderMetadataStrip(meta);
    renderBanners(meta);
}

function renderMetadataStrip(meta) {
    els.metadataStrip.innerHTML = "";
    els.metadataStrip.appendChild(
        buildChip("Calls",     formatCount(meta.call_count ?? 0))
    );
    els.metadataStrip.appendChild(
        buildChip("Dropped",   formatCount(meta.dropped_records ?? 0),
                  meta.dropped_records > 0 ? "warn" : null)
    );
    els.metadataStrip.appendChild(
        buildChip("Anomalies", formatCount(meta.anomaly_count ?? 0),
                  meta.anomaly_count > 0 ? "danger" : null)
    );

    const stateChip = buildChip("State", meta.state ?? "—");
    stateChip.classList.add(`metadata-chip--${meta.state ?? "unknown"}`);
    const dot = document.createElement("span");
    dot.className = "metadata-chip__dot";
    stateChip.insertBefore(dot, stateChip.firstChild);
    els.metadataStrip.appendChild(stateChip);
}

function buildChip(labelText, valueText, tone) {
    const chip = document.createElement("span");
    chip.className = "metadata-chip";
    if (tone === "warn")   chip.classList.add("metadata-chip--warn");
    if (tone === "danger") chip.classList.add("metadata-chip--danger");
    const label = document.createElement("span");
    label.className = "metadata-chip__label micro";
    label.textContent = labelText;
    chip.appendChild(label);
    const value = document.createElement("span");
    value.className = "metadata-chip__value";
    value.textContent = valueText;
    chip.appendChild(value);
    return chip;
}

function renderBanners(meta) {
    els.bannerZone.innerHTML = "";

    if ((meta.dropped_records ?? 0) > 0) {
        els.bannerZone.appendChild(buildBanner(
            "warn",
            "icon-alert-triangle",
            `Trace is incomplete — ${formatCount(meta.dropped_records)} dropped records during ingest. Aggregated totals are missing those calls.`,
            "alert"
        ));
    }
    if (meta.cpu_snapshot_available === false) {
        els.bannerZone.appendChild(buildBanner(
            "info",
            "icon-info",
            "CPU time was not captured (cpu_snapshot_mode = off). CPU columns show '—'.",
            "note"
        ));
    }
    if ((meta.anomaly_count ?? 0) > 0) {
        els.bannerZone.appendChild(buildBanner(
            "danger",
            "icon-alert-circle",
            `${formatCount(meta.anomaly_count)} data anomaly/anomalies detected. Hover any flagged row to see details.`,
            "alert"
        ));
    }
}

function buildBanner(tone, iconName, text, role) {
    const div = document.createElement("div");
    div.className = `banner banner--${tone}`;
    div.setAttribute("role", role);
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("class", "banner__icon");
    svg.setAttribute("aria-hidden", "true");
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute("href", `${ICON_HREF}#${iconName}`);
    svg.appendChild(use);
    div.appendChild(svg);
    div.appendChild(document.createTextNode(text));
    return div;
}

// ---- Tree hydration -----------------------------------------------

function hydrateTree(_meta, tree) {
    const apiNodes = tree.nodes ?? [];

    const byId = new Map();
    for (const n of apiNodes) byId.set(n.node_id, n);

    const rootChildren = apiNodes.filter((n) => n.parent_node_id === 1);
    const rows = [];

    function walk(parent, uiDepth) {
        const children = apiNodes.filter(
            (n) => n.parent_node_id === parent.node_id
        );
        for (const child of children) {
            rows.push(makeRow(child, parent, uiDepth));
            walk(child, uiDepth + 1);
        }
    }

    for (const child of rootChildren) {
        rows.push(makeRow(child, byId.get(1), 0));
        walk(child, 1);
    }

    for (const row of rows) {
        const api = byId.get(row.nodeId);
        if (!api) continue;
        row.childrenLoaded = !!api.children_loaded;
        row.hasChildren = !!api.has_children;
    }

    els.treeRows.innerHTML = "";

    virtualizer = new Virtualizer({
        viewport:      els.treeViewport,
        spacer:        els.treeSpacer,
        rowsContainer: els.treeRows,
        rowHeight:     ROW_HEIGHT,
        overscan:      OVERSCAN,
        renderRow:     renderRow,
    });
    virtualizer.setRows(rows);
    wireTreeKeyboard();
    wireDelegatedTooltips();

    if (rows.length === 0) {
        showEmptyState("No calls recorded.",
            "This trace's ingest produced no call records. " +
            "Check dropped_records and php-analyze logs.");
    }
}

function renderRow(row, index) {
    const allRows = virtualizer.getRows();
    const siblings = countSiblings(row, allRows);
    const isCurrentMatch =
        currentMatchIndex >= 0
        && index === currentMatches[currentMatchIndex];
    const isFocused = focusedRowIndex === index;
    return buildTreeRow(row, {
        onChevronClick:   onChevronClick,
        indentDepthForUi: row.indentDepthForUi,
        posInSet:         siblings.posInSet,
        setSize:          siblings.setSize,
        searchPattern:    currentQuery,
        isCurrentMatch,
        isFocused,
    });
}

function makeRow(apiNode, parentApi, uiDepth) {
    const parentTotal = parentApi ? parentApi.total_wall_ns : 0;
    return {
        nodeId:               apiNode.node_id,
        parentNodeId:         apiNode.parent_node_id,
        depth:                apiNode.depth,
        indentDepthForUi:     uiDepth,
        fqn:                  apiNode.fqn,
        file:                 apiNode.file,
        line:                 apiNode.line,
        kind:                 apiNode.kind,
        count:                apiNode.count,
        totalWallNs:          apiNode.total_wall_ns,
        selfWallNs:           apiNode.self_wall_ns,
        totalCpuUNs:          apiNode.total_cpu_u_ns,
        totalCpuSNs:          apiNode.total_cpu_s_ns,
        totalMemDeltaBytes:   apiNode.total_mem_delta_bytes,
        abnormalExitCount:    apiNode.abnormal_exit_count,
        anomalyCount:         apiNode.anomaly_count,
        hasChildren:          !!apiNode.has_children,
        childrenLoaded:       !!apiNode.children_loaded,
        expanded:             !!apiNode.children_loaded,
        parentTotalWallNs:    parentTotal,
        loadingChildren:      false,
        loadError:            false,
    };
}

function countSiblings(row, rows) {
    let setSize = 0;
    let posInSet = 0;
    for (const r of rows) {
        if (r.parentNodeId === row.parentNodeId) {
            setSize++;
            if (r.nodeId === row.nodeId) posInSet = setSize;
        }
    }
    return { posInSet: posInSet || 1, setSize: setSize || 1 };
}

// ---- Lazy expand / collapse (6a + 6b AbortController) ------------

async function onChevronClick(row) {
    const idx = virtualizer.findIndex((r) => r.nodeId === row.nodeId);
    if (idx < 0) return;
    const stored = virtualizer.getRows()[idx];

    if (stored.loadingChildren) {
        // User clicked while a request was in flight → treat as
        // a cancel (collapse).
        abortInFlight(stored.nodeId);
        stored.loadingChildren = false;
        stored.expanded = false;
        rerender();
        return;
    }
    if (stored.expanded) {
        collapseAt(idx);
        return;
    }
    if (stored.loadError) {
        stored.loadError = false;
    }
    if (stored.childrenLoaded) {
        stored.expanded = true;
        rerender();
        return;
    }
    await loadChildren(idx);
}

function collapseAt(idx) {
    const parent = virtualizer.getRows()[idx];

    // 6b: cancel any in-flight expand for this node.
    abortInFlight(parent.nodeId);

    let removeCount = 0;
    const rows = virtualizer.getRows();
    for (let i = idx + 1; i < rows.length; i++) {
        if (rows[i].indentDepthForUi > parent.indentDepthForUi) {
            removeCount++;
        } else {
            break;
        }
    }
    if (removeCount > 0) {
        virtualizer.removeRowsAt(idx + 1, removeCount);
    }
    parent.expanded = false;
    parent.childrenLoaded = false;
    rerender();
}

async function loadChildren(idx) {
    const row = virtualizer.getRows()[idx];
    const token = (expandTokens.get(row.nodeId) ?? 0) + 1;
    expandTokens.set(row.nodeId, token);

    const controller = new AbortController();
    expandControllers.set(row.nodeId, controller);

    row.loadingChildren = true;
    row.expanded = true;
    rerender();

    try {
        const body = await apiFetch(
            `/api/traces/${encodeURIComponent(currentKey)}/tree/` +
            `${row.nodeId}/children?sort=${currentSort}`,
            { signal: controller.signal }
        );
        if (expandTokens.get(row.nodeId) !== token) return;

        const newRows = (body.nodes ?? []).map((api) => ({
            nodeId:               api.node_id,
            parentNodeId:         api.parent_node_id,
            depth:                api.depth,
            indentDepthForUi:     row.indentDepthForUi + 1,
            fqn:                  api.fqn,
            file:                 api.file,
            line:                 api.line,
            kind:                 api.kind,
            count:                api.count,
            totalWallNs:          api.total_wall_ns,
            selfWallNs:           api.self_wall_ns,
            totalCpuUNs:          api.total_cpu_u_ns,
            totalCpuSNs:          api.total_cpu_s_ns,
            totalMemDeltaBytes:   api.total_mem_delta_bytes,
            abnormalExitCount:    api.abnormal_exit_count,
            anomalyCount:         api.anomaly_count,
            hasChildren:          !!api.has_children,
            childrenLoaded:       false,
            expanded:             false,
            parentTotalWallNs:    row.totalWallNs,
            loadingChildren:      false,
            loadError:            false,
        }));

        row.loadingChildren = false;
        row.childrenLoaded = true;
        virtualizer.insertRowsAt(idx + 1, newRows);
        rerender();
    } catch (err) {
        if (err && err.name === "AbortError") {
            // User cancelled — collapse path already cleaned up.
            return;
        }
        if (expandTokens.get(row.nodeId) !== token) return;
        row.loadingChildren = false;
        row.loadError = true;
        rerender();
    } finally {
        expandControllers.delete(row.nodeId);
    }
}

function abortInFlight(nodeId) {
    const c = expandControllers.get(nodeId);
    if (c) {
        c.abort();
        expandControllers.delete(nodeId);
    }
}

function rerender() {
    if (!virtualizer) return;
    virtualizer.setRows(virtualizer.getRows());
}

// ---- Sort wiring (6b) ---------------------------------------------

function wireSortChrome() {
    // Column headers
    els.columnHeader.querySelectorAll(".column-header__cell")
        .forEach((cell) => {
            cell.tabIndex = 0;
            cell.addEventListener("click", () => onColumnHeaderClick(cell));
            cell.addEventListener("keydown", (e) => {
                if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    onColumnHeaderClick(cell);
                }
            });
        });

    // Dropdown trigger
    els.sortTrigger.addEventListener("click", () => {
        toggleSortMenu(!isSortMenuOpen());
    });
    els.sortTrigger.addEventListener("keydown", (e) => {
        if (e.key === "ArrowDown" && !isSortMenuOpen()) {
            e.preventDefault();
            toggleSortMenu(true);
            const first = els.sortMenu.querySelector("li");
            first?.focus();
        }
    });

    // Menu items
    els.sortMenu.querySelectorAll("li[role='menuitemradio']")
        .forEach((item) => {
            item.addEventListener("click", () => {
                const sort = item.getAttribute("data-sort");
                if (sort) applySort(sort);
                toggleSortMenu(false);
                els.sortTrigger.focus();
            });
            item.addEventListener("keydown", (e) => {
                if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    item.click();
                }
            });
        });

    // Esc + outside-click close
    document.addEventListener("keydown", (e) => {
        if (e.key === "Escape" && isSortMenuOpen()) {
            toggleSortMenu(false);
            els.sortTrigger.focus();
        }
    });
    document.addEventListener("mousedown", (e) => {
        if (!isSortMenuOpen()) return;
        if (
            els.sortMenu.contains(e.target) ||
            els.sortTrigger.contains(e.target)
        ) return;
        toggleSortMenu(false);
    });
}

function isSortMenuOpen() {
    return !els.sortMenu.hidden;
}

function toggleSortMenu(open) {
    els.sortMenu.hidden = !open;
    els.sortTrigger.setAttribute("aria-expanded", String(open));
}

function onColumnHeaderClick(cell) {
    const col = cell.getAttribute("data-col");
    const sort = SORT_BY_COL[col];
    if (!sort) return;
    applySort(sort);
}

async function applySort(newSort) {
    if (!SORT_LABELS[newSort]) return;
    currentSort = newSort;
    renderActiveColumn();
    renderSortMenuChecked();

    if (!currentKey) return;
    try {
        const tree = await apiFetch(
            `/api/traces/${encodeURIComponent(currentKey)}/tree` +
            `?depth=2&sort=${currentSort}`
        );
        hydrateTree(traceMeta, tree);
        // Clear search state — the indices in currentMatches are
        // stale after the row list is rebuilt.
        clearSearchMatches();
        if (currentQuery) {
            applySearch(currentQuery);
        }
    } catch (err) {
        if (err instanceof ApiNetworkError && err.status === 404) {
            showEmptyState();
            return;
        }
        showErrorBanner("Couldn't re-sort — please retry.");
    }
}

function renderActiveColumn() {
    const meta = SORT_LABELS[currentSort];
    if (!meta || !els.columnHeader) return;

    els.columnHeader.querySelectorAll(".column-header__cell")
        .forEach((cell) => {
            cell.classList.remove("column-header__cell--active");
            cell.removeAttribute("aria-sort");
            // Strip any prior sort-indicator children.
            cell.querySelectorAll(".column-header__cell--sort-indicator")
                .forEach((n) => n.remove());
        });

    // The active column matches the sort's `col`. %Parent is the
    // proxy case: when the active sort is self_wall_desc AND the
    // user clicked %Parent most recently, we mark %Parent active
    // instead of Self. Lacking a memory of which click produced
    // the current sort, default to marking the literal sort col
    // (Self for self_wall_desc).
    const targetCol = meta.col;
    const cell = els.columnHeader.querySelector(`[data-col="${targetCol}"]`);
    if (!cell) return;
    cell.classList.add("column-header__cell--active");
    cell.setAttribute("aria-sort", meta.asc ? "ascending" : "descending");

    const indicator = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    indicator.setAttribute("class", "icon column-header__cell--sort-indicator");
    indicator.setAttribute("aria-hidden", "true");
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute(
        "href",
        `${ICON_HREF}#${meta.asc ? "icon-chevron-up" : "icon-chevron-down"}`
    );
    indicator.appendChild(use);
    cell.appendChild(indicator);
}

function renderSortMenuChecked() {
    els.sortMenu.querySelectorAll("li[role='menuitemradio']").forEach((item) => {
        const sort = item.getAttribute("data-sort");
        item.setAttribute("aria-checked", sort === currentSort ? "true" : "false");
    });
}

// ---- Search wiring (6b) -------------------------------------------

function wireSearchChrome() {
    const debouncedApply = debounce((q) => applySearch(q), 120);

    els.search.addEventListener("input", () => {
        const q = els.search.value;
        els.searchClear.hidden = q === "";
        debouncedApply(q);
    });
    els.search.addEventListener("keydown", (e) => {
        if (e.key === "Enter") {
            e.preventDefault();
            stepMatch(e.shiftKey ? -1 : +1);
        } else if (e.key === "Escape") {
            e.preventDefault();
            els.search.value = "";
            els.searchClear.hidden = true;
            applySearch("");
            els.search.blur();
        }
    });
    els.searchClear.addEventListener("click", () => {
        els.search.value = "";
        els.searchClear.hidden = true;
        applySearch("");
        els.search.focus();
    });
    els.searchPrev.addEventListener("click", () => stepMatch(-1));
    els.searchNext.addEventListener("click", () => stepMatch(+1));
}

function applySearch(q) {
    currentQuery = q;
    if (!virtualizer) {
        updateSearchCount();
        return;
    }
    if (q === "") {
        clearSearchMatches();
        rerender();
        return;
    }
    currentMatches = findMatches(virtualizer.getRows(), q);
    currentMatchIndex = currentMatches.length > 0 ? 0 : -1;
    updateSearchCount();
    rerender();
    if (currentMatchIndex >= 0) {
        virtualizer.scrollToIndex(currentMatches[currentMatchIndex]);
    }
}

function clearSearchMatches() {
    currentMatches = [];
    currentMatchIndex = -1;
    updateSearchCount();
}

function stepMatch(direction) {
    if (currentMatches.length === 0) return;
    currentMatchIndex =
        (currentMatchIndex + direction + currentMatches.length)
        % currentMatches.length;
    updateSearchCount();
    rerender();
    virtualizer.scrollToIndex(currentMatches[currentMatchIndex]);
}

function updateSearchCount() {
    if (!els.searchCount) return;
    if (currentQuery === "") {
        els.searchCount.textContent = "";
        els.searchCount.classList.remove("tree-search__count--no-matches");
        els.search.classList.remove("tree-search__input--no-matches");
        els.searchPrev.disabled = true;
        els.searchNext.disabled = true;
        return;
    }
    const n = currentMatches.length;
    if (n === 0) {
        els.searchCount.textContent = "no matches";
        els.searchCount.classList.add("tree-search__count--no-matches");
        els.search.classList.add("tree-search__input--no-matches");
        els.searchPrev.disabled = true;
        els.searchNext.disabled = true;
        return;
    }
    els.searchCount.textContent =
        `${currentMatchIndex + 1} of ${n} matches`;
    els.searchCount.classList.remove("tree-search__count--no-matches");
    els.search.classList.remove("tree-search__input--no-matches");
    els.searchPrev.disabled = false;
    els.searchNext.disabled = false;
}

// ---- Slash shortcut ----------------------------------------------

function wireSlashShortcut() {
    document.addEventListener("keydown", (e) => {
        if (e.key !== "/") return;
        if (e.ctrlKey || e.metaKey || e.altKey) return;
        const target = e.target;
        if (target && (
            target.tagName === "INPUT" ||
            target.tagName === "TEXTAREA" ||
            target.isContentEditable
        )) return;
        e.preventDefault();
        els.search.focus();
        els.search.select();
    });
}

// ---- Tree keyboard navigation (§3.3.8.2) -------------------------

function wireTreeKeyboard() {
    if (!els.treeViewport || els.treeViewport.dataset.kbdWired === "1") return;
    els.treeViewport.dataset.kbdWired = "1";
    els.treeViewport.tabIndex = 0;
    els.treeViewport.addEventListener("focus", () => {
        if (focusedRowIndex < 0 && virtualizer && virtualizer.getRows().length > 0) {
            focusedRowIndex = 0;
            rerender();
        }
    });
    els.treeViewport.addEventListener("keydown", onTreeKeydown);
}

function onTreeKeydown(event) {
    if (!virtualizer) return;
    const rows = virtualizer.getRows();
    const action = nextFocusIndex({
        rows,
        currentIndex: focusedRowIndex >= 0 ? focusedRowIndex : 0,
        key: event.key,
        shiftKey: event.shiftKey,
        ctrlKey: event.ctrlKey,
        metaKey: event.metaKey,
        altKey: event.altKey,
    });

    if (!action.consumed) return;
    event.preventDefault();

    if (action.expand) {
        const row = rows[action.newIndex];
        if (row) onChevronClick(row);
        return;
    }
    if (action.collapse) {
        const row = rows[action.newIndex];
        if (row) onChevronClick(row);
        return;
    }
    if (action.newIndex !== focusedRowIndex) {
        focusedRowIndex = action.newIndex;
        virtualizer.scrollToIndex(focusedRowIndex);
        rerender();
    }
}

// ---- Tooltip wiring ----------------------------------------------

function wireDelegatedTooltips() {
    // One listener on the tree-rows container handles every wired
    // child element. The selector picks up chevrons, anomaly icons,
    // and any element carrying data-tooltip.
    wireTooltipDelegated(
        els.treeRows,
        "[data-tooltip-original-title], [title], [data-tooltip], .fn-badge--int, .tree-row__chevron, .tree-row__anomaly-slot svg",
        (el) =>
            el.getAttribute("data-tooltip")
            || el.getAttribute("data-tooltip-original-title")
            || el.getAttribute("title")
            || ""
    );
}

// ---- beforeunload: abort everything ------------------------------

function wireBeforeUnload() {
    window.addEventListener("beforeunload", () => {
        for (const c of expandControllers.values()) {
            try { c.abort(); } catch { /* ignore */ }
        }
    });
}

// ---- Copy-key + modal (Phase 6a) ---------------------------------

function wireCopyButton() {
    els.copyBtn.addEventListener("click", onCopyClick);
    wireTooltip(els.copyBtn, "Copy full trace key to clipboard");
}

async function onCopyClick() {
    const full = currentKey;
    if (!full) return;
    if (navigator.clipboard && typeof navigator.clipboard.writeText === "function") {
        try {
            await navigator.clipboard.writeText(full);
            flashCopiedTooltip();
            return;
        } catch { /* fall through to modal */ }
    }
    openCopyModal(full);
}

function flashCopiedTooltip() {
    els.copyTooltip.hidden = false;
    setTimeout(() => { els.copyTooltip.hidden = true; }, 1500);
}

function wireModal() {
    els.copyModalClose.addEventListener("click", closeCopyModal);
    document.addEventListener("keydown", (e) => {
        if (e.key === "Escape" && !els.copyModal.hidden) {
            closeCopyModal();
        }
    });
}

function openCopyModal(key) {
    els.copyModalInput.value = key;
    els.copyModal.hidden = false;
    els.copyModalInput.focus();
    els.copyModalInput.select();
}

function closeCopyModal() {
    els.copyModal.hidden = true;
}

// ---- Empty + error states (6a) -----------------------------------

// Renders an inline empty-state message inside the tree zone — a
// small text-only block below the column header, replacing the
// loading skeleton (or the populated tree, in the no-rows path).
// The header chrome (breadcrumb, search bar, sort dropdown, column
// header) stays visible so the page feels continuous; only the
// scrollable viewport is swapped for the message.
//
// Defaults match the "Trace not found" 404 case. Callers pass
// explicit title + body for other paths (e.g., "No calls recorded.").
function showEmptyState(title, body) {
    if (!els.treeEmpty) return;
    if (els.treeEmptyTitle) {
        els.treeEmptyTitle.textContent = title || "Trace not found.";
    }
    if (els.treeEmptyBody) {
        els.treeEmptyBody.textContent =
            body || "It may have been pruned. Default retention is 30 days.";
    }
    els.treeViewport.hidden = true;
    els.treeEmpty.hidden = false;
}

function showErrorBanner(message) {
    els.errorBanner.hidden = false;
    els.errorBanner.className = "banner banner--danger list-error";
    els.errorBanner.innerHTML = "";
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("class", "banner__icon");
    svg.setAttribute("aria-hidden", "true");
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute("href", "/viz/assets/icons.svg#icon-alert-circle");
    svg.appendChild(use);
    els.errorBanner.appendChild(svg);
    els.errorBanner.appendChild(document.createTextNode(message + " "));
    const retry = document.createElement("button");
    retry.type = "button";
    retry.className = "button button--text";
    retry.textContent = "Retry";
    retry.addEventListener("click", () => {
        els.errorBanner.hidden = true;
        main();
    });
    els.errorBanner.appendChild(retry);
}

// ---- helpers -----------------------------------------------------

function truncateKey(k) {
    if (typeof k !== "string" || k.length < 12) return k ?? "";
    return k.slice(0, 4) + "…" + k.slice(-4);
}
