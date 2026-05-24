// Styled tooltip popper (§3.3.8.7).
//
//   wireTooltip(element, text?)            direct wiring
//   wireTooltipDelegated(rootEl, selector, textFn?)
//                                          one delegated listener
//                                          across many children
//                                          (useful for the
//                                          virtualized tree)
//   computeTooltipPosition({ rect, viewportHeight, popperHeight })
//                                          pure positioning math
//                                          (exported for tests)
//
// Implementation:
//   - 500-ms hover delay; immediate-show on focus.
//   - 200-ms grace period before fade-out.
//   - Position above the element by 8 px; flip below if that would
//     clip the viewport top.
//   - prefers-reduced-motion: handled by the CSS rule on
//     .phptv-tooltip — no per-show JS check needed.

/* eslint-env browser */

const SHOW_DELAY = 500;        // ms hover before show
const HIDE_DELAY = 200;        // ms grace period before hide
const OFFSET     = 8;          // px gap between trigger + popper

let popperEl = null;
let hideTimer = null;
let showTimer = null;

function getPopper() {
    if (popperEl) return popperEl;
    if (typeof document === "undefined") return null;
    popperEl = document.createElement("div");
    popperEl.className = "phptv-tooltip";
    popperEl.setAttribute("role", "tooltip");
    popperEl.hidden = true;
    document.body.appendChild(popperEl);
    return popperEl;
}

/**
 * Pure: choose the tooltip's `top` and placement.
 *
 * @param {{ rect: {top: number, bottom: number}, viewportHeight: number, popperHeight: number }} ctx
 * @returns {{ top: number, placement: "above"|"below" }}
 */
export function computeTooltipPosition(ctx) {
    const aboveY = ctx.rect.top - ctx.popperHeight - OFFSET;
    if (aboveY < OFFSET) {
        return { top: ctx.rect.bottom + OFFSET, placement: "below" };
    }
    return { top: aboveY, placement: "above" };
}

function show(triggerEl, text) {
    const popper = getPopper();
    if (!popper) return;
    popper.textContent = text;
    popper.hidden = false;
    // Position after content is set so popperHeight is meaningful.
    requestAnimationFrame(() => {
        const rect = triggerEl.getBoundingClientRect();
        const { top, placement } = computeTooltipPosition({
            rect: { top: rect.top, bottom: rect.bottom },
            viewportHeight: window.innerHeight,
            popperHeight: popper.offsetHeight,
        });
        popper.style.top = `${top}px`;
        popper.style.left = `${rect.left}px`;
        popper.dataset.placement = placement;
        popper.classList.add("is-visible");
    });
}

function scheduleHide() {
    const popper = getPopper();
    if (!popper) return;
    if (hideTimer !== null) clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
        popper.classList.remove("is-visible");
        popper.hidden = true;
        hideTimer = null;
    }, HIDE_DELAY);
}

function cancelHide() {
    if (hideTimer !== null) {
        clearTimeout(hideTimer);
        hideTimer = null;
    }
}

/**
 * Direct wiring for a single element.
 *
 * @param {HTMLElement} element
 * @param {string} [text]  defaults to element.title (which is
 *                         moved to data-tooltip-original-title to
 *                         avoid the native browser tooltip).
 */
export function wireTooltip(element, text) {
    if (!element) return;
    const resolvedText = typeof text === "string" && text.length > 0
        ? text
        : element.getAttribute("title") || "";
    if (resolvedText === "") return;

    // Move title aside so the browser doesn't fire the native popup.
    if (element.hasAttribute("title")) {
        element.setAttribute("data-tooltip-original-title", element.getAttribute("title"));
        element.removeAttribute("title");
    }

    element.addEventListener("mouseenter", () => {
        cancelHide();
        if (showTimer !== null) clearTimeout(showTimer);
        showTimer = setTimeout(() => {
            showTimer = null;
            show(element, resolvedText);
        }, SHOW_DELAY);
    });
    element.addEventListener("mouseleave", () => {
        if (showTimer !== null) {
            clearTimeout(showTimer);
            showTimer = null;
        }
        scheduleHide();
    });
    element.addEventListener("focus", () => {
        cancelHide();
        if (showTimer !== null) {
            clearTimeout(showTimer);
            showTimer = null;
        }
        show(element, resolvedText);
    });
    element.addEventListener("blur", () => {
        scheduleHide();
    });
}

/**
 * One listener on `rootEl` handles every matching descendant. The
 * tooltip text is read via `textFn(target)` (default: target.title
 * fallback). Used for the virtualized tree where re-wiring per row
 * per render would be wasteful.
 *
 * @param {HTMLElement} rootEl
 * @param {string} selector  CSS selector; `event.target.closest(selector)`
 * @param {(target: HTMLElement) => string} [textFn]
 */
export function wireTooltipDelegated(rootEl, selector, textFn) {
    if (!rootEl) return;
    let activeTarget = null;
    let activeText = "";

    const resolveText = (el) => {
        if (typeof textFn === "function") return textFn(el) || "";
        return el.getAttribute("data-tooltip-original-title")
            || el.getAttribute("title")
            || el.getAttribute("data-tooltip")
            || "";
    };

    rootEl.addEventListener("mouseenter", (event) => {
        const target = event.target.closest?.(selector);
        if (!target || !rootEl.contains(target)) return;
        const text = resolveText(target);
        if (!text) return;
        activeTarget = target;
        activeText = text;
        cancelHide();
        if (showTimer !== null) clearTimeout(showTimer);
        showTimer = setTimeout(() => {
            showTimer = null;
            show(activeTarget, activeText);
        }, SHOW_DELAY);
    }, true);

    rootEl.addEventListener("mouseleave", (event) => {
        const target = event.target.closest?.(selector);
        if (!target || target !== activeTarget) return;
        if (showTimer !== null) {
            clearTimeout(showTimer);
            showTimer = null;
        }
        scheduleHide();
        activeTarget = null;
    }, true);

    rootEl.addEventListener("focusin", (event) => {
        const target = event.target.closest?.(selector);
        if (!target || !rootEl.contains(target)) return;
        const text = resolveText(target);
        if (!text) return;
        activeTarget = target;
        cancelHide();
        if (showTimer !== null) {
            clearTimeout(showTimer);
            showTimer = null;
        }
        show(target, text);
    });

    rootEl.addEventListener("focusout", (event) => {
        if (event.target === activeTarget) {
            scheduleHide();
            activeTarget = null;
        }
    });
}
