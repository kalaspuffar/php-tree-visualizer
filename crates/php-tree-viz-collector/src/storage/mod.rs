//! Storage layer: `index.sqlite` for the trace list, a per-trace
//! `<key>.sqlite` for the dict / aggregated tree.
//!
//! Today writes only the trace row (in `index.sqlite`) and the
//! mirrored `trace_meta` + accumulated `dict` (in per-trace
//! files). The `nodes`, `call_to_node`, `pending_calls`, and
//! `anomalies` tables are created with the schema so the
//! aggregation slice that follows is purely additive.
//!
//! Implements `SPECIFICATION.md` §4.2 / §4.3 / §8.2 / AD-2.

mod aggregate;
mod error;
mod schema;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

pub use error::StorageError;

pub(crate) use aggregate::AggregateOutcome;

/// Counters surfaced by `Storage::finalize_trace`. The finalize loop
/// reads these to write its `finalized trace …` log line; tests read
/// them to assert the per-trace DQ-2 count and the trace's
/// `cpu_snapshot_available` outcome.
#[derive(Debug, Default, Clone, Copy)]
pub struct FinalizeOutcome {
    /// Rows inserted into `anomalies` with
    /// `kind = 'pending_parent_at_finalize'` during this finalize
    /// pass. Equals the number of rows that were in `pending_calls`
    /// at finalize time.
    pub pending_dq2: u32,
    /// `false` when every non-root `nodes` row in this trace has
    /// `total_cpu_u_ns + total_cpu_s_ns == 0` (the extension was
    /// configured with `cpu_snapshot_mode = off`, or every Call was
    /// sub-µs). Drives the UI's "CPU unavailable" mode per F-6.9.
    pub cpu_snapshot_available: bool,
}

use crate::http::BatchSubmission;
use crate::tracekey::TraceKey;
use crate::wire;

/// Owns the index connection and the (unbounded) cache of
/// per-trace connections. Single-task by design (AD-1); not
/// `Send + Sync` because rusqlite::Connection isn't.
pub struct Storage {
    index_conn: Connection,
    trace_conns: HashMap<TraceKey, Connection>,
    traces_dir: PathBuf,
}

// rusqlite::Connection is not Debug; hand-roll a Debug impl that
// omits the connections and surfaces the operationally interesting
// state (where the storage lives, how many traces we've touched).
impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Storage")
            .field("traces_dir", &self.traces_dir)
            .field("trace_conns_count", &self.trace_conns.len())
            .finish()
    }
}

impl Storage {
    /// Open (or create) `<data_dir>/index.sqlite` and prepare the
    /// per-trace connection map. Called once at server startup
    /// before the listener binds; a failure here exits the process
    /// with status `3`.
    pub fn open(data_dir: &Path, traces_dir: PathBuf) -> Result<Self, StorageError> {
        let index_path = data_dir.join("index.sqlite");
        let index_conn = open_connection(&index_path, schema::INDEX_SCHEMA)?;
        Ok(Self {
            index_conn,
            trace_conns: HashMap::new(),
            traces_dir,
        })
    }

