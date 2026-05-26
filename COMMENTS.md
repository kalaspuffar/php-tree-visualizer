# COMMENTS.md

Supplementary notes, clarifications, and review comments that sit on
top of `SPECIFICATION.md`. When `SPECIFICATION.md` and this file
conflict, this file is the more recent clarification — surface the
discrepancy before acting on it.

Entries are append-only by date; older entries that have been fully
absorbed into shipped behaviour are summarised rather than reproduced
verbatim. Slice-specific narratives are pruned once their lessons are
encoded in tests, constants, or comments inside the codebase.

---

## Status as of 2026-05-24

### Shipped

The Rust collector is complete through Phase 7 of
`SPECIFICATION.md` §10. Across 10 slices (`workspace-scaffold` →
`retention-sweeper`) the binary now:

- Accepts MessagePack batches on `POST /ingest/v1` with bearer-token
  auth, streams the body to a fsync'd tmp file, atomically renames
  into `<data_dir>/traces/<key>.raw/batch-NNNN.msgpack`, and returns
  `200` only after durability is established (INV-1).
- Decodes via `rmp-serde`, accumulates per-trace `dict`, aggregates
  every `Call` into the `nodes` tree per BR-1 (one bucket per
  `(parent_node_id, fn_id)`), resolves cross-batch parents through
  `pending_calls`, and records all three spec-defined anomaly kinds
  — `unresolved_fn` (DQ-1), `inverted_time` (DQ-3), and
  `pending_parent_at_finalize` (DQ-2) — into the per-trace
  `anomalies` table with the index `traces.anomaly_count` counter
  kept in sync.
- Runs three background tokio tasks sharing one
  `Arc<tokio::sync::Mutex<Storage>>`: the decoder (one batch at a
  time), the idle-finalize loop (default 5 s tick / 30 s threshold;
  flips `state='finalized'`, drains pending into DQ-2, computes
  `cpu_snapshot_available`), and the retention sweeper (default 60
  min tick; prunes traces past `retention_days` from disk and
  index).
- Tests: 184 unit + 5 doctest + 60 integration; 5 consecutive runs of
  the integration suite all 60/60 green.

`openspec/specs/` carries the seven capability specs the collector
implements: `collector-scaffold`, `collector-config`, `collector-wire`,
`collector-http`, `collector-storage`, `collector-finalize`,
`collector-retention`.

### What's next: the PHP API + frontend (Phases 3–6)

Per `SPECIFICATION.md` §10.9, with Phase 2 done, the dependency graph
unblocks **Phases 3, 4, 5, 6** — the entire web stack. Phase 8
(observability polish — small, Rust) is also unblocked but lower
priority. The handoff at this point is from the Rust persona
(`personas/RUST_DEVELOPER.md`) to the web persona
(`personas/WEB_DEVELOPER.md`) and the UX persona
(`personas/UX_DESIGNER.md`). See the next section for everything a
web-side session needs.

---

## For the web developer (start here)

### What you're building

`SPECIFICATION.md` §3.2 (PHP API) and §3.3 (Static frontend). Layout:

- **`/api/auth`** — `bootstrap.php` + `auth.php`. Token entry,
  HMAC-of-token-plus-server-salt session cookie. The salt lives in
  the same TOML config file the Rust collector reads
  (`[auth].session_salt`).
- **`/api/traces`** — `traces.php`. Lists the trace-list view from
  `index.sqlite.traces`. Supports a substring filter on
  `uri_or_script`.
- **`/api/traces/{key}`** — `trace.php`. Metadata for one trace.
- **`/api/traces/{key}/tree`** — `trace.php`. Returns the root +
  ≤depth-2 of the aggregated tree.
- **`/api/traces/{key}/tree/{node_id}/children`** — lazy expansion;
  the heaviest endpoint by far.
- **`/viz/login.html`**, **`/viz/index.html`**, **`/viz/trace.html`**
  — vanilla JS + ES modules. D3 v7 for hierarchy *layout* only;
  rendering is plain DOM via a custom virtualizer (§3.3.2).

§3.3.5 through §3.3.10 are the design system: colour tokens,
typography scale, page mockups, the call-tree row, interaction model
(search, keyboard, sort, lazy-expand, tooltips), empty/loading/error
states, and the visual encoding catalogue. The
`personas/UX_DESIGNER.md` session owns the visual side; the web dev
implements against the spec.

