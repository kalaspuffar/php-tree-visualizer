// Trace-list page wiring.
//
//   - Initial fetch: GET /api/traces. Replace skeletons with rows.
//   - Debounced filter (250 ms): re-fetch with `q`. Fade-replace.
//   - "Load more": fetch next page with current filter, append rows.
//   - Empty / filter-empty / error / skeleton states per §3.3.9.

import { apiFetch, ApiNetworkError } from "./api-client.js";
import { debounce } from "./debounce.js";
import { formatWall, formatRelative } from "./time.js";

const titleEl       = document.getElementById("traces-title");
const filterInput   = document.getElementById("filter-input");
const filterClear   = document.getElementById("filter-clear");
const filterWrap    = document.getElementById("filter");
const rowsEl        = document.getElementById("rows");
const footerEl      = document.getElementById("list-footer");
const errorEl       = document.getElementById("list-error");

const PAGE_LIMIT = 100;

// Track the unfiltered "total" so the title can render
// "{matched} of {total} match" when the filter is active.
let unfilteredTotal = null;

// Track current filter + paging for "Load more".
let currentQuery   = "";
let currentOffset  = 0;
let currentHasMore = false;
let currentTotal   = 0;

// In-flight fetch token so out-of-order responses are ignored.
let fetchToken = 0;

const TITLE_BASE = "Traces";

const COPY_NETWORK_ERROR =
    "Couldn't load traces — network error.";
const COPY_HTTP_ERROR = (status) =>
    `Couldn't load traces — the API returned ${status}.`;

// ---- entry point ----------------------------------------------------

document.addEventListener("DOMContentLoaded", () => {
    wireFilter();
    wireFooter();
    initialLoad();
});

async function initialLoad() {
    await runFetch({ q: "", offset: 0, append: false, recordUnfiltered: true });
}

// ---- fetch + render -------------------------------------------------

async function runFetch({ q, offset, append, recordUnfiltered }) {
    const token = ++fetchToken;
    setFilterLoading(true);
    hideError();
    try {
        const url =
            "/api/traces?" +
            new URLSearchParams({
                q,
                limit: String(PAGE_LIMIT),
                offset: String(offset),
            }).toString();
        const body = await apiFetch(url);

        // A later fetch superseded us. Ignore the result.
        if (token !== fetchToken) return;

        if (!body || !Array.isArray(body.items)) {
            renderError(COPY_HTTP_ERROR(200));
            return;
        }

        if (recordUnfiltered) {
            unfilteredTotal = body.total;
        }

        currentQuery   = q;
        currentOffset  = offset + body.items.length;
        currentHasMore = !!body.has_more;
        currentTotal   = body.total;

        renderRows(body.items, append);
        renderTitle();
        renderFooter();
    } catch (err) {
        if (token !== fetchToken) return;
        if (err instanceof ApiNetworkError) {
            renderError(err.status === 0 ? COPY_NETWORK_ERROR : COPY_HTTP_ERROR(err.status));
        } else {
            renderError(COPY_NETWORK_ERROR);
        }
    } finally {
        if (token === fetchToken) setFilterLoading(false);
    }
}

function renderRows(items, append) {
    if (!append) {
        rowsEl.innerHTML = "";
    }

    if (!append && items.length === 0) {
        renderEmptyState();
        return;
    }

    const frag = document.createDocumentFragment();
    for (const item of items) {
        frag.appendChild(buildRow(item));
    }
    rowsEl.appendChild(frag);
}