    /// Record a decoded batch into both databases.
    ///
    /// On `index.sqlite`: upsert the `traces` row (INSERT on first
    /// batch, UPDATE-with-increment on subsequent batches).
    ///
    /// On `<traces_dir>/<trace_key>.sqlite`: open lazily, apply
    /// schema on first contact, mirror `trace_meta`, accumulate
    /// `dict` entries with `INSERT OR IGNORE`.
    ///
    /// `received_at_ns` is the collector's `CLOCK_REALTIME` value
    /// captured at the moment the batch arrived in the decoder
    /// task — used for `first_batch_at_ns` (only on insert) and
    /// `last_batch_at_ns` (every upsert).
    pub fn record_batch(
        &mut self,
        submission: &BatchSubmission,
        batch: &wire::Batch,
        received_at_ns: i64,
    ) -> Result<AggregateOutcome, StorageError> {
        let key = &submission.trace_key;
        let meta = &batch.meta;

        // Sum per-batch wall ns. Clamp each call's delta to ≥ 0
        // so a DQ-3 Call (t_out < t_in) doesn't *decrease* the
        // running `traces.total_wall_ns`. The raw t_in / t_out
        // values still flow through to aggregate_calls, which
        // writes the inverted_time anomaly row with the original
        // (signed) values in `detail`.
        let call_count = batch.calls.len() as i64;
        let total_wall_delta: i64 = batch
            .calls
            .iter()
            .map(|c| c.t_out.saturating_sub(c.t_in).max(0))
            .sum();

        // ---- per-trace SQLite (runs first so its outcome feeds
        //      the index update with the actual anomaly count) ----
        let trace_conn = self.ensure_trace_conn(key)?;

        let tx = trace_conn.transaction().map_err(|e| StorageError::Query {
            context: "trace begin",
            source: e,
        })?;

        // Mirror trace_meta. INSERT OR REPLACE because the mirror
        // is overwritten in full on every batch (no per-batch
        // increment here — the aggregated counters live in index).
        tx.execute(
            MIRROR_TRACE_META_SQL,
            params![
                key.as_str(),
                &meta.trace_id,
                &meta.host,
                meta.pid as i64,
                meta.start_time,
                &meta.sapi,
                &meta.uri_or_script,
                "active",
                meta.dropped_records as i64,
                1i64, // cpu_snapshot_available — idle-finalize refines later
            ],
        )
        .map_err(|e| StorageError::Query {
            context: "mirror trace_meta",
            source: e,
        })?;

        // Accumulate dict.
        {
            let mut stmt = tx
                .prepare(INSERT_DICT_SQL)
                .map_err(|e| StorageError::Query {
                    context: "prepare dict insert",
                    source: e,
                })?;
            for entry in &batch.dict {
                stmt.execute(params![
                    entry.fn_id as i64,
                    &entry.fqn,
                    &entry.file,
                    entry.line as i64,
                    entry.kind as i64,
                ])
                .map_err(|e| StorageError::Query {
                    context: "insert dict entry",
                    source: e,
                })?;
            }
        }

        // Seed the synthetic root (idempotent INSERT OR IGNORE)
        // and aggregate the batch's Calls into the per-trace
        // nodes tree. Both run inside the same transaction so a
        // reader never sees a partial batch. The outcome's
        // `anomalies_added` feeds the index update below.
        aggregate::seed_synthetic_root(&tx)?;
        let outcome = aggregate::aggregate_calls(&tx, key, batch)?;

        tx.commit().map_err(|e| StorageError::Query {
            context: "trace commit",
            source: e,
        })?;

        // ---- index.sqlite upsert ----
        //
        // Runs AFTER the per-trace transaction so the `anomaly_count`
        // bump reflects what actually landed in the per-trace
        // `anomalies` table. If this index transaction fails after
        // the per-trace one committed, the trace will appear "empty"
        // in list queries until the extension retries the same batch
        // and the index update succeeds; the per-trace SQLite mean-
        // while holds the durable record. Reverse desync (index
        // ahead of per-trace) is not possible with this ordering.
        let anomalies_delta = outcome.anomalies_added as i64;
        let tx = self
            .index_conn
            .transaction()
            .map_err(|e| StorageError::Query {
                context: "index begin",
                source: e,
            })?;
        tx.execute(
            UPSERT_TRACE_SQL,
            params![
                key.as_str(),                // ?1 trace_key
                &meta.trace_id,              // ?2 trace_id
                &meta.host,                  // ?3 host
                meta.pid as i64,             // ?4 pid (sqlite INTEGER is i64)
                meta.start_time,             // ?5 start_time_ns
                &meta.sapi,                  // ?6 sapi
                &meta.uri_or_script,         // ?7 uri_or_script
                received_at_ns,              // ?8 first_batch_at_ns / last_batch_at_ns
                call_count,                  // ?9 call_count delta
                total_wall_delta,            // ?10 total_wall_ns delta
                meta.dropped_records as i64, // ?11 dropped_records
                anomalies_delta,             // ?12 anomaly_count delta
            ],
        )
        .map_err(|e| StorageError::Query {
            context: "upsert traces row",
            source: e,
        })?;
        tx.commit().map_err(|e| StorageError::Query {
            context: "index commit",
            source: e,
        })?;

        Ok(outcome)
    }

    /// Test-only: does the per-trace connection cache currently hold
    /// an entry for `key`? Used to assert the LRU eviction on
    /// `finalize_trace`.
    #[cfg(test)]
    pub(crate) fn has_cached_trace_conn(&self, key: &TraceKey) -> bool {
        self.trace_conns.contains_key(key)
    }

    fn ensure_trace_conn(&mut self, key: &TraceKey) -> Result<&mut Connection, StorageError> {
        if !self.trace_conns.contains_key(key) {
            let path = self.traces_dir.join(format!("{}.sqlite", key.as_str()));
            let conn = open_connection(&path, schema::TRACE_SCHEMA)?;
            self.trace_conns.insert(key.clone(), conn);
        }
        Ok(self
            .trace_conns
            .get_mut(key)
            .expect("just inserted; lookup must succeed"))
    }

    /// List every `'active'` trace whose `last_batch_at_ns` precedes
    /// `cutoff_ns`. Used by the idle-finalize loop; backed by the
    /// covering index `idx_traces_state_lastbatch` (per
    /// `SPECIFICATION.md` §4.2). Returns owned `TraceKey`s so the
    /// caller can then drive `finalize_trace` without holding the
    /// statement's cursor across the per-trace work.
    pub fn list_idle_active_traces(
        &mut self,
        cutoff_ns: i64,
    ) -> Result<Vec<TraceKey>, StorageError> {
        let mut stmt = self
            .index_conn
            .prepare_cached(
                "SELECT trace_key FROM traces \
                 WHERE state = 'active' AND last_batch_at_ns < ?1",
            )
            .map_err(|e| StorageError::Query {
                context: "prepare list_idle_active_traces",
                source: e,
            })?;
        let rows = stmt
            .query_map(params![cutoff_ns], |row| row.get::<_, String>(0))
            .map_err(|e| StorageError::Query {
                context: "query list_idle_active_traces",
                source: e,
            })?;
        let mut keys = Vec::new();
        for row in rows {
            let raw = row.map_err(|e| StorageError::Query {
                context: "iterate list_idle_active_traces",
                source: e,
            })?;
            keys.push(TraceKey::from_raw(&raw));
        }
        Ok(keys)
    }

