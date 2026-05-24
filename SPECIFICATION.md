# Project Specification: PHP Profiling Tree Visualizer

| | |
| --- | --- |
| **Version** | 0.1 (initial draft) |
| **Date** | 2026-05-23 |
| **Author** | Solution Architect (Claude Code session) |
| **Status** | Draft — pending stakeholder review |
| **Source requirements** | `REQUIREMENTS.md` v0.1 |
| **Wire contract** | `handover/{WIRE_FORMAT,HTTP_CONTRACT,OPERATIONAL_NOTES}.md` |
| **Authority chain** | Upstream `php-analyze` `SPECIFICATION.md` § 4.2 > `crates/php-analyze/src/wire.rs` > `handover/WIRE_FORMAT.md` |

Requirement IDs from `REQUIREMENTS.md` are cited inline (e.g. *F-1.7*, *NF-3.1*) so every design choice is traceable. Where a section satisfies multiple requirements, all are listed.

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Architecture Overview](#2-architecture-overview)
3. [System Components](#3-system-components)
4. [Data Architecture](#4-data-architecture)
5. [API Specifications](#5-api-specifications)
6. [Security Architecture](#6-security-architecture)
7. [Infrastructure and Deployment](#7-infrastructure-and-deployment)
8. [Integration Points](#8-integration-points)
9. [Testing Strategy](#9-testing-strategy)
10. [Implementation Plan](#10-implementation-plan)
11. [Risks and Mitigations](#11-risks-and-mitigations)
12. [Appendices](#12-appendices)

---

## 1. Executive Summary

### 1.1 Project overview

The visualizer is a self-hosted three-tier system, co-located on the existing PHP dev server, that consumes MessagePack profiling batches from the `php-analyze` PHP extension and presents them to developers as an interactive JProfiler-style call tree. The system is one Rust binary (ingest + decode + storage), one PHP API directory served by the existing PHP-FPM, one static frontend, and a flat filesystem of SQLite files acting as the persistence layer.

### 1.2 Key architectural decisions

| ID | Decision | Resolves |
| --- | --- | --- |
| AD-1 | **Async-decode ingest model.** The collector fsyncs raw `.msgpack` bytes to disk, returns 2xx, and a worker thread decodes them off the request path. | Q-2, NF-3.1, F-1.7 |
| AD-2 | **SQLite per trace + a single index DB.** One `<key>.sqlite` per trace holds its dict + aggregated tree; `index.sqlite` holds the trace-list rows. | Q-1, C-1.4, F-3.3, NF-2.1 |
| AD-3 | **Incremental aggregation with idle-finalize.** Each batch updates the aggregated tree on arrival; a trace is `finalized` after 30 s of no new batches. | F-3.3, F-3.4, NF-1.3 |
| AD-4 | **Raw batches retained for the full retention window.** Source of truth; allows rebuild if aggregation rules change. | F-3.5, R-7 |
| AD-5 | **Single Rust binary, four roles.** HTTP server, decoder worker, idle-finalizer, retention sweeper — all in one process connected by in-memory channels. | NF-6.1, NF-6.3 |
| AD-6 | **Filesystem-only IPC between Rust and PHP.** PHP opens SQLite files read-only (WAL mode); no internal RPC. | C-1.2, NF-6.1 |
| AD-7 | **Token as low-ceremony shared secret.** Treated as a "wrong destination" guard, not a security boundary. Same token gates ingest and UI. No rotation grace. | F-1.2, NF-4.1, stakeholder confirmation |
| AD-8 | **TLS terminated at the reverse proxy.** The Rust collector and PHP-FPM speak plain HTTP on localhost. | NF-4.5, NF-4.4, stakeholder confirmation |
| AD-9 | **Trace-key abstraction layer.** Storage keys are `(host, pid, start_time_ns)`-derived while `trace_id` is all-zero, swap to `trace_id` once distinct UUIDs ship — no schema change. | F-2.1, F-2.2, R-6 |
| AD-10 | **Frontend = vanilla JS + D3 hierarchy + a custom virtualizer.** No framework. | Q-3, NF-5.1 |

### 1.3 Success criteria handle

The MVP is complete when criteria *S-1* through *S-8* in `REQUIREMENTS.md` § 15 hold. Each of those maps to one or more acceptance tests in § 9.2 below.

---

## 2. Architecture Overview

### 2.1 Deployment view

```
┌────────────────────────────── Dev server (one host) ──────────────────────────────┐
│                                                                                    │
│  php-analyze ──HTTPS──▶ ┌─────────────────┐                                        │
│  (PHP ext on            │ Reverse proxy   │ (existing nginx or apache;             │
│   this host or another) │ TLS terminator  │  serves the dev application too)      │
│                         └────────┬────────┘                                        │
│                                  │                                                 │
│            ┌─────────────────────┼─────────────────────┐                          │
│            │                     │                     │                          │
│            ▼                     ▼                     ▼                          │
│    /ingest/v1            /api/* (PHP-FPM)        /viz/* (static)                  │
│    Rust collector :8088   php-tree-viz.so?       index.html, *.js, *.css          │
│    plain HTTP loopback    no — plain PHP files                                    │
│            │                     │                     │                          │
│            │ writes              │ reads               │                          │
│            ▼                     ▼                     ▼                          │
│    ┌─────────────────────────────────────────────────────────────────┐           │
│    │  /var/lib/php-tree-viz/                                          │           │
│    │    index.sqlite                          (WAL mode)              │           │
│    │    traces/                                                       │           │
│    │      <trace_key>.sqlite                  (WAL mode, per trace)   │           │
│    │      <trace_key>.raw/                                            │           │
│    │        batch-0001.msgpack                                        │           │
│    │        batch-0002.msgpack                                        │           │
│    │        ...                                                       │           │
│    │    tmp/                                                          │           │
│    │      <random>.partial                    (in-flight uploads)     │           │
│    └─────────────────────────────────────────────────────────────────┘           │
│                                                                                    │
│    systemd: php-tree-viz-collector.service     (the Rust binary)                  │
│             php-tree-viz-collector.socket      (optional; for activation)         │
│                                                                                    │
└────────────────────────────────────────────────────────────────────────────────────┘
```

### 2.2 Process and thread view (inside the Rust collector)

```
┌──────────────────────── php-tree-viz-collector ─────────────────────────┐
│                                                                          │
│   HTTP server (hyper/axum)         ◀──── POST /ingest/v1                │
│     ↓ accept                                                             │
│     ↓ verify Authorization header                                        │
│     ↓ verify Content-Type                                                │
│     ↓ write body → tmp/<rand>.partial (streamed, length-bounded)         │
│     ↓ peek meta.schema_version (cheap MessagePack header read)           │
│     ↓ if != 1: delete tmp file, return 422                              │
│     ↓ rename → traces/<key_pending>.raw/batch-NNNN.msgpack               │
│     ↓ fsync(file) + fsync(parent dir)                                    │
│     ↓ enqueue (raw_path, key_hint) → mpsc(bounded=256)                  │
│     ↓ return 200                                                         │
│                                                                          │
│   Decoder worker (single thread; cpu-bound work)                         │
│     ↓ recv from mpsc                                                     │
│     ↓ full MessagePack decode                                            │
│     ↓ resolve TraceKey (real trace_id ⊕ synthesized fallback)            │
│     ↓ open or create <key>.sqlite (WAL mode, single connection)          │
│     ↓ upsert trace_meta, accumulate dict, fold calls into nodes          │
│     ↓ resolve pending_calls whose parent now exists                      │
│     ↓ update index.sqlite row (state='active', counters, last_batch_at)  │
│     ↓ COMMIT                                                             │
│                                                                          │
│   Idle-finalizer (tokio interval, 5 s tick)                              │
│     ↓ SELECT trace_key FROM index WHERE state='active'                   │
│     ↓        AND last_batch_at_ns < now - 30s                            │
│     ↓ for each: resolve dangling pending_calls, mark state='finalized'   │
│                                                                          │
│   Retention sweeper (tokio interval, 1 h tick)                           │
│     ↓ SELECT trace_key FROM index WHERE start_time_ns < now - <window>   │
│     ↓ for each: unlink <key>.sqlite, rm -rf <key>.raw/, DELETE row       │
│                                                                          │
└──────────────────────────────────────────────────────────────────────────┘
```

### 2.3 Non-negotiable invariants

| INV | Statement | Why |
| --- | --- | --- |
| INV-1 | HTTP 200 is returned **only after** `fsync(raw_file)` and `fsync(raw_dir)` both return success. | F-1.7, NF-3.1, durability promise: extension forgets the batch on 2xx and has no spool |
| INV-2 | The collector never reads or logs the `Authorization` header content. | NF-4.3, S-8 |
| INV-3 | `t_in`, `t_out` (`CLOCK_MONOTONIC`) and `start_time` (`CLOCK_REALTIME`) are never subtracted or compared across domains. Wall-clock display is reconstructed only via `start_time + (t_in − first_call.t_in)`. | BR-4, BR-5 |
| INV-4 | Aggregated tree numbers (`total_wall_ns`, `self_wall_ns`, etc.) are sums computed once and updated incrementally; the same number is never derived from two different sources. | BR-1, BR-2, BR-3 |
| INV-5 | `schema_version != 1` returns non-2xx and the raw file is deleted. | F-1.5, NF-7.3 |
| INV-6 | Unknown extra keys inside v1 maps are silently ignored. | F-1.8, NF-7.2 |
| INV-7 | The bounded mpsc between accept and decoder caps in-memory liability; on full → return 503 → `php-analyze` retries. | NF-3.3 |
| INV-8 | The PHP API never writes to any SQLite file. Read-only opens, WAL-mode shared with the Rust writer. | AD-6 |

---

## 3. System Components

### 3.1 Rust collector (`php-tree-viz-collector`)

**Purpose:** Accept MessagePack batches over HTTP, persist them durably, decode them asynchronously into a per-trace SQLite store, finalize idle traces, prune expired traces.

**Sub-modules:**

| Module | Responsibility |
| --- | --- |
| `http::server` | Bind, route, parse headers, stream body to disk. Built on `hyper` + `axum` (or `actix-web`); choice is implementation detail. |
| `wire::decode` | MessagePack → typed structs per `crates/php-analyze/src/wire.rs`. Use `rmp-serde` per *D-2*. |
| `storage::index` | All reads/writes against `index.sqlite`. Owns the connection pool (one writer + N readers; readers used by idle-finalizer/sweeper for listing). |
| `storage::trace` | Per-trace SQLite open/upsert/aggregate. One connection cached per active trace; closed on idle-finalize. |
| `aggregate` | Pure functions: fold a decoded `Call` into an aggregated `Node`; resolve pending children when a parent arrives. |
| `tracekey` | The `TraceKey` abstraction (see § 4.1.1). |
| `finalize` | Idle-finalizer loop. |
| `retention` | Retention sweeper loop. |
| `config` | Load TOML config file, validate. |
| `obs` | Structured logging (token-redacted; one log line per accepted batch per *F-1.10*). |

**Interfaces:**

- *Inbound:* HTTP `POST /ingest/v1` (see § 5.1).
- *Outbound:* none over the network. All persistence is local filesystem.
- *Configuration:* `/etc/php-tree-viz/collector.toml` (paths, bearer token, retention window, queue size, bind address).

**Implementation notes:**

- Stream the request body directly to `tmp/<rand>.partial`; do not buffer in memory beyond a small (~64 KiB) read window. The `Content-Length` header may not be present (`php-analyze` is allowed to use chunked encoding); enforce a hard cap (e.g., 64 MiB per batch — well above the typical 1 MB) and 413 over that.
- The `tmp/` directory is on the same filesystem as `traces/` so the post-fsync `rename(2)` is atomic.
- One decoder thread is sufficient at the documented 25 MB/s peak (msgpack decode of ~10 K calls is single-digit ms with `rmp-serde`). The mpsc is sized 256 batches (~256 MiB worst case at typical batch size). Tunable in config.
- Per-trace SQLite connections are cached in an LRU keyed by `TraceKey`. The LRU evicts on idle-finalize or when its size exceeds a configured cap (default 64 concurrent open traces — far above any realistic concurrent-trace count for a <10-developer dev server).

**Acceptance criteria:**

- AC-3.1.1 — Accepts `POST` against the configured path with valid token + content-type and returns 200 within 100 ms of receiving the last byte (single-digit ms typical). *F-1.1, F-1.7*
- AC-3.1.2 — Returns 401 on wrong/missing token, 415 on wrong content-type, 422 on `schema_version != 1`, 400 on malformed MessagePack, 503 on full queue, 413 on oversize body. *F-1.3, F-1.4, F-1.5, F-1.6, NF-3.3*
- AC-3.1.3 — A `SIGKILL` against the process at any point loses no batch that was acknowledged with 2xx. *S-6, NF-3.4*
- AC-3.1.4 — Logs of a normal session contain zero occurrences of the configured token. *S-8, NF-4.3*

### 3.2 PHP API (under `/api/*`)

**Purpose:** Serve JSON to the browser. Read-only over the SQLite store written by the Rust collector.

**Layout:** Plain PHP files under a directory served by the existing PHP-FPM. No framework. PDO (with `sqlite` driver) for database access.

| File | Endpoint |
| --- | --- |
| `auth.php` | `POST /api/auth`, `POST /api/auth/logout` |
| `traces.php` | `GET /api/traces` |
| `trace.php` | `GET /api/traces/{key}`, `GET /api/traces/{key}/tree`, `GET /api/traces/{key}/tree/{node_id}/children` |
| `bootstrap.php` | Shared: config load, session check, SQLite open helpers. |

**Implementation notes:**

- PDO opens SQLite files with `PDO::ATTR_DEFAULT_FETCH_MODE = PDO::FETCH_ASSOC` and `?mode=ro`. WAL mode set by Rust at create time; PHP inherits it.
- Sessions: native PHP sessions backed by the filesystem session store. Cookie name `phptv_session`. The session value is an HMAC of the token plus a server-side random salt (the salt is stored in the collector config; PHP reads the same config file). On login, PHP verifies `token == config.token`; on success, sets the cookie.
- All endpoints require a valid session cookie except `POST /api/auth`. No CSRF protection in MVP — the entire surface is local and the token is low-stakes (*AD-7*); annotate this as a known constraint in operator docs.
- JSON encoding via `json_encode($data, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES)`.

**Acceptance criteria:**

- AC-3.2.1 — All listed endpoints respond with the documented JSON shape (see § 5).
- AC-3.2.2 — Requests without a valid session cookie to any non-auth endpoint return 401.
- AC-3.2.3 — The PHP code does not open any SQLite file in read-write mode (verified by grepping for `mode=rw` absence and PDO flag inspection).

### 3.3 Static frontend (under `/viz/*`)

**Purpose:** Render the trace list and the call tree in the browser.

**Stack:** Vanilla JS (ES2020+, ESM modules), HTML, CSS. D3 v7 for hierarchy layout primitives only — not for rendering (rendering is plain DOM via a custom virtualized list, see § 3.3.2).

**Pages:**

| Page | URL | Purpose |
| --- | --- | --- |
| Login | `/viz/login.html` | Token entry. POSTs to `/api/auth`. |
| Trace list | `/viz/index.html` | Renders `GET /api/traces`. Substring filter input. |
| Trace detail | `/viz/trace.html?key=<trace_key>` | Renders the aggregated tree. Lazy expansion. |

**Implementation notes:**

#### 3.3.1 Tree rendering model

The tree is rendered as a virtualized vertical list of rows, **not** a SVG tree. Each visible row corresponds to one expanded node; indentation conveys depth. This is the JProfiler model and is the lowest-friction way to hit *NF-1.3* (1 M-call trace rendered in ≤5 s) and *NF-1.4* (subtree expand ≤500 ms for ≤10 K nodes).

#### 3.3.2 Custom virtualizer

The visible list is a fixed-height scroll viewport that renders only the rows currently in view (+ a small overscan). The full flattened list of expanded node IDs is held in memory; rendering N visible rows is O(N), not O(tree-size). This easily handles 10 K visible expanded nodes; the practical lid is what fits in scroll perception, not what fits in RAM.

#### 3.3.3 Lazy expansion

Default: tree loaded to depth 2 from the root. Clicking a row's expand chevron fetches its children via `/api/traces/{key}/tree/{node_id}/children` and splices them into the flattened list at the correct position.

#### 3.3.4 Sort and search

- Children are returned pre-sorted by the requested column (per `?sort=`). Re-sort by clicking a column header → re-fetch.
- In-tree search (*F-6.10*, Should-have) is a client-side substring match over already-loaded node fqns; matches are highlighted and the viewport scrolls to the first.

#### 3.3.5 Design System

The frontend's visual tokens. Implemented as CSS custom properties on `:root` so a future light theme (deferred to v1.1) is a token-swap, not a rewrite. *Resolves: NF-5.1.*

##### 3.3.5.1 Color tokens

```css
:root {
  /* Surfaces */
  --bg:            #0E1116;
  --surface:       #161B23;
  --surface-2:     #1E2530;   /* hover, sticky headers */
  --surface-3:     #28303D;   /* pressed, header-hover */
  --border:        #2A3340;
  --border-strong: #3B475A;

  /* Foregrounds */
  --fg:           #E6E8EB;
  --fg-muted:     #98A2B3;
  --fg-subtle:    #5C6675;
  --fg-on-accent: #06212A;

  /* Brand / interactive */
  --accent:        #22D3EE;
  --accent-strong: #06B6D4;
  --accent-bg:     #164E5E;   /* selected row, current match */

  /* Semantic */
  --success:    #34D399;
  --warn:       #FBBF24;
  --warn-bg:    #3B2E0F;
  --danger:     #F87171;
  --danger-bg:  #3F1B1F;
  --info:       #60A5FA;
  --info-bg:    #1E2A3F;

  /* Hot-path gradient stops (interpolated by %parent at render time) */
  --hot-0:      #0E7490;   /*   0% */
  --hot-1:      #0891B2;
  --hot-2:      #22D3EE;
  --hot-3:      #67E8F9;   /* 100% */

  /* Focus ring */
  --focus:      #22D3EE;
}
```

Contrast against `--bg`: `--fg` 13.8:1 (AAA body), `--fg-muted` 6.4:1 (AA body), `--accent` 8.9:1, `--success`/`--warn`/`--danger`/`--info` all ≥4.5:1. Tertiary `--fg-subtle` (3.5:1) is used only for placeholders and never for essential information. *Resolves: AC-3.3.5.*

##### 3.3.5.2 Typography

```css
--font-sans: -apple-system, BlinkMacSystemFont, "Segoe UI", Inter,
             system-ui, sans-serif;
--font-mono: ui-monospace, "JetBrains Mono", "SF Mono",
             Menlo, Consolas, monospace;
```

Numeric cells use `font-variant-numeric: tabular-nums` so digits align vertically across rows. The call-tree page sets 14 px base; login and trace-list pages set 15 px.

| Role | Size / line-height | Weight | Family | Use |
| --- | --- | --- | --- | --- |
| display | 24 / 1.25 | 600 | sans | Page title (Login, Trace list) |
| h1 | 20 / 1.3 | 600 | sans | Section headings |
| h2 | 17 / 1.35 | 600 | sans | Sub-headings |
| body | 14 / 1.45 | 400 | sans | Default body, table cells |
| body-strong | 14 / 1.45 | 500 | sans | Trace-list row primary line |
| small | 12 / 1.4 | 400 | sans | file:line, secondary metadata, hints |
| micro | 11 / 1.35 | 500 | sans (uppercase, +0.04em tracking) | Column headers, badges, chip labels |
| code | 13 / 1.4 | 400 | mono, tabular | fqn cell, all numeric columns |

##### 3.3.5.3 Spacing scale

Base 8 px. 4 px is the only sub-base step (icon-to-text inline gaps).

| Token | Value | Use |
| --- | --- | --- |
| `--s-1` | 4 px  | inline icon-to-text gap |
| `--s-2` | 8 px  | small padding, gap between adjacent metric columns |
| `--s-3` | 12 px | row vertical padding, chip horizontal padding |
| `--s-4` | 16 px | indent per tree depth, card inner padding |
| `--s-5` | 24 px | page gutters (narrow viewport), card margin |
| `--s-6` | 32 px | section gaps |
| `--s-7` | 48 px | page gutters (≥1280 px viewport) |

##### 3.3.5.4 Density

| Element | Value |
| --- | --- |
| Call-tree row | 28 px tall (6 px top / 16 px content / 6 px bottom) |
| Call-tree sticky column header | 32 px tall |
| Call-tree sticky search bar | 40 px tall |
| Indent per tree depth | 16 px |
| Chevron hit area | 24 × 24 px (visual icon 16 × 16) |
| Trace-list row | 64 px tall (12 + 20 + 4 + 16 + 12) |
| Trace-list state dot | 8 × 8 px |
| Banner | 40 px tall (12 vertical padding) |
| Metadata-strip chip | 24 px tall, 12 px horizontal padding |

##### 3.3.5.5 Motion

| Element | Duration | Easing | Reduced-motion |
| --- | --- | --- | --- |
| Chevron rotate (expand/collapse) | 120 ms | ease-out | no rotate; instant icon swap |
| Row hover background | 100 ms | ease-out | instant |
| New rows appearing (lazy expand, sort change) | 80 ms fade | ease-out | instant |
| Banner appear / dismiss | 160 ms slide-fade | ease-out | instant |
| Filter-result fade-replace | 80 ms | ease-out | instant |
| Loading skeleton shimmer | 1500 ms loop | linear | static, no shimmer |
| Active-state pulsing dot | 1600 ms loop | ease-in-out | static dot |
| `loader` icon rotation | 1000 ms loop | linear | static dot replaces rotation |

All of the above are gated by `@media (prefers-reduced-motion: reduce)`. *Resolves: AC-3.3.8.*

##### 3.3.5.6 Iconography

[Lucide](https://lucide.dev) v0.x, MIT-licensed. Served as a single SVG sprite at `/viz/assets/icons.svg`; consumed via `<svg><use href="…#icon-name"/></svg>`. All icons inherit `currentColor` so token application happens via CSS.

| Icon | Use | Size |
| --- | --- | --- |
| `chevron-right` | Collapsed tree row | 16 px |
| `chevron-down` | Expanded tree row; sort-desc indicator | 16 px |
| `chevron-up` | Sort-asc indicator | 16 px |
| `loader` | Lazy-fetch in flight; login submit | 16 px, rotating 1 s linear |
| `search` | Search inputs (leading) | 16 px |
| `x` | Clear input (trailing) | 16 px |
| `arrow-left` | Breadcrumb back | 16 px |
| `alert-triangle` | Warn: abnormal exit, dropped records, incomplete trace | 16 px row / 20 px banner |
| `alert-circle` | Danger: data anomaly, child load failed | 16 px row / 20 px banner |
| `info` | Info: CPU-unavailable banner | 20 px |
| `copy` | Copy-to-clipboard (trace key) | 14 px |

##### 3.3.5.7 Focus ring

```css
:focus-visible {
  outline: 2px solid var(--focus);
  outline-offset: 1px;
  border-radius: inherit;
}
```

Applied via the global `:focus-visible` selector. Visible on every interactive element. Never suppressed.

##### 3.3.5.8 Minimum target sizes

| Element | Minimum hit area |
| --- | --- |
| Buttons (login, retry, clear-filter) | 32 × 32 px |
| Chevrons in tree rows | 24 × 24 px (visual icon 16 × 16) |
| Column-header sort | full column width × 32 px |
| Inline row icons (anomaly slot) | 24 × 28 px (full row height) |
| Token input (login) | full card width × 44 px |
| Copy-key button (detail header) | 24 × 24 px |

##### 3.3.5.9 Z-index layers

| Layer | z-index | Use |
| --- | --- | --- |
| base | 0 | normal flow |
| sticky | 10 | column headers, page header, search bar |
| banner | 20 | warn/danger/info banners |
| overlay | 100 | tooltips, dropdowns |
| modal | 1000 | reserved; not used in MVP |
| toast | 2000 | reserved; not used in MVP |

---

#### 3.3.6 Page visual specifications

##### 3.3.6.1 Login (`/viz/login.html`)

Layout: centered card on `--bg`. Card 360 × auto px, `--surface` background, 1 px `--border` outline, 8 px border-radius, 32 px inner padding, vertically centered in the viewport.

```
                ╭───────────────────────────────╮
                │                               │
                │   PHP Tree Visualizer         │  ← display, --fg
                │                               │
                │   ACCESS TOKEN                │  ← micro, --fg-muted
                │ ┌───────────────────────────┐ │
                │ │ paste your token          │ │  ← mono input, 44 px
                │ └───────────────────────────┘ │
                │ ┌───────────────────────────┐ │
                │ │         Sign in           │ │  ← --accent bg, 44 px
                │ └───────────────────────────┘ │
                │                               │
                │   This profiler is for the    │  ← small, --fg-subtle
                │   local team only. Ask the    │
                │   operator for the token.     │
                ╰───────────────────────────────╯
```

Anatomy:

| Element | Spec |
| --- | --- |
| Title | "PHP Tree Visualizer". display weight 600, `--fg`. 24 px bottom margin. |
| Label | "Access token". micro style, `--fg-muted`. 4 px bottom margin. |
| Token input | Full card width, 44 px tall, 12 px horizontal padding, `--font-mono`, placeholder `--fg-subtle` "paste your token". `type="password"`, `autocomplete="current-password"`. Auto-focus on page load. 16 px bottom margin. |
| Submit button | Full card width, 44 px tall, `--accent` background, `--fg-on-accent` text, body-strong, 6 px radius. 24 px bottom margin. |
| Hint | small, `--fg-subtle`, 1.4 line-height. |

States:

| State | Visual |
| --- | --- |
| Default | As above. |
| Submitting | Button disabled; button text becomes "Signing in…" with a `loader` icon on the left. Input read-only. |
| Token rejected (401) | A 40 px danger banner appears between title and label: `alert-circle` 20 px + "**Token rejected.** Check the value with your operator." Input value persists; cursor returns to input. |
| Network error | Same danger banner: "**Couldn't reach the API.** Retry in a moment." |

Submit triggers: Enter key in input; click on button. *Resolves: F-1.2 surface, NF-4.x.*

##### 3.3.6.2 Trace list (`/viz/index.html`)

Layout: 1024 px max-width content area, centered. Horizontal page padding: 48 px on viewports ≥1280 px, 24 px otherwise.

Header zone (sticky):

```
┌─ Traces · 3,142 total ─────────────────────  [🔍 filter by URI or script ] ┐
└───────────────────────────────────────────────────────────────────────────┘
```

- **Page title.** "Traces" (display) + " · " (`--fg-subtle`) + "{N} total" (body, `--fg-muted`). Left-aligned. When the filter is active, the count switches to "{matched} of {total} match".
- **Filter input.** Right-aligned, 320 × 36 px, `--font-mono`, leading `search` icon, trailing `x` icon (visible only when input has value). Placeholder "filter by URI or script". Each keystroke debounced 250 ms → fires `GET /api/traces?q=…`. While the request is in flight, a 2 px `--accent` progress strip animates at the bottom of the input. On response, rows fade-replace (80 ms). *Resolves: F-5.3, NF-2.2.*

Row anatomy (64 px):

```
┌─ row ─────────────────────────────────────────────────────────────────────┐
│ ● /srv/app/bin/run-tests.php                            248,932 calls     │
│   cli · dev-1 · 12345 · 3m ago    [⚠ 42 dropped]    1.54 s    finalized   │
└───────────────────────────────────────────────────────────────────────────┘
  ↑                                                                      ↑
  state dot                                                       state badge
```

CSS grid `16px / 1fr / auto`:

| Region | Content | Style |
| --- | --- | --- |
| state dot | 8 × 8 px vertically centered. `--success` if finalized, `--warn` pulsing if active. | Pulse: 1600 ms ease-in-out loop, scale 1.0 ↔ 1.4 + opacity 1.0 ↔ 0.5. Disabled by reduced-motion. |
| main / line 1 | `uri_or_script` | body-strong, `--fg`, truncate-right with ellipsis on overflow |
| main / line 2 | `{sapi} · {host} · {pid} · {relative_time}` then optional badges `[⚠ N dropped]` (warn) and `[⚠ N anomalies]` (danger) | small, `--fg-muted`. Badges: 11 px micro, 4 px corner radius, 2 px horizontal padding, semantic-tinted bg |
| metrics / line 1 | `{call_count}` + " calls" | body-strong, mono tabular, `--fg`, right-aligned |
| metrics / line 2 | `{wall_auto_scaled}` + 8 px gap + state badge chip | small + chip. Chip: 22 × auto px, 4 px radius, `--success`/`--warn` tinted bg, micro text |

Row states:

| State | Visual |
| --- | --- |
| Default | bg `--surface` |
| Hover | bg `--surface-2`, cursor pointer (entire row is the click target) |
| Focused (keyboard) | 2 px `--accent` left border, bg `--accent-bg` |
| Click | navigates to `/viz/trace.html?key={trace_key}` (no selection persistence) |

Footer: when `has_more` is true, a 48 px-tall footer below the last row contains "Showing {N} of {M} traces" (small, `--fg-muted`) on the left and "[Load more]" (text button, `--accent`) on the right.

Empty / loading / error states: see §3.3.9.

##### 3.3.6.3 Trace detail (`/viz/trace.html?key=<trace_key>`)

Layout: full-width container (no max-width — the tree wants horizontal room). 24 px horizontal page padding.

Header zone (96 px + conditional banners):

```
← Traces  /  /srv/app/bin/run-tests.php                      key: 9b3a…f8c2  ⎘
─────────────────────────────────────────────────────────────────────────────
  248,932 calls   ·   1.54 s wall   ·   0 dropped   ·   0 anomalies   ·   ● finalized
─────────────────────────────────────────────────────────────────────────────
  ⚠  Trace is incomplete — 42 dropped records during ingest                       ← banner zone
  ⓘ  CPU time was not captured (cpu_snapshot_mode = off). CPU columns show '—'.   (conditional)
─────────────────────────────────────────────────────────────────────────────
```

Row 1 — breadcrumb (32 px):

- `arrow-left` icon + "Traces" — text link, `--accent`, hover underline.
- ` / ` separator (`--fg-subtle`).
- `uri_or_script` — `--fg`, truncate-right with ellipsis.
- Far right: "KEY: " (micro, `--fg-muted`) + truncated trace key first-4 + ellipsis + last-4 (`--font-mono`, `--fg`) + `copy` icon-button (24 × 24 hit). Click → copies full key to clipboard, 1.5 s tooltip "Copied" below the icon.

Row 2 — metadata strip (40 px):

Each chip is 24 px tall, 12 px horizontal padding, 4 px radius, `--surface-2` background, 1 px `--border` outline. Chip text: micro label + body value (8 px gap). Chips, in order, separated by 12 px:

| Chip | Source |
| --- | --- |
| `Calls {call_count}` | `index.sqlite.traces.call_count` |
| `Wall {total_wall_auto_scaled}` | `total_wall_ns` |
| `Dropped {dropped_records}` | `dropped_records` (tint `--warn` if > 0) |
| `Anomalies {anomaly_count}` | `anomaly_count` (tint `--danger` if > 0) |
| `● State {state}` | dot uses state color (see §3.3.10) |

Chips are not interactive in MVP.

Banner zone (each 40 px when present; stack vertically; order = severity ascending: incomplete, then cpu-unavailable, then anomalies):

| Trigger | Banner |
| --- | --- |
| `dropped_records > 0` | `--warn-bg` background, `alert-triangle` 20 px `--warn`, text "Trace is incomplete — {N} dropped records during ingest. Aggregated totals are missing those calls." |
| `cpu_snapshot_available == false` | `--info-bg`, `info` 20 px `--info`, text "CPU time was not captured (`cpu_snapshot_mode = off`). CPU columns show '—'." |
| `anomaly_count > 0` | `--danger-bg`, `alert-circle` 20 px `--danger`, text "{N} data anomalies detected. Hover any flagged row to see details." |

Tree zone (fills remaining viewport height; vertical scroll on the tree itself, not the page):

- **Sticky search bar (40 px).** See §3.3.8 (Search). Left: search input (280 × 32 px) + prev/next + match count. Right: "Sort by: {column} ▾" dropdown (24 px hit).
- **Sticky column header (32 px).** See §3.3.7.
- **Virtualized list of tree rows (28 px each).** See §3.3.7.

Tree zone empty / loading / error states: see §3.3.9.

---

#### 3.3.7 Call-tree row specification

This section is the implementer's reference for the single row that the virtualizer renders. *Resolves: F-6.1 through F-6.10, NF-1.4.*

##### 3.3.7.1 Anatomy

A row is a CSS grid: one flexible Function cell + five fixed-width metric cells.

```
┌──────────────────────────────────── Row (28 px) ────────────────────────────────────┐
│ Function (flex, min 320 px)                          │ Count │ Total │ Self │ %Par │ Mem │
│ [indent×d][chev][fqn (mono, truncate-middle)][badge][file:line muted][anomaly icons] │   …  │   …  │  …  │  …  │  …  │
└─────────────────────────────────────────────────────────────────────────────────────┘
  ←16×d→  20px  ←      flex, min 0       →  40    ← right-aligned in cell →  24×28
```

Column widths and behavior:

| Column | Width | Alignment | Font | Notes |
| --- | --- | --- | --- | --- |
| Function | flex (min 320) | left | mono | Chevron + indent + fqn + badge + file:line + anomaly icons |
| Count | 80 px | right | mono tabular | |
| Total | 96 px | right | mono tabular | Wall duration, auto-scaled |
| Self | 96 px | right | mono tabular | `total - children_total`, auto-scaled |
| %Parent | 140 px | right | mono tabular | 80 px hot-path bar + 8 px gap + 40 px percent text |
| Mem | 96 px | right | mono tabular | Memory delta, signed, auto-scaled |

Gap between adjacent cells: 12 px. Total fixed-cells width: 508 + 4 × 12 = 556 px. Function column receives `viewport - 556 - 48 (padding) - 8 (scrollbar)`. At 1280 px viewport: ~668 px for Function. At 720 px: ~108 px → horizontal scroll appears on the tree zone (acceptable per NF-5.3).

##### 3.3.7.2 Hot-path bar

| Element | Spec |
| --- | --- |
| Track | 80 × 6 px, vertically centered, `--surface-2` background, 3 px corner radius |
| Fill | `width: {percent}%`, `background: linear-gradient(to right, var(--hot-0), var(--hot-3))`, 3 px corner radius |
| Numeric percent | Right of bar with 8 px gap, 40 px right-aligned cell, code style |

Because the fill is a clipped window onto a fixed gradient, a 5% bar shows only the deepest `--hot-0` slice; a 100% bar reaches `--hot-3`. The lightness of the visible fill correlates monotonically with magnitude. *Resolves: AC-3.3.10.*

##### 3.3.7.3 Row states

| State | Visual |
| --- | --- |
| Default | bg `--surface`; fqn `--fg`; file:line `--fg-muted`; metrics `--fg` |
| Hover | bg `--surface-2`; chevron cursor `pointer` |
| Focused (keyboard or click) | 2 px `--accent` left border; bg `--accent-bg` |
| Expanded | chevron is `chevron-down` |
| Collapsed | chevron is `chevron-right` |
| Leaf (no children) | no chevron rendered; same 20 px reserved for alignment |
| Loading children | chevron replaced with `loader` 16 px rotating, `--accent` |
| Children-load-failed | chevron replaced with `alert-circle` 16 px `--danger`; row click retries |
| Search match (substring highlight) | matched substring of fqn gets a `--warn` 1 px box outline at 30% alpha + `--warn` underline at 60% alpha |
| Current search match (prev/next target) | whole row bg `--accent-bg` (overrides hover) |

##### 3.3.7.4 Number formats

Durations (auto-scale, 3 significant figures):

| Range | Unit | Examples |
| --- | --- | --- |
| x < 1 µs | ns | `847 ns`, `41.2 ns`, `1.20 ns` |
| 1 µs ≤ x < 1 ms | µs | `12.4 µs`, `187 µs`, `999 µs` |
| 1 ms ≤ x < 1 s | ms | `1.20 ms`, `187 ms`, `999 ms` |
| 1 s ≤ x < 60 s | s | `1.20 s`, `12.4 s`, `59.9 s` |
| x ≥ 60 s | m:ss | `2:14`, `12:01` (cap display at `99:59`) |

Memory deltas (auto-scale, explicit sign, 3 significant figures):

| Range | Unit | Examples |
| --- | --- | --- |
| \|x\| < 1 KB | B | `+247 B`, `-12 B` |
| 1 KB ≤ \|x\| < 1 MB | KB | `+812 KB`, `-3.40 KB` |
| 1 MB ≤ \|x\| < 1 GB | MB | `+1.20 MB`, `-50.5 MB` |
| \|x\| ≥ 1 GB | GB | `+2.30 GB` |
| x == 0 | B | `±0 B` (in `--fg-subtle`) |

Sign rule: positive `+`, negative `-`, zero `±`. Negative values render in `--danger`. Positives in `--fg`. Zero in `--fg-subtle`.

Counts: locale thousands separator (default en-US comma): `1`, `42`, `1,201`, `248,932`.

Percent of parent:

| Range | Format |
| --- | --- |
| x ≥ 10 | `42%` (integer) |
| 1 ≤ x < 10 | `4.2%` (one decimal) |
| 0.1 ≤ x < 1 | `0.4%` |
| 0 < x < 0.1 | `<0.1%` |
| x == 0 | `0%` (`--fg-subtle`) |

CPU columns when `cpu_snapshot_available == false`: render `—` (em dash) in `--fg-muted`. Tooltip: "CPU time not captured for this trace."

CPU columns when globally available but a specific row's CPU == 0: render `0 ns` in `--fg-muted`. Tooltip: "Sub-microsecond call, or CPU not sampled for this call."

##### 3.3.7.5 Internal-function badge

For `dict.kind == 3`:

- Inline `[int]` badge between fqn and file:line slot.
- Style: 11 px micro mono, `--fg-muted` text, 1 px `--border-strong` outline, 4 px horizontal padding, 2 px corner radius, vertical-aligned middle.
- The file:line cell is empty (per the wire format, `file = ""` and `line = 0` for internal functions). The badge stands in.

##### 3.3.7.6 Closure label

For `dict.kind == 2`:

- fqn shows the wire value literally (e.g., `closure:app/Db.php:42`).
- file:line slot shows the same `app/Db.php:42` redundantly so the row keeps its visual rhythm.
- No badge.

##### 3.3.7.7 Anomaly icon slot

A 24 × 28 px slot to the right of file:line, before the Count column. Icons stack horizontally if multiple kinds apply:

| Condition | Icon | Color | Tooltip |
| --- | --- | --- | --- |
| `node.abnormal_exit_count > 0` | `alert-triangle` 16 px | `--warn` | "{n} call(s) exited abnormally (e.g., uncaught exception)" |
| `node.anomaly_count > 0` (DQ-1/2/3) | `alert-circle` 16 px | `--danger` | Dynamic per-kind list (see §3.3.10) |

##### 3.3.7.8 Sticky column header

| Element | Spec |
| --- | --- |
| Container | 32 px tall, `--surface-2` background, 1 px `--border-strong` bottom border, sticky to top of tree zone (z-index 10) |
| Cell text | micro style (11 px uppercase +0.04em), `--fg-muted` |
| Hover | cell bg becomes `--surface-3`, cursor `pointer` |
| Active sort cell | text becomes `--accent`; a `chevron-down` (or `chevron-up` for fqn-asc) sits 4 px right of the text; 2 px `--accent` bottom underline overrides the cell's bottom border |

Click behavior is specified in §3.3.8 (Sort).

---

#### 3.3.8 Interaction model

Extends the technical sketches in §3.3.3 and §3.3.4 with full visual + keyboard behavior. *Resolves: F-6.7, F-6.8, F-6.10, NF-5.2, AC-3.3.7.*

##### 3.3.8.1 In-tree search

| Aspect | Spec |
| --- | --- |
| Trigger | Keystroke in the search input, or pressing `/` anywhere on the page (focuses the input and selects existing content) |
| Debounce | 120 ms after the last keystroke before recomputing matches |
| Scope | Substring match against `fqn`, case-insensitive, no regex. Matches against the in-memory flat list (currently expanded + loaded nodes). Subtrees not yet fetched are not searched — this is a known limitation; the match count reflects only loaded nodes |
| Visual (matched row) | Matched substring of fqn: `--warn` 1 px box outline at 30% alpha + `--warn` underline at 60% alpha. Rest of the row unaffected |
| Visual (count) | "{N} of {M} matches" to the right of the input, small text `--fg-muted`. If M == 0: "no matches" (`--danger`) and the input bottom border becomes `--danger` 1 px |
| Prev/next | `chevron-up`/`chevron-down` icon-buttons (24 × 24 hit each) next to the count. Click → scroll viewport to center the next/prev matching row; that row also receives the "current match" highlight (whole-row `--accent-bg`) |
| Keyboard inside input | `Enter` → next match; `Shift+Enter` → previous; `Esc` → clear query and blur input |
| Clearing | Click trailing `x` icon, or `Esc` while focused |

##### 3.3.8.2 Tree keyboard navigation

Tab order: page header chrome → search input → prev button → next button → sort dropdown → tree (column header focus first, then first row).

Once focus is in the tree:

| Key | Action |
| --- | --- |
| `↓` / `↑` | Move focus to next / previous row in the flattened visible list |
| `→` | If collapsed and has children: expand (lazy-fetch if needed). If already expanded: focus first child |
| `←` | If expanded: collapse. If collapsed or leaf: focus parent |
| `Home` / `End` | Focus first / last visible row |
| `PgUp` / `PgDn` | Move focus by 10 rows |
| `Enter` | No-op in MVP (reserved for future node-detail pane) |
| `Tab` | Leave the tree; cycle to next chrome element |

Focused-row visual: 2 px `--accent` left border + `--accent-bg` background. If focus moves outside the viewport, scroll the tree zone so the focused row is in view (16 px margin from top/bottom of zone).

##### 3.3.8.3 Sort

| Trigger | Behavior |
| --- | --- |
| Click metric header (Count, Total, Self, %Parent, Mem) | Request `sort={column}_desc`. Default direction is desc because the useful sort for time/memory/count is largest-first |
| Click Function header | Request `sort=fqn_asc`. Ascending only |
| Click already-active header | Re-fetches with the same sort (force-refresh; useful when trace is still `active` and new data has arrived). No UI state change |
| Sort by ▾ dropdown | Same set as column-header clicks. Redundant control offered for discoverability and for users who scrolled horizontally past the headers |

Sort applies to children of every parent — the API returns each subtree in the requested order. Sibling rows are re-ordered; nesting is preserved.

**Re-sort while subtrees are expanded.** Collapse all dynamically-loaded depth ≥ 2 subtrees back to "not loaded," then re-render with the new sort. The user re-expands as needed. Rationale: avoids a costly client-side resort across heterogeneously-loaded children and keeps the API as the single source of sort order. Documented in operator notes; revisit if this proves annoying in practice.

##### 3.3.8.4 Lazy-expand choreography

1. User clicks the chevron, or presses `→` on a row.
2. Chevron immediately becomes `loader` (1 s linear rotation, `--accent`).
3. UI fires `GET /api/traces/{key}/tree/{node_id}/children?sort=<current>` (§5.7).
4. On 200: chevron becomes `chevron-down`. Child rows are spliced into the flat list directly after the parent, with an 80 ms fade-in. Virtualizer recomputes scroll bounds.
5. On 4xx/5xx: chevron becomes `alert-circle` (`--danger`). Tooltip "Could not load children. Click to retry." Row click triggers another attempt; chevron returns to `loader`.
6. If the user clicks `←` or navigates away during load: an `AbortController` cancels the request. The chevron returns to `chevron-right`.

##### 3.3.8.5 Trace-list filter

- Each keystroke (debounced 250 ms) fires `GET /api/traces?q=<query>`.
- During the in-flight request: existing rows stay visible (no flicker). A 2 px `--accent` progress strip animates inside the filter input's bottom border.
- On 200: rows fade-replace (80 ms cross-fade).
- On 0 results: rows are replaced by the empty-state block (§3.3.9).
- On error: a danger banner pinned to the top of the list — "Couldn't load traces — {brief}. [Retry]".

##### 3.3.8.6 Trace-key copy

The `copy` icon-button next to the truncated trace key in the detail header. Click → copies full trace key to clipboard via `navigator.clipboard.writeText`, shows a 1.5 s tooltip "Copied" below the icon. Fallback for browsers without the clipboard API: open a modal with the full key pre-selected.

##### 3.3.8.7 Tooltip behavior

| Aspect | Spec |
| --- | --- |
| Trigger | Hover for 500 ms; or focus the element via keyboard |
| Appear | Fade in 100 ms; 200 ms delay before fade-out on un-hover |
| Position | Above the trigger by 8 px; if that would clip the viewport top, flip to below |
| Style | `--surface-3` background, 1 px `--border-strong`, 6 px padding, 4 px radius, small text, max-width 320 px |
| Reduced-motion | Instant appear/disappear, no fades |

Tooltips are used only for short clarifying text. Anything longer than two sentences belongs in a banner or a future detail pane.

---

#### 3.3.9 Empty / loading / error states

All inline illustrations are stroke-only SVG, ≤96 × 96 px, `--fg-subtle`, 2 px stroke. They are geometric shapes (empty box, magnifying glass with a question, broken key, plug-disconnected) — not mascots, not characters. They appear centered, 32 px below the page chrome, with 16 px between illustration and headline.

| Page | Context | Illustration | Headline (display-h2) | Body (body) | Action |
| --- | --- | --- | --- | --- | --- |
| Trace list | No traces ingested yet | Empty-box outline | **No traces yet.** | Run a PHP script with `php-analyze` configured to POST to `/ingest/v1`. The trace will appear here within a few minutes. | — |
| Trace list | Filter returns 0 | Magnifying-glass-with-question outline | **No traces match "{q}".** | — | `[Clear filter]` text button |
| Trace list | First-paint loading | (no illustration) | (no copy) | 5 skeleton rows, 64 px each, shimmering | — |
| Trace list | Load error | (no illustration) | — | Danger banner pinned to top of list: "Couldn't load traces — {brief}." | `[Retry]` |
| Trace detail | 404 (key not found) | Broken-key outline | **Trace not found.** | It may have been pruned. Default retention is 30 days. | `[← Back to traces]` |
| Trace detail | First-paint loading | (no illustration) | (no copy) | Header chrome rendered fully; tree zone shows 5 skeleton rows, 28 px each | — |
| Trace detail | Trace has 0 calls (defensive) | Empty-box outline | **No calls recorded.** | This trace's ingest produced no call records. Check `dropped_records` and `php-analyze` logs. | — |
| Trace detail | Lazy-fetch error | (inline on row) | — | Chevron becomes `alert-circle` (`--danger`); tooltip "Could not load children. Click to retry." | row click |
| Trace detail | Search has 0 matches | (inline) | — | Count text "no matches" (`--danger`); input bottom border `--danger` | — |
| Login | Initial | (no illustration) | (the form) | — | — |
| Login | Token rejected (401) | (banner above input) | — | "**Token rejected.** Check the value with your operator." | — |
| Login | Network error | (banner above input) | — | "**Couldn't reach the API.** Retry in a moment." | — |

Skeleton row pattern: a `--surface` block (28 px tall on detail page, 64 px on list page) with one or two narrower `--surface-2` rectangles inside, shimmering by translating a horizontal gradient over 1500 ms. Skeletons disappear instantly when real content arrives — no fade-out (the content's fade-in covers the transition).

---

#### 3.3.10 Visual encoding catalog

The implementer's authoritative reference for every visual state in the UI. Each encoding pairs an icon (where applicable), a color tone, and an accessible label or tooltip. **No state is conveyed by color alone.**

| Encoding | Trigger | Icon | Color | Placement | Tooltip / `aria-label` |
| --- | --- | --- | --- | --- | --- |
| Internal function | `dict.kind == 3` | `[int]` text badge | `--fg-muted` text, `--border-strong` outline | Between fqn and file:line slot | "Internal function (PHP core)" |
| Closure | `dict.kind == 2` | (none) | `--fg` (same as user fn) | fqn shown as `closure:<file>:<line>` | "Closure defined at {file}:{line}" |
| Abnormal exit (row) | `node.abnormal_exit_count > 0` | `alert-triangle` 16 px | `--warn` | Row anomaly slot | "{n} call(s) exited abnormally (e.g., uncaught exception)" |
| Data anomaly (row) | `node.anomaly_count > 0` | `alert-circle` 16 px | `--danger` | Row anomaly slot | Dynamic: "Data anomalies — {kind1}: {n1}; {kind2}: {n2}; …" |
| Trace incomplete (list row) | `trace.dropped_records > 0` | `alert-triangle` 14 px | `--warn` | Inline badge in trace-list row line 2 | "{n} dropped records during ingest" |
| Trace incomplete (page) | same | `alert-triangle` 20 px | `--warn` text/icon, `--warn-bg` bg | Detail-page banner | "Trace is incomplete — {n} dropped records during ingest. Aggregated totals are missing those calls." |
| CPU unavailable (row) | `cpu_snapshot_available == false` | (em dash) | `--fg-muted` | Self-CPU columns | "CPU time not captured for this trace." |
| CPU unavailable (page) | same | `info` 20 px | `--info` text/icon, `--info-bg` bg | Detail-page banner | "CPU time was not captured (cpu_snapshot_mode = off). CPU columns show '—'." |
| Anomalies present (page) | `trace.anomaly_count > 0` | `alert-circle` 20 px | `--danger` text/icon, `--danger-bg` bg | Detail-page banner | "{n} data anomalies detected. Hover any flagged row to see details." |
| State: active | `trace.state == "active"` | (pulsing dot) | `--warn` | List-row left edge / detail metadata strip | "Still receiving batches" |
| State: finalized | `trace.state == "finalized"` | (solid dot) | `--success` | same | "All batches received" |
| Active sort column | column matches current `?sort=` | `chevron-down` (or `chevron-up` for fqn-asc) 14 px | `--accent` text + 2 px `--accent` underline | Column header | "Sorted by {col} descending" / "Sorted by {col} ascending" |
| Search match (row) | fqn substring-matches the query | (none) | matched substring: `--warn` 1 px outline 30% alpha + `--warn` underline 60% alpha | Inline in fqn cell | (covered by the overall match count) |
| Current search match | prev/next nav target | (none) | row bg `--accent-bg` | Whole row | "Match {i} of {N}" |
| Focused row (keyboard) | row has focus | (none) | 2 px `--accent` left border + `--accent-bg` bg | Whole row | (row `aria-selected` — post-MVP) |
| Loading children | lazy fetch in flight | `loader` 16 px rotating | `--accent` | Replaces chevron | "Loading children…" |
| Child-load-failed | API error on children fetch | `alert-circle` 16 px | `--danger` | Replaces chevron | "Could not load children. Click to retry." |
| Negative memory delta | `mem_delta < 0` | (none) | `--danger` | Mem column number | "Memory was freed during this call" |
| Zero memory delta | `mem_delta == 0` | (none) | `--fg-subtle` | Mem column number | (none) |
| Zero CPU (globally available) | row CPU == 0 while `cpu_snapshot_available == true` | (none) | `--fg-muted` | CPU columns | "Sub-microsecond call, or CPU not sampled for this call" |
| Inverted time (DQ-3) | `t_out < t_in` on any merged call | (folded into "Data anomaly") | — | Row anomaly slot | Included in anomaly tooltip |
| Unresolved fn_id (DQ-1) | dict entry missing for a referenced `fn_id` | "unresolved fn_id {N}" text in place of fqn | `--danger` text + 2 px `--danger` row left border | Function cell | "Function dictionary entry was never sent for fn_id {N}." |
| Pending parent at finalize (DQ-2) | parent of a call was never observed | (folded into "Data anomaly") | — | Row anomaly slot | Included in anomaly tooltip |

Three governing rules:

1. **Color is never the sole carrier of information.** Every encoding above pairs color with an icon, a label, or a tooltip. Color-vision-deficient developers (~8% of the audience) lose nothing.
2. **`--warn` is the "data may be incomplete" tone; `--danger` is the "data integrity is questionable" tone.** They are not interchangeable. `dropped_records` is warn (we know what's missing). DQ-1 / DQ-2 / DQ-3 are danger (something is structurally off).
3. **Banners are reserved for trace-level state, never per-row state.** Per-row state lives in the anomaly icon slot.

**Acceptance criteria:**

- AC-3.3.1 — Trace-list page renders 3,000 rows + filters by substring within 500 ms. *NF-2.2, F-5.3*
- AC-3.3.2 — Trace-detail page renders the root + depth-2 children for a 1 M-call trace within 5 s of navigation. *NF-1.3*
- AC-3.3.3 — Subtree expansion of a node with ≤10 K aggregated children updates the viewport within 500 ms. *NF-1.4*
- AC-3.3.4 — All visual encodings in §3.3.10 render correctly against handcrafted fixtures.
- AC-3.3.5 — Every foreground/background pair in §3.3.5.1 meets WCAG AA contrast: ≥4.5:1 for body text, ≥3:1 for large text and UI components. Verified by an automated contrast check across the token table at build time. `--fg-subtle` (3.5:1) is permitted only for placeholders and non-essential decoration; flagged by the same check if applied to essential text.
- AC-3.3.6 — All numeric cells (Count, Total, Self, %Parent, Mem; trace-list metric lines) render with `font-variant-numeric: tabular-nums`. Verified by a visual-regression fixture spanning ns→s and B→GB ranges.
- AC-3.3.7 — Full keyboard scope per §3.3.8.2 is functional without mouse interaction. Verified by a Playwright test that locates a known function via `/` + arrow keys and expands its parent in ≤10 keystrokes.
- AC-3.3.8 — `prefers-reduced-motion: reduce` removes all transitions and loops listed in §3.3.5.5; no element animates (loader rotations become a static state). Verified by browser-emulation test.
- AC-3.3.9 — Every empty / loading / error state listed in §3.3.9 renders correctly against a corresponding fixture. Verified by visual-regression tests.
- AC-3.3.10 — The hot-path bar's visible fill lightness correlates monotonically with `%parent`: a 1% bar shows only the `--hot-0` slice; a 100% bar reaches `--hot-3`. Verified by snapshot at 1%, 25%, 50%, 75%, 100%.
- AC-3.3.11 — Every icon-only or color-only encoding in §3.3.10 carries a matching `aria-label` or `title` attribute. Verified by a static test that scans the rendered HTML against the catalog.

### 3.4 Reverse proxy / fronting

**Purpose:** Terminate TLS (if configured), route paths to backends, enforce that the collector is not internet-exposed.

**Choice:** The existing nginx (or apache) that already serves the dev application. No new fronting layer.

**Routing rules:**

| Path prefix | Backend | Notes |
| --- | --- | --- |
| `/ingest/v1` | `http://127.0.0.1:8088` (Rust collector) | Pass through `Authorization` and `Content-Type`. Do not buffer the body (`proxy_request_buffering off` in nginx). |
| `/api/`     | PHP-FPM via the existing `php_fastcgi` config | Standard PHP serving. |
| `/viz/`     | Static files under `/var/www/php-tree-viz/static/` | `try_files` for SPA-style routing if needed. |
| `/`         | Existing dev application | Unchanged. |

**Acceptance criteria:**

- AC-3.4.1 — Loopback-only bind of the Rust collector (`127.0.0.1:8088`) is enforced by config; the bind address is not configurable to a public interface without an explicit override flag.
- AC-3.4.2 — TLS, if configured, is terminated at the proxy; the Rust collector never sees TLS.

### 3.5 Filesystem layout

```
/var/lib/php-tree-viz/        owned by collector user (e.g., phptv:phptv 2770)
├── index.sqlite              0660
├── index.sqlite-wal          0660 (managed by SQLite)
├── index.sqlite-shm          0660 (managed by SQLite)
├── traces/                   2770 — setgid; new files inherit the group
│   └── <trace_key>.sqlite    0660 — created by collector, read+written by PHP
│   └── <trace_key>.sqlite-wal 0660 (managed by SQLite)
│   └── <trace_key>.sqlite-shm 0660 (managed by SQLite)
│   └── <trace_key>.raw/      2770 — raw batches, append-only during ingest
│       ├── batch-0001.msgpack
│       ├── batch-0002.msgpack
│       └── ...
└── tmp/                      2770 — in-flight uploads; cleaned at startup

/etc/php-tree-viz/
└── collector.toml            0640 — root:phptv ownership; contains the bearer token

/var/log/php-tree-viz/        owned by collector user
└── collector.log             (or journald, depending on systemd unit)
```

**Why `0660` and `2770`, not `0640` and `0750`:** SQLite operates in
WAL mode and **every concurrent connection** — including read-only
opens by the PHP API — has to update the `*-shm` file to maintain
the wal-index reader counter. "Read-only via the group bit" perms
(`0640`) cause the API to fail with `SQLITE_READONLY: attempt to
write a readonly database` on any per-trace endpoint. The data
directory is therefore mode `2770` (setgid so new files inherit the
shared group), the collector runs with `UMask=0007` (see § 3.6), and
files inside the directory land at `0660` — group-readable AND
group-writable. World access stays denied. The bearer-token gate
at the HTTP edge is the access boundary, not the file mode bits.

**Shared-group membership:**

- The collector user (canonically `phptv` for a dedicated-user
  production deployment, or `www-data` for the single-user
  convention the `etc/install-debian.sh` script uses) — owns and
  writes everything under `/var/lib/php-tree-viz/`.
- The PHP-FPM user (typically `www-data`) — a member of the shared
  group; reads AND writes within the data directory via the `0660`
  / `2770` mode bits. The "writes" are only the SQLite-WAL state
  updates SQLite makes on every reader connection; the PHP API
  code itself never explicitly issues writes (INV-8).

**Acceptance criteria:**

- AC-3.5.1 — `www-data` can `SELECT` from `index.sqlite` and from
  `traces/*.sqlite`. The access boundary protecting the data is the
  loopback-only HTTP gate plus the data directory's shared-group
  membership, not the file mode bits. SQLite's WAL-mode reader-counter
  update on the `*-shm` file is the only write the PHP API user
  performs on the data dir.
- AC-3.5.2 — `<trace_key>.raw/` filenames sort lexicographically by arrival order (4-digit zero-padded counter rolls over at 9999 with a documented behavior — see § 4.4.2).

### 3.6 systemd unit

```ini
# /etc/systemd/system/php-tree-viz-collector.service
[Unit]
Description=PHP-analyze profiling tree collector
After=network.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=notify
User=phptv
Group=phptv
# Files written by the collector land at 0660 so shared-group readers
# (the PHP API) can update SQLite WAL/SHM. Co-owned with § 3.5's
# file modes — any change here MUST also update § 3.5.
UMask=0007
ExecStart=/usr/local/bin/php-tree-viz-collector --config /etc/php-tree-viz/collector.toml
Restart=on-failure
RestartSec=5
LimitNOFILE=65536

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/php-tree-viz /var/log/php-tree-viz
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

**Acceptance criteria:**

- AC-3.6.1 — `systemctl restart php-tree-viz-collector` loses no batch that was acknowledged with 2xx (cross-checked with the AC-3.1.3 SIGKILL test).
- AC-3.6.2 — The service runs as a non-root user, with no write access to anything outside the data and log directories.

---

## 4. Data Architecture

### 4.1 Conceptual models

#### 4.1.1 `TraceKey` (the trace identity abstraction)

```rust
struct TraceKey(String);  // 32 hex chars

impl TraceKey {
    fn from_meta(meta: &Meta) -> Self {
        if meta.trace_id != ALL_ZERO_UUID {
            Self(meta.trace_id.simple().to_string())  // 32 hex, no dashes
        } else {
            // Synthesized fallback. Hash because the raw tuple is verbose
            // and may contain characters unsafe for filenames.
            let mut h = Sha256::new();
            h.update(meta.host.as_bytes());
            h.update(&meta.pid.to_le_bytes());
            h.update(&meta.start_time_ns.to_le_bytes());
            Self(hex::encode(&h.finalize()[..16]))
        }
    }
}
```

**Properties:**

- 32 hex characters always (the upstream UUID format is 32 hex). Filename-safe.
- Synthesis happens **only** when `trace_id` is all-zero. The day distinct UUIDs ship, `from_meta` switches branch at runtime — no schema change.
- The same `TraceKey` is used as:
  - SQLite primary key in `index.sqlite.traces.trace_key`.
  - Filename stem: `traces/<trace_key>.sqlite`, `traces/<trace_key>.raw/`.
  - URL path segment: `/api/traces/<trace_key>`.

#### 4.1.2 `Trace` (one row in `index.sqlite`)

Identified by `TraceKey`. Carries the most recent metadata seen for the trace, aggregate counters used by the list view, and a `state` indicating whether the trace is still receiving batches.

#### 4.1.3 `DictEntry` (per trace, accumulated)

Maps `fn_id` → `(fqn, file, line, kind)`. Stored once per trace in its dedicated `.sqlite`. Accumulated across all batches in the trace.

#### 4.1.4 `Node` (aggregated tree node)

The collapsed unit. Identified by `(trace_key, node_id)` where `node_id` is locally unique within the trace. Each node represents the collapse of all sibling calls to the same `fn_id` under the same parent node, per BR-1.

#### 4.1.5 `PendingCall` (transient per trace)

A decoded call whose parent has not yet been observed. Stored in the per-trace SQLite until it resolves; deleted on resolve. Resolves on the next batch in the same trace, or at idle-finalize (anything still pending then is flagged as anomaly DQ-2).

#### 4.1.6 `Anomaly` (per trace)

A record of data-quality issues detected during decode or aggregation. Three kinds today (DQ-1, DQ-2, DQ-3); extensible by adding new `kind` strings.

### 4.2 Database schema: `index.sqlite`

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;        -- WAL + NORMAL is durable across power loss
PRAGMA foreign_keys = ON;

CREATE TABLE traces (
  trace_key            TEXT    PRIMARY KEY,
  -- Raw fields, mirrored from meta. trace_id may be all-zero.
  trace_id             TEXT    NOT NULL,
  host                 TEXT    NOT NULL,
  pid                  INTEGER NOT NULL,
  start_time_ns        INTEGER NOT NULL,   -- CLOCK_REALTIME, ns since epoch
  sapi                 TEXT    NOT NULL CHECK (sapi IN ('cli', 'fpm-fcgi')),
  uri_or_script        TEXT    NOT NULL,

  -- Lifecycle.
  state                TEXT    NOT NULL CHECK (state IN ('active', 'finalized'))
                       DEFAULT 'active',
  first_batch_at_ns    INTEGER NOT NULL,   -- collector wall-clock on first batch
  last_batch_at_ns     INTEGER NOT NULL,   -- collector wall-clock on most recent batch

  -- Aggregated counters used by the list view (denormalized).
  batch_count          INTEGER NOT NULL DEFAULT 0,
  call_count           INTEGER NOT NULL DEFAULT 0,
  total_wall_ns        INTEGER NOT NULL DEFAULT 0,
  dropped_records      INTEGER NOT NULL DEFAULT 0,
  anomaly_count        INTEGER NOT NULL DEFAULT 0,
  cpu_snapshot_available INTEGER NOT NULL DEFAULT 1  -- 0 if all CPU = 0 across trace
);

CREATE INDEX idx_traces_start_time     ON traces (start_time_ns DESC);
CREATE INDEX idx_traces_uri            ON traces (uri_or_script);
CREATE INDEX idx_traces_state_lastbatch ON traces (state, last_batch_at_ns);
                                       -- ^ used by the idle-finalizer
```

**Notes:**

- `WAL + synchronous=NORMAL` is durable: writes survive process crash; only an OS-level power loss could lose the very last few committed transactions. Acceptable per *NF-3.4*.
- `start_time_ns` is `CLOCK_REALTIME` — used for retention and human display.
- `last_batch_at_ns` is the collector's own `CLOCK_REALTIME` measurement of when the batch arrived, **not** any `t_in`/`t_out` value. This is fine — it's only used for idle-detection, never for duration arithmetic (*INV-3*).
- `cpu_snapshot_available` is computed lazily: starts `1`, set to `0` if every call across the entire trace has `cpu_u + cpu_s == 0`. Determined at idle-finalize time. Drives the UI's "CPU unavailable" mode (*F-6.9*).

### 4.3 Database schema: `<trace_key>.sqlite` (per trace)

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

-- Mirror of the index row for this trace; lets the PHP API render the
-- trace-detail page from a single SQLite file without joining across DBs.
CREATE TABLE trace_meta (
  trace_key            TEXT    PRIMARY KEY,
  trace_id             TEXT    NOT NULL,
  host                 TEXT    NOT NULL,
  pid                  INTEGER NOT NULL,
  start_time_ns        INTEGER NOT NULL,
  sapi                 TEXT    NOT NULL,
  uri_or_script        TEXT    NOT NULL,
  state                TEXT    NOT NULL,
  dropped_records      INTEGER NOT NULL DEFAULT 0,
  cpu_snapshot_available INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE dict (
  fn_id   INTEGER PRIMARY KEY,
  fqn     TEXT    NOT NULL,
  file    TEXT    NOT NULL,         -- may be "" for internal (kind=3)
  line    INTEGER NOT NULL,          -- may be 0 for internal
  kind    INTEGER NOT NULL           -- 0=function, 1=method, 2=closure, 3=internal
                  CHECK (kind BETWEEN 0 AND 3)
);

CREATE TABLE nodes (
  node_id              INTEGER PRIMARY KEY AUTOINCREMENT,
  parent_node_id       INTEGER REFERENCES nodes(node_id),  -- NULL = root
  fn_id                INTEGER NOT NULL REFERENCES dict(fn_id),
  depth                INTEGER NOT NULL,                   -- cached for the UI

  call_count           INTEGER NOT NULL DEFAULT 0,
  total_wall_ns        INTEGER NOT NULL DEFAULT 0,
  children_total_wall_ns INTEGER NOT NULL DEFAULT 0,       -- updated by children
  -- self_wall_ns is total_wall_ns - children_total_wall_ns; computed on read.

  total_cpu_u_ns       INTEGER NOT NULL DEFAULT 0,
  total_cpu_s_ns       INTEGER NOT NULL DEFAULT 0,
  total_mem_delta_bytes INTEGER NOT NULL DEFAULT 0,
  abnormal_exit_count  INTEGER NOT NULL DEFAULT 0,

  UNIQUE (parent_node_id, fn_id)     -- BR-1: one bucket per (parent, fn)
);
CREATE INDEX idx_nodes_parent ON nodes (parent_node_id);
CREATE INDEX idx_nodes_fn     ON nodes (fn_id);

-- Maps wire-level call_id → aggregated node_id within this trace.
-- Used during decode to fold children into the correct bucket.
CREATE TABLE call_to_node (
  call_id INTEGER PRIMARY KEY,
  node_id INTEGER NOT NULL REFERENCES nodes(node_id)
);

-- Calls observed before their parent. Drained on each batch and on idle-finalize.
CREATE TABLE pending_calls (
  call_id              INTEGER PRIMARY KEY,
  parent_call_id       INTEGER NOT NULL,
  fn_id                INTEGER NOT NULL,
  t_in_ns              INTEGER NOT NULL,
  t_out_ns             INTEGER NOT NULL,
  cpu_u_ns             INTEGER NOT NULL,
  cpu_s_ns             INTEGER NOT NULL,
  mem_in_bytes         INTEGER NOT NULL,
  mem_out_bytes        INTEGER NOT NULL,
  abnormal_exit        INTEGER NOT NULL    -- 0 or 1
);
CREATE INDEX idx_pending_parent ON pending_calls (parent_call_id);

CREATE TABLE anomalies (
  rowid          INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id        INTEGER REFERENCES nodes(node_id),     -- nullable
  kind           TEXT    NOT NULL,
                 -- one of: 'unresolved_fn', 'pending_parent_at_finalize',
                 --         'inverted_time'
  count          INTEGER NOT NULL DEFAULT 1,
  sample_call_id INTEGER,
  detail         TEXT
);
CREATE INDEX idx_anomalies_node ON anomalies (node_id);
```

**Notes:**

- **Synthetic root.** The collector inserts one node with `parent_node_id = NULL` and `fn_id` pointing at a synthetic dict entry (`fn_id = 0`, `fqn = "<root>"`, `kind = 0`). All top-level calls (`parent_call_id = 0` on the wire) become children of this node. This avoids special-casing the root in queries.
- **`children_total_wall_ns`** is incremented on the parent whenever a child node has its `total_wall_ns` increased. Self time on read is `total_wall_ns - children_total_wall_ns`. This keeps writes O(1) per call and reads trivial.
- **`UNIQUE (parent_node_id, fn_id)`** enforces *BR-1*: only one node per `(parent, fn)` pair. The decoder `INSERT … ON CONFLICT DO UPDATE` to fold a new call into the existing bucket.

### 4.4 Raw batch storage

#### 4.4.1 Layout

```
traces/<trace_key>.raw/batch-NNNN.msgpack
```

- `NNNN` is a 4-digit zero-padded counter, starting at `0001`, incremented per batch within the trace.
- The counter is taken from `index.sqlite.traces.batch_count` after the row is upserted (or computed from `MAX(file_number) + 1` on collector restart for traces whose `state = 'active'`).

#### 4.4.2 Counter rollover

Typical traces are 1–50 batches (*OPERATIONAL_NOTES*). The hard cap is 9999. If `batch_count == 9999` and another batch arrives:

- The batch is still persisted (don't break the durability contract). It is written as `batch-9999.NNNNN.msgpack` (decimal-overflow suffix).
- An anomaly row is inserted: `kind = 'batch_count_overflow'`.

This is a defensive measure; the requirements don't anticipate it.

#### 4.4.3 Cleanup on success

Raw files are **not** deleted after decode. They are deleted only by the retention sweeper. Rationale: *F-3.5*, AD-4.

#### 4.4.4 Cleanup on startup

`tmp/*.partial` is deleted at startup — anything there did not survive the fsync rename, so it can't have been acked with 2xx.

### 4.5 Data invariants

| INV | Statement |
| --- | --- |
| DI-1 | Every `nodes.fn_id` resolves to a `dict.fn_id` in the same database. Enforced by foreign key. |
| DI-2 | At most one `nodes` row exists per `(parent_node_id, fn_id)`. Enforced by `UNIQUE`. |
| DI-3 | `nodes.total_wall_ns >= nodes.children_total_wall_ns` (self time is non-negative). Verified at idle-finalize; violations become DQ-3 anomalies. |
| DI-4 | `pending_calls` is empty when `trace_meta.state = 'finalized'`. Anything left is flagged DQ-2 anomaly and counted in `anomaly_count`. |
| DI-5 | The synthetic root exists in every per-trace database with `node_id = 1, parent_node_id = NULL`. |

---

## 5. API Specifications

### 5.1 Ingest: `POST /ingest/v1`

**Handled by:** Rust collector.

**Request:**

| Header | Value |
| --- | --- |
| `Authorization` | `Bearer <token>` (token matches collector config) |
| `Content-Type` | `application/vnd.php-analyze.v1+msgpack` |
| `Content-Length` | Optional (chunked encoding allowed; cap 64 MiB enforced) |

Body: MessagePack-encoded v1 batch per `handover/WIRE_FORMAT.md`.

**Response:**

| Status | When | Body |
| --- | --- | --- |
| `200 OK` | Batch fsynced to disk; queued for decode | empty |
| `400 Bad Request` | Malformed MessagePack or shape violation | `{"error": "malformed_msgpack", "detail": "<msg>"}` |
| `401 Unauthorized` | Missing or wrong token | `{"error": "unauthorized"}` |
| `413 Payload Too Large` | Body exceeded 64 MiB | `{"error": "too_large"}` |
| `415 Unsupported Media Type` | Wrong `Content-Type` | `{"error": "unsupported_content_type"}` |
| `422 Unprocessable Entity` | `schema_version != 1` | `{"error": "unsupported_schema_version", "got": <n>}` |
| `503 Service Unavailable` | Bounded queue full | `{"error": "backpressure"}` |

**Retry semantics** (per `handover/HTTP_CONTRACT.md`): non-2xx triggers `php-analyze`'s 3-retry policy.

**Requirements:** *F-1.1 through F-1.10, F-2.x (consumes meta), F-3.1, NF-3.1, NF-3.3, NF-4.1.*

### 5.2 `POST /api/auth`

**Handled by:** PHP API.

**Request body** (`application/json`):

```json
{ "token": "..." }
```

**Response:**

| Status | When |
| --- | --- |
| `204 No Content` | Token matches config. Sets `phptv_session` cookie (HttpOnly, SameSite=Lax, Path=/). |
| `401 Unauthorized` | Token mismatch. |
| `400 Bad Request` | Missing or malformed body. |

### 5.3 `POST /api/auth/logout`

Clears the cookie. Always responds `204`.

### 5.4 `GET /api/traces`

**Query parameters:**

| Param | Default | Notes |
| --- | --- | --- |
| `q` | (empty) | Substring filter against `uri_or_script` (case-insensitive). |
| `limit` | `100` | Max 500. |
| `offset` | `0` | For paging. |
| `sort` | `start_time_desc` | Currently the only supported value. (Reserved for future.) |

**Response** (`application/json`):

```json
{
  "items": [
    {
      "trace_key": "9b3a...",
      "trace_id":  "00000000-0000-0000-0000-000000000000",
      "host":      "dev-server-1",
      "pid":       12345,
      "start_time": "2026-05-23T10:32:14.123456789Z",
      "sapi":      "cli",
      "uri_or_script": "/srv/app/bin/run-tests.php",
      "state":     "finalized",
      "call_count": 248932,
      "total_wall_ns": 1543210000,
      "dropped_records": 0,
      "anomaly_count": 0,
      "cpu_snapshot_available": true
    }
  ],
  "total": 1417,
  "has_more": true
}
```

**Implementation:**

```sql
SELECT trace_key, trace_id, host, pid, start_time_ns, sapi, uri_or_script,
       state, call_count, total_wall_ns, dropped_records, anomaly_count,
       cpu_snapshot_available
FROM traces
WHERE (:q = '' OR uri_or_script LIKE '%' || :q || '%')
ORDER BY start_time_ns DESC
LIMIT :limit OFFSET :offset;
```

**Requirements:** *F-5.1, F-5.2, F-5.3, F-5.4, F-5.5.*

### 5.5 `GET /api/traces/{key}`

**Response:**

```json
{
  "trace_key": "9b3a...",
  "trace_id":  "00000000-...",
  "host":      "dev-server-1",
  "pid":       12345,
  "start_time": "2026-05-23T10:32:14.123456789Z",
  "sapi":      "cli",
  "uri_or_script": "/srv/app/bin/run-tests.php",
  "state":     "finalized",
  "dropped_records": 0,
  "anomaly_count": 0,
  "cpu_snapshot_available": true,
  "root_node_id": 1
}
```

Returns `404` if no row in `index.sqlite` for `key`.

### 5.6 `GET /api/traces/{key}/tree`

**Query parameters:**

| Param | Default | Notes |
| --- | --- | --- |
| `depth` | `2` | Eager-load to this depth from the root. Max 4. |
| `sort` | `total_wall_desc` | One of: `total_wall_desc`, `self_wall_desc`, `count_desc`, `mem_delta_desc`, `fqn_asc`. |

**Response:**

```json
{
  "root_node_id": 1,
  "nodes": [
    {
      "node_id": 1,
      "parent_node_id": null,
      "depth": 0,
      "fqn": "<root>",
      "file": "",
      "line": 0,
      "kind": 0,
      "count": 1,
      "total_wall_ns": 1543210000,
      "self_wall_ns": 8200,
      "total_cpu_u_ns": 1100000000,
      "total_cpu_s_ns": 220000000,
      "total_mem_delta_bytes": 8192000,
      "abnormal_exit_count": 0,
      "anomaly_count": 0,
      "has_children": true,
      "children_loaded": true
    }
  ]
}
```

**Response shape rules:**

- `nodes` is a flat array, parents before children, in the order the UI should render.
- `children_loaded == true` means all children of that node are present in the response (up to `depth`). `false` means the UI should fetch them lazily via § 5.7.
- `self_wall_ns = total_wall_ns - children_total_wall_ns` is computed by PHP before serialization.
- `fqn`, `file`, `line`, `kind` are joined in from `dict` server-side — no separate dict endpoint.

### 5.7 `GET /api/traces/{key}/tree/{node_id}/children`

**Query parameters:**

| Param | Default | Notes |
| --- | --- | --- |
| `sort` | `total_wall_desc` | Same options as § 5.6. |
| `limit` | (unlimited) | Optional cap; for nodes with many children. |
| `offset` | `0` | For paging if `limit` is set. |

**Response:** Same shape as § 5.6's `nodes` array — children of `node_id`, with their own `children_loaded == false` unless they happen to be leaves.

**Implementation:**

```sql
SELECT n.node_id, n.parent_node_id, n.depth,
       n.call_count, n.total_wall_ns, n.children_total_wall_ns,
       n.total_cpu_u_ns, n.total_cpu_s_ns, n.total_mem_delta_bytes,
       n.abnormal_exit_count,
       d.fqn, d.file, d.line, d.kind,
       COALESCE(a.cnt, 0) AS anomaly_count,
       EXISTS(SELECT 1 FROM nodes c WHERE c.parent_node_id = n.node_id) AS has_children
FROM nodes n
JOIN dict d ON d.fn_id = n.fn_id
LEFT JOIN (SELECT node_id, COUNT(*) AS cnt FROM anomalies GROUP BY node_id) a
       ON a.node_id = n.node_id
WHERE n.parent_node_id = :node_id
ORDER BY <sort-clause>
LIMIT :limit OFFSET :offset;
```

**Requirements:** *F-6.1 through F-6.10, NF-1.3, NF-1.4.*

---

## 6. Security Architecture

### 6.1 Threat model

The system runs on a trusted internal network with no path to the public internet (*NF-4.4, A-4, C-3.2*). Threats considered:

| Threat | Treatment |
| --- | --- |
| Hostile actor on the public internet | Out of scope. Network policy prevents this. |
| Curious-but-authorized colleague on the dev VLAN | Token check on ingest and on UI access keeps casual access out. |
| Misconfigured `php-analyze` shipping to the wrong endpoint | Token mismatch → 401 → batch dropped after 3 retries; the misconfigured host doesn't pollute our store. |
| Token leak in logs | INV-2: collector never logs `Authorization` header content. Verified by S-8. |
| Token leak in JS | The PHP API issues a session cookie; raw token is never sent to JS after the initial login POST. |

Out of scope (per *NF-4.6*): token rotation grace, mTLS, OIDC, multi-tenant tokens, PII redaction.

### 6.2 Token model

- One bearer token, set in `/etc/php-tree-viz/collector.toml`. Read by both the Rust collector and the PHP API (via a `require_once` of a tiny config bridge).
- Token is treated as a low-stakes "wrong destination" guard per stakeholder confirmation. Not rotated except on suspicion of compromise.
- Length / charset: at least 32 bytes of base64url. Generated by `openssl rand -base64 32` or equivalent.

### 6.3 Session cookie

- Name: `phptv_session`.
- Value: HMAC-SHA256(token || random-salt || user-agent-fingerprint), base64url. The salt is a per-installation secret in the same config file.
- Attributes: `HttpOnly`, `SameSite=Lax`, `Path=/`. `Secure` if the reverse proxy serves HTTPS (set by config flag).
- Lifetime: session duration tied to the PHP session GC defaults (1440 s default). No "remember me."

### 6.4 Logging hygiene

- Collector log format omits headers entirely; logs only: timestamp, accepted/rejected, status code, trace_key (or `?` if pre-decode), batch size, call count, error class.
- The PHP API logs to syslog; format strings never include `$_SERVER['HTTP_AUTHORIZATION']` or the cookie value.

### 6.5 File permissions

See § 3.5. Operator user owns config (`/etc/php-tree-viz/collector.toml`) and storage (`/var/lib/php-tree-viz/`). PHP-FPM user is in the collector group for read-only access.

---

## 7. Infrastructure and Deployment

### 7.1 Target environment

- One Linux host (the existing PHP dev server).
- Filesystem: any POSIX filesystem supporting `fsync` and atomic `rename`. ext4 / xfs / btrfs all qualify.
- Required: PHP 8.x with `pdo_sqlite`. Already present per *D-3*.
- Required: nginx or apache, already configured to front PHP-FPM.

### 7.2 Capacity planning

| Resource | Sizing rationale | Reserve |
| --- | --- | --- |
| Disk | ~100 GB compressed for 30 days @ heavy-usage estimate (*9.3*). Raw bytes dominate; aggregated SQLite is ~10% of raw size. | 150 GB recommended; alert at 80% (R-1). |
| RAM | Decoder uses a few hundred MB peak (one trace's pending state + the mpsc); SQLite WAL files are bounded by autocheckpoint (default 1000 pages = 4 MiB per DB). | 2 GB plenty. |
| CPU | Decoder is single-threaded; ~50 MB/s msgpack throughput on a modest core (well above the 25 MB/s peak burst). | 1 core dedicated; co-existence with PHP-FPM is fine. |
| Open file descriptors | Up to 64 concurrent per-trace SQLite handles (LRU cap) × 3 fds (DB, WAL, SHM) + listening sockets + raw files. `LimitNOFILE=65536` in the unit. | Generous. |

### 7.3 Configuration file

```toml
# /etc/php-tree-viz/collector.toml

[server]
bind = "127.0.0.1:8088"
max_body_bytes = 67108864          # 64 MiB
queue_capacity = 256

[auth]
token = "REPLACE_ME"               # base64url, ≥32 bytes
session_salt = "REPLACE_ME_TOO"    # base64url, ≥32 bytes, distinct from token

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30

[finalize]
idle_seconds = 30                  # trace marked finalized after this much silence
tick_seconds = 5

[retention]
tick_minutes = 60

[log]
level = "info"
format = "json"                    # or "text"
```

The PHP API reads the same file via a thin loader. Only the `[auth]` and `[storage]` sections are relevant to it.

### 7.4 Operator runbook (excerpt)

| Task | Procedure |
| --- | --- |
| Install | Place binary at `/usr/local/bin/php-tree-viz-collector`, drop `collector.toml` in `/etc/php-tree-viz/`, deploy PHP files to `/var/www/php-tree-viz/api`, deploy static frontend, `systemctl enable --now php-tree-viz-collector`. |
| Token rotation | Edit `[auth].token`. `systemctl restart php-tree-viz-collector`. Update `php-analyze`'s `php.ini` token. `systemctl reload php-fpm`. (Brief overlap window: in-flight requests with the old token return 401; `php-analyze` retries; batches accepted within ~3 retries.) |
| Shorten retention | Edit `[storage].retention_days`. `systemctl reload php-tree-viz-collector` (SIGHUP triggers config re-read). Next sweeper tick applies the new threshold. |
| Check disk | `du -sh /var/lib/php-tree-viz/{traces,traces/*.raw}`. |
| Inspect a single trace's data | `sqlite3 -readonly /var/lib/php-tree-viz/traces/<key>.sqlite`. |

### 7.5 Acceptance criteria

- AC-7.1 — A fresh install on a clean host accepts a test batch within 30 minutes of starting the runbook.
- AC-7.2 — SIGHUP re-reads `retention_days` without restart. Other settings require restart.

---

## 8. Integration Points

### 8.1 Upstream: `php-analyze` extension

Authority: `handover/HTTP_CONTRACT.md`, `handover/WIRE_FORMAT.md`. The collector implements its side of the contract exactly. Open question Q-9 (closure naming with column info) is upstream's call; we display whatever `dict.fqn` says.

### 8.2 Internal: filesystem-as-IPC between Rust and PHP

The Rust collector and the PHP API share no in-process state. The contract between them is:

- **Files:** `index.sqlite`, `traces/<key>.sqlite`. Schemas in § 4.
- **Read consistency:** SQLite WAL mode + `synchronous=NORMAL` guarantees PHP readers see a consistent point-in-time snapshot, never a torn read.
- **Schema versioning:** A `PRAGMA user_version` is set on every SQLite file by Rust at create time. PHP checks it on open and refuses to serve a file whose version it doesn't recognize (returns 500 with an explicit error; operator must redeploy matching PHP).

### 8.3 No other integrations

No Slack, no email, no metrics push, no CI hook (*10.3*).

---

## 9. Testing Strategy

### 9.1 Test pyramid

| Level | Owner | Scope |
| --- | --- | --- |
| Unit | Implementation persona | Pure aggregation functions; the `TraceKey` resolver; durations; anomaly classification. |
| Property-based | Implementation persona | The aggregation invariants in § 4.5 hold over generated random call sequences. |
| Component | Implementation persona | A single end-to-end run of decoder against a recorded `.msgpack` file produces the expected SQLite shape. |
| Replay-integration | Project | The collector + decoder, fed all three handover fixture workloads, ends in a state where `index.sqlite` has 3 finalized traces and the three per-trace files contain valid aggregation. |
| Browser E2E | Project | A scripted browser session logs in, finds a trace, opens it, expands a subtree, sorts. Run by playwright or similar. |

### 9.2 Acceptance test matrix

| Acceptance | Test type | Verification |
| --- | --- | --- |
| *S-1* (handover fixtures accepted) | Replay-integration | All 9 `.msgpack` files from `handover/batches/` POSTed; collector returns 200 on each; 3 `index.sqlite.traces` rows after idle-finalize. |
| *S-2* (5-minute workflow) | Manual + scripted | Stopwatch from `php-analyze` POST to UI render. |
| *S-3* (aggregation correctness) | Unit + replay | Hand-computed totals for one fixture compared against `<key>.sqlite` rows; equality required. |
| *S-4* (substring filter) | E2E | `GET /api/traces?q=run-tests` returns only matching rows. |
| *S-5* (retention) | Long-running | Test mode with `retention_days=1` and synthetic 2-day-old traces; sweeper removes them. |
| *S-6* (`kill -9` durability) | Chaos | Mid-batch `SIGKILL`; restart; the most recent acked batch must be present in the per-trace DB after decode-on-resume. |
| *S-7* (incomplete trace flagged) | E2E | Inject a fixture with `dropped_records > 0`; UI displays the banner. |
| *S-8* (no token leak in logs) | Static + dynamic | `grep -c <token> /var/log/php-tree-viz/*.log` returns 0 after a full normal session. |

### 9.3 Test fixtures

The three captured workloads in `handover/batches/` are the primary parse-test inputs. Per `handover/batches/README.md`, **assert on structure, not on bytes** — re-running the extension produces different `start_time`/`pid`/`t_in` values.

Additional handcrafted fixtures (to be created in `tests/fixtures/`):

- A trace whose calls arrive out of `(call_id, parent)` order — to exercise `pending_calls` resolution.
- A trace with every `cpu_u + cpu_s == 0` — to exercise *F-6.9* (CPU unavailable mode).
- A trace with `t_out < t_in` on one call — to exercise DQ-3 anomaly.
- A trace with non-zero `dropped_records` — to exercise BR-6 incomplete-trace marking.
- A trace with a referenced `fn_id` whose dict entry never appears — to exercise DQ-1.
- A batch with `schema_version = 2` — must be rejected with 422.

---

## 10. Implementation Plan

Phased to keep each step end-to-end usable. Each phase has explicit acceptance criteria; OpenSpec changes (one per phase, in `openspec/changes/`) drive implementation per the repository's workflow.

### 10.1 Phase 1: Honest ingest stub

**Components:** Rust collector — HTTP server, token check, content-type check, body-streaming-to-tmp, fsync, atomic rename, 2xx. No decoding.

**Acceptance:**

- All three handover batch fixtures (9 files) POSTed return 200 within 100 ms each.
- Wrong token → 401. Wrong content-type → 415. Oversize body → 413.
- After `SIGKILL` during a POST, no `.partial` files remain after restart and no acked batch is missing on disk.

**Dependencies:** D-2 (`rmp-serde` — for `peek_schema_version` only at this phase).

**Estimated effort:** 2–3 days (Rust developer).

### 10.2 Phase 2: Decoder, index DB, per-trace SQLite, idle-finalize

**Components:** Rust collector — decoder worker, `index.sqlite` schema, `<key>.sqlite` schema, aggregation logic, idle-finalizer.

**Acceptance:**

- After replaying the three handover workloads, `index.sqlite` has 3 rows with `state = 'finalized'` after 35 s.
- For each fixture, `<key>.sqlite.nodes.total_wall_ns` summed across root's children equals the hand-computed total wall time for that workload (±1 ns rounding).
- DQ-1, DQ-2, DQ-3 anomalies appear correctly against the handcrafted fixtures from § 9.3.

**Dependencies:** Phase 1; D-2.

**Estimated effort:** 5–7 days (Rust developer).

### 10.3 Phase 3: PHP API — auth + trace list

**Components:** `bootstrap.php`, `auth.php`, `traces.php`.

**Acceptance:**

- `POST /api/auth` with correct token returns 204 + cookie; wrong token returns 401.
- `GET /api/traces` without cookie returns 401; with cookie returns the JSON shape per § 5.4 over a populated `index.sqlite`.
- `GET /api/traces?q=run-tests` filters correctly.

**Dependencies:** Phase 2; D-3.

**Estimated effort:** 2–3 days (PHP / Web developer).

### 10.4 Phase 4: PHP API — trace metadata + tree fetch

**Components:** `trace.php` — endpoints § 5.5, § 5.6, § 5.7.

**Acceptance:**

- `GET /api/traces/{key}` returns the metadata JSON for a known key; 404 for unknown.
- `GET /api/traces/{key}/tree?depth=2` returns the root + ≤2 levels; `has_children` correctly set.
- `GET /api/traces/{key}/tree/{node_id}/children?sort=self_wall_desc` returns children sorted by computed self time.

**Dependencies:** Phase 3.

**Estimated effort:** 3–4 days.

### 10.5 Phase 5: Frontend — trace list view

**Components:** `index.html`, list-page JS, login flow.

**Acceptance:**

- Login flow works against `POST /api/auth`.
- Trace list renders 3,000 rows; substring filter input updates the list within 500 ms.
- Clicking a row navigates to `/viz/trace.html?key=...`.

**Dependencies:** Phase 3.

**Estimated effort:** 3–4 days (Web developer + UX designer).

### 10.6 Phase 6: Frontend — trace detail (the call tree)

**Components:** `trace.html`, tree-page JS, virtualizer, lazy expansion, visual conventions.

**Acceptance:**

- A 1 M-call trace renders root + depth-2 within 5 s.
- Subtree expansion of a ≤10 K-child node updates within 500 ms.
- All visual conventions in § 3.3 render correctly against handcrafted fixtures.
- Incomplete-trace banner shows for `dropped_records > 0`.
- "CPU unavailable" mode shows when `cpu_snapshot_available == false`.

**Dependencies:** Phase 4.

**Estimated effort:** 7–10 days (Web developer + UX designer).

### 10.7 Phase 7: Retention sweeper

**Components:** Rust collector — retention loop.

**Acceptance:**

- With `retention_days=1` and synthetic 2-day-old traces in `index.sqlite`, a tick removes the per-trace files, the raw directory, and the index row.
- Logs report freed disk and removed-trace count (*F-4.4*).

**Dependencies:** Phase 2.

**Estimated effort:** 1–2 days.

### 10.8 Phase 8: Observability + polish

**Components:** Structured logging refinements; disk-usage gauge in the logs; static analysis to assert token never reaches logs; performance pass.

**Acceptance:** All of *S-1* through *S-8* hold simultaneously on a fresh install.

**Dependencies:** All previous.

**Estimated effort:** 2–3 days.

### 10.9 Phase dependency graph

```
Phase 1 (ingest stub)
  └─▶ Phase 2 (decoder + DB)
        ├─▶ Phase 3 (PHP auth/list)
        │     ├─▶ Phase 4 (PHP tree)
        │     │     └─▶ Phase 6 (UI tree)
        │     └─▶ Phase 5 (UI list)
        └─▶ Phase 7 (retention)
              └─▶ Phase 8 (polish)
```

Phases 5 and 4-then-6 can run in parallel after Phase 3.

---

## 11. Risks and Mitigations

### 11.1 Restated from `REQUIREMENTS.md` § 14, with design-side treatment

| ID | Risk | Treatment in this design |
| --- | --- | --- |
| R-1 | 2xx-means-durable violated under disk pressure / crash. | INV-1 (fsync before 2xx). Disk usage logged hourly; alert threshold at 80%. Bounded queue ensures we don't enqueue past memory. If fsync fails: return 5xx, don't ack. |
| R-2 | Misbehaving script produces a single trace large enough to fill disk. | Bounded queue (NF-3.3, INV-7). `php-analyze` drops on retry exhaustion, counted in `dropped_records`. BR-6 surfaces incomplete trace to user. |
| R-3 | Wire-format ambiguity discovered mid-implementation. | Authority chain stated above. Open issue upstream and document in § 16-equivalent. Specification version bump on resolution. |
| R-4 | Trace volume exceeds estimate; disk pressure earlier than expected. | `retention_days` is config-driven (no rebuild). SIGHUP re-reads it. |
| R-5 | Schema v2 ships earlier than expected. | INV-5 rejects non-v1; the v1 code path is untouched by a future v2. URL is `/ingest/v1`; v2 lives at `/ingest/v2` alongside. |
| R-6 | All-zero `trace_id` replaced upstream mid-MVP. | AD-9 / `TraceKey::from_meta` branches at runtime on the all-zero check. Same code handles both. |
| R-7 | Aggregated tree is the wrong shape; users want raw per-call data. | AD-4 keeps raw bytes for the retention window. Re-aggregation is a re-decode against the same files. |
| R-8 | Browser rendering too slow for large traces. | Virtualizer + lazy expansion + depth-2 default (§ 3.3.2, § 3.3.3). NF-1.3 / NF-1.4 budgets explicit; AC-3.3.2 / AC-3.3.3 verify. |

### 11.2 Design-induced risks

| ID | Risk | Treatment |
| --- | --- | --- |
| DR-1 | SQLite-per-trace at ~3,000 files stresses some filesystems' directory lookups. | All access is by exact path; no `readdir`-based scans on the hot path. Listing is via `index.sqlite`, never `ls`. |
| DR-2 | LRU of 64 open SQLite handles + WAL files = up to 256 fds. | `LimitNOFILE=65536` in the unit. Cap is configurable. |
| DR-3 | Idle-finalize timeout of 30 s causes a trace to be marked finalized while a slow client is still in retry-and-resend for an old batch. | The handler still accepts the late batch and re-opens the per-trace DB. The trace returns to `state='active'`. Cost: one extra finalize cycle. Documented behavior. |
| DR-4 | Two batches from the same trace arrive concurrently via separate TCP connections and race to update `<key>.sqlite`. | Per-trace SQLite connection lives in the single decoder thread. The mpsc serializes; no race. If we later move to multi-threaded decode, we must keep a per-`TraceKey` mutex. |
| DR-5 | PHP API and Rust collector disagree on SQLite schema after upgrade. | `PRAGMA user_version` check on every PHP open. Mismatch → explicit 500 + log. Deploys must update both halves together. |
| DR-6 | The session-cookie HMAC includes user-agent fingerprint; UA changes (browser update) invalidate sessions. | Acceptable — re-login is one POST. Documented in operator runbook. |

---

## 12. Appendices

### 12.1 Glossary (additions to `REQUIREMENTS.md` § 17.1)

| Term | Meaning |
| --- | --- |
| **TraceKey** | The 32-hex-character storage key for a trace. Equals `trace_id` (minus dashes) when distinct; SHA-256-derived from `(host, pid, start_time_ns)` while `trace_id` is all-zero. |
| **Synthetic root** | The `node_id = 1, parent_node_id = NULL` row in every per-trace DB. Parents all top-level (`parent_call_id = 0`) calls. |
| **`children_total_wall_ns`** | The sum of children's `total_wall_ns` for a node, maintained denormalized so self time is a subtraction on read. |
| **Idle-finalize** | The transition from `state='active'` to `state='finalized'` after 30 s of no new batches on a trace. |
| **Pending call** | A decoded call whose parent has not yet been observed; held in `pending_calls` until it resolves or the trace finalizes. |

### 12.2 Open question → decision map

| Open question (`REQUIREMENTS.md` § 16) | Resolved in this document |
| --- | --- |
| Q-1 Storage tier | § 1.2 AD-2; § 4. SQLite per-trace + index. |
| Q-2 Sync vs async decode | § 1.2 AD-1; § 2.2. Async. |
| Q-3 Frontend stack | § 1.2 AD-10; § 3.3. Vanilla JS + D3 hierarchy + custom virtualizer. |
| Q-4 Deployment | § 3.6, § 7. systemd. |
| Q-5 Queue size | § 3.1 implementation notes; § 7.3 config. Default 256 (~256 MiB). |
| Q-6 Retain raw batches | § 1.2 AD-4; § 4.4. Yes, full retention window. |
| Q-7 Token rotation | § 7.4 runbook. Edit config + restart + update `php-analyze` + reload FPM. No grace. |
| Q-8 HTTP fronting | § 1.2 AD-8; § 3.4. Existing nginx/apache, TLS-terminated there. |
| Q-9 Closure column info | Upstream. Display whatever `dict.fqn` says. |

### 12.3 Wire-format authority chain reminder

For every wire-format question that arises during implementation, the order is:

1. Upstream `php-analyze` `SPECIFICATION.md` § 4.2 (the authoritative spec).
2. Upstream `crates/php-analyze/src/wire.rs` (the canonical Rust types).
3. `handover/WIRE_FORMAT.md` (a digest of the above for this repo).

Contradictions are flagged to the extension team; this document defers to whatever resolution upstream chooses.

### 12.4 Out-of-scope confirmation

This document confirms the *Won't-have* list in `REQUIREMENTS.md` § 4.4 is out of scope. In particular: multi-tenancy, external SLA, PII, real-time freshness, replay/backfill, token rotation grace, per-pool rate limits, and the `php-analyze` extension itself.

### 12.5 Change history

| Version | Date | Author | Notes |
| --- | --- | --- | --- |
| 0.1 | 2026-05-23 | Solution Architect | Initial draft from `REQUIREMENTS.md` v0.1 + stakeholder confirmation on AD-1, AD-2, AD-3, AD-7, AD-8. |
| 0.2 | 2026-05-23 | UX / Visualization Designer | Augmented §3.3 with Design System (§3.3.5), Page Visual Specifications for Login / Trace list / Trace detail (§3.3.6), Call-tree row specification (§3.3.7), Interaction model — search, keyboard, sort, lazy-expand, tooltips (§3.3.8), Empty / loading / error states (§3.3.9), and the Visual encoding catalog (§3.3.10). Added AC-3.3.5 through AC-3.3.11. Updated AC-3.3.4 to reference §3.3.10. No changes to §1, §2, §4 (data architecture), §5 (APIs), §6 (security), §7 (deployment), §8 (integration), §9 (testing strategy), §10 (implementation plan), §11 (risks), or §12.1–§12.4. |

---

*End of SPECIFICATION.md (v0.1).*
