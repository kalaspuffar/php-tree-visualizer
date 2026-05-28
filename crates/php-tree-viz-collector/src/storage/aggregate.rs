//! Per-batch fold of `Call` records into the per-trace `nodes`
//! tree. Implements `SPECIFICATION.md` §4.3 / §4.1.4 / BR-1.
//!
//! The flow inside one call to [`aggregate_calls`]:
//!
//! 1. Build the set of known `dict.fn_id` for this trace (one
//!    SELECT). The set already reflects the just-arrived batch's
//!    dict entries — `record_batch` inserts them before calling
//!    here.
//! 2. Walk `batch.calls` **in reverse order**. The recorder
//!    emits records in exit order; reversing yields
//!    parents-before-children within a batch, so most in-batch
//!    parent lookups succeed against `call_to_node` on the
//!    first try.
//! 3. For each Call:
//!    - If its `fn_id` is not yet in `dict`, park it in
//!      `pending_calls` (the introducing `DictEntry` will arrive
//!      in a later batch — see the `tolerate-out-of-order-batches`
//!      change). The Call is **not** dropped; no anomaly row is
//!      written here.
//!    - Otherwise, resolve `parent_node_id`: `1` if
//!      `c.parent == 0`, else
//!      `SELECT node_id FROM call_to_node WHERE call_id = ?`.
//!    - If parent resolved: upsert the `(parent_node_id, fn_id)`
//!      bucket (RETURNING node_id), bump the parent's
//!      `children_total_wall_ns`, map `call_id → node_id`.
//!    - If parent unresolved (cross-batch parent): insert into
//!      `pending_calls`.
//! 4. After the in-batch loop, drain `pending_calls` with a
//!    seed-then-cascade worklist: one **seed** pass finds the
//!    rows resolvable right now (`fn_id` in `dict` AND
//!    `parent_call_id` zero or in `call_to_node`), folds them,
//!    and enqueues their `call_id`s; the **cascade** then follows
//!    each resolved `call_id` to its pending children via the
//!    `parent_call_id` index, folding those whose `fn_id` is
//!    known. Total work is ~`O(N)` in the number of rows resolved
//!    rather than `O(N × depth)` — no per-level full-table scan.
//!
//! Only one anomaly kind is written by this module:
//!
//! - **DQ-3 (`inverted_time`)**: a Call has `t_out < t_in`. The
//!   Call still folds into a node (its wall delta is clamped to 0
//!   via `.max(0)` to honour DI-3), and one anomaly row with
//!   `node_id = <resulting node>` and `detail = "t_in=<I>,t_out=<O>"`
//!   is inserted.
//!
//! **DQ-1 (`unresolved_fn`)** and **DQ-2 (`pending_parent_at_finalize`)**
//! are written by `Storage::finalize_trace`. At finalize, every
//! row still in `pending_calls` is classified by whether its
//! `fn_id` is in `dict`: if not, it becomes DQ-1; otherwise (the
//! residual cause must be a never-arrived parent) it becomes
//! DQ-2. This split is necessary because batch arrival order is
//! not guaranteed to match recorder produce order — the dict-
//! defining batch may arrive after batches that reference it.

use std::collections::{HashSet, VecDeque};

use rusqlite::{params, Transaction};

use super::StorageError;
use crate::tracekey::TraceKey;
use crate::wire;

/// `node_id = 1` is the synthetic root (`SPECIFICATION.md`
/// §4.3 notes). All top-level calls (wire `parent == 0`) become
/// its children.
const SYNTHETIC_ROOT_NODE_ID: i64 = 1;

/// Stable kind strings written into `anomalies.kind`. The
/// SPECIFICATION (§4.3) treats these as a small enum; pinning them
/// to constants here keeps the literal out of the call sites so
/// a typo in `'unresloved_fn'` can't slip past the type system.
const KIND_UNRESOLVED_FN: &str = "unresolved_fn";
const KIND_INVERTED_TIME: &str = "inverted_time";
/// DQ-2: a pending row was never resolved by the time the trace
/// finalized. Written by `storage::Storage::finalize_trace`; never by
/// the in-batch aggregation path (which only emits the two kinds
/// above). Pinned here so the literal lives in one place; the
/// helper that does the actual INSERT is below.
pub(super) const KIND_PENDING_PARENT_AT_FINALIZE: &str = "pending_parent_at_finalize";

/// Counters surfaced to the caller for the `batch accepted`
/// log event and the tests.
#[derive(Debug, Default)]
pub struct AggregateOutcome {
    /// Distinct node rows touched (inserted or updated) by
    /// this batch's aggregation. Includes both the in-batch
    /// loop and the drain pass.
    pub nodes_touched: u32,
    /// Count of new `pending_calls` rows inserted during this
    /// batch's in-batch loop.
    pub pending_added: u32,
    /// Count of `pending_calls` rows resolved (and deleted)
    /// during this batch's drain pass.
    pub pending_resolved: u32,
    /// Total `pending_calls` rows remaining in the trace
    /// after the drain pass.
    pub pending_total: u32,
    /// Calls parked in `pending_calls` because their `fn_id` was
    /// not yet in the trace's `dict` when the batch was aggregated.
    /// The introducing `DictEntry` is expected in a later batch
    /// (out-of-order arrival is allowed); only rows still unresolved
    /// at finalize become DQ-1 anomalies. This counter exists so
    /// operators can see how much fn_id-reordering buffering each
    /// batch triggers via the `batch accepted` log event.
    pub dict_pending_added: u32,
    /// Calls that folded into a node despite `t_out < t_in` (DQ-3).
    /// Each one also bumps `anomalies_added`.
    pub dq3_inverted: u32,
    /// Total `anomalies` rows inserted by this batch's
    /// aggregation. Equals `dq3_inverted` today — DQ-1 and DQ-2
    /// are written exclusively by `Storage::finalize_trace`. Kept
    /// as its own field so the `index.sqlite.traces.anomaly_count`
    /// bump has a single source of truth and so future kinds can
    /// add to it without rewriting the call site.
    pub anomalies_added: u32,
    /// Number of calls in `batch.calls` that this aggregation
    /// **actually processed** (folded in-batch or parked into
    /// `pending_calls`). Redelivered calls — those whose `call_id`
    /// was already in `call_to_node` or `pending_calls` when the
    /// batch arrived — are excluded. `Storage::record_batch` binds
    /// this as the `index.sqlite.traces.call_count` delta, replacing
    /// the previous `batch.calls.len()`: see the `idempotent-ingest`
    /// capability.
    pub call_count_delta: u32,
    /// Sum of `c.t_out.saturating_sub(c.t_in).max(0)` over the same
    /// new-call subset that contributes to `call_count_delta`. The
    /// `.max(0)` clamp keeps DI-3 (`total_wall_ns ≥ 0`) intact for
    /// DQ-3 inverted-time calls, mirroring the clamp the in-batch
    /// loop uses when folding. `Storage::record_batch` binds this as
    /// the `index.sqlite.traces.total_wall_ns` delta.
    pub total_wall_ns_delta: i64,
    /// Count of calls in `batch.calls` whose `call_id` was already
    /// known (`call_to_node` or `pending_calls`) when the batch
    /// arrived, and which were therefore skipped — no fold, no park,
    /// no counter delta. Observability for at-least-once delivery;
    /// surfaced on the `batch accepted` log event as `redelivered`.
    pub redelivered_skipped: u32,
}

/// Insert the synthetic root rows (`dict.fn_id=0`,
/// `nodes.node_id=1`) if they don't already exist. Idempotent
/// across batches; subsequent invocations are no-ops via
/// `INSERT OR IGNORE`.
pub(super) fn seed_synthetic_root(tx: &Transaction) -> Result<(), StorageError> {
    tx.execute(
        "INSERT OR IGNORE INTO dict (fn_id, fqn, file, line, kind) \
         VALUES (0, '<root>', '', 0, 0)",
        [],
    )
    .map_err(|e| StorageError::Query {
        context: "seed synthetic-root dict entry",
        source: e,
    })?;
    tx.execute(
        "INSERT OR IGNORE INTO nodes \
         (node_id, parent_node_id, fn_id, depth, \
          call_count, total_wall_ns, children_total_wall_ns, \
          total_cpu_u_ns, total_cpu_s_ns, total_mem_delta_bytes, \
          abnormal_exit_count) \
         VALUES (1, NULL, 0, 0, 0, 0, 0, 0, 0, 0, 0)",
        [],
    )
    .map_err(|e| StorageError::Query {
        context: "seed synthetic-root node row",
        source: e,
    })?;
    Ok(())
}