function renderEmptyState() {
    rowsEl.innerHTML = "";
    if (currentQuery !== "") {
        // No matches for the active filter.
        const block = document.createElement("li");
        block.className = "empty-state";
        block.innerHTML = `
          <svg class="empty-state__illustration" viewBox="0 0 96 96" fill="none"
               stroke="currentColor" stroke-width="2"
               stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <circle cx="42" cy="42" r="22"/>
            <path d="m72 72-15-15"/>
            <path d="M42 35v10"/>
            <path d="M42 50h.01"/>
          </svg>
          <h2 class="h2 empty-state__title"></h2>
          <p class="body empty-state__body"></p>
          <button type="button" class="button button--text">Clear filter</button>
        `;
        block.querySelector(".empty-state__title").textContent =
            `No traces match "${currentQuery}".`;
        block.querySelector(".empty-state__body").textContent = "";
        block.querySelector("button").addEventListener("click", () => {
            filterInput.value = "";
            filterClear.hidden = true;
            currentQuery = "";
            debouncedFilter("");
        });
        rowsEl.appendChild(block);
    } else {
        // No traces at all.
        const block = document.createElement("li");
        block.className = "empty-state";
        block.innerHTML = `
          <svg class="empty-state__illustration" viewBox="0 0 96 96" fill="none"
               stroke="currentColor" stroke-width="2"
               stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <rect x="14" y="22" width="68" height="52" rx="4"/>
            <path d="M14 36h68"/>
          </svg>
          <h2 class="h2 empty-state__title">No traces yet.</h2>
          <p class="body empty-state__body">
            Run a PHP script with <code>php-analyze</code> configured to POST
            to <code>/ingest/v1</code>. The trace will appear here within a
            few minutes.
          </p>
        `;
        rowsEl.appendChild(block);
    }
}

function buildRow(item) {
    const a = document.createElement("a");
    a.href = `/viz/trace.html?key=${encodeURIComponent(item.trace_key)}`;
    a.className =
        "trace-row" +
        (item.state === "active" ? " trace-row--active" : " trace-row--finalized");
    a.setAttribute(
        "aria-label",
        `Trace ${item.uri_or_script}, ${item.state}, ${item.call_count.toLocaleString()} calls`
    );
    a.title = item.uri_or_script;

    const dot = document.createElement("span");
    dot.className = "trace-row__state-dot";
    dot.setAttribute("aria-hidden", "true");

    const main = document.createElement("span");
    main.className = "trace-row__main";
    const uri = document.createElement("span");
    uri.className = "body-strong trace-row__uri";
    uri.textContent = item.uri_or_script;
    main.appendChild(uri);

    const sub = document.createElement("span");
    sub.className = "small trace-row__sub";
    const ts = relativeFromIso(item.start_time);
    sub.appendChild(document.createTextNode(
        `${item.sapi} · ${item.host} · ${item.pid} · ${ts}`
    ));
    if (item.dropped_records > 0) {
        sub.appendChild(buildChip(
            "warn",
            "icon-alert-triangle",
            `${item.dropped_records.toLocaleString()} dropped`
        ));
    }
    if (item.anomaly_count > 0) {
        sub.appendChild(buildChip(
            "danger",
            "icon-alert-circle",
            `${item.anomaly_count.toLocaleString()} anomalies`
        ));
    }
    main.appendChild(sub);

    const metrics = document.createElement("span");
    metrics.className = "trace-row__metrics";
    const calls = document.createElement("span");
    calls.className = "body-strong code trace-row__calls";
    calls.textContent = `${item.call_count.toLocaleString()} calls`;
    metrics.appendChild(calls);

    const wallAndState = document.createElement("span");
    wallAndState.className = "small trace-row__wall-and-state";
    wallAndState.appendChild(document.createTextNode(formatWall(item.total_wall_ns)));
    const stateChip = document.createElement("span");
    stateChip.className = "trace-row__state-chip";
    stateChip.textContent = item.state;
    wallAndState.appendChild(stateChip);
    metrics.appendChild(wallAndState);

    a.appendChild(dot);
    a.appendChild(main);
    a.appendChild(metrics);
    return a;
}

