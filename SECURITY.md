# Security

This document covers the project's threat model, the bearer-token model, the no-token-in-logs invariant, and how to report a vulnerability.

## Threat model

The collector and the web stack are designed for deployment on a **trusted internal network**. There is **no path to the public internet** in the supported topology, and the spec assumes that one is not introduced (NF-4.4, A-4, C-3.2 in [SPECIFICATION.md §6.1](./SPECIFICATION.md)). Concretely:

- The collector binds a loopback socket (`127.0.0.1` or `[::1]`). Configurations that bind anything else are rejected at startup with a single stderr line and exit code 2.
- The reverse proxy that fronts the PHP API and the static frontend is the only path through which external requests reach the collector's HTTP endpoint, and it MUST terminate TLS for any traffic it serves on a network segment broader than `lo`.
- The PHP API and the static frontend require a session cookie derived from the bearer token; sessions are HMAC-signed with a server-side salt.

If you need to expose this service on the public internet, the threat model needs to be redone first. The current code does not consider that audience and has not been hardened against it (no rate limiting, no anti-bot measures, no public-internet-grade auditing).

For depth on the threat model — including each documented threat and how the design addresses it — see [SPECIFICATION.md §6](./SPECIFICATION.md).

## Bearer-token model

The collector accepts MessagePack ingest only from a client that carries a bearer token matching `[auth].token` in the TOML configuration. The token is a long-lived string:

- **Length:** ≥32 characters (validated at config load; shorter configs are rejected at startup).
- **Distinct from `[auth].session_salt`:** the same string cannot be used as both the bearer token and the session salt (validated at config load).
- **Comparison:** constant-time on the request hot path, so timing observations cannot leak the token byte by byte.

**Rotation.** There is no in-process reload of the token. To rotate:

1. Edit `[auth].token` in the collector's TOML config (the path the binary was started with, typically `/etc/php-tree-viz/collector.toml`).
2. Restart the collector binary (e.g. `systemctl restart php-tree-viz-collector`).
3. Push the new token to the upstream `php-analyze` extension's configuration on every host that POSTs to this collector.

The session cookie used by the PHP API and frontend is an HMAC of the bearer token and `[auth].session_salt`. Rotating either invalidates every active session — operators sign back in with the new token.

## INV-2: no token in logs

The collector MUST NOT emit the bearer token, the session salt, or the literal ASCII string `Authorization` in any log event. This is invariant INV-2 in the spec (see [SPECIFICATION.md §6.4](./SPECIFICATION.md)).

The invariant is verified by an automated regression test under [`crates/php-tree-viz-collector/tests/observability.rs`](./crates/php-tree-viz-collector/tests/observability.rs). The test installs the real `tracing` subscriber, drives a representative session (auth-missing → wrong-token → accepted batches → finalize → retention → shutdown), and asserts that none of the three byte sequences appears anywhere in captured stdout or stderr.

If you contribute to the collector and need to log anything that might carry header content, the existing `Secret`-wrapping types in `crates/php-tree-viz-collector/src/config/secret.rs` redact via `Debug` and `Display`. Use them; never `format!("{cfg:?}")` against a non-wrapped type that could contain a token.

## Reporting a vulnerability

Email security reports to **mailto.woden@gmail.com**. Please include:

- A clear description of the vulnerability and the affected version (commit hash or release tag).
- A minimal reproducer if you have one.
- Your name or handle, if you'd like to be credited in the fix's commit message.

**Expected first response:** within **10 business days**. If you don't hear back, please retry — the project is solo-maintained and the inbox is checked manually.

**Scope.** This reporting channel covers vulnerabilities in:

- The Rust collector under `crates/php-tree-viz-collector/`.
- The PHP API under `api/`.
- The static frontend under `viz/`.
- The reverse-proxy snippet shapes under `etc/`.

**Out of scope:** vulnerabilities in the upstream `php-analyze` PHP extension itself. The extension is a separate project at <https://github.com/kalaspuffar/php-analyze> and has its own reporting channel. If you find an issue in the extension's wire-format handling that affects this collector's behaviour, please file with the extension first; the visualizer's role in that chain is downstream.

Please do **not** open public GitHub issues for security problems. Email first; a public discussion can follow once a fix is shipped.

---

Back to [README.md](./README.md). For the full threat model and design rationale, see [SPECIFICATION.md §6](./SPECIFICATION.md).
