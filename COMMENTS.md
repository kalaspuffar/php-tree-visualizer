# COMMENTS.md

Supplementary notes, clarifications, and review comments that sit on top of
`SPECIFICATION.md` v0.1. When `SPECIFICATION.md` and this file conflict,
this file is treated as the more recent clarification — surface the
discrepancy before acting on it.

Append new entries at the bottom; do not edit history.

---

## 2026-05-23 — Placeholder

No supplementary notes at this time. `SPECIFICATION.md` v0.2 (with the
UX-augmented §3.3) is the working source of truth. Use the authority chain
in §1 for wire-format questions and the implementation phases in §10 for
ordering.

## 2026-05-23 — Workflow: push / review / merge / checkout main is manual

The end-of-step handoff is split between the Rust developer and the
operator (the human reviewer):

- **Rust developer's responsibilities, per step:** branch from `main`,
  open the OpenSpec change, implement, run `cargo fmt` / `cargo clippy
  --all-targets --all-features -- -D warnings` / `cargo test` /
  `openspec validate <change-id>`, commit, then stop and report the
  branch name, the OpenSpec change ID, and a short summary. The
  developer does **not** push, merge, or switch branches.
- **Operator's responsibilities, per step:** push the feature branch,
  open the pull request, review, merge to `main`, and `git checkout
  main`. The operator confirms completion before the developer starts
  the next step.

Implications for the developer:

- Treat `git push` and any operation on `main` as out of scope. If a
  push appears to be needed (e.g. CI on the branch), surface it as a
  question rather than acting.
