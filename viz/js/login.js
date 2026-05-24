// Login page wiring.
//
//  - POSTs the token to /api/auth via apiAuthFetch (no 401 redirect:
//    the login page is itself the destination).
//  - On 204: navigates to the validated `next` query param (must
//    start with /viz/) or /viz/index.html.
//  - On 401: renders the "Token rejected" banner.
//  - On network/unexpected error: renders the "Couldn't reach" banner.

import { apiAuthFetch, ApiNetworkError } from "./api-client.js";

const form        = document.getElementById("login-form");
const tokenInput  = document.getElementById("login-token");
const submitBtn   = document.getElementById("login-submit");
const submitIcon  = document.getElementById("login-submit-icon");
const submitLabel = document.getElementById("login-submit-label");
const banner      = document.getElementById("login-banner");
const bannerText  = document.getElementById("login-banner-text");

const COPY_REJECTED      = "Token rejected. Check the value with your operator.";
const COPY_NETWORK_ERROR = "Couldn't reach the API. Retry in a moment.";

form.addEventListener("submit", onSubmit);

async function onSubmit(event) {
    event.preventDefault();
    hideBanner();
    setSubmitting(true);

    const token = tokenInput.value;

    try {
        const response = await apiAuthFetch("/api/auth", {
            method: "POST",
            body: { token },
        });

        if (response.status === 204) {
            // Replace, not assign — the back button shouldn't return to login.
            location.replace(resolveNext());
            return;
        }

        if (response.status === 401) {
            showBanner(COPY_REJECTED);
            tokenInput.focus();
            tokenInput.select();
        } else {
            // Any other status (5xx, unexpected 4xx): same UX as a
            // network failure. The user retries; the operator
            // inspects logs.
            showBanner(COPY_NETWORK_ERROR);
        }
    } catch (err) {
        if (err instanceof ApiNetworkError) {
            showBanner(COPY_NETWORK_ERROR);
        } else {
            // Unknown error class — surface the generic copy. Do not
            // include the error message in the user-facing string;
            // operator-side inspection covers detail.
            showBanner(COPY_NETWORK_ERROR);
        }
    } finally {
        setSubmitting(false);
    }
}

/**
 * `next` query param: must start with `/viz/` to be honored. Anything
 * else (absolute URL to another origin, /api/*, /etc.) falls back to
 * the default. Prevents open-redirect via a crafted login URL.
 */
function resolveNext() {
    const fallback = "/viz/index.html";
    const params = new URLSearchParams(location.search);
    const raw = params.get("next");
    if (typeof raw !== "string") return fallback;
    if (!raw.startsWith("/viz/")) return fallback;
    return raw;
}

function setSubmitting(submitting) {
    submitBtn.disabled = submitting;
    tokenInput.readOnly = submitting;
    submitLabel.textContent = submitting ? "Signing in…" : "Sign in";
    submitIcon.hidden = !submitting;
}

function showBanner(copy) {
    bannerText.textContent = copy;
    banner.hidden = false;
}

function hideBanner() {
    banner.hidden = true;
    bannerText.textContent = "";
}