/// Aggregate every call in `batch.calls` into the per-trace
/// `nodes` tree per BR-1. The caller must have already
/// accumulated `dict` and seeded the synthetic root (see
/// `super::Storage::record_batch`).
pub(super) fn aggregate_calls(
    tx: &Transaction,
    _trace_key: &TraceKey,
    batch: &wire::Batch,
) -> Result<AggregateOutcome, StorageError> {
    let mut outcome = AggregateOutcome::default();

    let known = known_fn_ids(tx)?;

    // ---- in-batch loop (reverse order = parents first) ----
    for c in batch.calls.iter().rev() {
        // Idempotency against per-trace `call_id` redelivery
        // (`idempotent-ingest`). The new php-analyze shipper is
        // at-least-once: under retry the same `call_id` arrives
        // more than once. If we've already recorded this call —
        // folded (`call_to_node`) or parked (`pending_calls`) —
        // skip it entirely: no fold, no park, no counter delta.
        // The two PRIMARY KEY constraints already dedup the row
        // state; this check makes the counter sites dedup-aware
        // too. Same-batch repeats are caught here as well — the
        // first iteration's fold/park populates the table the
        // second iteration's lookup hits.
        if call_id_already_known(tx, c.call_id)? {
            outcome.redelivered_skipped += 1;
            continue;
        }
        // New call this batch: contributes to the trace's
        // `call_count` and `total_wall_ns` deltas.
        // `.max(0)` mirrors the in-batch fold's clamp so DQ-3
        // inverted-time calls (`t_out < t_in`) never make
        // `traces.total_wall_ns` decrease (DI-3).
        outcome.call_count_delta += 1;
        outcome.total_wall_ns_delta += c.t_out.saturating_sub(c.t_in).max(0);

        if !known.contains(&c.fn_id) {
            // The `DictEntry` for this `fn_id` is in a batch we
            // haven't seen yet. Park the Call in `pending_calls`;
            // the drain pass (here and at finalize) will fold it
            // once the dict-defining batch arrives. No anomaly row
            // — DQ-1 is emitted exclusively by `finalize_trace`
            // for rows whose `fn_id` truly never arrives.
            insert_pending_call(tx, c)?;
            outcome.pending_added += 1;
            outcome.dict_pending_added += 1;
            continue;
        }

        let parent_node_id = resolve_parent(tx, c.parent)?;
        match parent_node_id {
            Some(pid) => {
                fold_call_into_nodes(tx, pid, c, &mut outcome)?;
                outcome.nodes_touched += 1;
                // In-batch folds use the wire `depth` directly and
                // don't seed the drain worklist; the drain's own seed
                // pass re-discovers any child made resolvable by this
                // fold (its parent is now in `call_to_node`).
            }
            None => {
                insert_pending_call(tx, c)?;
                outcome.pending_added += 1;
            }
        }
    }

    // ---- drain pending_calls ----
    //
    // Gate: a previously-pending row can only become resolvable if
    // this batch grew `dict` (unblocking an fn-pending row) or folded
    // a Call in the in-batch loop (unblocking a parent-pending child
    // via a new `call_to_node` row). A pure-park batch — empty dict
    // and zero in-batch folds — cannot resolve anything, so skip the
    // O(N) seed scan entirely. `nodes_touched` is only bumped by the
    // in-batch loop at this point (drain has not run yet), so it is
    // exactly the in-batch fold count. Treating a non-empty `dict` as
    // "may have grown" is conservative: a fully-duplicate dict runs a
    // no-op drain rather than risk skipping a needed one.
    let in_batch_folded = outcome.nodes_touched > 0;
    if !batch.dict.is_empty() || in_batch_folded {
        drain_pending(tx, &known, &mut outcome)?;
    }

    // Total pending rows after drain — operator visibility via
    // the log line. Anything left here is a genuine cross-batch
    // dependency that will resolve when the parent's batch
    // arrives, OR a DQ-2 candidate at finalize time.
    outcome.pending_total = count_pending(tx)?;

    Ok(outcome)
}

/// Select the set of `fn_id` already in this trace's `dict`
/// (including the synthetic root's `fn_id = 0`). Used to
/// pre-validate Calls so a DQ-1 doesn't blow the transaction
/// via FK violation on `nodes.fn_id`.
fn known_fn_ids(tx: &Transaction) -> Result<HashSet<u32>, StorageError> {
    let mut stmt =
        tx.prepare_cached("SELECT fn_id FROM dict")
            .map_err(|e| StorageError::Query {
                context: "prepare known_fn_ids",
                source: e,
            })?;
    let rows = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(|e| StorageError::Query {
            context: "query known_fn_ids",
            source: e,
        })?;
    let mut set = HashSet::new();
    for row in rows {
        let id = row.map_err(|e| StorageError::Query {
            context: "iterate known_fn_ids",
            source: e,
        })?;
        // Cast back from i64 (SQLite INTEGER). fn_ids are
        // u32-sized in the wire spec.
        set.insert(id as u32);
    }
    Ok(set)
}

/// Has the collector already recorded this `call_id` for this
/// trace? True iff `call_id` appears in `call_to_node` (folded) or
/// `pending_calls` (parked). Both tables have `call_id` as PRIMARY
/// KEY, so each branch is a single indexed point lookup; the
/// combined `UNION ALL … LIMIT 1` short-circuits at the first hit.
/// Used by the in-batch loop to make `Storage::record_batch`
/// idempotent against at-least-once delivery
/// (`idempotent-ingest`).
fn call_id_already_known(tx: &Transaction, call_id: u32) -> Result<bool, StorageError> {
    let mut stmt = tx
        .prepare_cached(
            "SELECT 1 FROM call_to_node WHERE call_id = ?1 \
             UNION ALL \
             SELECT 1 FROM pending_calls WHERE call_id = ?1 \
             LIMIT 1",
        )
        .map_err(|e| StorageError::Query {
            context: "prepare call_id_already_known",
            source: e,
        })?;
    match stmt.query_row(params![call_id as i64], |_| Ok(())) {
        Ok(()) => Ok(true),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(e) => Err(StorageError::Query {
            context: "query call_id_already_known",
            source: e,
        }),
    }
}

/// `parent` on the wire is the parent call's `call_id`; `0`
/// means "no parent" (top-level call → synthetic root).
fn resolve_parent(tx: &Transaction, parent_call_id: u32) -> Result<Option<i64>, StorageError> {
    if parent_call_id == 0 {
        return Ok(Some(SYNTHETIC_ROOT_NODE_ID));
    }
    let mut stmt = tx
        .prepare_cached("SELECT node_id FROM call_to_node WHERE call_id = ?1")
        .map_err(|e| StorageError::Query {
            context: "prepare resolve_parent",
            source: e,
        })?;
    let result = stmt
        .query_row(params![parent_call_id as i64], |row| row.get::<_, i64>(0))
        .map(Some);
    match result {
        Ok(node_id) => Ok(node_id),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(StorageError::Query {
            context: "query resolve_parent",
            source: e,
        }),
    }
}