- Each new step branches from `main` (which the operator has already
  fast-forwarded to include the previous step's merge). Do **not**
  branch from the previous step's local branch — that branch has been
  superseded by the merge commit on `main`.
- The OpenSpec change archive step (`openspec archive <change-id>`) is
  the developer's responsibility, but it happens after the merge —
  i.e. once back on `main` with the merge commit visible. Confirm with
  the operator before archiving if the timing is ambiguous.

## 2026-05-23 — OpenSpec parser gotchas (caught the hard way)

Three rules the `openspec validate` parser enforces that don't show
up in the templates and have each cost a roundtrip:

- **MODIFIED requirement headers must match the existing main-spec
  header exactly** (whitespace-insensitive, but otherwise literal).
  When writing a delta under `## MODIFIED Requirements`, copy the
  `### Requirement: <name>` line verbatim from
  `openspec/specs/<capability>/spec.md`. If you "improve" the title
  while editing the body, archive will fail with a header-mismatch
  error and the change has to be re-edited.
- **ADDED requirements need `SHALL` or `MUST` in the first non-empty
  body line.** Spreading SHALL across multiple sentences (e.g. "For
  every Call …, the collector SHALL …") triggers
  `must contain SHALL or MUST`. Rephrase so the very first sentence
  carries the modal: "The collector SHALL …, for every Call …".
- **Archive the predecessor change before proposing the next one
  that MODIFIES the same requirement.** If `change-A` MODIFIES
  `Requirement X` and stays open while `change-B` also wants to
  MODIFY `Requirement X`, the deltas race at archive time. Run
  `/opsx:archive change-A` first (after operator merge), then
  `/opsx:propose change-B`.

Also: **`openspec/` is gitignored**. `git status` will look empty
after creating proposal / design / specs / tasks artifacts; that's
correct. Never `git add openspec/`. The artifacts live on the
developer's local disk and propagate forward only via the
`openspec/changes/archive/YYYY-MM-DD-<name>/` directory that the
archive command creates locally.

## 2026-05-23 — `i64::saturating_sub` does NOT clamp at zero

`saturating_sub` saturates at `i64::MIN` / `i64::MAX`, **not at 0**.
`400i64.saturating_sub(500)` returns `-100`, not `0`. We discovered
this during `anomaly-detection`: DQ-3 Calls (`t_out < t_in`) had
been silently producing *negative* wall deltas across all earlier
slices, which then *decreased* `nodes.total_wall_ns` and violated
DI-3 (`total_wall_ns >= children_total_wall_ns`).

Fix pattern, used in `crates/php-tree-viz-collector/src/storage/`:

- `aggregate.rs::fold_call_into_nodes` — `let wall =
  c.t_out.saturating_sub(c.t_in).max(0);`
- `mod.rs::record_batch` — per-call wall delta sum uses
  `.saturating_sub(…).max(0)` for the same reason.

Whenever you write `t_out - t_in` or similar wall/CPU arithmetic
that must stay non-negative, chain `.max(0)`. Or use a
`wall_delta(c)` helper if a third site shows up.

## 2026-05-23 — `Storage::record_batch` ordering: per-trace first, then index

Since `anomaly-detection` (`feat/anomaly-detection`), the
`record_batch` two-DB ordering is:

1. Open per-trace transaction. Mirror `trace_meta`, accumulate
   `dict`, seed synthetic root, run `aggregate::aggregate_calls`
   (which writes nodes, call_to_node, pending_calls, anomalies).
   Commit. The aggregation `outcome` (including
   `anomalies_added`) is the output.
2. Open `index.sqlite` transaction. UPSERT the `traces` row with
   `outcome.anomalies_added` bound to the `anomaly_count` delta
   parameter. Commit.

The order is deliberate: it makes "index ahead of per-trace"
impossible. The remaining failure mode is "per-trace landed but
index didn't" — the trace simply doesn't appear in list queries
until the extension retries the batch. That's the better window:
no ghost rows pointing at empty SQLite files.

Any future writer that touches both databases inside one
`record_batch` invocation must keep this ordering. If a new
counter on `traces` needs to reflect per-trace state, plumb it
through `AggregateOutcome` and add it to the index UPSERT
parameters the same way `anomalies_added` is.

## 2026-05-23 — Captured fixtures cannot exercise aggregation on their own

All three `handover/batches/{flat_calls,json_batch,recursive_walk}/`
families are **mid-trace snapshots**. In every one of them, every
chain of Calls roots on a parent `call_id` whose own Call record
hasn't reached the collector yet (the script body is still
executing when the batch was captured). Aggregating any of them
in isolation lands 0 user nodes — everything goes to
`pending_calls`.

For tests that need real aggregation, real `call_to_node` rows,
real anomaly rows, etc., use the synthetic batch builders in
`crates/php-tree-viz-collector/tests/http_skeleton.rs`:

- `build_test_batch(schema_version, trace_id, host, pid,
  start_time)` — empty dict + empty calls; the workhorse for
  pure HTTP / index-DB tests.
- `build_test_batch_with_chain(host, pid, start_time)` — one
  top-level Call + one child; exercises the BR-1 fold and
  `call_to_node`.
- `build_test_batch_with_unresolved_fn(host, pid, start_time)` —
  one Call referencing `fn_id=99` (deliberately absent from dict).
  Canonical DQ-1 shape.
- `build_test_batch_with_inverted_time(host, pid, start_time)` —
  one Call with `t_in=500, t_out=400, fn=7 (in dict)`. Canonical
  DQ-3 shape.

The captured fixtures are still useful as wire-format smoke tests
(real bytes from a real extension) and as "no anomalies on a
well-formed input" baselines. They are **not** suitable for
asserting "aggregation produced N nodes".

## 2026-05-23 — rusqlite's `tx.execute(SQL, …)` re-parses every time

`rusqlite::Connection::execute(sql, params)` internally calls
`prepare(sql)`, which does **not** consult the statement cache.
For a hot path that runs the same SQL more than ~100 times per
call, this dominates wall time: `aggregation-core` had a 10K-call
batch taking ~5 s before this was understood; switching the inner
loops to `tx.prepare_cached(SQL)` + `stmt.execute(params)` cut it
to ~200 ms.

Concretely: every SQL string in
`crates/php-tree-viz-collector/src/storage/aggregate.rs` is run
through `tx.prepare_cached(...)`. If you add another hot-path
INSERT or UPDATE — anomaly inserts, future retention deletes,
finalize updates — use `prepare_cached`, not `tx.execute`.

`tx.execute(sql, …)` is fine for one-shot statements (the
synthetic-root seed, the `trace_meta` mirror, the `traces`
UPSERT) where re-parsing once per batch is invisible.

## 2026-05-23 — Tests synchronise on stdout, not on `sleep`

Once `aggregation-core` made the decoder's per-batch work
~200 ms (up from ~5 ms in the placeholder slice), fixed-sleep
tests flaked: a `std::thread::sleep(Duration::from_millis(250))`
after a `200 OK` reply wasn't always enough for the per-trace
transaction to commit.

There's a helper on the test `Collector` struct
(`crates/php-tree-viz-collector/tests/http_skeleton.rs`):

```rust
collector.wait_for_stdout("decoded batch", Duration::from_secs(5));
```

It polls the captured stdout every 20 ms until the substring
appears, panicking with a full stdout + stderr dump on timeout.
Use this — not `sleep` — whenever a test needs to observe the
post-decoder state of `index.sqlite` or the per-trace
`<key>.sqlite`. For tests that send N batches in a row, poll
until `stdout.matches("decoded batch").count() >= N`.

## 2026-05-23 — Anomaly `kind` strings are constants, not literals

`SPECIFICATION.md` §4.3 fixes the `anomalies.kind` column to a
small enum: `'unresolved_fn'`, `'pending_parent_at_finalize'`,
`'inverted_time'`. The schema has no `CHECK` constraint, so a
typo would silently land in the table.

Pinned in `crates/php-tree-viz-collector/src/storage/aggregate.rs`:

```rust
const KIND_UNRESOLVED_FN: &str = "unresolved_fn";
const KIND_INVERTED_TIME: &str = "inverted_time";
```

Tests assert on the literal strings (so they catch a constant
that drifts from the spec), but production call sites only ever
reference the constants. The third kind
(`pending_parent_at_finalize`) is owned by the idle-finalize
slice — when adding it, define `KIND_PENDING_PARENT_AT_FINALIZE`
the same way; do not inline the string.

## 2026-05-23 — `TraceKey::from_raw` is no longer `#[cfg(test)]`

`tracekey.rs::from_raw(s)` wraps a `String` into a `TraceKey`
without validation. It was test-only — the only production
constructor is `from_meta(meta)`, which validates and either
copies the wire `trace_id` or synthesises from `(host, pid,
start_time)`. `idle-finalize` needed to round-trip 32-hex stems
*back* out of `SELECT trace_key FROM traces` so the finalize
loop could call `finalize_trace(&key, …)`, so the gate had to
come off.

The doc comment on `from_raw` now spells out the contract:
production callers SHOULD only pass strings that this codebase
previously produced via `from_meta` and persisted into a
`traces.trace_key` column. There is no runtime check; the
caller is trusted because the upstream of every production use
is our own write path. If a future caller wants to accept
external input as a trace key, build a validating constructor
beside `from_raw` rather than feeding the unchecked one.

Concrete production callers today (`crates/php-tree-viz-collector`):

- `storage::Storage::list_idle_active_traces` — the only one.

If you add a second one, audit it: what's the source of the
string, and is the implicit "32-hex stem produced by us"
contract honoured?

## 2026-05-23 — Two-DB counters: reconcile absolutely at finalize, accumulate at record

Two distinct write patterns now touch `index.sqlite.traces.anomaly_count`:

- **`Storage::record_batch`** uses `anomaly_count = anomaly_count
  + excluded.anomaly_count` in `UPSERT_TRACE_SQL`. Correct — the
  per-batch delta is computed inside the per-trace transaction
  and bumped on the next index transaction. The two transactions
  commit in order (per-trace first), so a crash between them
  leaves the per-trace ahead; the extension's retry resends the
  batch, the per-trace inserts the same anomalies *again* (we
  accept this trade-off — see `anomaly-detection`'s design.md),
  and the index catches up. Drift window is bounded to "one
  failed batch's worth".
- **`Storage::finalize_trace`** must use `anomaly_count = ?1`
  with the *absolute* per-trace `SELECT COUNT(*) FROM anomalies`
  value as the parameter. Additive arithmetic here looks fine on
  the happy path but diverges from the
  `traces.anomaly_count == per-trace COUNT(*)` invariant under
  a crash between finalize_trace's per-trace commit and its
  index commit:

  1. Per-trace tx commits: pending drained, N DQ-2 rows inserted,
     `trace_meta.state = 'finalized'`.
  2. Process killed before the index UPDATE.
  3. On restart, the finalize loop's next tick sees the trace
     still `state = 'active'` in the index and retries.
  4. The retry's `pending_calls` is already empty → 0 new DQ-2
     inserts → if the SQL were additive (`+ 0`), the index
     counter would stay at its pre-finalize value, *missing N*.

  Computing the absolute count from inside the per-trace tx and
  passing it through means the retry's index UPDATE reconciles
  to the right number regardless of how many partial attempts
  preceded it.

Test pin: `late_batch_after_finalize_reactivates_state` and
`finalize_trace_is_idempotent_under_retry` in
`crates/php-tree-viz-collector/src/storage/mod.rs::tests` cover
the happy path and the simulated crash window respectively. The
crash test rolls the index DB back manually after the first
`finalize_trace` returns, then calls `finalize_trace` again on
the same key and asserts (a) no duplicate DQ-2 rows landed and
(b) the index counter is now correct.

The rule generalises: **when one logical update spans two
serialised transactions on two databases, the second
transaction must be able to compute its target value from
durable state, not from "what the first transaction told me to
add".** Additive deltas only work when both transactions commit
under a single failure boundary (one fsync, one process). The
per-trace + index pair doesn't qualify.

## 2026-05-23 — Dropping a WAL-mode SQLite Connection checkpoints the sidecars

Dropping a `rusqlite::Connection` against a WAL-mode database
triggers an implicit checkpoint at close: pages in
`<key>.sqlite-wal` get applied to the main `.sqlite` file and the
`-wal` typically shrinks to near-zero. `-shm` follows. This is
standard SQLite behaviour but it has a sharp edge in any code that
*stats* the sidecar sizes relative to the connection's lifecycle.

Caught during `retention-sweeper`'s `delete_trace_freed_bytes_sums_per_trace_plus_raw`
test. The original test:

1. `record_batch(...)` — leaves `-wal` and `-shm` populated.
2. Test stat's the trio → sees the populated sizes (~115 KB).
3. Test calls `delete_trace(&key)`.
4. `delete_trace` does `self.trace_conns.remove(key)` *first* —
   dropping the cached `Connection`, which checkpoints WAL → main
   and shrinks the sidecars to ~0.
5. `delete_trace` then stats → sees the post-checkpoint sizes (~57 KB).
6. Test asserts pre-stat == helper's stat. Fails by ~58 KB.

Both numbers are "correct" measurements of the same files at
different points in their lifecycle. The bug was in the test's
mental model, not the helper.

**The rule:** if you need to compare a pre-and-post measurement of
WAL-mode SQLite file sizes across a code path that may drop the
`Connection`, drop it yourself first so both measurements are
taken from the same lifecycle state. In the fixed test:

```rust
storage.trace_conns.remove(&key); // checkpoint NOW
let trio_total = stat_files(&trio_paths);
let outcome = storage.delete_trace(&key).unwrap();
assert_eq!(outcome.freed_bytes, trio_total + raw_size);
```

This generalises: every operation that asserts on per-trace
SQLite *byte-size* invariants — disk-usage gauges (future Phase 8
work per §3.6 / R-1), retention threshold experiments, file-size
based test fixtures — needs the same discipline. The PHP API
read path doesn't have this concern because it opens its own
read-only connections that don't write to the WAL.

Pinned in `crates/php-tree-viz-collector/src/storage/mod.rs::tests::delete_trace_freed_bytes_sums_per_trace_plus_raw`.
