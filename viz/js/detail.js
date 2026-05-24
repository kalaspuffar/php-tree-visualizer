// Orchestrator for /viz/trace.html.
//
// Reads ?key= from the URL, fires the metadata + tree fetches in
// parallel, hydrates the header chrome + tree zone, wires lazy
// expansion + collapse on chevron clicks, wires the copy-key
// button.
//
// Tied together via:
//   api-client.js  - apiFetch (401 redirect, ApiNetworkError)
//   virtualizer.js - Virtualizer class
//   tree-row.js    - buildTreeRow
//   time.js        - formatDuration + formatCount

/* eslint-env browser */

import { apiFetch, ApiNetworkError } from "./api-client.js";
import { Virtualizer } from "./virtualizer.js";
import { buildTreeRow } from "./tree-row.js";
import { formatDuration, formatCount } from "./time.js";

const ROW_HEIGHT = 28;
const OVERSCAN = 6;
const ICON_HREF = "/viz/assets/icons.svg";

const HEX32 = /^[0-9a-f]{32}$/;

// ---- DOM handles ----------------------------------------------------

const els = {};
function $(id) { return document.getElementById(id); }

document.addEventListener("DOMContentLoaded", () => {
    els.uri              = $("trace-uri");
    els.keyShort         = $("trace-key-short");
    els.copyBtn          = $("copy-key");
    els.copyTooltip      = $("copy-tooltip");
    els.metadataStrip    = $("metadata-strip");
    els.bannerZone       = $("banner-zone");
    els.errorBanner      = $("error-banner");
    els.emptyState       = $("empty-state");
    els.treeWrapper      = $("tree-wrapper");
    els.treeViewport     = $("tree-viewport");
    els.treeSpacer       = $("tree-spacer");
    els.treeRows         = $("tree-rows");
    els.copyModal        = $("copy-modal");
    els.copyModalInput   = $("copy-modal-input");
    els.copyModalClose   = $("copy-modal-close");

    main();
});

let virtualizer = null;
let currentKey = null;
let traceMeta = null;
// Map of nodeId -> { expandToken, abortReason } — used to ignore
// stale lazy-expand responses on rapid expand/collapse.
const expandTokens = new Map();