/// Upsert the `(parent_node_id, fn_id)` bucket in `nodes`,
/// bump the parent's `children_total_wall_ns`, map the Call's
/// `call_id` to the resulting node, and — if the Call's wall is
/// inverted (`t_out < t_in`, DQ-3) — write an `inverted_time`
/// anomaly row attached to the resulting `node_id`. The Call still
/// folds in either case; `saturating_sub` clamps the wall delta
/// to 0 so `nodes.total_wall_ns` never decreases (DI-3).
///
/// Returns the resulting node's `node_id` so the drain cascade can
/// carry it forward as the parent_node_id of this Call's children
/// without a follow-up `call_to_node` lookup.
fn fold_call_into_nodes(
    tx: &Transaction,
    parent_node_id: i64,
    c: &wire::Call,
    outcome: &mut AggregateOutcome,
) -> Result<i64, StorageError> {
    // DI-3 requires `total_wall_ns >= children_total_wall_ns`
    // (self time non-negative). `saturating_sub` only clamps at
    // i64::MIN/MAX — for `t_out < t_in` it returns a negative
    // delta. Clamp to zero explicitly so DQ-3 Calls don't
    // *decrease* an already-aggregated wall.
    let wall = c.t_out.saturating_sub(c.t_in).max(0);
    // Memory delta is allowed to be negative (a Call that freed
    // memory) — the UI tints negative deltas red but does not
    // treat them as an invariant violation.
    let mem_delta = c.mem_out.saturating_sub(c.mem_in);
    let abnormal_count: i64 = if c.abnormal_exit { 1 } else { 0 };

    let node_id: i64 = {
        let mut stmt = tx
            .prepare_cached(UPSERT_NODE_SQL)
            .map_err(|e| StorageError::Query {
                context: "prepare upsert node",
                source: e,
            })?;
        stmt.query_row(
            params![
                parent_node_id, // ?1
                c.fn_id as i64, // ?2
                c.depth as i64, // ?3
                wall,           // ?4 total_wall_ns delta
                c.cpu_u,        // ?5
                c.cpu_s,        // ?6
                mem_delta,      // ?7
                abnormal_count, // ?8
            ],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Query {
            context: "upsert node",
            source: e,
        })?
    };

    // Maintain parent's children_total_wall_ns. Per spec §4.3
    // note: "children_total_wall_ns is incremented on the parent
    // whenever a child node has its total_wall_ns increased.
    // Self time on read is total_wall_ns - children_total_wall_ns."
    {
        let mut stmt = tx
            .prepare_cached(
                "UPDATE nodes SET children_total_wall_ns = children_total_wall_ns + ?1 \
                 WHERE node_id = ?2",
            )
            .map_err(|e| StorageError::Query {
                context: "prepare bump parent children_total_wall_ns",
                source: e,
            })?;
        stmt.execute(params![wall, parent_node_id])
            .map_err(|e| StorageError::Query {
                context: "bump parent children_total_wall_ns",
                source: e,
            })?;
    }

    // Map the wire-level call_id to the resulting node_id so
    // downstream Calls referencing this call_id as a parent can
    // look it up.
    {
        let mut stmt = tx
            .prepare_cached("INSERT OR IGNORE INTO call_to_node (call_id, node_id) VALUES (?1, ?2)")
            .map_err(|e| StorageError::Query {
                context: "prepare insert call_to_node",
                source: e,
            })?;
        stmt.execute(params![c.call_id as i64, node_id])
            .map_err(|e| StorageError::Query {
                context: "insert call_to_node",
                source: e,
            })?;
    }

    // DQ-3: t_out < t_in. The Call still folded (wall was clamped
    // to 0 above); attach the anomaly to the resulting node so the
    // UI's per-row anomaly badge lights up.
    if c.t_out < c.t_in {
        insert_inverted_time_anomaly(tx, node_id, c.call_id, c.t_in, c.t_out)?;
        outcome.dq3_inverted += 1;
        outcome.anomalies_added += 1;
    }

    Ok(node_id)
}

/// Insert a Call into `pending_calls` for a future batch's
/// drain pass. The Call's full payload is stored verbatim
/// because the drain pass needs the same deltas the in-batch
/// loop would have computed.
fn insert_pending_call(tx: &Transaction, c: &wire::Call) -> Result<(), StorageError> {
    let mut stmt = tx
        .prepare_cached(
            "INSERT OR IGNORE INTO pending_calls \
             (call_id, parent_call_id, fn_id, t_in_ns, t_out_ns, \
              cpu_u_ns, cpu_s_ns, mem_in_bytes, mem_out_bytes, abnormal_exit) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .map_err(|e| StorageError::Query {
            context: "prepare insert pending_calls row",
            source: e,
        })?;
    stmt.execute(params![
        c.call_id as i64,
        c.parent as i64,
        c.fn_id as i64,
        c.t_in,
        c.t_out,
        c.cpu_u,
        c.cpu_s,
        c.mem_in,
        c.mem_out,
        i64::from(c.abnormal_exit),
    ])
    .map_err(|e| StorageError::Query {
        context: "insert pending_calls row",
        source: e,
    })?;
    Ok(())
}

/// Resolve every pending row whose `fn_id` is in `dict` AND whose
/// `parent_call_id` is `0` (top-level) or already in `call_to_node`,
/// transitively, using a seed-then-cascade worklist. Nothing here
/// writes anomaly rows — DQ-1 and DQ-2 emission is exclusively
/// `Storage::finalize_trace`'s job.
///
/// The `known` argument carries the dict-fn_id set the caller built
/// at the top of `aggregate_calls`, already reflecting the just-
/// accumulated batch dict.
///
/// **Why seed-then-cascade.** The previous implementation re-scanned
/// the entire `pending_calls` table once per cascade level, which is
/// `O(N × depth)` — catastrophic for a deeply-recursive trace with a
/// multi-million-row backlog. Instead:
///
/// 1. **Seed:** one scan ([`DRAIN_RESOLVABLE_SQL`]) finds the rows
///    resolvable against the *current* `call_to_node` (children whose
///    parent folded in an earlier batch or the in-batch loop) plus
///    top-level rows. This also catches the "late `DictEntry`, parent
///    resolved earlier" case, because it re-examines all currently-
///    resolvable rows each drain. Fold each; enqueue its `call_id`.
/// 2. **Cascade:** pop a resolved `call_id` `p` and look up its pending
///    children by the `parent_call_id` index ([`DRAIN_CHILDREN_SQL`]);
///    fold those whose `fn_id` is known; enqueue them. Each pending
///    row is visited once (when its parent is popped), so the cascade
///    is `O(rows resolved)`.
fn drain_pending(
    tx: &Transaction,
    known: &HashSet<u32>,
    outcome: &mut AggregateOutcome,
) -> Result<(), StorageError> {
    // Worklist entries carry the resolved row's `(call_id, node_id,
    // depth)` so the cascade can fold its children without a
    // follow-up `call_to_node` lookup (node_id) or `nodes` depth
    // lookup (a child's depth is the parent's depth + 1).
    let mut worklist: VecDeque<(i64, i64, u32)> = VecDeque::new();

    // ---- seed pass ----
    let seed: Vec<PendingRow> = {
        let mut stmt =
            tx.prepare_cached(DRAIN_RESOLVABLE_SQL)
                .map_err(|e| StorageError::Query {
                    context: "prepare drain seed query",
                    source: e,
                })?;
        let rows = stmt
            .query_map(params![SYNTHETIC_ROOT_NODE_ID], PendingRow::from_row)
            .map_err(|e| StorageError::Query {
                context: "query drain seed",
                source: e,
            })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| StorageError::Query {
                context: "collect drain seed rows",
                source: e,
            })?
    };
    for row in &seed {
        // The SQL cannot test `fn_id ∈ known` against the in-memory
        // set; rows with an unknown fn_id stay pending (a future
        // batch's dict, or finalize's DQ-1, handles them).
        if !known.contains(&row.fn_id) {
            continue;
        }
        // The seed is the only place we look up the parent's depth
        // (once per seed row); the cascade carries depth forward.
        let parent_depth: u32 = tx
            .query_row(
                "SELECT depth FROM nodes WHERE node_id = ?1",
                params![row.parent_node_id],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| StorageError::Query {
                context: "lookup seed parent depth",
                source: e,
            })? as u32;
        let depth = parent_depth + 1;
        let node_id = resolve_pending_row(tx, row, depth, outcome)?;
        worklist.push_back((row.call_id, node_id, depth));
    }

    // ---- cascade ----
    while let Some((parent_call_id, parent_node_id, parent_depth)) = worklist.pop_front() {
        let children: Vec<PendingRow> = {
            let mut stmt =
                tx.prepare_cached(DRAIN_CHILDREN_SQL)
                    .map_err(|e| StorageError::Query {
                        context: "prepare drain children query",
                        source: e,
                    })?;
            let rows = stmt
                .query_map(
                    params![parent_call_id, parent_node_id],
                    PendingRow::from_row,
                )
                .map_err(|e| StorageError::Query {
                    context: "query drain children",
                    source: e,
                })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| StorageError::Query {
                    context: "collect drain children rows",
                    source: e,
                })?
        };
        let child_depth = parent_depth + 1;
        for row in &children {
            if !known.contains(&row.fn_id) {
                continue; // fn_id not yet in dict — stays pending
            }
            let node_id = resolve_pending_row(tx, row, child_depth, outcome)?;
            worklist.push_back((row.call_id, node_id, child_depth));
        }
    }
    Ok(())
}