    /// Close a trace's lifecycle: drain `pending_calls` into DQ-2
    /// anomalies, compute `cpu_snapshot_available`, flip
    /// `state = 'finalized'` in both databases, and evict the
    /// per-trace connection from the cache.
    ///
    /// Per-trace transaction commits before the index transaction
    /// — same ordering as `record_batch`. The index can never be
    /// "ahead of" the per-trace DB; the failure window is just
    /// "per-trace committed but index didn't", which a retry on the
    /// next tick reconciles idempotently (DQ-2 rows aren't
    /// re-inserted because `pending_calls` is already empty).
    ///
    /// `now_ns` is accepted for forward compatibility (e.g. recording
    /// a finalize timestamp later) but is unused in this slice.
    pub fn finalize_trace(
        &mut self,
        key: &TraceKey,
        _now_ns: i64,
    ) -> Result<FinalizeOutcome, StorageError> {
        let trace_conn = self.ensure_trace_conn(key)?;
        let tx = trace_conn.transaction().map_err(|e| StorageError::Query {
            context: "finalize trace begin",
            source: e,
        })?;

        // ---- DQ-2 inserts + pending drain ----
        let pending = collect_pending_rows(&tx)?;
        let pending_dq2 = pending.len() as u32;
        for (call_id, parent_call_id) in &pending {
            aggregate::insert_pending_parent_at_finalize_anomaly(
                &tx,
                *call_id as u32,
                *parent_call_id as u32,
            )?;
        }
        // Bulk wipe — cheaper than per-row DELETE and we just turned
        // every row above into an anomaly. The transaction makes
        // this either-both-or-neither with the inserts.
        tx.execute("DELETE FROM pending_calls", [])
            .map_err(|e| StorageError::Query {
                context: "drain pending_calls at finalize",
                source: e,
            })?;

        // ---- cpu_snapshot_available ----
        let cpu_available = compute_cpu_snapshot_available(&tx)?;

        // ---- anomaly_count: take the absolute per-trace row count
        //      (post-insert) so the index UPDATE below reconciles
        //      rather than accumulates. This makes finalize_trace
        //      idempotent under crash-then-retry: a second invocation
        //      sees `pending_calls` already empty, inserts zero new
        //      anomaly rows, and overwrites `traces.anomaly_count`
        //      with the same (already-correct) value. Additive
        //      arithmetic would have left the index counter
        //      underreporting after a crash between the per-trace
        //      and index commits.
        let absolute_anomaly_count: i64 = tx
            .query_row("SELECT COUNT(*) FROM anomalies", [], |row| row.get(0))
            .map_err(|e| StorageError::Query {
                context: "query absolute anomaly_count at finalize",
                source: e,
            })?;

        // ---- flip per-trace state ----
        tx.execute(
            "UPDATE trace_meta SET state = 'finalized', cpu_snapshot_available = ?1",
            params![i64::from(cpu_available)],
        )
        .map_err(|e| StorageError::Query {
            context: "update trace_meta state at finalize",
            source: e,
        })?;

        tx.commit().map_err(|e| StorageError::Query {
            context: "finalize trace commit",
            source: e,
        })?;

        // ---- index.sqlite: separate transaction, runs second so a
        //      crash in between leaves the per-trace DB carrying the
        //      durable record. Idempotent retry on next tick.
        let tx = self
            .index_conn
            .transaction()
            .map_err(|e| StorageError::Query {
                context: "finalize index begin",
                source: e,
            })?;
        tx.execute(
            "UPDATE traces \
             SET state = 'finalized', \
                 anomaly_count = ?1, \
                 cpu_snapshot_available = ?2 \
             WHERE trace_key = ?3",
            params![
                absolute_anomaly_count,
                i64::from(cpu_available),
                key.as_str(),
            ],
        )
        .map_err(|e| StorageError::Query {
            context: "update traces row at finalize",
            source: e,
        })?;
        tx.commit().map_err(|e| StorageError::Query {
            context: "finalize index commit",
            source: e,
        })?;

        // Per `SPECIFICATION.md` §3.1: "Per-trace SQLite connections
        // are cached in an LRU keyed by `TraceKey`. The LRU evicts on
        // idle-finalize …". The cache is a plain HashMap today; the
        // retention slice will turn it into a real LRU with the
        // configured cap.
        self.trace_conns.remove(key);

        Ok(FinalizeOutcome {
            pending_dq2,
            cpu_snapshot_available: cpu_available,
        })
    }
}

/// Pull every `(call_id, parent_call_id)` pair out of `pending_calls`
/// into an owned Vec, so the statement's cursor is dropped before the
/// caller mutates the table. Used by `Storage::finalize_trace`.
fn collect_pending_rows(tx: &Connection) -> Result<Vec<(i64, i64)>, StorageError> {
    let mut stmt = tx
        .prepare_cached("SELECT call_id, parent_call_id FROM pending_calls")
        .map_err(|e| StorageError::Query {
            context: "prepare collect_pending_rows",
            source: e,
        })?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .map_err(|e| StorageError::Query {
            context: "query collect_pending_rows",
            source: e,
        })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| StorageError::Query {
            context: "iterate collect_pending_rows",
            source: e,
        })?);
    }
    Ok(out)
}

/// `cpu_snapshot_available` per `SPECIFICATION.md` §4.2: `false` when
/// every non-root `nodes` row has `total_cpu_u_ns + total_cpu_s_ns
/// == 0`. The synthetic root (`node_id = 1`) is excluded because its
/// counters stay at zero by design. An empty trace (no user nodes)
/// reports `false` — there is nothing to suggest CPU data was
/// available.
fn compute_cpu_snapshot_available(tx: &Connection) -> Result<bool, StorageError> {
    let any_cpu: i64 = tx
        .query_row(
            "SELECT CASE \
                WHEN COALESCE(SUM(total_cpu_u_ns) + SUM(total_cpu_s_ns), 0) > 0 \
                THEN 1 ELSE 0 \
              END \
              FROM nodes WHERE node_id != 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Query {
            context: "query cpu_snapshot_available",
            source: e,
        })?;
    Ok(any_cpu != 0)
}

