<?php

declare(strict_types=1);

/**
 * Trace-detail handlers and SQL builders.
 *
 *   phptv_handle_trace_meta(string $key): never
 *   phptv_handle_trace_tree(string $key, array $query): never
 *   phptv_handle_trace_children(string $key, int $nodeId, array $query): never
 *   phptv_node_row_to_item(array $row, ...): array
 *
 * All SQL is full-literal const strings with a single token-style
 * <<SORT>> placeholder substituted from the whitelist. User input
 * never reaches the SQL string; values bind through PDO placeholders.
 *
 * INV-8: read-only PDO via open_index_db_ro / open_trace_db_ro.
 * DR-5: PRAGMA user_version=1 enforced inside the openers.
 */

require_once __DIR__ . '/config.php';
require_once __DIR__ . '/response.php';
require_once __DIR__ . '/storage.php';
require_once __DIR__ . '/session.php';

// --- Sort whitelist --------------------------------------------------
//
// The five §5.6 sort param values mapped to literal ORDER BY clauses.
// Each clause ends with a stable secondary tie-break on n.node_id ASC
// so paging via LIMIT/OFFSET is deterministic.

const PHPTV_SORT_CLAUSES = [
    'total_wall_desc' => 'n.total_wall_ns DESC, n.node_id ASC',
    'self_wall_desc'  => '(n.total_wall_ns - n.children_total_wall_ns) DESC, n.node_id ASC',
    'count_desc'      => 'n.call_count DESC, n.node_id ASC',
    'mem_delta_desc'  => 'n.total_mem_delta_bytes DESC, n.node_id ASC',
    'fqn_asc'         => 'd.fqn ASC, n.node_id ASC',
];

const PHPTV_DEFAULT_SORT = 'total_wall_desc';
const PHPTV_MAX_DEPTH = 4;
const PHPTV_DEFAULT_DEPTH = 2;
const PHPTV_CHILDREN_MAX_LIMIT = 1000;

// --- SQL templates ---------------------------------------------------
//
// <<SORT>> is the only substitution point. The replacement is selected
// from PHPTV_SORT_CLAUSES — never from raw user input — so the final
// statement contains no untrusted bytes.

const SQL_TREE_FETCH = <<<'SQL'
WITH RECURSIVE
  tree(node_id, depth_in_tree) AS (
    SELECT node_id, 0 FROM nodes WHERE parent_node_id IS NULL
    UNION ALL
    SELECT n.node_id, t.depth_in_tree + 1
    FROM nodes n
    JOIN tree t ON n.parent_node_id = t.node_id
    WHERE t.depth_in_tree < :max_depth
  )
SELECT n.node_id, n.parent_node_id, n.depth, t.depth_in_tree,
       n.call_count, n.total_wall_ns, n.children_total_wall_ns,
       n.total_cpu_u_ns, n.total_cpu_s_ns, n.total_mem_delta_bytes,
       n.abnormal_exit_count,
       d.fqn, d.file, d.line, d.kind,
       COALESCE(a.cnt, 0) AS anomaly_count,
       EXISTS(SELECT 1 FROM nodes c WHERE c.parent_node_id = n.node_id) AS has_children_int
FROM nodes n
JOIN tree t USING (node_id)
JOIN dict d ON d.fn_id = n.fn_id
LEFT JOIN (SELECT node_id, COUNT(*) AS cnt FROM anomalies GROUP BY node_id) a
       ON a.node_id = n.node_id
ORDER BY t.depth_in_tree ASC, <<SORT>>
SQL;

const SQL_CHILDREN_FETCH_NO_LIMIT = <<<'SQL'
SELECT n.node_id, n.parent_node_id, n.depth,
       n.call_count, n.total_wall_ns, n.children_total_wall_ns,
       n.total_cpu_u_ns, n.total_cpu_s_ns, n.total_mem_delta_bytes,
       n.abnormal_exit_count,
       d.fqn, d.file, d.line, d.kind,
       COALESCE(a.cnt, 0) AS anomaly_count,
       EXISTS(SELECT 1 FROM nodes c WHERE c.parent_node_id = n.node_id) AS has_children_int
