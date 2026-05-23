//! Embedded SQL DDL for the two SQLite shapes. Transcribed
//! verbatim from `SPECIFICATION.md` §4.2 (index.sqlite) and §4.3
//! (per-trace `<key>.sqlite`).
//!
//! Each `CREATE TABLE` / `CREATE INDEX` uses `IF NOT EXISTS` so
//! re-applying the schema on an already-initialised file is a
//! no-op. The `user_version` gate in `super::open_connection`
//! means this should only run on a fresh DB; the idempotent form
//! is defensive.

/// Index DB schema (`SPECIFICATION.md` §4.2). Pragmas are applied
/// separately by `open_connection` so this string contains only
/// the table + indexes.
pub(super) const INDEX_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS traces (
  trace_key            TEXT    PRIMARY KEY,
  trace_id             TEXT    NOT NULL,
  host                 TEXT    NOT NULL,
  pid                  INTEGER NOT NULL,
  start_time_ns        INTEGER NOT NULL,
  sapi                 TEXT    NOT NULL CHECK (sapi IN ('cli', 'fpm-fcgi')),
  uri_or_script        TEXT    NOT NULL,
  state                TEXT    NOT NULL CHECK (state IN ('active', 'finalized'))
                       DEFAULT 'active',
  first_batch_at_ns    INTEGER NOT NULL,
  last_batch_at_ns     INTEGER NOT NULL,
  batch_count          INTEGER NOT NULL DEFAULT 0,
  call_count           INTEGER NOT NULL DEFAULT 0,
  total_wall_ns        INTEGER NOT NULL DEFAULT 0,
  dropped_records      INTEGER NOT NULL DEFAULT 0,
  anomaly_count        INTEGER NOT NULL DEFAULT 0,
  cpu_snapshot_available INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_traces_start_time      ON traces (start_time_ns DESC);
CREATE INDEX IF NOT EXISTS idx_traces_uri             ON traces (uri_or_script);
CREATE INDEX IF NOT EXISTS idx_traces_state_lastbatch ON traces (state, last_batch_at_ns);
";

/// Per-trace DB schema (`SPECIFICATION.md` §4.3). All six tables
/// are created even though this slice only writes to `trace_meta`
/// and `dict`; the aggregation slice will start writing to
/// `nodes`, `call_to_node`, `pending_calls`, and `anomalies`
/// without a migration step.
pub(super) const TRACE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS trace_meta (
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

CREATE TABLE IF NOT EXISTS dict (
  fn_id   INTEGER PRIMARY KEY,
  fqn     TEXT    NOT NULL,
  file    TEXT    NOT NULL,
  line    INTEGER NOT NULL,
  kind    INTEGER NOT NULL CHECK (kind BETWEEN 0 AND 3)
);

CREATE TABLE IF NOT EXISTS nodes (
  node_id              INTEGER PRIMARY KEY AUTOINCREMENT,
  parent_node_id       INTEGER REFERENCES nodes(node_id),
  fn_id                INTEGER NOT NULL REFERENCES dict(fn_id),
  depth                INTEGER NOT NULL,

  call_count           INTEGER NOT NULL DEFAULT 0,
  total_wall_ns        INTEGER NOT NULL DEFAULT 0,
  children_total_wall_ns INTEGER NOT NULL DEFAULT 0,

  total_cpu_u_ns       INTEGER NOT NULL DEFAULT 0,
  total_cpu_s_ns       INTEGER NOT NULL DEFAULT 0,
  total_mem_delta_bytes INTEGER NOT NULL DEFAULT 0,
  abnormal_exit_count  INTEGER NOT NULL DEFAULT 0,

  UNIQUE (parent_node_id, fn_id)
);
CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes (parent_node_id);
CREATE INDEX IF NOT EXISTS idx_nodes_fn     ON nodes (fn_id);

CREATE TABLE IF NOT EXISTS call_to_node (
  call_id INTEGER PRIMARY KEY,
  node_id INTEGER NOT NULL REFERENCES nodes(node_id)
);

CREATE TABLE IF NOT EXISTS pending_calls (
  call_id              INTEGER PRIMARY KEY,
  parent_call_id       INTEGER NOT NULL,
  fn_id                INTEGER NOT NULL,
  t_in_ns              INTEGER NOT NULL,
  t_out_ns             INTEGER NOT NULL,
  cpu_u_ns             INTEGER NOT NULL,
  cpu_s_ns             INTEGER NOT NULL,
  mem_in_bytes         INTEGER NOT NULL,
  mem_out_bytes        INTEGER NOT NULL,
  abnormal_exit        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pending_parent ON pending_calls (parent_call_id);

CREATE TABLE IF NOT EXISTS anomalies (
  rowid          INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id        INTEGER REFERENCES nodes(node_id),
  kind           TEXT    NOT NULL,
  count          INTEGER NOT NULL DEFAULT 1,
  sample_call_id INTEGER,
  detail         TEXT
);
CREATE INDEX IF NOT EXISTS idx_anomalies_node ON anomalies (node_id);
";

/// The current schema version recorded in `PRAGMA user_version`.
/// Bump this and add a migration when the schemas change.
pub(super) const SCHEMA_VERSION: u32 = 1;
