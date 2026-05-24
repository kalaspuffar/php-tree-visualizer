// Unit tests for viz/js/api-client.js.

import {
    apiFetch,
    apiAuthFetch,
    ApiNetworkError,
    _setRedirectHookForTests,
    _setFetchHookForTests,
    _resetForTests,
} from "../../viz/js/api-client.js";
import {
    assert_eq,
    assert_true,
    assert_contains,
    assert_throws,
    report_done,
} from "./lib/assert.js";

// Run tests inside an async IIFE so we can await throughout.
const TOKEN = "SENTINEL-TOKEN-" + "x".repeat(40);

await (async () => {
    // ---- 4.5 GET + POST shape ---------------------------------------

    {
        _resetForTests();
        let lastUrl, lastInit;
        _setFetchHookForTests(async (url, init) => {
            lastUrl = url;
            lastInit = init;
            return new Response('{"ok":true}', {
                status: 200,
                headers: { "content-type": "application/json" },
            });
        });

        const body = await apiFetch("/api/traces");
        assert_eq("/api/traces", lastUrl, "apiFetch passes the path");
        assert_eq("same-origin", lastInit.credentials, "credentials default");
        assert_eq({ ok: true }, body, "json body decoded");
    }

    {
        _resetForTests();
        let lastInit;
        _setFetchHookForTests(async (_url, init) => {
            lastInit = init;
            return new Response(null, { status: 204 });
        });

        const body = await apiFetch("/api/auth/logout", {
            method: "POST",
            body: { hello: "world" },
        });
        assert_eq(null, body, "204 returns null");
        assert_eq("POST", lastInit.method, "method propagates");
        assert_eq(
            "application/json",
            lastInit.headers["Content-Type"],
            "JSON content-type set for object body"
        );
        assert_eq('{"hello":"world"}', lastInit.body, "object body is JSON-encoded");
    }

    // ---- 4.6 401 redirects via the hook; second 401 short-circuits --

    {
        _resetForTests();
        const redirected = [];
        _setRedirectHookForTests((url) => redirected.push(url));
        _setFetchHookForTests(async () =>
            new Response('{"error":"unauthorized"}', {
                status: 401,
                headers: { "content-type": "application/json" },
            })
        );

        const err1 = await assert_throws(
            ApiNetworkError,
            () => apiFetch("/api/traces"),
            "401 throws ApiNetworkError"
        );
        assert_eq(401, err1.status, "ApiNetworkError.status = 401");
        assert_eq(1, redirected.length, "first 401 redirects");

        // Second 401 should NOT call the hook again.
        await assert_throws(
            ApiNetworkError,
            () => apiFetch("/api/traces"),
            "second 401 still throws"
        );
        assert_eq(1, redirected.length, "second 401 does NOT re-redirect");

        // Redirect target includes /viz/login.html?next=…
        assert_contains(redirected[0], "/viz/login.html?next=", "redirect target shape");
    }

    // ---- 4.7 fetch rejection -> ApiNetworkError(status=0) -----------

    {
        _resetForTests();
        _setFetchHookForTests(async () => {
            throw new Error("connection refused");
        });

        const err = await assert_throws(
            ApiNetworkError,
            () => apiFetch("/api/traces"),
            "rejection throws ApiNetworkError"
        );
        assert_eq(0, err.status, "rejection status = 0");
        assert_contains(err.message, "network error", "message tagged as network error");
        assert_contains(err.message, "connection refused", "includes original message");
    }

    // ---- 4.8 5xx -> ApiNetworkError without leaking body -----------

    {
        _resetForTests();
        const bodyWithSecret = `{"token":"${TOKEN}"}`;
        _setFetchHookForTests(async () =>
            new Response(bodyWithSecret, {
                status: 500,
                headers: { "content-type": "application/json" },
            })
        );

        const err = await assert_throws(
            ApiNetworkError,
            () => apiFetch("/api/traces"),
            "500 throws"
        );
        assert_eq(500, err.status, "status = 500");
        assert_true(
            !err.message.includes(TOKEN),
            "request body / token NOT in error message"
        );
    }

    // ---- apiAuthFetch does NOT redirect on 401 ---------------------

    {
        _resetForTests();
        const redirected = [];
        _setRedirectHookForTests((url) => redirected.push(url));
        _setFetchHookForTests(async () =>
            new Response('{"error":"unauthorized"}', {
                status: 401,
                headers: { "content-type": "application/json" },
            })
        );

        const r = await apiAuthFetch("/api/auth", {
            method: "POST",
            body: { token: TOKEN },
        });
        assert_eq(401, r.status, "apiAuthFetch returns 401 to caller");
        assert_eq(0, redirected.length, "apiAuthFetch does NOT redirect on 401");
        assert_eq({ error: "unauthorized" }, r.body, "body is parsed");
    }

    // ---- apiAuthFetch on network rejection throws ------------------

    {
        _resetForTests();
        _setFetchHookForTests(async () => {
            throw new Error("dns failure");
        });
        const err = await assert_throws(
            ApiNetworkError,
            () => apiAuthFetch("/api/auth", { method: "POST", body: { token: TOKEN } }),
            "apiAuthFetch on rejection throws"
        );
        assert_eq(0, err.status, "rejection status = 0");
        assert_true(!err.message.includes(TOKEN), "no token in error message");
    }

    // ---- apiAuthFetch 204 success ----------------------------------

    {
        _resetForTests();
        _setFetchHookForTests(async () => new Response(null, { status: 204 }));
        const r = await apiAuthFetch("/api/auth", {
            method: "POST",
            body: { token: TOKEN },
        });
        assert_eq(204, r.status, "204 success surfaced to caller");
        assert_eq(null, r.body, "no body on 204");
    }
})();

report_done();
