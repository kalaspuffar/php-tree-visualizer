// API client used by every authenticated page.
//
//   apiFetch(path, options): on 401, redirects to /viz/login.html;
//                            on other failures, throws ApiNetworkError.
//   apiAuthFetch(path, options): does NOT redirect on 401 — the
//                                login page's own POST uses this so
//                                a rejected token renders a banner
//                                instead of redirecting to itself.
//   ApiNetworkError: thrown on network failure or any non-2xx that
//                    isn't 401-via-apiFetch.
//
// Cookies are sent on every call via `credentials: 'same-origin'`.
// The frontend never reads or writes `document.cookie` directly.

/* eslint-env browser */

export class ApiNetworkError extends Error {
    /**
     * @param {string} message  Short description (no request body)
     * @param {number} status   HTTP status, or 0 for connection failure
     */
    constructor(message, status) {
        super(message);
        this.name = "ApiNetworkError";
        this.status = status;
    }
}

// Module-local guard: ensures concurrent 401s issue exactly one
// redirect (design D-12). Exposed via the test seam below so unit
// tests can reset it between cases.
let __redirecting = false;

/**
 * Test seam: replace the redirect implementation (default uses
 * `location.replace`). The hook receives the absolute path to
 * navigate to. Reset to null to restore the real implementation.
 *
 * @type {((url: string) => void) | null}
 */
let __redirectHook = null;

export function _setRedirectHookForTests(hook) {
    __redirectHook = hook;
    __redirecting = false;
}

export function _resetForTests() {
    __redirectHook = null;
    __redirecting = false;
}

/**
 * Test seam for swapping `fetch`. Defaults to the global `fetch`.
 *
 * @type {typeof fetch | null}
 */
let __fetchHook = null;

export function _setFetchHookForTests(hook) {
    __fetchHook = hook;
}

function doFetch(input, init) {
    const f = __fetchHook ?? (typeof fetch === "function" ? fetch : null);
    if (!f) {
        return Promise.reject(new Error("fetch is not available"));
    }
    return f(input, init);
}

function redirectToLogin() {
    if (__redirecting) {
        return;
    }
    __redirecting = true;
    const here =
        (typeof location !== "undefined" ? location.pathname + location.search : "/viz/index.html");
    const next = encodeURIComponent(here);
    const target = `/viz/login.html?next=${next}`;
    if (__redirectHook) {
        __redirectHook(target);
        return;
    }
    if (typeof location !== "undefined") {
        location.replace(target);
    }
}

/**
 * Build the fetch init for a JSON-aware request. If `body` is a plain
 * object, JSON-encode it and set the content-type; otherwise pass
 * through.
 *
 * @param {RequestInit & { body?: any }} options
 * @returns {RequestInit}
 */
function buildInit(options = {}) {
    const init = {
        credentials: "same-origin",
        ...options,
    };
    if (options.body && typeof options.body === "object" && !(options.body instanceof FormData)) {
        init.headers = {
            "Content-Type": "application/json",
            ...(options.headers || {}),
        };
        init.body = JSON.stringify(options.body);
    }
    return init;
}

/**
 * Parse the response body as JSON if the content-type advertises it.
 * Returns null for 204 No Content or for empty bodies.
 *
 * @param {Response} response
 * @returns {Promise<any | null>}
 */
async function parseBody(response) {
    if (response.status === 204) {
        return null;
    }
    const ct = response.headers.get("content-type") || "";
    if (!ct.toLowerCase().includes("application/json")) {
        return null;
    }
    try {
        return await response.json();
    } catch {
        return null;
    }
}

/**
 * Authenticated fetch. On 401, redirects to login (no return); on
 * other failures, throws ApiNetworkError.
 *
 * @param {string} path
 * @param {RequestInit & { body?: any }} [options]
 */
export async function apiFetch(path, options) {
    let response;
    try {
        response = await doFetch(path, buildInit(options));
    } catch (err) {
        const message =
            err && err.message ? `network error: ${err.message}` : "network error";
        throw new ApiNetworkError(message, 0);
    }
    if (response.status === 401) {
        redirectToLogin();
        // Throw so any awaiting code unwinds; the navigation will
        // tear down the page before the rejection surfaces visually.
        throw new ApiNetworkError("session required", 401);
    }
    if (!response.ok && response.status !== 204) {
        throw new ApiNetworkError(`request failed with status ${response.status}`, response.status);
    }
    return parseBody(response);
}

/**
 * Auth-flow fetch. Same as apiFetch but does NOT redirect on 401 —
 * the caller (login.js) handles 401 with its own banner. Other
 * non-2xx still throw ApiNetworkError.
 *
 * @param {string} path
 * @param {RequestInit & { body?: any }} [options]
 * @returns {Promise<{ status: number, body: any | null }>}
 */
export async function apiAuthFetch(path, options) {
    let response;
    try {
        response = await doFetch(path, buildInit(options));
    } catch (err) {
        const message =
            err && err.message ? `network error: ${err.message}` : "network error";
        throw new ApiNetworkError(message, 0);
    }
    const body = await parseBody(response);
    return { status: response.status, body };
}