FROM nodes n
JOIN dict d ON d.fn_id = n.fn_id
LEFT JOIN (SELECT node_id, COUNT(*) AS cnt FROM anomalies GROUP BY node_id) a
       ON a.node_id = n.node_id
WHERE n.parent_node_id = :node_id
ORDER BY <<SORT>>
SQL;

const SQL_CHILDREN_FETCH_LIMIT = <<<'SQL'
SELECT n.node_id, n.parent_node_id, n.depth,
       n.call_count, n.total_wall_ns, n.children_total_wall_ns,
       n.total_cpu_u_ns, n.total_cpu_s_ns, n.total_mem_delta_bytes,
       n.abnormal_exit_count,
       d.fqn, d.file, d.line, d.kind,
       COALESCE(a.cnt, 0) AS anomaly_count,
       EXISTS(SELECT 1 FROM nodes c WHERE c.parent_node_id = n.node_id) AS has_children_int
FROM nodes n
JOIN dict d ON d.fn_id = n.fn_id
LEFT JOIN (SELECT node_id, COUNT(*) AS cnt FROM anomalies GROUP BY node_id) a
       ON a.node_id = n.node_id
WHERE n.parent_node_id = :node_id
ORDER BY <<SORT>>
LIMIT :limit OFFSET :offset
SQL;

const SQL_TRACE_META = 'SELECT trace_key, trace_id, host, pid, start_time_ns, sapi, uri_or_script, state, dropped_records, cpu_snapshot_available FROM trace_meta LIMIT 1';
const SQL_INDEX_ANOMALY_COUNT = 'SELECT anomaly_count FROM traces WHERE trace_key = :trace_key';

/**
 * Substitute the sort clause from the whitelist into the SQL template.
 * The keys of PHPTV_SORT_CLAUSES are validated before this is called;
 * the values are full literal ORDER BY fragments with no user input.
 */
function phptv_build_sql(string $template, string $sortKey): string
{
    if (!isset(PHPTV_SORT_CLAUSES[$sortKey])) {
        // Defensive: callers MUST validate first. If we get here, the
        // validator regressed; refuse rather than build a broken SQL.
        throw new \LogicException("unwhitelisted sort key: {$sortKey}");
    }
    return str_replace('<<SORT>>', PHPTV_SORT_CLAUSES[$sortKey], $template);
}

// --- Handlers --------------------------------------------------------

/**
 * Handle GET /api/traces/{key}. Joins the per-trace trace_meta row
 * with the index's anomaly_count (denormalized at finalize time by
 * the collector — COMMENTS.md field-semantics) so the response shape
 * matches §5.5.
 */
function phptv_handle_trace_meta(string $key): never
{
    dispatch_method('GET');
    require_session();

    $traceDb = phptv_open_trace_db_or_404($key);

    $stmt = $traceDb->prepare(SQL_TRACE_META);
    $stmt->execute();
    $row = $stmt->fetch();
    if (!is_array($row)) {
        // File exists but trace_meta is empty — should not happen
        // (the collector seeds it on first batch). Treat as 404.
        json_error(404, 'not_found');
    }

    $anomalyCount = 0;
    try {
        $indexDb = open_index_db_ro();
        $idxStmt = $indexDb->prepare(SQL_INDEX_ANOMALY_COUNT);
        $idxStmt->bindValue(':trace_key', $key, \PDO::PARAM_STR);
        $idxStmt->execute();
        $idxRow = $idxStmt->fetch();
        if (is_array($idxRow) && isset($idxRow['anomaly_count'])) {
            $anomalyCount = (int) $idxRow['anomaly_count'];
        }
    } catch (\RuntimeException) {
        // Index DB missing or unreadable: surface as 0 anomalies.
        // The per-trace data is the source of truth for the rest of
        // the response; the missing index is an operator concern,
        // not a user-facing 500.
    }

    json_success(200, [
        'trace_key'              => (string) $row['trace_key'],
        'trace_id'               => (string) $row['trace_id'],
        'host'                   => (string) $row['host'],
        'pid'                    => (int) $row['pid'],
        'start_time'             => format_rfc3339_ns((int) $row['start_time_ns']),
        'sapi'                   => (string) $row['sapi'],
        'uri_or_script'          => (string) $row['uri_or_script'],
        'state'                  => (string) $row['state'],
        'dropped_records'        => (int) $row['dropped_records'],
        'anomaly_count'          => $anomalyCount,
        'cpu_snapshot_available' => ((int) $row['cpu_snapshot_available']) === 1,
        'root_node_id'           => 1,
    ]);
}