The OpenSpec scaffold for this work is empty — `openspec/specs/` has
no `php-api` or `frontend` capability yet. A new
`/opsx:propose php-api-auth-list` (or similar) is the natural first
slice. Phase 3 in §10.3 is the smallest sensible chunk.

### What you read first (in order)

1. `SPECIFICATION.md` §3.2, §3.3, §5 (API shapes), §6 (security).
2. `SPECIFICATION.md` §4.2 + §4.3 — the SQLite schemas you're
   reading. The PHP code never writes to either DB (see INV-8 below).
3. The shipped Rust collector's specs in `openspec/specs/` —
   especially `collector-storage/spec.md`. Every column the API
   serves is produced by a requirement there; if the UI ever
   disagrees with what's on disk, the storage spec is the source of
   truth for what *should* be there.
4. `handover/` — the upstream wire contract. The web side doesn't
   touch the wire, but understanding what `dropped_records`,
   `start_time` (CLOCK_REALTIME), `t_in/t_out` (CLOCK_MONOTONIC),
   `cpu_u/cpu_s`, etc. *mean* matters for rendering them.

### Non-negotiable invariants the web side must hold

These are restated from `SPECIFICATION.md` §2.3 with the practical
implications:

- **INV-2 — Authorization header content is never logged.** PHP
  request logs (PHP-FPM access logs, error logs, `error_log` calls)
  must not include the cookie or any token bytes. Strip them
  upstream in the nginx/apache config, and avoid `var_dump($_SERVER)`
  in any reachable code path.
- **INV-3 — `t_in`/`t_out` are CLOCK_MONOTONIC; `start_time_ns` is
  CLOCK_REALTIME.** Never subtract across domains. The only
  wall-clock display the spec permits is
  `start_time + (t_in − first_call.t_in)`. The collector doesn't
  pre-compute this; the API or the frontend does.
- **INV-8 — The PHP API never writes to any SQLite file.** Open
  with `?mode=ro` and `PDO::ATTR_DEFAULT_FETCH_MODE =
  PDO::FETCH_ASSOC`. Any test that catches `mode=rw` is a hard
  regression.

### Schema-version gate (DR-5)

Both `index.sqlite` and `<key>.sqlite` carry `PRAGMA user_version`
set to `1` by the Rust collector at create time. The PHP API SHALL
check it on every open and refuse to serve a file with an unknown
value (return 500, log the path + the version). This protects against
a Rust upgrade landing without a PHP upgrade. Wrap it in
`bootstrap.php` once; every other PHP file inherits the check via
the shared connection-opener helper.

### Field semantics worth pinning before you render anything

- **`self_wall_ns` is computed on read**, not stored:
  `total_wall_ns - children_total_wall_ns`. Both columns live on
  every `nodes` row. The Rust collector maintains
  `children_total_wall_ns` denormalized so this stays O(1) (§4.3
  notes, DI-3).
- **`cpu_snapshot_available`** is `0` when *every* `nodes` row in
  the trace (excluding the synthetic root) has
  `total_cpu_u_ns + total_cpu_s_ns == 0`. It's set at idle-finalize
  time and mirrored to both `traces.cpu_snapshot_available` (index)
  and `trace_meta.cpu_snapshot_available` (per-trace). Drives the
  "CPU unavailable" UI mode per F-6.9.
- **`state` flips both ways.** `'active'` → `'finalized'` via the
  idle-finalize loop (30 s of no batches). A late batch flips it
  back to `'active'`, observable in both DBs. The UI's
  "all batches received" indicator goes solid only on `'finalized'`.
- **The synthetic root is `nodes.node_id = 1`, `parent_node_id IS
  NULL`, `fn_id = 0`, `fqn = '<root>'`.** Every per-trace DB has
  one. Top-level calls (wire `parent == 0`) become its children.
  Don't render it as a real row — render its *children* as the
  top-level entries in the tree.
- **`pending_calls` is empty when `trace_meta.state =
  'finalized'`** (DI-4). Anything left at finalize time becomes a
  DQ-2 anomaly row. The UI displays the count via
  `traces.anomaly_count`.
- **`dropped_records` is wire-monotonic.** The extension counts
  bursts dropped on its side and ships the running total in every
  batch's `meta`. The collector stores the latest value verbatim;
  it's not a delta. Render as "N records dropped this trace" —
  surface a banner when non-zero (BR-6).
- **`trace_id` may be all-zeros** (the upstream UUID-v7 plumbing is
  queued). The `trace_key` column is always a 32-hex stem;
  trust that, not `trace_id`, for routing and storage.