/// Fold one pending row into the `nodes` tree at the given `depth`
/// and delete it from `pending_calls`. `row.parent_node_id` is the
/// resolved parent's node_id (the synthetic root for top-level
/// rows). Returns the resulting node's `node_id` so the caller can
/// carry it onto the cascade worklist as the children's
/// parent_node_id.
fn resolve_pending_row(
    tx: &Transaction,
    row: &PendingRow,
    depth: u32,
    outcome: &mut AggregateOutcome,
) -> Result<i64, StorageError> {
    let synthetic_call = wire::Call {
        call_id: row.call_id as u32,
        parent: row.parent_call_id as u32,
        fn_id: row.fn_id,
        depth,
        t_in: row.t_in_ns,
        t_out: row.t_out_ns,
        cpu_u: row.cpu_u_ns,
        cpu_s: row.cpu_s_ns,
        mem_in: row.mem_in_bytes,
        mem_out: row.mem_out_bytes,
        abnormal_exit: row.abnormal_exit != 0,
    };
    let node_id = fold_call_into_nodes(tx, row.parent_node_id, &synthetic_call, outcome)?;
    tx.execute(
        "DELETE FROM pending_calls WHERE call_id = ?1",
        params![row.call_id],
    )
    .map_err(|e| StorageError::Query {
        context: "delete drained pending",
        source: e,
    })?;
    outcome.pending_resolved += 1;
    outcome.nodes_touched += 1;
    Ok(node_id)
}

/// SQL for the drain **seed** pass's parent-resolvability filter. The
/// LEFT JOIN + COALESCE collapses two cases into one row shape:
///
/// - `parent_call_id = 0` (top-level): no `call_to_node` match;
///   COALESCE picks `?1` (the synthetic root's node_id), so the row
///   is resolvable against the root.
/// - `parent_call_id != 0` and present in `call_to_node`:
///   COALESCE picks `c.node_id`, the resolved parent.
///
/// Rows where `parent_call_id != 0` and `call_to_node` has no match
/// are filtered out by the `WHERE` clause — they remain pending.
const DRAIN_RESOLVABLE_SQL: &str = "
SELECT p.call_id, p.parent_call_id, p.fn_id, p.t_in_ns, p.t_out_ns,
       p.cpu_u_ns, p.cpu_s_ns, p.mem_in_bytes, p.mem_out_bytes,
       p.abnormal_exit, COALESCE(c.node_id, ?1) AS parent_node_id
FROM pending_calls p
LEFT JOIN call_to_node c ON c.call_id = p.parent_call_id
WHERE p.parent_call_id = 0 OR c.node_id IS NOT NULL
";

/// SQL for the drain **cascade**: the pending children of a single
/// just-resolved parent `call_id` (`?1`), backed by the
/// `idx_pending_parent` index. `?2` is the parent's `node_id`, bound
/// in as the synthetic `parent_node_id` column so the row shape
/// matches [`PendingRow::from_row`] without a join.
const DRAIN_CHILDREN_SQL: &str = "
SELECT call_id, parent_call_id, fn_id, t_in_ns, t_out_ns,
       cpu_u_ns, cpu_s_ns, mem_in_bytes, mem_out_bytes,
       abnormal_exit, ?2 AS parent_node_id
FROM pending_calls
WHERE parent_call_id = ?1
";

/// DQ-1 anomaly insert. Called by `Storage::finalize_trace` for
/// every row left in `pending_calls` at finalize whose `fn_id`
/// never made it into `dict`. The Call never folded into a `nodes`
/// row, so `node_id` is NULL; the `detail` string carries the
/// missing `fn_id` so the UI / operator can identify which dict
/// entry was lost in transit. Never called from the in-batch
/// aggregation path or the drain pass — both park unknown-fn_id
/// calls instead, deferring the verdict to finalize.
pub(super) fn insert_unresolved_fn_anomaly(
    tx: &Transaction,
    call_id: u32,
    fn_id: u32,
) -> Result<(), StorageError> {
    let mut stmt = tx
        .prepare_cached(
            "INSERT INTO anomalies (node_id, kind, sample_call_id, detail) \
             VALUES (NULL, ?1, ?2, ?3)",
        )
        .map_err(|e| StorageError::Query {
            context: "prepare unresolved_fn anomaly insert",
            source: e,
        })?;
    let detail = format!("fn_id={fn_id}");
    stmt.execute(params![KIND_UNRESOLVED_FN, call_id as i64, detail])
        .map_err(|e| StorageError::Query {
            context: "insert unresolved_fn anomaly",
            source: e,
        })?;
    Ok(())
}

/// DQ-3 anomaly insert. The Call still folds into a node (via
/// `saturating_sub`, which clamps the wall delta to 0), so
/// `node_id` is the resulting bucket and the `detail` string
/// records the raw inverted `t_in` / `t_out` values for diagnostics.
fn insert_inverted_time_anomaly(
    tx: &Transaction,
    node_id: i64,
    call_id: u32,
    t_in: i64,
    t_out: i64,
) -> Result<(), StorageError> {
    let mut stmt = tx
        .prepare_cached(
            "INSERT INTO anomalies (node_id, kind, sample_call_id, detail) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .map_err(|e| StorageError::Query {
            context: "prepare inverted_time anomaly insert",
            source: e,
        })?;
    let detail = format!("t_in={t_in},t_out={t_out}");
    stmt.execute(params![node_id, KIND_INVERTED_TIME, call_id as i64, detail])
        .map_err(|e| StorageError::Query {
            context: "insert inverted_time anomaly",
            source: e,
        })?;
    Ok(())
}

/// DQ-2 anomaly insert. Called by `Storage::finalize_trace` for every
/// row left in `pending_calls` at finalize time — the row's parent's
/// Call record never reached the collector, so the Call never folded
/// into a node and `node_id` is NULL. `detail` carries the orphan
/// `parent_call_id` so the operator can see which wire-level parent
/// went missing.
pub(super) fn insert_pending_parent_at_finalize_anomaly(
    tx: &Transaction,
    call_id: u32,
    parent_call_id: u32,
) -> Result<(), StorageError> {
    let mut stmt = tx
        .prepare_cached(
            "INSERT INTO anomalies (node_id, kind, sample_call_id, detail) \
             VALUES (NULL, ?1, ?2, ?3)",
        )
        .map_err(|e| StorageError::Query {
            context: "prepare pending_parent_at_finalize anomaly insert",
            source: e,
        })?;
    let detail = format!("parent_call_id={parent_call_id}");
    stmt.execute(params![
        KIND_PENDING_PARENT_AT_FINALIZE,
        call_id as i64,
        detail,
    ])
    .map_err(|e| StorageError::Query {
        context: "insert pending_parent_at_finalize anomaly",
        source: e,
    })?;
    Ok(())
}

fn count_pending(tx: &Transaction) -> Result<u32, StorageError> {
    tx.query_row("SELECT COUNT(*) FROM pending_calls", [], |row| {
        row.get::<_, i64>(0)
    })
    .map(|n| n as u32)
    .map_err(|e| StorageError::Query {
        context: "count pending_calls",
        source: e,
    })
}

#[derive(Debug)]
struct PendingRow {
    call_id: i64,
    parent_call_id: i64,
    fn_id: u32,
    t_in_ns: i64,
    t_out_ns: i64,
    cpu_u_ns: i64,
    cpu_s_ns: i64,
    mem_in_bytes: i64,
    mem_out_bytes: i64,
    abnormal_exit: i64,
    parent_node_id: i64,
}

impl PendingRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            call_id: row.get(0)?,
            parent_call_id: row.get(1)?,
            fn_id: row.get::<_, i64>(2)? as u32,
            t_in_ns: row.get(3)?,
            t_out_ns: row.get(4)?,
            cpu_u_ns: row.get(5)?,
            cpu_s_ns: row.get(6)?,
            mem_in_bytes: row.get(7)?,
            mem_out_bytes: row.get(8)?,
            abnormal_exit: row.get(9)?,
            parent_node_id: row.get(10)?,
        })
    }
}