/**
 * Handle GET /api/traces/{key}/tree?depth=N&sort=…
 *
 * @param array<string, mixed> $query
 */
function phptv_handle_trace_tree(string $key, array $query): never
{
    dispatch_method('GET');
    require_session();

    [$depth, $sort] = phptv_parse_tree_query($query);

    $traceDb = phptv_open_trace_db_or_404($key);

    $sql = phptv_build_sql(SQL_TREE_FETCH, $sort);
    $stmt = $traceDb->prepare($sql);
    $stmt->bindValue(':max_depth', $depth, \PDO::PARAM_INT);
    $stmt->execute();
    $rows = $stmt->fetchAll();

    $items = [];
    foreach ($rows as $row) {
        $depthInTree = (int) $row['depth_in_tree'];
        $hasChildren = ((int) $row['has_children_int']) === 1;
        // D-5: children_loaded is true when the node's children are
        // present in the response OR the node has no children. The
        // latter avoids forcing the UI to lazy-fetch a leaf's zero
        // children just because it sits at the depth boundary.
        $childrenLoaded = ($depthInTree < $depth) || !$hasChildren;
        $items[] = phptv_node_row_to_item($row, $childrenLoaded);
    }

    json_success(200, [
        'root_node_id' => 1,
        'nodes'        => $items,
    ]);
}

/**
 * Handle GET /api/traces/{key}/tree/{node_id}/children?sort=…&limit=N&offset=K
 *
 * @param array<string, mixed> $query
 */
function phptv_handle_trace_children(string $key, int $nodeId, array $query): never
{
    dispatch_method('GET');
    require_session();

    [$sort, $limit, $offset] = phptv_parse_children_query($query);

    $traceDb = phptv_open_trace_db_or_404($key);

    if ($limit === null) {
        $sql = phptv_build_sql(SQL_CHILDREN_FETCH_NO_LIMIT, $sort);
        $stmt = $traceDb->prepare($sql);
        $stmt->bindValue(':node_id', $nodeId, \PDO::PARAM_INT);
    } else {
        $sql = phptv_build_sql(SQL_CHILDREN_FETCH_LIMIT, $sort);
        $stmt = $traceDb->prepare($sql);
        $stmt->bindValue(':node_id', $nodeId, \PDO::PARAM_INT);
        $stmt->bindValue(':limit', $limit, \PDO::PARAM_INT);
        $stmt->bindValue(':offset', $offset, \PDO::PARAM_INT);
    }
    $stmt->execute();
    $rows = $stmt->fetchAll();

    $items = [];
    foreach ($rows as $row) {
        // children_loaded is always false for nodes returned by the
        // children endpoint — the UI lazily expands them (§5.7).
        $items[] = phptv_node_row_to_item($row, false);
    }

    json_success(200, ['nodes' => $items]);
}

// --- Query parsers ---------------------------------------------------

/**
 * @param array<string, mixed> $query
 * @return array{0:int, 1:string} [depth, sortKey]
 */
function phptv_parse_tree_query(array $query): array
{
    $depthRaw = $query['depth'] ?? (string) PHPTV_DEFAULT_DEPTH;
    if (!is_string($depthRaw) && !is_int($depthRaw)) {
        json_error(400, 'bad_request');
    }
    $depthStr = (string) $depthRaw;
    if (!preg_match('/^[1-9][0-9]*$/', $depthStr)) {
        json_error(400, 'bad_request');
    }
    $depth = (int) $depthStr;
    if ($depth < 1 || $depth > PHPTV_MAX_DEPTH) {
        json_error(400, 'bad_request');
    }

    $sort = $query['sort'] ?? PHPTV_DEFAULT_SORT;
    if (!is_string($sort) || !isset(PHPTV_SORT_CLAUSES[$sort])) {
        json_error(400, 'bad_request');
    }

    return [$depth, $sort];
}

/**
 * @param array<string, mixed> $query
 * @return array{0:string, 1:int|null, 2:int} [sortKey, limit-or-null, offset]
 */