### Anomaly kinds (the `anomalies.kind` enum)

Three strings, no `CHECK` constraint at the SQL level — the integrity
is maintained by Rust constants:

- `'unresolved_fn'` — a Call referenced a `fn_id` not in `dict`. The
  Call doesn't fold into any node, so `anomalies.node_id IS NULL`.
  `detail = "fn_id=<N>"`.
- `'inverted_time'` — a Call had `t_out < t_in`. Folds into the node
  anyway with `total_wall_ns += 0`. `node_id` points at the
  resulting node. `detail = "t_in=<I>,t_out=<O>"`.
- `'pending_parent_at_finalize'` — a row in `pending_calls` at
  idle-finalize time. The Call's parent never arrived. `node_id IS
  NULL`, `detail = "parent_call_id=<N>"`.

`detail` is stable enough to render verbatim in tooltips (§3.3.10).

### Session/auth contract (§3.2 + §7.3)

- Token lives in `[auth].token` of the TOML config. PHP reads the
  same file. On `POST /api/auth`, compare the submitted token to
  `[auth].token` in constant time (`hash_equals`).
- Cookie name: `phptv_session`. Value: HMAC-SHA256 of the token plus
  `[auth].session_salt` (also in the TOML). PHP sessions backed by
  the filesystem session store.
- `session_salt` must be ≥ 32 chars and distinct from `token`
  (already enforced by the collector's config validation; PHP can
  re-assert).
- AD-7: no CSRF protection in MVP. The entire surface is local-only
  per AD-8. Document this in the operator-facing notes.

### Frontend rendering model (§3.3.1 – §3.3.4)

The tree is a virtualized vertical list of rows, not an SVG tree. Each
visible row is one expanded node; indentation conveys depth. JProfiler
shape. The full flattened list of expanded `node_id`s lives in memory;
only rows in the scroll viewport (+ small overscan) are rendered.

- Default load: root + depth 2.
- Click chevron → fetch `…/tree/{node_id}/children?sort=…` → splice
  the response into the flattened list at the right position.
- Sort changes re-fetch (server-sorted, since `self_wall_ns` is
  computed and only the API knows the right ORDER BY).
- In-tree search is client-side substring match over already-loaded
  `fqn` values; matches highlight and the viewport scrolls to the
  first.

### Workflow

The push/review/merge/checkout-main split is unchanged: the developer
branches from `main`, opens the OpenSpec change, implements, runs the
relevant linters and tests, commits, then stops. The operator pushes,
opens the PR, reviews, merges, and `git checkout main`. The developer
then runs `/opsx:archive <change-id>`. See `personas/WEB_DEVELOPER.md`
§ "Branching Rules" — same shape as the Rust persona.

---

## Deferred items (tracked, not in any active slice)

These came up during Rust slices and were intentionally pushed out;
none are blockers for Phases 3–6.

- **SIGHUP-driven re-read of `retention_days`** (AC-7.2, §7.4
  runbook). The runbook documents "edit `retention_days`, send
  `systemctl reload`, the next sweeper tick applies the new
  threshold." Currently a restart is needed. Multi-subsystem
  config-reload concern; deserves a dedicated slice.
- **LRU size cap on the per-trace connection cache** (§3.1: "default
  64 concurrent open traces"). The cache is an unbounded
  `HashMap<TraceKey, Connection>` today. Idle-finalize and retention
  both `remove()` entries as a side effect, but no upper bound is
  enforced. At <10 concurrent active traces this is invisible;
  revisit if profiling shows otherwise.
- **DI-3 verification at finalize.** §4.5 says
  "`nodes.total_wall_ns >= nodes.children_total_wall_ns` — verified
  at idle-finalize; violations become DQ-3 anomalies." In practice
  the inline `.max(0)` clamp in `fold_call_into_nodes` keeps DI-3
  holding continuously, and every DQ-3 case writes a row at ingest.
  A finalize-time DI-3 sweep would double-count or false-positive;
  intentional Non-Goal. If a concrete divergence ever appears,
  revisit.
- **`batch_count` overflow handling** (§4.4.2). The defensive
  `batch-9999.NNNNN.msgpack` shape for traces crossing the 4-digit
  counter is not implemented. Never observed in the documented
  workload (typical traces are 1–50 batches).
- **Re-aggregation from raw bytes** (R-7 mitigation that AD-4
  enables). Belongs in a `replay` capability that doesn't exist
  yet.
- **Literal handover-fixture replay** for the §10.2 / S-1
  acceptance. Only `flat_calls/batch-0001.msgpack` is embedded in
  `tests/fixtures/`; the §10.2 acceptance is currently met by
  `three_synthetic_traces_finalize_independently`, which uses three
  synthetic batches from `build_test_batch_with_chain`. Copying the
  remaining 8 fixture files in is pure test-data plumbing.
- **Phase 8 (observability + polish)** — disk-usage gauge in logs
  (R-1), structured-logging refinements, static analysis to assert
  the token never reaches stderr, a profiling pass. Small Rust
  slice; unlocked but not yet started.

---

## Workflow: push / review / merge / checkout main is manual

The end-of-step handoff is split between the developer and the
operator. Applies to **every** persona — Rust, web, UX, code
reviewer.

**Developer, per step:** branch from `main`, open the OpenSpec
change, implement, run the relevant lint/format/test gates,
`openspec validate <change-id>`, commit, then stop and report the
branch name, the OpenSpec change ID, and a short summary. The
developer does **not** push, merge, or switch branches.

**Operator, per step:** push the feature branch, open the pull
request, review, merge to `main`, and `git checkout main`. Confirms
completion before the developer starts the next step.

Implications:

- Treat `git push` and any operation on `main` as out of scope. If a
  push appears to be needed (e.g. CI on the branch), surface it as
  a question rather than acting.
- Each new step branches from `main` (which the operator has already
  fast-forwarded to include the previous step's merge). Do **not**
  branch from the previous step's local branch — that branch has
  been superseded.
- The OpenSpec archive step (`openspec archive <change-id>`) is the
  developer's responsibility, but it happens after the merge — i.e.
  once back on `main` with the merge commit visible. Confirm with
  the operator before archiving if the timing is ambiguous.

---

## OpenSpec parser gotchas

Four rules the `openspec validate` parser enforces that aren't in
the templates and each cost a roundtrip the first time:

- **MODIFIED requirement headers must match the existing main-spec
  header exactly** (whitespace-insensitive, but otherwise literal).
  When writing a delta under `## MODIFIED Requirements`, copy the
  `### Requirement: <name>` line verbatim from
  `openspec/specs/<capability>/spec.md`. Editing the title in the
  delta will fail with a header-mismatch error at archive time.
