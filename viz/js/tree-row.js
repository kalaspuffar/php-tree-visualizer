// Pure DOM builder for one call-tree row per SPECIFICATION.md §3.3.7.
//
// `buildTreeRow(row, options)` returns an <li role="treeitem"> with
// the documented CSS grid: Function cell (indent + chevron + fqn +
// optional badge + file:line + anomaly icons) + Count + Total +
// Self + %Parent (hot-path bar + label) + Mem.
//
// `row` is a descriptor of the shape design.md D-3 specifies:
//   {
//     nodeId, parentNodeId, depth,
//     fqn, file, line, kind,
//     count, totalWallNs, selfWallNs,
//     totalCpuUNs, totalCpuSNs,
//     totalMemDeltaBytes, abnormalExitCount,
//     anomalyCount,
//     hasChildren, childrenLoaded, expanded,
//     parentTotalWallNs,
//     loadingChildren, loadError,
//   }
//
// `options` may include:
//   onChevronClick(row):  invoked when the user clicks the chevron
//                         on an expandable row; the orchestrator
//                         (detail.js) is responsible for the
//                         lazy-fetch + splice and for re-rendering
//                         the row through the virtualizer.
//   indentDepthForUi:     visible depth, separate from the row's
//                         absolute depth in the trace's full tree.
//                         The synthetic root is dropped, so the
//                         top-level rendered row has indentDepthForUi
//                         = 0 (no indent), aria-level = 1.
//   posInSet / setSize:   ARIA position metadata.

/* eslint-env browser */

import {
    formatDuration,
    formatMemDelta,
    formatPercentOfParent,
    formatCount,
} from "./time.js";

const ICON_HREF = "/viz/assets/icons.svg";
const MAX_INDENT_BUCKET = 20;     // matches the CSS rules
const MAX_PCT_BUCKET    = 100;    // 21 buckets in steps of 5

const KIND_INTERNAL = 3;
const KIND_CLOSURE  = 2;
const UNRESOLVED_FN_PREFIX = "unresolved fn_id";

/**
 * Build the row element. Pure: takes data + a click hook, returns a
 * DOM node. The orchestrator wires the node into the virtualizer.
 *
 * @param {object} row
 * @param {object} options
 * @returns {HTMLLIElement}
 */
export function buildTreeRow(row, options = {}) {
    const li = document.createElement("li");
    li.setAttribute("role", "treeitem");
    li.setAttribute("data-node-id", String(row.nodeId));
    li.tabIndex = -1;

    const indentBucket = clampIndentBucket(options.indentDepthForUi ?? 0);
    li.classList.add("tree-row", `tree-row--indent-${indentBucket}`);

    if (typeof options.posInSet === "number") {
        li.setAttribute("aria-posinset", String(options.posInSet));
    }
    if (typeof options.setSize === "number") {
        li.setAttribute("aria-setsize", String(options.setSize));
    }
    const ariaLevel = (options.indentDepthForUi ?? 0) + 1;
    li.setAttribute("aria-level", String(ariaLevel));

    if (row.hasChildren) {
        li.setAttribute("aria-expanded", row.expanded ? "true" : "false");
    }

    // The unresolved-fn variant (§3.3.10): replace fqn, mark left
    // border. Detected by the literal prefix the future audit pass
    // would emit; the present production endpoint never produces
    // one, but the visual encoding ships now (design D-12).
    const isUnresolved =
        typeof row.fqn === "string"
        && row.fqn.startsWith(UNRESOLVED_FN_PREFIX);
    if (isUnresolved) {
        li.classList.add("tree-row--unresolved");
    }

    if (row.loadingChildren) {
        li.classList.add("tree-row--loading");
    }
    if (row.loadError) {
        li.classList.add("tree-row--load-error");
    }

    // Full fqn lives in the row's `title` for hover + screen-reader
    // recovery on truncation. Internal functions get the documented
    // tooltip instead.
    li.title = row.kind === KIND_INTERNAL
        ? "Internal function (PHP core)"
        : (row.fqn ?? "");

    li.appendChild(buildFunctionCell(row, options, isUnresolved));
    li.appendChild(buildNumericCell(formatCount(row.count), "tree-row__cell"));
    li.appendChild(buildNumericCell(formatDuration(row.totalWallNs), "tree-row__cell"));
    li.appendChild(buildNumericCell(formatDuration(row.selfWallNs), "tree-row__cell"));
    li.appendChild(buildPctCell(row));
    li.appendChild(buildMemCell(row));

    return li;
}