/// UPSERT for the `nodes` bucket per BR-1. `RETURNING node_id`
/// (SQLite 3.35+; bundled rusqlite ships much newer) hands back
/// the node_id regardless of whether the INSERT or the UPDATE
/// branch ran, so the caller can bump the parent and write
/// `call_to_node` without a separate SELECT.
const UPSERT_NODE_SQL: &str = "
INSERT INTO nodes (
  parent_node_id, fn_id, depth,
  call_count, total_wall_ns, total_cpu_u_ns, total_cpu_s_ns,
  total_mem_delta_bytes, abnormal_exit_count
) VALUES (
  ?1, ?2, ?3,
  1, ?4, ?5, ?6,
  ?7, ?8
)
ON CONFLICT(parent_node_id, fn_id) DO UPDATE SET
  call_count            = nodes.call_count + 1,
  total_wall_ns         = nodes.total_wall_ns + excluded.total_wall_ns,
  total_cpu_u_ns        = nodes.total_cpu_u_ns + excluded.total_cpu_u_ns,
  total_cpu_s_ns        = nodes.total_cpu_s_ns + excluded.total_cpu_s_ns,
  total_mem_delta_bytes = nodes.total_mem_delta_bytes + excluded.total_mem_delta_bytes,
  abnormal_exit_count   = nodes.abnormal_exit_count + excluded.abnormal_exit_count