async function main() {
    const params = new URLSearchParams(location.search);
    const key = params.get("key");

    if (!key || !HEX32.test(key)) {
        showEmptyState();
        return;
    }
    currentKey = key;

    // Pre-fill the breadcrumb's key span with the truncated form so
    // first paint has something to copy.
    els.keyShort.textContent = truncateKey(key);
    wireCopyButton();
    wireModal();

    try {
        const [meta, tree] = await Promise.all([
            apiFetch(`/api/traces/${encodeURIComponent(key)}`),
            apiFetch(
                `/api/traces/${encodeURIComponent(key)}/tree` +
                `?depth=2&sort=total_wall_desc`
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

// ---- Header hydration ----------------------------------------------

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
    // We don't have total_wall_ns on the §5.5 metadata response;
    // pull it from the index via a separate field if needed. For
    // now omit (the §5.5 sample omits it too). If/when the API
    // grows the field, hydrate it here.
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

// ---- Tree hydration -------------------------------------------------

function hydrateTree(_meta, tree) {
    const apiNodes = tree.nodes ?? [];

    // Drop the synthetic root (node_id=1, parent=null, fqn="<root>").
    // Map of nodeId -> apiNode for parent total lookups.
    const byId = new Map();
    for (const n of apiNodes) byId.set(n.node_id, n);

    const rootChildren = apiNodes
        .filter((n) => n.parent_node_id === 1);

    const rows = [];
    // BFS so parents appear before children, respecting the source
    // ordering at each level (the API already sorted by sort param).
    function walk(parent, uiDepth) {
        // children of `parent` are apiNodes whose parent_node_id =
        // parent.node_id, preserving the order they appear in the
        // response (the API ordered them by sort param).
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

    // Mark which rows had children that arrived in this response —
    // those have childrenLoaded=true. Any row whose
    // children_loaded === false on the API side (its children are
    // beyond depth=2) has childrenLoaded=false here so lazy expand
    // fires.
    for (const row of rows) {
        const api = byId.get(row.nodeId);
        if (!api) continue;
        row.childrenLoaded = !!api.children_loaded;
        row.hasChildren = !!api.has_children;
    }

    // Wire the virtualizer against the now-empty rows container.
    // Replace the skeletons.
    els.treeRows.innerHTML = "";

    virtualizer = new Virtualizer({
        viewport:      els.treeViewport,
        spacer:        els.treeSpacer,
        rowsContainer: els.treeRows,
        rowHeight:     ROW_HEIGHT,
        overscan:      OVERSCAN,
        renderRow:     (row, index) => {
            // Recompute posInSet / setSize relative to siblings.
            const siblings = virtualizer
                ? countSiblings(row, virtualizer.getRows())
                : { posInSet: 1, setSize: 1 };
            return buildTreeRow(row, {
                onChevronClick: onChevronClick,
                indentDepthForUi: row.indentDepthForUi,
                posInSet: siblings.posInSet,
                setSize: siblings.setSize,
            });
        },
    });
    virtualizer.setRows(rows);

    if (rows.length === 0) {
        // Real "no calls" empty state — defensive.
        showEmptyState("No calls recorded.",
            "This trace's ingest produced no call records. " +
            "Check dropped_records and php-analyze logs.");
    }
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
    return {
        posInSet: posInSet || 1,
        setSize: setSize || 1,
    };
}

// ---- Lazy expand / collapse ----------------------------------------

async function onChevronClick(row) {
    const idx = virtualizer.findIndex((r) => r.nodeId === row.nodeId);
    if (idx < 0) return;
    const stored = virtualizer.getRows()[idx];

    if (stored.expanded) {
        // Collapse: remove all descendants.
        collapseAt(idx);
        return;
    }
    if (stored.loadError) {
        // Retry — fall through to the load path.
        stored.loadError = false;
    }
    if (stored.childrenLoaded) {
        // Re-expand without re-fetch — children weren't dropped on
        // collapse if we cached them; in this slice we always drop
        // on collapse, so this branch is unreachable. Kept as a
        // defensive no-op for future caching.
        stored.expanded = true;
        rerenderRow(idx);
        return;
    }
    await loadChildren(idx);
}

function collapseAt(idx) {
    const parent = virtualizer.getRows()[idx];
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
    rerenderRow(idx);
}

async function loadChildren(idx) {
    const row = virtualizer.getRows()[idx];
    const token = (expandTokens.get(row.nodeId) ?? 0) + 1;
    expandTokens.set(row.nodeId, token);

    row.loadingChildren = true;
    row.expanded = true;
    rerenderRow(idx);

    try {
        const body = await apiFetch(
            `/api/traces/${encodeURIComponent(currentKey)}/tree/` +
            `${row.nodeId}/children?sort=total_wall_desc`
        );
        // Stale-response guard.
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
        rerenderRow(idx);
    } catch (err) {
        if (expandTokens.get(row.nodeId) !== token) return;
        row.loadingChildren = false;
        row.loadError = true;
        rerenderRow(idx);
    }
}

function rerenderRow(idx) {
    // The simplest correct implementation: re-render the whole
    // visible window. The window is small (≤30 rows), so the cost
    // is sub-ms. A row-pool optimization would be premature.
    virtualizer.setRows(virtualizer.getRows());
}

// ---- Copy-key + modal ----------------------------------------------

function wireCopyButton() {
    els.copyBtn.addEventListener("click", onCopyClick);
}

async function onCopyClick() {
    const full = currentKey;
    if (!full) return;

    if (navigator.clipboard && typeof navigator.clipboard.writeText === "function") {
        try {
            await navigator.clipboard.writeText(full);
            flashCopiedTooltip();
            return;
        } catch {
            /* fall through to modal */
        }
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

// ---- Empty + error states ------------------------------------------

function showEmptyState(title, body) {
    els.treeWrapper.hidden = true;
    els.emptyState.hidden = false;

    if (title) {
        const titleEl = els.emptyState.querySelector(".empty-state__title");
        if (titleEl) titleEl.textContent = title;
    }
    if (body) {
        const bodyEl = els.emptyState.querySelector(".empty-state__body");
        if (bodyEl) bodyEl.textContent = body;
    }
}

function showErrorBanner(message) {
    els.errorBanner.hidden = false;
    els.errorBanner.innerHTML = "";
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("class", "banner__icon");
    svg.setAttribute("aria-hidden", "true");
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute("href", `${ICON_HREF}#icon-alert-circle`);
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

// ---- helpers -------------------------------------------------------

function truncateKey(k) {
    if (typeof k !== "string" || k.length < 12) return k ?? "";
    return k.slice(0, 4) + "…" + k.slice(-4);
}