- **ADDED requirements need `SHALL` or `MUST` in the first sentence
  of the body.** The parser splits on `.`; abbreviations like
  `e.g.` or function signatures with parenthesised type lists can
  fool the splitter. Lead with a plain-prose sentence:
  "The collector SHALL expose `fn_name(...)`, which …". Don't open
  with backticks containing the SHALL inside.
- **Archive the predecessor change before proposing the next one
  that MODIFIES the same requirement.** If two open changes both
  touch the same `Requirement:`, the deltas race at archive time.
  Run `/opsx:archive <prev>` (after operator merge), then
  `/opsx:propose <next>`.
- **`openspec/` is gitignored.** `git status` will look empty after
  creating proposal / design / specs / tasks artifacts; that's
  correct. Never `git add openspec/`. Artifacts propagate forward
  via the `openspec/changes/archive/YYYY-MM-DD-<name>/` directory
  the archive command creates locally.

---

## Rust-side knowledge worth keeping

For the next Rust slice (Phase 8, or a future re-aggregation /
config-reload / LRU-cap slice).

### `i64::saturating_sub` does NOT clamp at zero

`saturating_sub` saturates at `i64::MIN` / `i64::MAX`, not at `0`.
`400i64.saturating_sub(500)` returns `-100`. Anywhere wall/CPU
arithmetic must stay non-negative, chain `.max(0)`. The two existing
call sites are `aggregate::fold_call_into_nodes` and
`record_batch`'s per-call wall-delta sum; both DQ-3 paths depend on
this clamp to keep DI-3 (`total_wall_ns >= children_total_wall_ns`).

### `prepare_cached`, not `tx.execute(sql, …)`, on hot paths

`rusqlite::Connection::execute(sql, params)` internally calls
`prepare(sql)`, which does **not** consult the statement cache. For
hot loops (anomaly inserts, node upserts, pending drain) this
dominates wall time — `aggregation-core` measured ~5 s on a 10 K
batch, dropped to ~200 ms after switching to `prepare_cached`.