RETURNING node_id
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracekey::TraceKey;
    use crate::wire::{Batch, Call, DictEntry, Meta};
    use rusqlite::Connection;

    /// Open an in-memory SQLite, apply the per-trace schema,
    /// seed the synthetic root. Returns a single Connection.
    /// Tests can run multiple aggregations against this; each
    /// aggregation opens its own transaction inside the test.
    fn fresh_trace_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; \
             PRAGMA synchronous = NORMAL; \
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        conn.execute_batch(super::super::schema::TRACE_SCHEMA)
            .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        seed_synthetic_root(&tx).unwrap();
        tx.commit().unwrap();
        conn
    }

    fn aggregate_in(conn: &mut Connection, batch: &Batch) -> AggregateOutcome {
        let key = TraceKey::from_raw("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let tx = conn.transaction().unwrap();
        let outcome = aggregate_calls(&tx, &key, batch).unwrap();
        tx.commit().unwrap();
        outcome
    }

    fn meta() -> Meta {
        Meta {
            schema_version: 1,
            trace_id: "00000000-0000-0000-0000-000000000000".into(),
            host: "h".into(),
            pid: 1,
            start_time: 1,
            sapi: "cli".into(),
            uri_or_script: "x".into(),
            dropped_records: 0,
        }
    }

    fn dict_entry(fn_id: u32) -> DictEntry {
        DictEntry {
            fn_id,
            fqn: format!("ns\\fn_{fn_id}"),
            file: "/tmp/x.php".into(),
            line: 1,
            kind: 0,
        }
    }

    fn call(call_id: u32, parent: u32, fn_id: u32, wall: i64) -> Call {
        Call {
            call_id,
            parent,
            fn_id,
            depth: if parent == 0 { 1 } else { 2 },
            t_in: 0,
            t_out: wall,
            cpu_u: wall / 10,
            cpu_s: wall / 20,
            mem_in: 0,
            mem_out: wall * 8,
            abnormal_exit: false,
        }
    }

    /// Insert dict entries directly (the real `record_batch` would
    /// have done this before calling `aggregate_calls`).
    fn install_dict(conn: &Connection, entries: &[DictEntry]) {
        for d in entries {
            conn.execute(
                "INSERT OR IGNORE INTO dict (fn_id, fqn, file, line, kind) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![d.fn_id as i64, &d.fqn, &d.file, d.line as i64, d.kind as i64],
            )
            .unwrap();
        }
    }

    // ---- synthetic root ----

    #[test]
    fn synthetic_root_exists_after_seed() {
        let conn = fresh_trace_db();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE node_id = 1 AND parent_node_id IS NULL AND fn_id = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM dict WHERE fn_id = 0 AND fqn = '<root>'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn seed_is_idempotent() {
        let conn = fresh_trace_db();
        // Call seed again on its own transaction.
        let tx = conn.unchecked_transaction().unwrap();
        seed_synthetic_root(&tx).unwrap();
        tx.commit().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes WHERE node_id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    // ---- in-batch loop ----

    #[test]
    fn single_top_level_call_lands_under_root() {
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7)]);
        let batch = Batch {
            meta: meta(),
            dict: vec![dict_entry(7)],
            calls: vec![call(1, 0, 7, 200)],
        };
        let outcome = aggregate_in(&mut conn, &batch);
        assert_eq!(outcome.nodes_touched, 1);
        assert_eq!(outcome.pending_added, 0);

        let (parent_id, fn_id, call_count, total_wall): (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT parent_node_id, fn_id, call_count, total_wall_ns FROM nodes WHERE fn_id = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(parent_id, SYNTHETIC_ROOT_NODE_ID);
        assert_eq!(fn_id, 7);
        assert_eq!(call_count, 1);
        assert_eq!(total_wall, 200);

        // root's children_total_wall_ns bumped
        let root_children: i64 = conn
            .query_row(
                "SELECT children_total_wall_ns FROM nodes WHERE node_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(root_children, 200);

        // call_to_node populated
        let mapped: i64 = conn
            .query_row(
                "SELECT node_id FROM call_to_node WHERE call_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(mapped > 1, "should be a real node, not synthetic root");
    }

    #[test]
    fn br1_buckets_same_parent_fn() {
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7)]);
        let batch = Batch {
            meta: meta(),
            dict: vec![dict_entry(7)],
            calls: vec![call(1, 0, 7, 100), call(2, 0, 7, 200), call(3, 0, 7, 50)],
        };
        let outcome = aggregate_in(&mut conn, &batch);
        // 3 calls collapse into ONE node row (BR-1). nodes_touched
        // counts touches, not distinct nodes — 3 calls all touched
        // the same row, so the counter is 3.
        assert_eq!(outcome.nodes_touched, 3);

        let n_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes WHERE fn_id = 7", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n_rows, 1, "BR-1: one bucket per (parent, fn)");

        let (cc, tw): (i64, i64) = conn
            .query_row(
                "SELECT call_count, total_wall_ns FROM nodes WHERE fn_id = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cc, 3);
        assert_eq!(tw, 350);
    }

    #[test]
    fn different_fn_ids_under_same_parent_are_distinct_nodes() {
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7), dict_entry(8)]);
        let batch = Batch {
            meta: meta(),
            dict: vec![dict_entry(7), dict_entry(8)],
            calls: vec![call(1, 0, 7, 100), call(2, 0, 8, 200)],
        };
        aggregate_in(&mut conn, &batch);
        let n_under_root: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE parent_node_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_under_root, 2);
    }

    #[test]
    fn three_deep_chain_in_reverse_exit_order_resolves_in_one_batch() {
        // Wire shape: children exit first, so the batch is
        // [grandchild, child, parent] in array order. Reverse
        // iteration (parent first) means no pending needed.
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(1), dict_entry(2), dict_entry(3)]);
        let batch = Batch {
            meta: meta(),
            dict: vec![dict_entry(1), dict_entry(2), dict_entry(3)],
            calls: vec![
                // grandchild (call_id=10), parent is child (call_id=20)
                Call {
                    call_id: 10,
                    parent: 20,
                    fn_id: 1,
                    depth: 3,
                    t_in: 1,
                    t_out: 5,
                    cpu_u: 0,
                    cpu_s: 0,
                    mem_in: 0,
                    mem_out: 0,
                    abnormal_exit: false,
                },
                // child (call_id=20), parent is top-level (call_id=30)
                Call {
                    call_id: 20,
                    parent: 30,
                    fn_id: 2,
                    depth: 2,
                    t_in: 1,
                    t_out: 10,
                    cpu_u: 0,
                    cpu_s: 0,
                    mem_in: 0,
                    mem_out: 0,
                    abnormal_exit: false,
                },
                // top-level (call_id=30), parent = 0
                Call {
                    call_id: 30,
                    parent: 0,
                    fn_id: 3,
                    depth: 1,
                    t_in: 1,
                    t_out: 20,
                    cpu_u: 0,
                    cpu_s: 0,
                    mem_in: 0,
                    mem_out: 0,
                    abnormal_exit: false,
                },
            ],
        };
        let outcome = aggregate_in(&mut conn, &batch);
        assert_eq!(outcome.pending_added, 0, "reverse iter handles in-batch");
        assert_eq!(outcome.pending_total, 0);

        // Three non-root nodes + synthetic root
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 4, "synthetic root + 3 user-call nodes");
    }

    // ---- pending_calls + drain ----

    #[test]
    fn cross_batch_parent_goes_pending_and_resolves_later() {
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7), dict_entry(8)]);

        // Batch A: a child whose parent (call_id=100) isn't seen.
        let batch_a = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![call(10, 100, 7, 50)],
        };
        let outcome_a = aggregate_in(&mut conn, &batch_a);
        assert_eq!(outcome_a.pending_added, 1);
        assert_eq!(outcome_a.nodes_touched, 0);
        assert_eq!(outcome_a.pending_total, 1);

        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending, 1);
        let nodes_fn_7: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes WHERE fn_id = 7", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(nodes_fn_7, 0, "child should be pending, not aggregated yet");

        // Batch B: the parent shows up.
        let batch_b = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![call(100, 0, 8, 300)],
        };
        let outcome_b = aggregate_in(&mut conn, &batch_b);
        assert_eq!(outcome_b.pending_resolved, 1);
        assert_eq!(outcome_b.pending_total, 0);

        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending, 0);
        let nodes_fn_7: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes WHERE fn_id = 7", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(nodes_fn_7, 1, "child should now be aggregated under fn=8");
    }

    #[test]
    fn pending_whose_parent_never_arrives_stays_put() {
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7), dict_entry(99)]);

        // Batch with one orphan child (parent call_id=999 never arrives).
        let orphan_batch = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![call(10, 999, 7, 50)],
        };
        aggregate_in(&mut conn, &orphan_batch);

        // Send 10 unrelated batches; none reference call_id=999 as a Call.
        for i in 0..10 {
            let unrelated_batch = Batch {
                meta: meta(),
                dict: vec![],
                calls: vec![call(500 + i, 0, 99, 100)],
            };
            aggregate_in(&mut conn, &unrelated_batch);
        }

        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pending_calls WHERE call_id = 10",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1, "orphan must still be pending");
    }

    #[test]
    fn pure_park_batches_skip_drain_then_dict_batch_resolves_backlog() {
        // The drain gate: a batch with an empty `dict` that folds
        // nothing in-batch cannot make any pending row resolvable, so
        // its drain is skipped. The accumulated backlog must still
        // resolve when a later dict-bearing batch arrives.
        let mut conn = fresh_trace_db();
        // fn=8 deliberately NOT in dict yet.
        install_dict(&conn, &[dict_entry(7)]);

        // Two pure-park batches: top-level calls with fn=8 (unknown),
        // empty batch dict → parked on fn_id, drain skipped.
        for cid in [10u32, 11u32] {
            let park_batch = Batch {
                meta: meta(),
                dict: vec![],
                calls: vec![call(cid, 0, 8, 100)],
            };
            aggregate_in(&mut conn, &park_batch);
        }
        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending, 2, "both fn=8 calls park while fn=8 is unknown");
        let nodes_fn8: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes WHERE fn_id = 8", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(nodes_fn8, 0, "nothing folded yet");

        // Dict-bearing batch introduces fn=8 → gate runs the drain →
        // both parked rows (parent=0, fn=8 now known) resolve.
        install_dict(&conn, &[dict_entry(8)]);
        let dict_batch = Batch {
            meta: meta(),
            dict: vec![dict_entry(8)],
            calls: vec![],
        };
        let outcome = aggregate_in(&mut conn, &dict_batch);
        assert_eq!(outcome.pending_resolved, 2, "backlog drains on dict batch");

        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending, 0);
        let nodes_fn8: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE fn_id = 8 AND parent_node_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            nodes_fn8, 1,
            "both fn=8 calls collapse into one root bucket"
        );
        let n_anom: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_anom, 0, "drain writes no anomalies");
    }

    // ---- idempotent-ingest: redelivery is a counter no-op ----

    #[test]
    fn redelivered_call_is_skipped_and_counter_delta_excludes_it() {
        // First delivery: one top-level Call folds normally.
        // outcome.call_count_delta = 1, redelivered_skipped = 0.
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7)]);
        let batch = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![call(1, 0, 7, 100)],
        };
        let outcome = aggregate_in(&mut conn, &batch);
        assert_eq!(outcome.call_count_delta, 1);
        assert_eq!(outcome.total_wall_ns_delta, 100);
        assert_eq!(outcome.redelivered_skipped, 0);
        assert_eq!(outcome.nodes_touched, 1);
        let (node_calls, node_wall): (i64, i64) = conn
            .query_row(
                "SELECT call_count, total_wall_ns FROM nodes WHERE fn_id = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(node_calls, 1);
        assert_eq!(node_wall, 100);

        // Second delivery (same call_id): redelivered, skipped.
        // outcome.call_count_delta = 0, redelivered_skipped = 1.
        // Node row is unchanged — no double-fold.
        let outcome2 = aggregate_in(&mut conn, &batch);
        assert_eq!(outcome2.call_count_delta, 0);
        assert_eq!(outcome2.total_wall_ns_delta, 0);
        assert_eq!(outcome2.redelivered_skipped, 1);
        assert_eq!(outcome2.nodes_touched, 0);
        let (node_calls, node_wall): (i64, i64) = conn
            .query_row(
                "SELECT call_count, total_wall_ns FROM nodes WHERE fn_id = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(node_calls, 1, "redelivery must not double-count");
        assert_eq!(node_wall, 100, "redelivery must not double-add wall");
    }

    #[test]
    fn redelivered_pending_call_is_skipped_no_double_park() {
        // First delivery: parent unknown, so the Call parks. One
        // row in pending_calls; pending_added = 1.
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7)]);
        let batch = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![call(10, 999, 7, 50)], // parent 999 never seen
        };
        let outcome = aggregate_in(&mut conn, &batch);
        assert_eq!(outcome.call_count_delta, 1);
        assert_eq!(outcome.pending_added, 1);
        assert_eq!(outcome.redelivered_skipped, 0);
        let n_pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_pending, 1);

        // Re-deliver the same Call. Now its call_id is in
        // pending_calls → skipped. pending_added = 0, no second
        // row inserted, call_count_delta = 0.
        let outcome2 = aggregate_in(&mut conn, &batch);
        assert_eq!(outcome2.call_count_delta, 0);
        assert_eq!(outcome2.pending_added, 0);
        assert_eq!(outcome2.redelivered_skipped, 1);
        let n_pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_pending, 1, "no second park");
    }

    #[test]
    fn same_call_id_twice_in_one_batch_records_once() {
        // Pathological-but-possible: a single batch carrying the
        // same call_id twice. Reverse iteration processes the
        // SECOND-occurring entry first (`batch.calls.iter().rev()`).
        // The first iteration folds it (no in-batch_loop dedup hit
        // because call_to_node was empty); the second iteration's
        // dedup lookup hits and skips it.
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7)]);
        let batch = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![call(5, 0, 7, 100), call(5, 0, 7, 999)],
        };
        let outcome = aggregate_in(&mut conn, &batch);
        assert_eq!(outcome.call_count_delta, 1);
        assert_eq!(outcome.redelivered_skipped, 1);
        assert_eq!(outcome.nodes_touched, 1);
        let (node_calls, node_wall): (i64, i64) = conn
            .query_row(
                "SELECT call_count, total_wall_ns FROM nodes WHERE fn_id = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(node_calls, 1);
        // Wall is whichever occurrence folded first (reverse order →
        // the LATER call_id wins in iteration order, but call_ids are
        // equal here, so it's the second-in-array, which the wire spec
        // says shouldn't happen; either 100 or 999 is correct per the
        // spec's tiebreak — we don't pin which).
        assert!(node_wall == 100 || node_wall == 999, "got {node_wall}");
    }

    // ---- DQ-1 deferral (park unknown-fn_id calls; finalize emits) ----

    #[test]
    fn unknown_fn_id_call_parks_and_rest_aggregate() {
        // Previously this test asserted in-batch DQ-1 skip + anomaly.
        // After `tolerate-out-of-order-batches`, the unknown-fn_id
        // call parks in `pending_calls` instead. No anomaly row is
        // written during `record_batch`; the rest of the batch's
        // valid calls still fold.
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7)]); // fn_id=99 deliberately missing
        let batch = Batch {
            meta: meta(),
            dict: vec![dict_entry(7)],
            calls: vec![call(1, 0, 99, 100), call(2, 0, 7, 200)],
        };
        let outcome = aggregate_in(&mut conn, &batch);
        assert_eq!(outcome.dict_pending_added, 1);
        assert_eq!(outcome.anomalies_added, 0);
        assert_eq!(outcome.nodes_touched, 1, "fn=7 still folds");
        assert_eq!(outcome.pending_total, 1);

        let n_under_root: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE parent_node_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_under_root, 1, "only fn=7 should land in nodes");

        let n_anom: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n_anom, 0,
            "no anomaly during record_batch; DQ-1 deferred to finalize"
        );

        let n_pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pending_calls WHERE call_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_pending, 1, "unknown-fn_id call sits in pending_calls");
    }

    #[test]
    fn synthetic_root_fn0_is_in_known_set_so_root_children_resolve() {
        // Children of root reference parent=0 (the wire convention),
        // so resolve_parent returns Some(1) without consulting
        // call_to_node, and the child's own fn_id is checked against
        // the dict. This test just sanity-checks that fn_id=0 is in
        // the known set after seed — the entry point used by both
        // aggregate_calls and drain_pending.
        let conn = fresh_trace_db();
        let tx = conn.unchecked_transaction().unwrap();
        let known = known_fn_ids(&tx).unwrap();
        assert!(
            known.contains(&0u32),
            "synthetic root's fn_id must be in known set"
        );
    }

    // ---- DQ-1 anomaly emission moved to finalize ----

    #[test]
    fn unknown_fn_id_call_does_not_write_anomaly_during_record() {
        // The aggregation layer never writes `unresolved_fn` itself.
        // That row appears only at `Storage::finalize_trace`, against
        // any pending row whose fn_id never made it into `dict`.
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7)]); // fn_id=99 missing
        let batch = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![call(42, 0, 99, 100)],
        };
        let outcome = aggregate_in(&mut conn, &batch);
        assert_eq!(outcome.dict_pending_added, 1);
        assert_eq!(outcome.anomalies_added, 0);
        assert_eq!(outcome.nodes_touched, 0);

        let n_anom: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_anom, 0);

        let pending_call_id: i64 = conn
            .query_row("SELECT call_id FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending_call_id, 42);
    }

    #[test]
    fn multiple_unknown_fn_id_calls_all_park() {
        // Three unknown-fn_id calls + one valid call. Pre-change,
        // the three would have produced three `unresolved_fn` rows
        // immediately. Post-change, they wait in pending_calls for
        // the dict to arrive (or for finalize to convert them to
        // DQ-1).
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7)]);
        let batch = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![
                call(1, 0, 99, 100),
                call(2, 0, 99, 100),
                call(3, 0, 99, 100),
                call(4, 0, 7, 200), // valid — still folds
            ],
        };
        let outcome = aggregate_in(&mut conn, &batch);
        assert_eq!(outcome.dict_pending_added, 3);
        assert_eq!(outcome.anomalies_added, 0);
        assert_eq!(outcome.nodes_touched, 1, "only fn=7 folds");

        let n_pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_pending, 3);

        let n_anom: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_anom, 0);
    }

    // ---- DQ-3 anomaly inserts ----

    #[test]
    fn dq3_writes_inverted_time_anomaly_row() {
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7)]);
        let inverted = Call {
            call_id: 99,
            parent: 0,
            fn_id: 7,
            depth: 1,
            t_in: 500,
            t_out: 400,
            cpu_u: 0,
            cpu_s: 0,
            mem_in: 0,
            mem_out: 0,
            abnormal_exit: false,
        };
        let batch = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![inverted],
        };
        let outcome = aggregate_in(&mut conn, &batch);
        assert_eq!(outcome.dq3_inverted, 1);
        assert_eq!(outcome.anomalies_added, 1);
        assert_eq!(outcome.nodes_touched, 1, "Call still folds into a node");

        // The Call landed in a real node with wall=0 (saturating_sub).
        let (node_id_actual, total_wall): (i64, i64) = conn
            .query_row(
                "SELECT node_id, total_wall_ns FROM nodes WHERE fn_id = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(total_wall, 0, "saturating_sub clamps the inverted wall");

        let (anom_node_id, kind, sample_call_id, detail): (i64, String, i64, String) = conn
            .query_row(
                "SELECT node_id, kind, sample_call_id, detail FROM anomalies",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            anom_node_id, node_id_actual,
            "inverted_time anomaly attaches to the resulting node"
        );
        assert_eq!(kind, KIND_INVERTED_TIME);
        assert_eq!(sample_call_id, 99);
        assert_eq!(detail, "t_in=500,t_out=400");
    }

    #[test]
    fn dq3_on_same_bucket_attaches_anomaly_to_that_node() {
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7)]);
        let normal = Call {
            call_id: 1,
            parent: 0,
            fn_id: 7,
            depth: 1,
            t_in: 0,
            t_out: 100,
            cpu_u: 0,
            cpu_s: 0,
            mem_in: 0,
            mem_out: 0,
            abnormal_exit: false,
        };
        let inverted = Call {
            call_id: 2,
            parent: 0,
            fn_id: 7,
            depth: 1,
            t_in: 500,
            t_out: 400,
            cpu_u: 0,
            cpu_s: 0,
            mem_in: 0,
            mem_out: 0,
            abnormal_exit: false,
        };
        let batch = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![normal, inverted],
        };
        aggregate_in(&mut conn, &batch);

        let (node_id_actual, call_count, total_wall): (i64, i64, i64) = conn
            .query_row(
                "SELECT node_id, call_count, total_wall_ns FROM nodes WHERE fn_id = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(call_count, 2);
        assert_eq!(
            total_wall, 100,
            "the normal call contributes 100; the inverted clamps to 0"
        );

        let anom_node_id: i64 = conn
            .query_row(
                "SELECT node_id FROM anomalies WHERE kind = 'inverted_time'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(anom_node_id, node_id_actual);
    }

    #[test]
    fn no_anomaly_for_normal_calls() {
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7)]);
        let batch = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![call(1, 0, 7, 100)],
        };
        let outcome = aggregate_in(&mut conn, &batch);
        assert_eq!(outcome.anomalies_added, 0);

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    // ---- DQ-2 helper ----

    #[test]
    fn dq2_anomaly_helper_writes_expected_row() {
        let conn = fresh_trace_db();
        let tx = conn.unchecked_transaction().unwrap();
        insert_pending_parent_at_finalize_anomaly(&tx, 42, 999).unwrap();
        tx.commit().unwrap();

        let (node_id, kind, sample_call_id, detail): (Option<i64>, String, i64, String) = conn
            .query_row(
                "SELECT node_id, kind, sample_call_id, detail FROM anomalies",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(node_id, None);
        assert_eq!(kind, KIND_PENDING_PARENT_AT_FINALIZE);
        assert_eq!(kind, "pending_parent_at_finalize");
        assert_eq!(sample_call_id, 42);
        assert_eq!(detail, "parent_call_id=999");
    }

    #[test]
    fn drain_pass_folds_pending_when_late_dict_arrives() {
        // Replaces the pre-change `dq1_via_drain_pending_still_writes_anomaly`.
        // The drain pass no longer writes DQ-1 anomalies. Instead, a
        // pending row whose fn_id was unknown at park time gets folded
        // into `nodes` the moment a later batch carries the missing
        // `DictEntry`. No anomaly row is written.
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(7)]); // fn=99 still missing

        // Batch A: child Call with parent=999 (unseen) AND fn_id=99
        // (also missing). With the new logic, the fn_id check parks
        // the call first; the same row would be parked by the parent
        // check anyway. Either way it lands in pending_calls.
        let batch_a = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![call(10, 999, 99, 100)],
        };
        let outcome_a = aggregate_in(&mut conn, &batch_a);
        assert_eq!(outcome_a.dict_pending_added, 1);
        assert_eq!(outcome_a.anomalies_added, 0);
        assert_eq!(outcome_a.pending_added, 1);

        let n_pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_pending, 1);

        // Batch B: introduces fn_id=99 in the dict and the parent
        // (call_id=999, parent=root, fn=7). Both blockers go away
        // at once → drain folds the pending row.
        let batch_b = Batch {
            meta: meta(),
            dict: vec![dict_entry(99)],
            calls: vec![call(999, 0, 7, 500)],
        };
        // Mirror what `record_batch` would do: insert the new dict
        // entry before calling `aggregate_calls`. The test harness's
        // `aggregate_in` doesn't run the full `record_batch` path,
        // so dict accumulation happens here.
        install_dict(&conn, &[dict_entry(99)]);
        let outcome_b = aggregate_in(&mut conn, &batch_b);
        assert_eq!(outcome_b.pending_resolved, 1);
        assert_eq!(outcome_b.anomalies_added, 0);

        let n_pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_pending, 0);

        let n_anom: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n_anom, 0,
            "drain pass folds the call; no DQ-1 anomaly anywhere"
        );

        let n_fn_99: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes WHERE fn_id = 99", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n_fn_99, 1);
    }

    // ---- out-of-order arrival ----

    #[test]
    fn late_dict_resolves_parked_top_level_call() {
        // Batch A: one top-level Call with fn=99 — but fn=99 not in
        // dict yet. Expectation: one pending row, no nodes, no
        // anomalies.
        let mut conn = fresh_trace_db();
        // No `install_dict` for fn=99 yet.
        let batch_a = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![call(1, 0, 99, 100)],
        };
        let outcome_a = aggregate_in(&mut conn, &batch_a);
        assert_eq!(outcome_a.dict_pending_added, 1);
        assert_eq!(outcome_a.nodes_touched, 0);
        assert_eq!(outcome_a.anomalies_added, 0);
        let n_pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_pending, 1);
        let n_anom: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_anom, 0);

        // Batch B: introduces fn=99 in the dict, no calls. Expectation:
        // the pending row drains, one nodes row appears for
        // (parent=root, fn=99), still no anomalies.
        install_dict(&conn, &[dict_entry(99)]);
        let batch_b = Batch {
            meta: meta(),
            dict: vec![dict_entry(99)],
            calls: vec![],
        };
        let outcome_b = aggregate_in(&mut conn, &batch_b);
        assert_eq!(outcome_b.pending_resolved, 1);
        assert_eq!(outcome_b.anomalies_added, 0);
        let n_pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_pending, 0);
        let n_anom: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_anom, 0);
        let (parent, fn_id_): (i64, i64) = conn
            .query_row(
                "SELECT parent_node_id, fn_id FROM nodes WHERE fn_id = 99",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(parent, SYNTHETIC_ROOT_NODE_ID);
        assert_eq!(fn_id_, 99);
    }

    #[test]
    fn late_dict_resolves_parked_child_call() {
        // Batch A: parent Call (call_id=20, parent=0, fn=8 in dict)
        // and child Call (call_id=10, parent=20, fn=99 NOT in dict).
        // Expectation: parent folds, child parks on fn_id.
        let mut conn = fresh_trace_db();
        install_dict(&conn, &[dict_entry(8)]);
        let batch_a = Batch {
            meta: meta(),
            dict: vec![dict_entry(8)],
            calls: vec![call(10, 20, 99, 50), call(20, 0, 8, 300)],
        };
        let outcome_a = aggregate_in(&mut conn, &batch_a);
        assert_eq!(outcome_a.dict_pending_added, 1, "child parked on fn_id");
        assert!(outcome_a.nodes_touched >= 1, "parent folds");

        let n_pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_pending, 1);
        let parent_node_id: i64 = conn
            .query_row("SELECT node_id FROM nodes WHERE fn_id = 8", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(parent_node_id > 1);

        // Batch B: introduces fn=99 in dict, no calls.
        install_dict(&conn, &[dict_entry(99)]);
        let batch_b = Batch {
            meta: meta(),
            dict: vec![dict_entry(99)],
            calls: vec![],
        };
        let outcome_b = aggregate_in(&mut conn, &batch_b);
        assert_eq!(outcome_b.pending_resolved, 1);
        assert_eq!(outcome_b.anomalies_added, 0);

        // Child now folded under the parent's node.
        let (child_parent_node, child_fn_id): (i64, i64) = conn
            .query_row(
                "SELECT parent_node_id, fn_id FROM nodes WHERE fn_id = 99",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(child_parent_node, parent_node_id);
        assert_eq!(child_fn_id, 99);
        let n_anom: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_anom, 0);
    }

    #[test]
    fn reverse_order_three_batches_yield_same_tree() {
        // One trace, three logical batches A/B/C. Replay forward in
        // one DB, reverse in another, then compare the resulting
        // `nodes` rows under the same `(parent_node_id, fn_id)` key.
        // The trees must be byte-identical apart from auto-assigned
        // `node_id`s.
        //
        // Layout:
        //   A introduces fn=7 (top-level Call call_id=1, wall=100)
        //   B introduces fn=8 as a child of A's call (call_id=2,
        //       parent=1, wall=200) — fn=8 dict in B.
        //   C introduces fn=9 as a child of B's call (call_id=3,
        //       parent=2, wall=50) — fn=9 dict in C.
        let mk_a = || Batch {
            meta: meta(),
            dict: vec![dict_entry(7)],
            calls: vec![call(1, 0, 7, 100)],
        };
        let mk_b = || Batch {
            meta: meta(),
            dict: vec![dict_entry(8)],
            calls: vec![call(2, 1, 8, 200)],
        };
        let mk_c = || Batch {
            meta: meta(),
            dict: vec![dict_entry(9)],
            calls: vec![call(3, 2, 9, 50)],
        };

        // Forward arrival order: A, B, C.
        let mut conn_fwd = fresh_trace_db();
        for batch in [mk_a(), mk_b(), mk_c()] {
            install_dict(&conn_fwd, &batch.dict);
            aggregate_in(&mut conn_fwd, &batch);
        }

        // Reverse arrival order: C, B, A.
        let mut conn_rev = fresh_trace_db();
        for batch in [mk_c(), mk_b(), mk_a()] {
            install_dict(&conn_rev, &batch.dict);
            aggregate_in(&mut conn_rev, &batch);
        }

        // Compare the per-`(parent_node_id, fn_id)` projection. We
        // can't compare `node_id`s directly (they're autoincrement
        // and depend on insertion order), so project to the bucket
        // key + counters and assert the sets are equal.
        fn project(conn: &rusqlite::Connection) -> Vec<(i64, i64, i64, i64)> {
            let mut stmt = conn
                .prepare(
                    "SELECT COALESCE(parent_node_id, -1), fn_id, call_count, total_wall_ns \
                     FROM nodes \
                     ORDER BY fn_id",
                )
                .unwrap();
            stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        }

        // Pre-finalize: both DBs may differ in pending_calls (reverse
        // run still has the C-batch row parked until later batches
        // arrive). Drain both fully by sending one terminal "all
        // dicts present" no-op batch, then compare.
        let terminal = Batch {
            meta: meta(),
            dict: vec![],
            calls: vec![],
        };
        aggregate_in(&mut conn_fwd, &terminal);
        aggregate_in(&mut conn_rev, &terminal);

        // After drain, no pending rows should remain in either DB.
        let pending_fwd: i64 = conn_fwd
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        let pending_rev: i64 = conn_rev
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending_fwd, 0, "forward run drained");
        assert_eq!(pending_rev, 0, "reverse run drained too");

        // No anomalies in either run.
        let anom_fwd: i64 = conn_fwd
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        let anom_rev: i64 = conn_rev
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(anom_fwd, 0);
        assert_eq!(anom_rev, 0);

        let fwd = project(&conn_fwd);
        let rev = project(&conn_rev);
        assert_eq!(
            fwd, rev,
            "post-drain node projections must be equal under any \
             arrival permutation within a trace"
        );
    }
}