function buildChip(tone, iconId, text) {
    const chip = document.createElement("span");
    chip.className = `chip chip--${tone}`;
    const icon = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    icon.setAttribute("class", "icon");
    icon.setAttribute("aria-hidden", "true");
    icon.setAttribute("width", "14");
    icon.setAttribute("height", "14");
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute("href", `/viz/assets/icons.svg#${iconId}`);
    icon.appendChild(use);
    chip.appendChild(icon);
    chip.appendChild(document.createTextNode(text));
    return chip;
}

function relativeFromIso(iso) {
    if (typeof iso !== "string") return "";
    const ms = Date.parse(iso);
    if (Number.isNaN(ms)) return "";
    return formatRelative(ms / 1000);
}

// ---- title + footer + error -----------------------------------------

function renderTitle() {
    const titleText = `${TITLE_BASE}`;
    const separator = " · ";
    const countText =
        currentQuery === ""
            ? `${currentTotal.toLocaleString()} total`
            : `${currentTotal.toLocaleString()} of ${(unfilteredTotal ?? currentTotal).toLocaleString()} match`;

    titleEl.innerHTML = "";
    titleEl.appendChild(document.createTextNode(titleText));
    const sep = document.createElement("span");
    sep.className = "traces-title__separator";
    sep.textContent = separator;
    titleEl.appendChild(sep);
    const cnt = document.createElement("span");
    cnt.className = "body traces-title__count";
    cnt.textContent = countText;
    titleEl.appendChild(cnt);
}

function renderFooter() {
    if (!currentHasMore) {
        footerEl.hidden = true;
        footerEl.innerHTML = "";
        return;
    }
    footerEl.hidden = false;
    footerEl.innerHTML = "";
    const summary = document.createElement("span");
    summary.className = "small";
    summary.textContent =
        `Showing ${currentOffset.toLocaleString()} of ${currentTotal.toLocaleString()} traces`;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "button button--text";
    btn.textContent = "Load more";
    btn.addEventListener("click", () => {
        runFetch({
            q: currentQuery,
            offset: currentOffset,
            append: true,
            recordUnfiltered: false,
        });
    });
    footerEl.appendChild(summary);
    footerEl.appendChild(btn);
}

function renderError(message) {
    errorEl.hidden = false;
    errorEl.className = "banner banner--danger list-error";
    errorEl.innerHTML = "";
    const icon = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    icon.setAttribute("class", "banner__icon");
    icon.setAttribute("aria-hidden", "true");
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute("href", "/viz/assets/icons.svg#icon-alert-circle");
    icon.appendChild(use);
    errorEl.appendChild(icon);
    errorEl.appendChild(document.createTextNode(message + " "));
    const retry = document.createElement("button");
    retry.type = "button";
    retry.className = "button button--text";
    retry.textContent = "Retry";
    retry.addEventListener("click", () => {
        runFetch({
            q: currentQuery,
            offset: 0,
            append: false,
            recordUnfiltered: unfilteredTotal === null,
        });
    });
    errorEl.appendChild(retry);
}

function hideError() {
    errorEl.hidden = true;
    errorEl.innerHTML = "";
}

// ---- filter wiring --------------------------------------------------

const debouncedFilter = debounce((q) => {
    runFetch({ q, offset: 0, append: false, recordUnfiltered: false });
}, 250);

function wireFilter() {
    filterInput.addEventListener("input", () => {
        const q = filterInput.value;
        filterClear.hidden = q === "";
        debouncedFilter(q);
    });
    filterInput.addEventListener("keydown", (e) => {
        if (e.key === "Escape") {
            filterInput.value = "";
            filterClear.hidden = true;
            debouncedFilter("");
        }
    });
    filterClear.addEventListener("click", () => {
        filterInput.value = "";
        filterClear.hidden = true;
        filterInput.focus();
        debouncedFilter("");
    });
}

function setFilterLoading(loading) {
    if (loading) {
        filterWrap.classList.add("filter--loading");
    } else {
        filterWrap.classList.remove("filter--loading");
    }
}

function wireFooter() {
    // No-op; footer button is wired per-render in renderFooter() so
    // each Load-more click captures the current paging state.
}