`tx.execute(...)` is fine for one-shot statements (synthetic-root
seed, `trace_meta` mirror, `traces` UPSERT) where re-parsing once
per batch is invisible. Reserve the dance for inner loops.

### Anomaly `kind` strings are constants

§4.3 fixes the enum to three strings (`unresolved_fn`,
`inverted_time`, `pending_parent_at_finalize`). The schema has no
`CHECK` constraint, so a typo lands silently. Pinned in
`crates/php-tree-viz-collector/src/storage/aggregate.rs`:

```rust
pub(super) const KIND_UNRESOLVED_FN: &str = "unresolved_fn";
pub(super) const KIND_INVERTED_TIME: &str = "inverted_time";
pub(super) const KIND_PENDING_PARENT_AT_FINALIZE: &str =
    "pending_parent_at_finalize";
```

Tests assert on the literal strings (to catch constant drift).
Production call sites only ever reference the constants. If a fourth
kind ever appears, define it the same way; never inline a `kind`
string at the call site.

### `TraceKey::from_raw` is production-visible

It was `#[cfg(test)]` until `idle-finalize` needed to round-trip
32-hex stems out of `SELECT trace_key FROM traces`. The constructor
performs no validation; the implicit contract is "the caller has a
string that this codebase previously produced via `from_meta` and
persisted into a `trace_key` column". Today's only production
callers are `Storage::list_idle_active_traces` and
`Storage::list_expired_traces`. If a future call site accepts
external input, build a validating sibling constructor — don't feed
the unchecked one.

### Two-DB write ordering

Three storage operations span both `index.sqlite` and the per-trace
`<key>.sqlite`:

- **`record_batch`**: per-trace transaction first (mirror
  `trace_meta`, accumulate `dict`, fold into `nodes`, write
  anomalies), then `index.sqlite` upsert. `anomaly_count` bumps
  *additively* using the outcome's `anomalies_added` delta —
  re-delivery just double-counts, which we accept (per
  `anomaly-detection`'s design.md).
- **`finalize_trace`**: per-trace transaction first (DQ-2 inserts,
  pending drain, `cpu_snapshot_available` compute,
  `trace_meta.state = 'finalized'`), then `index.sqlite` UPDATE.
  `anomaly_count` is set *absolutely* to the per-trace
  `SELECT COUNT(*) FROM anomalies` value, not additively. This is
  the only way the retry path stays idempotent: if the per-trace
  commits and the index doesn't, a second `finalize_trace` finds
  `pending_calls` empty, inserts 0 new DQ-2 rows, and would
  underreport with additive arithmetic.
- **`delete_trace`**: filesystem operations first (eviction → stat
  → unlink), then `index.sqlite` DELETE. Files-first because a crash
  between FS and index leaves a recoverable row (next tick retries,
  unlinks become `NotFound`-idempotent); reverse order would orphan
  files on disk with no way to find them again.

The generalised rule: **when one logical update spans two serialised
transactions on two databases, the second transaction must compute
its target from durable state, not from "what the first transaction
told me to add", *unless* both operations naturally accumulate.**
`record_batch` accumulates (every batch is unique work).
`finalize_trace` and `delete_trace` are idempotent goal states.

### WAL-mode SQLite Connection drops trigger checkpoints

Dropping a `rusqlite::Connection` against a WAL-mode DB triggers an
implicit checkpoint at close: pages in `<key>.sqlite-wal` get
applied to `<key>.sqlite`, and `-wal` typically shrinks to near-zero.
`-shm` follows.

The sharp edge: any code that *stats* the sidecar sizes around a
`Storage` operation that drops a cached `Connection` will measure
different sizes before and after the drop. Caught during the
`delete_trace_freed_bytes_sums_per_trace_plus_raw` test. The fix
pattern when you need a stable pre-stat:

```rust
storage.trace_conns.remove(&key); // checkpoint NOW
let pre = stat_files(&trio_paths);
let outcome = storage.delete_trace(&key)?;
assert_eq!(outcome.freed_bytes, pre + raw_size);
```

This also matters for any future disk-usage gauge work (Phase 8 /
R-1): take stat samples at well-defined connection-lifecycle points
or you'll undercount.

### Storage is single-task; access through `Arc<tokio::sync::Mutex<…>>`

`rusqlite::Connection` is `!Send`. AD-1 keeps `Storage` single-task.
Three tokio tasks (decoder, finalize, retention) share one `Storage`
instance via `Arc<tokio::sync::Mutex<Storage>>`. Contention at the
documented load is negligible: decoder ~ms per batch, finalize ~ms
per finalized trace, retention sub-ms per pruned trace plus
filesystem syscalls. If a future slice introduces a fourth task or
makes any of these slower, audit the lock-hold durations.

### Test helpers in `tests/http_skeleton.rs`

The captured `handover/batches/` fixtures are **mid-trace
snapshots** — every Call's parent is the still-running script body
whose own exit hasn't reached us. Aggregating any of them in
isolation lands 0 user nodes. For tests that need real aggregation,
use the synthetic builders:

- `build_test_batch(...)` — empty dict + empty calls.
- `build_test_batch_with_chain(host, pid, start_time)` — top-level
  + child. Exercises BR-1 fold + `call_to_node`.
- `build_test_batch_with_unresolved_fn(...)` — DQ-1 shape.
- `build_test_batch_with_inverted_time(...)` — DQ-3 shape.
- `build_test_batch_with_orphan_pending(...)` — child whose parent
  never arrives; becomes a DQ-2 anomaly at finalize.

Tests synchronise on stdout, not on `sleep`. The `Collector` struct's
`wait_for_stdout("decoded batch", Duration::from_secs(5))` polls
every 20 ms and panics with a full stdout/stderr dump on timeout.
For tests sending N batches, use `wait_until(&collector, |stdout|
count_matches(stdout, "decoded batch") >= N, ...)`. Fixed sleeps
flake under load — `cargo test --all-targets` on a busy CI box
routinely produces 200 ms+ jitter where the 5 ms baseline tests
were written.

---

## 2026-05-26 — Out-of-order batch arrival within one `trace_id` is an accepted ingest contract

The collector is now reorder-tolerant for batches sharing a
`trace_id` (change: `tolerate-out-of-order-batches`). Recorders are
free to ship batches concurrently and/or out of produce order; the
visualizer reconstructs the trace identically to in-order arrival,
provided the full set eventually lands while the trace is still
`'active'` (or briefly reactivates per DR-3 before the missing
batch arrives).

Practically: a call referencing an `fn_id` whose introducing
`DictEntry` is in a later-arriving batch is **parked in
`pending_calls`**, not dropped. The drain pass (per batch + at
finalize) resolves the row once both its `fn_id` is in `dict` AND
its `parent_call_id` is bound (`0` or in `call_to_node`).

DQ-1 (`unresolved_fn`) is therefore emitted only at
`Storage::finalize_trace`, alongside DQ-2
(`pending_parent_at_finalize`), for residual `pending_calls` rows
that never resolved. The split:

- `fn_id ∉ dict` at finalize → DQ-1 (missing dict is the more
  diagnostic miss; emits DQ-1 even if parent is also unbound).
- `fn_id ∈ dict` at finalize but parent never seen → DQ-2.

Consequences for downstream code and UX:

- While a trace is `'active'`, `traces.anomaly_count` and the
  UI's anomaly badge **under-report DQ-1**. The badge is "stable
  at finalize," not in-flight. The status-dot pulse on `'active'`
  already signals "in flight."
- A trace finalized once and then reactivated by a late batch
  (DR-3) keeps any DQ-1 rows written by the first finalize, even
  if the late batch's dict now resolves them into `nodes`. This
  is an accepted trade-off; revisit if it becomes user-visible.
- `pending_calls` may grow larger during the active window than
  it used to: every unknown-fn_id call now parks. The §7.2
  "active trace ≤ a few MB" sizing envelope and the 30 s
  idle-finalize timeout are the existing controls — no new cap.

The `batch accepted` structured event now carries an additional
field, `dict_pending=<N>`, counting per-batch parks-on-fn_id, so
operators can see how much reorder buffering each batch triggers.

The `trace finalized` structured event now carries
`pending_dq1=<N>` alongside `pending_dq2=<N>`.

The same `pending_calls` table holds both kinds of pending rows
— no schema migration was needed. Classification at finalize is
a `SELECT fn_id FROM dict` snapshot at finalize time.

---

## How to update this file

Append new entries to the bottom under a `## YYYY-MM-DD — <topic>`
header. When a slice ships and its lesson is now encoded in the
codebase (a constant, a comment, a test), summarise rather than
delete — keep the rule, drop the slice-specific narrative. The
goal is for a future session to read this file end-to-end and
walk away with the rules, the open work, and the pointers, without
having to reconstruct the history of how each rule was discovered.