/// Open a SQLite file, apply pragmas, run the schema-version
/// gate. Used for both index.sqlite and per-trace databases.
fn open_connection(path: &Path, schema_sql: &str) -> Result<Connection, StorageError> {
    let conn = Connection::open(path).map_err(|source| StorageError::Open {
        path: path.to_path_buf(),
        source,
    })?;

    // Apply pragmas on every connection. WAL is sticky in the file
    // header; synchronous + foreign_keys are per-connection.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; \
         PRAGMA synchronous = NORMAL; \
         PRAGMA foreign_keys = ON;",
    )
    .map_err(|source| StorageError::Open {
        path: path.to_path_buf(),
        source,
    })?;

    let user_version: u32 = conn
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(|source| StorageError::Open {
            path: path.to_path_buf(),
            source,
        })? as u32;

    match user_version {
        0 => {
            // Fresh DB: apply schema, mark with our version.
            conn.execute_batch(schema_sql)
                .map_err(|source| StorageError::SchemaApply {
                    path: path.to_path_buf(),
                    source,
                })?;
            conn.pragma_update(None, "user_version", schema::SCHEMA_VERSION as i64)
                .map_err(|source| StorageError::SchemaApply {
                    path: path.to_path_buf(),
                    source,
                })?;
        }
        v if v == schema::SCHEMA_VERSION => {
            // Known version: nothing to do.
        }
        got => {
            return Err(StorageError::UnknownSchemaVersion {
                path: path.to_path_buf(),
                got,
            });
        }
    }

    Ok(conn)
}

/// Upsert SQL for the index `traces` row. `excluded.*` refers to
/// the values that would have been inserted on conflict; on the
/// INSERT path the anomaly delta seeds the column, on the UPDATE
/// path it accumulates.
const UPSERT_TRACE_SQL: &str = "
INSERT INTO traces (
  trace_key, trace_id, host, pid, start_time_ns, sapi, uri_or_script,
  state, first_batch_at_ns, last_batch_at_ns,
  batch_count, call_count, total_wall_ns, dropped_records,
  anomaly_count, cpu_snapshot_available
) VALUES (
  ?1, ?2, ?3, ?4, ?5, ?6, ?7,
  'active', ?8, ?8,
  1, ?9, ?10, ?11,
  ?12, 1
)
ON CONFLICT(trace_key) DO UPDATE SET
  batch_count       = traces.batch_count + 1,
  call_count        = traces.call_count + excluded.call_count,
  total_wall_ns     = traces.total_wall_ns + excluded.total_wall_ns,
  last_batch_at_ns  = excluded.last_batch_at_ns,
  dropped_records   = excluded.dropped_records,
  anomaly_count     = traces.anomaly_count + excluded.anomaly_count,
  -- A batch for a previously-finalized trace flips it back to active.
  -- The per-trace `trace_meta` mirror is INSERT OR REPLACE'd to
  -- 'active' on the same code path, keeping the two databases in
  -- agreement. Per `SPECIFICATION.md` DR-3.
  state             = 'active'
";