function phptv_parse_children_query(array $query): array
{
    $sort = $query['sort'] ?? PHPTV_DEFAULT_SORT;
    if (!is_string($sort) || !isset(PHPTV_SORT_CLAUSES[$sort])) {
        json_error(400, 'bad_request');
    }

    $limit = null;
    if (isset($query['limit'])) {
        $limitRaw = $query['limit'];
        if (!is_string($limitRaw) && !is_int($limitRaw)) {
            json_error(400, 'bad_request');
        }
        $limitStr = (string) $limitRaw;
        if (!preg_match('/^[1-9][0-9]*$/', $limitStr)) {
            json_error(400, 'bad_request');
        }
        $limit = (int) $limitStr;
        if ($limit < 1 || $limit > PHPTV_CHILDREN_MAX_LIMIT) {
            json_error(400, 'bad_request');
        }
    }

    $offset = 0;
    if (isset($query['offset'])) {
        $offsetRaw = $query['offset'];
        if (!is_string($offsetRaw) && !is_int($offsetRaw)) {
            json_error(400, 'bad_request');
        }
        $offsetStr = (string) $offsetRaw;
        if (!preg_match('/^(0|[1-9][0-9]*)$/', $offsetStr)) {
            json_error(400, 'bad_request');
        }
        $offset = (int) $offsetStr;
    }

    return [$sort, $limit, $offset];
}

// --- Row -> item -----------------------------------------------------

/**
 * Map one row from the tree or children query to the §5.6 JSON shape.
 *
 * @param array<string, mixed> $row
 * @return array<string, mixed>
 */
function phptv_node_row_to_item(array $row, bool $childrenLoaded): array
{
    $total = (int) $row['total_wall_ns'];
    $childrenTotal = (int) $row['children_total_wall_ns'];
    // D-6: clamp at zero. DI-3 guarantees total >= children_total, but
    // defensive clamping costs one max() call and yields a UI-safe
    // value (no negative %parent heatmap math) if a future regression
    // ever publishes a violation.
    $selfWall = max(0, $total - $childrenTotal);

    return [
        'node_id'                => (int) $row['node_id'],
        'parent_node_id'         => $row['parent_node_id'] === null
                                  ? null
                                  : (int) $row['parent_node_id'],
        'depth'                  => (int) $row['depth'],
        'fqn'                    => (string) $row['fqn'],
        'file'                   => (string) $row['file'],
        'line'                   => (int) $row['line'],
        'kind'                   => (int) $row['kind'],
        'count'                  => (int) $row['call_count'],
        'total_wall_ns'          => $total,
        'self_wall_ns'           => $selfWall,
        'total_cpu_u_ns'         => (int) $row['total_cpu_u_ns'],
        'total_cpu_s_ns'         => (int) $row['total_cpu_s_ns'],
        'total_mem_delta_bytes'  => (int) $row['total_mem_delta_bytes'],
        'abnormal_exit_count'    => (int) $row['abnormal_exit_count'],
        'anomaly_count'          => (int) $row['anomaly_count'],
        'has_children'           => ((int) $row['has_children_int']) === 1,
        'children_loaded'        => $childrenLoaded,
    ];
}

/**
 * Open <key>.sqlite read-only. Returns 404 not_found if the file is
 * missing (D-7: missing-file is indistinguishable from missing-row
 * from the user's perspective). Rethrows other open failures so the
 * top-level handler maps them appropriately
 * (e.g. SchemaVersionMismatch -> 500 schema_version_mismatch).
 */
function phptv_open_trace_db_or_404(string $key): \PDO
{
    try {
        return open_trace_db_ro($key);
    } catch (SchemaVersionMismatch $e) {
        // Schema-version mismatch is a real operator-level event,
        // not a 404. Re-throw so the top-level handler maps it to
        // 500 schema_version_mismatch with the syslog line.
        throw $e;
    } catch (\RuntimeException $e) {
        // open_trace_db_ro throws RuntimeException for the
        // "sqlite file not found" path (D-7). Match on the message
        // prefix so we don't swallow other RuntimeExceptions.
        if (str_starts_with($e->getMessage(), 'sqlite file not found')) {
            json_error(404, 'not_found');
        }
        throw $e;
    }
}