function buildFunctionCell(row, options, isUnresolved) {
    const wrap = document.createElement("span");
    wrap.className = "tree-row__function";

    // Indent — width comes from the CSS class on the row.
    const indent = document.createElement("span");
    indent.className = "tree-row__indent";
    indent.setAttribute("aria-hidden", "true");
    wrap.appendChild(indent);

    // Chevron. Always emitted so the row's horizontal rhythm stays
    // consistent; hidden via `hidden` attribute on leaves so the
    // 20-px slot is reserved.
    const chevron = document.createElement("button");
    chevron.type = "button";
    chevron.className = "tree-row__chevron";
    if (!row.hasChildren) {
        chevron.hidden = true;
        chevron.tabIndex = -1;
    } else {
        chevron.setAttribute(
            "aria-label",
            row.loadError
                ? "Retry loading children"
                : row.expanded ? "Collapse children" : "Expand children"
        );
        if (row.loadError) {
            chevron.title =
                "Could not load children. Click to retry.";
        }
        if (options.onChevronClick) {
            chevron.addEventListener("click", (e) => {
                e.stopPropagation();
                options.onChevronClick(row);
            });
        }
    }
    chevron.appendChild(chevronIcon(row));
    wrap.appendChild(chevron);

    // fqn (or unresolved-fn replacement).
    const fqn = document.createElement("span");
    fqn.className = "tree-row__fqn";
    fqn.textContent = row.fqn ?? "";
    wrap.appendChild(fqn);

    // Internal-function badge.
    if (row.kind === KIND_INTERNAL) {
        const badge = document.createElement("span");
        badge.className = "fn-badge--int";
        badge.title = "Internal function (PHP core)";
        badge.textContent = "[int]";
        wrap.appendChild(badge);
    }

    // file:line slot. Internal functions skip the slot (per
    // §3.3.7.5). Closures show the redundant suffix.
    if (row.kind !== KIND_INTERNAL) {
        const fileLine = document.createElement("span");
        fileLine.className = "tree-row__file-line";
        if (row.file) {
            fileLine.textContent =
                row.line > 0 ? `${row.file}:${row.line}` : row.file;
        }
        wrap.appendChild(fileLine);
    }

    // Anomaly slot — abnormal-exit and/or data-anomaly icons.
    wrap.appendChild(buildAnomalySlot(row));

    return wrap;
}

function chevronIcon(row) {
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("class", "icon");
    svg.setAttribute("aria-hidden", "true");
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    let iconName = "icon-chevron-right";
    if (row.loadError)             iconName = "icon-alert-circle";
    else if (row.loadingChildren)  iconName = "icon-loader";
    else if (row.expanded)         iconName = "icon-chevron-down";
    use.setAttribute("href", `${ICON_HREF}#${iconName}`);
    svg.appendChild(use);
    return svg;
}

function buildAnomalySlot(row) {
    const slot = document.createElement("span");
    slot.className = "tree-row__anomaly-slot";

    if ((row.abnormalExitCount ?? 0) > 0) {
        slot.appendChild(
            anomalyIcon(
                "icon-alert-triangle",
                "anomaly-icon--warn",
                `${row.abnormalExitCount} call(s) exited abnormally (e.g., uncaught exception)`
            )
        );
    }
    if ((row.anomalyCount ?? 0) > 0) {
        slot.appendChild(
            anomalyIcon(
                "icon-alert-circle",
                "anomaly-icon--danger",
                `${row.anomalyCount} data anomaly/anomalies`
            )
        );
    }
    return slot;
}

function anomalyIcon(iconName, toneClass, titleText) {
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("class", `icon ${toneClass}`);
    svg.setAttribute("role", "img");
    const title = document.createElementNS("http://www.w3.org/2000/svg", "title");
    title.textContent = titleText;
    svg.appendChild(title);
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute("href", `${ICON_HREF}#${iconName}`);
    svg.appendChild(use);
    return svg;
}

function buildNumericCell(text, baseClass) {
    const cell = document.createElement("span");
    cell.className = baseClass;
    cell.textContent = text;
    return cell;
}

function buildPctCell(row) {
    const cell = document.createElement("span");
    cell.className = "tree-row__pct-cell";

    const pct = computePctOfParent(row);
    const { text, tone } = formatPercentOfParent(pct);
    const bucket = pctBucket(pct);

    const bar = document.createElement("span");
    bar.className = "hot-bar";
    const fill = document.createElement("span");
    fill.className = `hot-bar__fill hot-bar__fill--${bucket}`;
    bar.appendChild(fill);
    cell.appendChild(bar);

    const label = document.createElement("span");
    label.className =
        "tree-row__pct-text" + (tone === "zero" ? " tree-row__cell--zero" : "");
    label.textContent = text;
    cell.appendChild(label);

    return cell;
}

function buildMemCell(row) {
    const { text, tone } = formatMemDelta(row.totalMemDeltaBytes);
    const cell = document.createElement("span");
    let cls = "tree-row__cell";
    if (tone === "negative") cls += " tree-row__cell--danger";
    else if (tone === "zero") cls += " tree-row__cell--zero";
    cell.className = cls;
    cell.textContent = text;
    return cell;
}

/**
 * Compute percentage of parent's total_wall_ns. The synthetic root
 * has no parent total — its children are the "top of tree" and we
 * use them as the reference for their own %parent. We compute pct
 * relative to the same denominator the spec implies: the parent
 * node's total_wall_ns.
 *
 * If parentTotalWallNs is missing, zero, or smaller than the row's
 * own total (which would yield > 100%), clamp at 0 / 100 for
 * presentation. The clamps preserve invariants (DI-3) while
 * tolerating fixture quirks.
 */
function computePctOfParent(row) {
    const parent = row.parentTotalWallNs;
    const own = row.totalWallNs;
    if (!Number.isFinite(parent) || parent <= 0) return 0;
    if (!Number.isFinite(own) || own <= 0) return 0;
    const ratio = (own / parent) * 100;
    if (ratio < 0) return 0;
    if (ratio > 100) return 100;
    return ratio;
}

function pctBucket(pct) {
    // Round to nearest multiple of 5 in [0, 100].
    const rounded = Math.round(pct / 5) * 5;
    if (rounded < 0)               return 0;
    if (rounded > MAX_PCT_BUCKET)  return MAX_PCT_BUCKET;
    return rounded;
}

function clampIndentBucket(depth) {
    if (!Number.isFinite(depth) || depth < 0) return 0;
    if (depth > MAX_INDENT_BUCKET) return MAX_INDENT_BUCKET;
    return Math.floor(depth);
}

// Exported for unit tests.
export const _internals = {
    pctBucket,
    clampIndentBucket,
    computePctOfParent,
};