const MIRROR_TRACE_META_SQL: &str = "
INSERT OR REPLACE INTO trace_meta (
  trace_key, trace_id, host, pid, start_time_ns, sapi, uri_or_script,
  state, dropped_records, cpu_snapshot_available
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
";

const INSERT_DICT_SQL: &str = "
INSERT OR IGNORE INTO dict (fn_id, fqn, file, line, kind) VALUES (?1, ?2, ?3, ?4, ?5)
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracekey::TraceKey;
    use crate::wire::{Batch, Call, DictEntry, Meta};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "phptv-storage-{}-{}-{}",
            std::process::id(),
            label,
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("traces")).unwrap();
        dir
    }

    fn dummy_submission(key: &TraceKey, traces_dir: &Path) -> BatchSubmission {
        BatchSubmission {
            path: traces_dir
                .join(format!("{}.raw", key.as_str()))
                .join("batch-0001.msgpack"),
            trace_key: key.clone(),
        }
    }

    fn meta(host: &str, pid: u64, start_time: i64) -> Meta {
        Meta {
            schema_version: 1,
            trace_id: "00000000-0000-0000-0000-000000000000".into(),
            host: host.into(),
            pid,
            start_time,
            sapi: "cli".into(),
            uri_or_script: "/tmp/x.php".into(),
            dropped_records: 0,
        }
    }

    fn dict_entry(fn_id: u32, fqn: &str) -> DictEntry {
        DictEntry {
            fn_id,
            fqn: fqn.into(),
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
            depth: 0,
            t_in: 0,
            t_out: wall,
            cpu_u: 0,
            cpu_s: 0,
            mem_in: 0,
            mem_out: 0,
            abnormal_exit: false,
        }
    }

    // ---- open path ----

    #[test]
    fn open_creates_index_sqlite_with_user_version_1() {
        let dir = unique_dir("create_index");
        let storage =
            Storage::open(&dir, dir.join("traces")).expect("open should succeed on fresh dir");
        assert!(dir.join("index.sqlite").is_file());

        let v: i64 = storage
            .index_conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn open_preserves_existing_index_sqlite() {
        let dir = unique_dir("preserve_index");
        let _ = Storage::open(&dir, dir.join("traces")).unwrap();
        // Second open on the same file: user_version stays 1, no error.
        let storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let v: i64 = storage
            .index_conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn open_rejects_unknown_schema_version() {
        let dir = unique_dir("reject_version");
        // Pre-create index.sqlite with user_version = 99.
        {
            let conn = Connection::open(dir.join("index.sqlite")).unwrap();
            conn.pragma_update(None, "user_version", 99i64).unwrap();
        }
        let err =
            Storage::open(&dir, dir.join("traces")).expect_err("user_version=99 must be rejected");
        match err {
            StorageError::UnknownSchemaVersion { got, .. } => assert_eq!(got, 99),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn open_applies_wal_pragma() {
        let dir = unique_dir("wal_check");
        let storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let mode: String = storage
            .index_conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    // ---- record_batch ----

    fn batch_with(dict: Vec<DictEntry>, calls: Vec<Call>, m: Meta) -> Batch {
        Batch {
            meta: m,
            dict,
            calls,
        }
    }

    #[test]
    fn first_batch_inserts_trace_row_with_expected_counters() {
        let dir = unique_dir("first_batch");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();

        let key = TraceKey::from_raw("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let sub = dummy_submission(&key, &dir.join("traces"));
        let b = batch_with(
            vec![dict_entry(1, "ns\\foo"), dict_entry(2, "ns\\bar")],
            vec![call(1, 0, 1, 100), call(2, 0, 2, 200), call(3, 0, 1, 300)],
            meta("dev-1", 12, 1_700_000_000_000_000_000),
        );
        storage.record_batch(&sub, &b, 9_000_000_000).unwrap();

        let row: (i64, i64, i64, i64) = storage
            .index_conn
            .query_row(
                "SELECT batch_count, call_count, total_wall_ns, first_batch_at_ns
                 FROM traces WHERE trace_key = ?1",
                params![key.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(row.0, 1, "batch_count");
        assert_eq!(row.1, 3, "call_count");
        assert_eq!(row.2, 600, "total_wall_ns = 100+200+300");
        assert_eq!(row.3, 9_000_000_000, "first_batch_at_ns");
    }

    #[test]
    fn second_batch_increments_counters_in_index() {
        let dir = unique_dir("second_batch");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let sub = dummy_submission(&key, &dir.join("traces"));

        let b1 = batch_with(
            vec![dict_entry(1, "f1")],
            vec![call(1, 0, 1, 100)],
            meta("h", 1, 1),
        );
        storage.record_batch(&sub, &b1, 1_000).unwrap();

        let b2 = batch_with(
            vec![],
            vec![call(2, 0, 1, 200), call(3, 0, 1, 50)],
            meta("h", 1, 1),
        );
        storage.record_batch(&sub, &b2, 2_000).unwrap();

        let row: (i64, i64, i64, i64, i64) = storage
            .index_conn
            .query_row(
                "SELECT batch_count, call_count, total_wall_ns, first_batch_at_ns, last_batch_at_ns
                 FROM traces WHERE trace_key = ?1",
                params![key.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(row.0, 2);
        assert_eq!(row.1, 3);
        assert_eq!(row.2, 350);
        assert_eq!(row.3, 1_000, "first stays");
        assert_eq!(row.4, 2_000, "last updates");
    }

    #[test]
    fn dict_is_idempotent_across_batches() {
        let dir = unique_dir("dict_idempotent");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("cccccccccccccccccccccccccccccccc");
        let sub = dummy_submission(&key, &dir.join("traces"));

        let b = batch_with(vec![dict_entry(7, "ns\\seven")], vec![], meta("h", 1, 1));
        storage.record_batch(&sub, &b, 1).unwrap();
        storage.record_batch(&sub, &b, 2).unwrap();
        storage.record_batch(&sub, &b, 3).unwrap();

        let trace_path = dir.join("traces").join(format!("{}.sqlite", key.as_str()));
        let conn = Connection::open(&trace_path).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM dict WHERE fn_id = 7", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(n, 1, "dict should not duplicate fn_id 7");
    }

    #[test]
    fn only_anomalies_stays_empty_after_aggregation_core() {
        // Renamed from `aggregation_tables_stay_empty_after_record_batch`
        // when `aggregation-core` made the decoder write to
        // `nodes` / `call_to_node` / `pending_calls`. Only the
        // `anomalies` table remains empty until the anomaly slice.
        let dir = unique_dir("agg_post_core");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("dddddddddddddddddddddddddddddddd");
        let sub = dummy_submission(&key, &dir.join("traces"));

        let b = batch_with(
            vec![dict_entry(1, "f")],
            vec![call(1, 0, 1, 10)],
            meta("h", 1, 1),
        );
        storage.record_batch(&sub, &b, 1).unwrap();

        let trace_path = dir.join("traces").join(format!("{}.sqlite", key.as_str()));
        let conn = Connection::open(&trace_path).unwrap();

        // `anomalies` is the only table still empty in this slice.
        let n_anom: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            n_anom, 0,
            "anomalies should be empty until the anomaly slice"
        );

        // `nodes` populates: synthetic root + the one user call's node.
        let n_nodes: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n_nodes, 2, "synthetic root + one user node");

        // `call_to_node` populates: one row for the one user call.
        let n_c2n: i64 = conn
            .query_row("SELECT COUNT(*) FROM call_to_node", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n_c2n, 1);

        // `pending_calls` empty (the call had parent=0 → resolved
        // immediately against the synthetic root).
        let n_pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n_pending, 0);
    }

    #[test]
    fn trace_meta_mirrors_the_index_row() {
        let dir = unique_dir("mirror_trace_meta");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
        let sub = dummy_submission(&key, &dir.join("traces"));

        let m = meta("dev-2", 99, 1_500_000_000_000_000_000);
        storage
            .record_batch(&sub, &batch_with(vec![], vec![], m), 1_234_567)
            .unwrap();

        let trace_path = dir.join("traces").join(format!("{}.sqlite", key.as_str()));
        let conn = Connection::open(&trace_path).unwrap();
        let row: (
            String,
            String,
            String,
            i64,
            i64,
            String,
            String,
            String,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT trace_key, trace_id, host, pid, start_time_ns, sapi,
                        uri_or_script, state, dropped_records, cpu_snapshot_available
                 FROM trace_meta",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, key.as_str());
        assert_eq!(row.2, "dev-2");
        assert_eq!(row.3, 99);
        assert_eq!(row.4, 1_500_000_000_000_000_000);
        assert_eq!(row.7, "active");
        assert_eq!(row.9, 1);
    }

    #[test]
    fn per_trace_sqlite_has_user_version_1() {
        let dir = unique_dir("per_trace_uv");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("ffffffffffffffffffffffffffffffff");
        let sub = dummy_submission(&key, &dir.join("traces"));
        storage
            .record_batch(&sub, &batch_with(vec![], vec![], meta("h", 1, 1)), 1)
            .unwrap();

        let trace_path = dir.join("traces").join(format!("{}.sqlite", key.as_str()));
        let conn = Connection::open(&trace_path).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn anomaly_count_on_traces_row_mirrors_per_trace_anomalies() {
        // Build a batch with 3 DQ-1 Calls (missing fn_id=99) and
        // 2 DQ-3 Calls (t_out < t_in on a known fn_id=7), plus
        // one normal Call. Expect 5 anomaly rows in the per-trace
        // db and `traces.anomaly_count = 5` in the index.
        let dir = unique_dir("anom_count");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("44444444444444444444444444444444");
        let sub = dummy_submission(&key, &dir.join("traces"));

        let inverted = |id: u32| Call {
            call_id: id,
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
        let b = batch_with(
            vec![dict_entry(7, "ok")],
            vec![
                call(10, 0, 99, 100), // DQ-1
                call(11, 0, 99, 100), // DQ-1
                call(12, 0, 99, 100), // DQ-1
                inverted(20),         // DQ-3
                inverted(21),         // DQ-3
                call(30, 0, 7, 100),  // normal
            ],
            meta("anom-host", 1, 1),
        );
        storage.record_batch(&sub, &b, 1_000).unwrap();

        let n_index: i64 = storage
            .index_conn
            .query_row("SELECT anomaly_count FROM traces", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_index, 5);

        let trace_path = dir.join("traces").join(format!("{}.sqlite", key.as_str()));
        let conn = Connection::open(&trace_path).unwrap();
        let n_per_trace: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_per_trace, 5);

        // Second batch: one more DQ-3, no DQ-1. Counter accumulates.
        let b2 = batch_with(vec![], vec![inverted(22)], meta("anom-host", 1, 1));
        storage.record_batch(&sub, &b2, 2_000).unwrap();
        let n_index2: i64 = storage
            .index_conn
            .query_row("SELECT anomaly_count FROM traces", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_index2, 6);
        let n_per_trace2: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_per_trace2, 6);
    }

    // ---- finalize_trace ----

    /// Build a Call with explicit CPU values. The default `call()`
    /// helper above zeroes them out, which is the wrong shape for the
    /// CPU-available finalize test.
    fn call_with_cpu(
        call_id: u32,
        parent: u32,
        fn_id: u32,
        wall: i64,
        cpu_u: i64,
        cpu_s: i64,
    ) -> Call {
        Call {
            call_id,
            parent,
            fn_id,
            depth: 0,
            t_in: 0,
            t_out: wall,
            cpu_u,
            cpu_s,
            mem_in: 0,
            mem_out: 0,
            abnormal_exit: false,
        }
    }

    #[test]
    fn finalize_trace_drains_pending_into_dq2_anomalies() {
        let dir = unique_dir("finalize_dq2");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("11111111111111111111111111111111");
        let sub = dummy_submission(&key, &dir.join("traces"));

        // A child whose parent (call_id=999) never arrives → goes
        // to pending_calls and stays there.
        let b = batch_with(
            vec![dict_entry(7, "ns\\seven")],
            vec![call(42, 999, 7, 100)],
            meta("orphan-host", 1, 1),
        );
        storage.record_batch(&sub, &b, 1_000).unwrap();

        let outcome = storage.finalize_trace(&key, 2_000).unwrap();
        assert_eq!(outcome.pending_dq2, 1);

        let trace_path = dir.join("traces").join(format!("{}.sqlite", key.as_str()));
        let conn = Connection::open(&trace_path).unwrap();

        let n_pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_pending, 0, "pending_calls drained at finalize");

        let (node_id, kind, sample_call_id, detail): (Option<i64>, String, i64, String) = conn
            .query_row(
                "SELECT node_id, kind, sample_call_id, detail FROM anomalies",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(node_id, None);
        assert_eq!(kind, "pending_parent_at_finalize");
        assert_eq!(sample_call_id, 42);
        assert_eq!(detail, "parent_call_id=999");
    }

    #[test]
    fn finalize_trace_cpu_unavailable_when_all_cpu_zero() {
        let dir = unique_dir("finalize_cpu_off");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("22222222222222222222222222222222");
        let sub = dummy_submission(&key, &dir.join("traces"));

        let b = batch_with(
            vec![dict_entry(7, "ns\\seven")],
            vec![call(1, 0, 7, 100)], // cpu_u=0, cpu_s=0
            meta("cpu-off-host", 1, 1),
        );
        storage.record_batch(&sub, &b, 1_000).unwrap();

        let outcome = storage.finalize_trace(&key, 2_000).unwrap();
        assert!(!outcome.cpu_snapshot_available);

        // Mirrored in both databases.
        let cpu_index: i64 = storage
            .index_conn
            .query_row(
                "SELECT cpu_snapshot_available FROM traces WHERE trace_key = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cpu_index, 0);

        let trace_path = dir.join("traces").join(format!("{}.sqlite", key.as_str()));
        let conn = Connection::open(&trace_path).unwrap();
        let cpu_meta: i64 = conn
            .query_row("SELECT cpu_snapshot_available FROM trace_meta", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cpu_meta, 0);
    }

    #[test]
    fn finalize_trace_cpu_available_when_any_call_has_cpu() {
        let dir = unique_dir("finalize_cpu_on");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("33333333333333333333333333333333");
        let sub = dummy_submission(&key, &dir.join("traces"));

        let b = batch_with(
            vec![dict_entry(7, "ns\\seven")],
            vec![call_with_cpu(1, 0, 7, 100, 50, 10)],
            meta("cpu-on-host", 1, 1),
        );
        storage.record_batch(&sub, &b, 1_000).unwrap();

        let outcome = storage.finalize_trace(&key, 2_000).unwrap();
        assert!(outcome.cpu_snapshot_available);

        let cpu_index: i64 = storage
            .index_conn
            .query_row(
                "SELECT cpu_snapshot_available FROM traces WHERE trace_key = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cpu_index, 1);

        let trace_path = dir.join("traces").join(format!("{}.sqlite", key.as_str()));
        let conn = Connection::open(&trace_path).unwrap();
        let cpu_meta: i64 = conn
            .query_row("SELECT cpu_snapshot_available FROM trace_meta", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cpu_meta, 1);
    }

    #[test]
    fn finalize_trace_flips_state_in_both_databases() {
        let dir = unique_dir("finalize_state");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("55555555555555555555555555555555");
        let sub = dummy_submission(&key, &dir.join("traces"));

        let b = batch_with(
            vec![dict_entry(7, "ns\\seven")],
            vec![call(1, 0, 7, 100)],
            meta("state-host", 1, 1),
        );
        storage.record_batch(&sub, &b, 1_000).unwrap();

        // Sanity: state starts as 'active'.
        let state_before: String = storage
            .index_conn
            .query_row(
                "SELECT state FROM traces WHERE trace_key = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state_before, "active");

        storage.finalize_trace(&key, 2_000).unwrap();

        let state_after: String = storage
            .index_conn
            .query_row(
                "SELECT state FROM traces WHERE trace_key = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state_after, "finalized");

        let trace_path = dir.join("traces").join(format!("{}.sqlite", key.as_str()));
        let conn = Connection::open(&trace_path).unwrap();
        let state_meta: String = conn
            .query_row("SELECT state FROM trace_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state_meta, "finalized");
    }

    #[test]
    fn finalize_trace_evicts_per_trace_connection() {
        let dir = unique_dir("finalize_evict");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("66666666666666666666666666666666");
        let sub = dummy_submission(&key, &dir.join("traces"));

        let b = batch_with(
            vec![dict_entry(7, "ns\\seven")],
            vec![call(1, 0, 7, 100)],
            meta("evict-host", 1, 1),
        );
        storage.record_batch(&sub, &b, 1_000).unwrap();
        assert!(
            storage.has_cached_trace_conn(&key),
            "record_batch caches the per-trace conn"
        );

        storage.finalize_trace(&key, 2_000).unwrap();
        assert!(
            !storage.has_cached_trace_conn(&key),
            "finalize_trace evicts the per-trace conn"
        );
    }

    #[test]
    fn finalize_trace_anomaly_count_reflects_dq2() {
        let dir = unique_dir("finalize_anom_count");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("77777777777777777777777777777777");
        let sub = dummy_submission(&key, &dir.join("traces"));

        // Build a batch with mixed anomalies + pending:
        //   - 1 DQ-1 (missing fn_id=99)
        //   - 1 DQ-3 (inverted time on fn_id=7)
        //   - 2 orphan children (cross-batch parent never seen)
        //   - 1 normal Call
        let inverted = Call {
            call_id: 100,
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
        let b = batch_with(
            vec![dict_entry(7, "ok")],
            vec![
                call(10, 0, 99, 100),  // DQ-1
                inverted,              // DQ-3 at fn=7
                call(20, 999, 7, 100), // orphan → pending
                call(21, 999, 7, 100), // orphan → pending
                call(30, 0, 7, 100),   // normal at fn=7
            ],
            meta("mix-host", 1, 1),
        );
        storage.record_batch(&sub, &b, 1_000).unwrap();

        // After record_batch: 2 anomalies (DQ-1 + DQ-3), 2 pending.
        let n_index_before: i64 = storage
            .index_conn
            .query_row(
                "SELECT anomaly_count FROM traces WHERE trace_key = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_index_before, 2);

        // Finalize adds 2 DQ-2 rows → 4 total in per-trace.
        let outcome = storage.finalize_trace(&key, 2_000).unwrap();
        assert_eq!(outcome.pending_dq2, 2);

        let n_index_after: i64 = storage
            .index_conn
            .query_row(
                "SELECT anomaly_count FROM traces WHERE trace_key = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_index_after, 4);

        let trace_path = dir.join("traces").join(format!("{}.sqlite", key.as_str()));
        let conn = Connection::open(&trace_path).unwrap();
        let n_per_trace: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_per_trace, 4);
    }

    #[test]
    fn late_batch_after_finalize_reactivates_state() {
        let dir = unique_dir("late_batch");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("88888888888888888888888888888888");
        let sub = dummy_submission(&key, &dir.join("traces"));

        let b1 = batch_with(
            vec![dict_entry(7, "f")],
            vec![call(1, 0, 7, 100)],
            meta("late-host", 1, 1),
        );
        storage.record_batch(&sub, &b1, 1_000).unwrap();
        storage.finalize_trace(&key, 2_000).unwrap();

        // Confirm finalize landed.
        let s: String = storage
            .index_conn
            .query_row(
                "SELECT state FROM traces WHERE trace_key = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(s, "finalized");

        // Late batch for the same trace.
        let b2 = batch_with(vec![], vec![call(2, 0, 7, 50)], meta("late-host", 1, 1));
        storage.record_batch(&sub, &b2, 5_000).unwrap();

        let (state, batch_count, total_wall): (String, i64, i64) = storage
            .index_conn
            .query_row(
                "SELECT state, batch_count, total_wall_ns FROM traces WHERE trace_key = ?1",
                params![key.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "active", "late batch reactivates traces.state");
        assert_eq!(batch_count, 2, "batch_count accumulates across the gap");
        assert_eq!(total_wall, 150);

        let trace_path = dir.join("traces").join(format!("{}.sqlite", key.as_str()));
        let conn = Connection::open(&trace_path).unwrap();
        let state_meta: String = conn
            .query_row("SELECT state FROM trace_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            state_meta, "active",
            "trace_meta mirror flips back to active too"
        );
    }

    #[test]
    fn finalize_trace_is_idempotent_under_retry() {
        // Simulate the partial-commit window: the first finalize's
        // per-trace transaction committed (pending drained, DQ-2
        // anomalies inserted, trace_meta.state='finalized'), but the
        // index update is lost. A second call must NOT duplicate
        // anomaly rows, and must bring the index DB into agreement.
        let dir = unique_dir("finalize_idempotent");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();
        let key = TraceKey::from_raw("99999999999999999999999999999999");
        let sub = dummy_submission(&key, &dir.join("traces"));

        let b = batch_with(
            vec![dict_entry(7, "ok")],
            vec![
                call(20, 999, 7, 100), // orphan → pending
                call(21, 999, 7, 100), // orphan → pending
            ],
            meta("retry-host", 1, 1),
        );
        storage.record_batch(&sub, &b, 1_000).unwrap();

        // First finalize: lands DQ-2 + commits both databases.
        storage.finalize_trace(&key, 2_000).unwrap();

        // Roll the index back to make it look like the index update
        // never committed (simulating the crash window).
        storage
            .index_conn
            .execute(
                "UPDATE traces SET state = 'active', anomaly_count = 0, \
                 cpu_snapshot_available = 1 WHERE trace_key = ?1",
                params![key.as_str()],
            )
            .unwrap();

        // The per-trace DB still shows the first attempt's work.
        let trace_path = dir.join("traces").join(format!("{}.sqlite", key.as_str()));
        let ro = Connection::open(&trace_path).unwrap();
        let n_anom_before_retry: i64 = ro
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_anom_before_retry, 2, "first attempt landed 2 DQ-2 rows");

        // Retry. pending_calls is already empty, so no new DQ-2 rows
        // should be written. The retry should reconcile the index.
        let outcome = storage.finalize_trace(&key, 3_000).unwrap();
        assert_eq!(outcome.pending_dq2, 0, "no new DQ-2 inserts on retry");

        let n_anom_after_retry: i64 = ro
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_anom_after_retry, 2, "retry must not duplicate DQ-2 rows");

        let (state, anomaly_count): (String, i64) = storage
            .index_conn
            .query_row(
                "SELECT state, anomaly_count FROM traces WHERE trace_key = ?1",
                params![key.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "finalized", "retry flips state to finalized");
        assert_eq!(
            anomaly_count, 2,
            "retry reconciles anomaly_count to the per-trace COUNT(*)"
        );
    }

    #[test]
    fn list_idle_active_traces_returns_expected_keys() {
        let dir = unique_dir("list_idle");
        let mut storage = Storage::open(&dir, dir.join("traces")).unwrap();

        // Three traces with last_batch_at_ns = 100, 200, 300.
        let mut make = |hex: &str, host: &str, ts: i64| {
            let key = TraceKey::from_raw(hex);
            let sub = dummy_submission(&key, &dir.join("traces"));
            let b = batch_with(
                vec![dict_entry(7, "f")],
                vec![call(1, 0, 7, 10)],
                meta(host, 1, 1),
            );
            storage.record_batch(&sub, &b, ts).unwrap();
            key
        };
        let k1 = make("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa01", "h1", 100);
        let k2 = make("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa02", "h2", 200);
        let _k3 = make("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa03", "h3", 300);

        // Cutoff at 250 → k1 and k2 idle, k3 not.
        let idle = storage.list_idle_active_traces(250).unwrap();
        let idle_strs: Vec<&str> = idle.iter().map(|k| k.as_str()).collect();
        assert_eq!(idle_strs.len(), 2);
        assert!(idle_strs.contains(&k1.as_str()));
        assert!(idle_strs.contains(&k2.as_str()));

        // After finalising k1, the next call returns only k2.
        storage.finalize_trace(&k1, 250).unwrap();
        let idle = storage.list_idle_active_traces(250).unwrap();
        let idle_strs: Vec<&str> = idle.iter().map(|k| k.as_str()).collect();
        assert_eq!(idle_strs, vec![k2.as_str()]);
    }
}
