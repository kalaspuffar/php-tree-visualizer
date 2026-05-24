<?php

declare(strict_types=1);

/**
 * GET /api/traces — list traces from index.sqlite.
 *
 * Query parameters (SPECIFICATION.md §5.4):
 *   q       substring filter on uri_or_script (case-insensitive),
 *           default empty (no filter). SQL wildcards %, _, \ in $q are
 *           escaped so they match literally.
 *   limit   1..500, default 100
 *   offset  >= 0, default 0
 *   sort    only "start_time_desc" is accepted today
 *
 * Response shape: {items: [...], total: <int>, has_more: <bool>}.
 *
 * Implementation notes:
 *   - Read-only PDO with DR-5 gate (open_index_db_ro).
 *   - Two queries: COUNT(*) for total, SELECT for the page.
 *   - All user input is bound via :placeholders; no concatenation.
 *   - start_time_ns is rendered via format_rfc3339_ns.
 *   - cpu_snapshot_available (int 0/1) is coerced to JSON bool.
 */

require_once __DIR__ . '/bootstrap.php';

// Full-literal SQL strings — no user input, no $ interpolation. The
// behavioral SQL-injection test plus the static "no $variable inside
// prepare()" assertion in traces_test.php both cover this. Declared
// at file scope so they exist before the dispatch call below.
const SQL_COUNT_NO_FILTER = 'SELECT COUNT(*) FROM traces';
const SQL_COUNT_WITH_FILTER =
    "SELECT COUNT(*) FROM traces WHERE uri_or_script LIKE :pattern ESCAPE '\\'";
const SQL_PAGE_NO_FILTER =
    'SELECT trace_key, trace_id, host, pid, start_time_ns, sapi, uri_or_script, state, call_count, total_wall_ns, dropped_records, anomaly_count, cpu_snapshot_available FROM traces ORDER BY start_time_ns DESC LIMIT :limit OFFSET :offset';
const SQL_PAGE_WITH_FILTER =
    "SELECT trace_key, trace_id, host, pid, start_time_ns, sapi, uri_or_script, state, call_count, total_wall_ns, dropped_records, anomaly_count, cpu_snapshot_available FROM traces WHERE uri_or_script LIKE :pattern ESCAPE '\\' ORDER BY start_time_ns DESC LIMIT :limit OFFSET :offset";

phptv_handle_traces_list();

function phptv_handle_traces_list(): void
{
    dispatch_method('GET');
    require_session();

    [$q, $limit, $offset, $sort] = phptv_parse_traces_query($_GET);

    // sort is currently single-valued; the parser already validated.
    unset($sort);

    $pdo = open_index_db_ro();

    [$rows, $total] = phptv_query_traces($pdo, $q, $limit, $offset);

    $items = array_map('phptv_row_to_item', $rows);
    $hasMore = ($offset + count($items)) < $total;

    json_success(200, [
        'items'    => $items,
        'total'    => $total,
        'has_more' => $hasMore,
    ]);
}

/**
 * Validate the query string. 400 bad_request on any out-of-range or
 * non-conforming input.
 *
 * @param array<string, mixed> $query
 * @return array{0:string, 1:int, 2:int, 3:string}
 */
function phptv_parse_traces_query(array $query): array
{
    $q = $query['q'] ?? '';
    if (!is_string($q)) {
        json_error(400, 'bad_request');
    }

    $limitRaw = $query['limit'] ?? '100';
    if (!is_string($limitRaw) && !is_int($limitRaw)) {
        json_error(400, 'bad_request');
    }
    $limitStr = (string) $limitRaw;
    if (!preg_match('/^[0-9]+$/', $limitStr)) {
        json_error(400, 'bad_request');
    }
    $limit = (int) $limitStr;
    if ($limit < 1 || $limit > 500) {
        json_error(400, 'bad_request');
    }

    $offsetRaw = $query['offset'] ?? '0';
    if (!is_string($offsetRaw) && !is_int($offsetRaw)) {
        json_error(400, 'bad_request');
    }
    $offsetStr = (string) $offsetRaw;
    if (!preg_match('/^[0-9]+$/', $offsetStr)) {
        json_error(400, 'bad_request');
    }
    $offset = (int) $offsetStr;

    $sort = $query['sort'] ?? 'start_time_desc';
    if (!is_string($sort) || $sort !== 'start_time_desc') {
        json_error(400, 'bad_request');
    }

    return [$q, $limit, $offset, $sort];
}

/**
 * Execute the two queries (count + page) and return raw rows plus
 * the matching total. Wildcard chars in $q are escaped so they
 * match literally.
 *
 * @return array{0:list<array<string, mixed>>, 1:int}
 */
function phptv_query_traces(\PDO $pdo, string $q, int $limit, int $offset): array
{
    $hasQ = ($q !== '');
    $countStmt = $hasQ
        ? $pdo->prepare(SQL_COUNT_WITH_FILTER)
        : $pdo->prepare(SQL_COUNT_NO_FILTER);
    if ($hasQ) {
        $countStmt->bindValue(
            ':pattern',
            phptv_like_pattern($q),
            \PDO::PARAM_STR
        );
    }
    $countStmt->execute();
    $total = (int) $countStmt->fetchColumn();

    $pageStmt = $hasQ
        ? $pdo->prepare(SQL_PAGE_WITH_FILTER)
        : $pdo->prepare(SQL_PAGE_NO_FILTER);
    if ($hasQ) {
        $pageStmt->bindValue(
            ':pattern',
            phptv_like_pattern($q),
            \PDO::PARAM_STR
        );
    }
    $pageStmt->bindValue(':limit', $limit, \PDO::PARAM_INT);
    $pageStmt->bindValue(':offset', $offset, \PDO::PARAM_INT);
    $pageStmt->execute();
    /** @var list<array<string, mixed>> $rows */
    $rows = $pageStmt->fetchAll();

    return [$rows, $total];
}

/**
 * Build the bound LIKE pattern value: escape SQL wildcards in $q,
 * wrap in % on both sides.
 */
function phptv_like_pattern(string $q): string
{
    return '%' . phptv_escape_like($q) . '%';
}

/**
 * Escape SQL LIKE wildcards in the user input so they match literally
 * under `ESCAPE '\\'`. Backslash must be escaped first.
 */
function phptv_escape_like(string $q): string
{
    return strtr($q, [
        '\\' => '\\\\',
        '%'  => '\\%',
        '_'  => '\\_',
    ]);
}

/**
 * Map one SQLite row onto the JSON shape per §5.4.
 *
 * @param array<string, mixed> $row
 * @return array<string, mixed>
 */
function phptv_row_to_item(array $row): array
{
    return [
        'trace_key'              => (string) $row['trace_key'],
        'trace_id'               => (string) $row['trace_id'],
        'host'                   => (string) $row['host'],
        'pid'                    => (int) $row['pid'],
        'start_time'             => format_rfc3339_ns((int) $row['start_time_ns']),
        'sapi'                   => (string) $row['sapi'],
        'uri_or_script'          => (string) $row['uri_or_script'],
        'state'                  => (string) $row['state'],
        'call_count'             => (int) $row['call_count'],
        'total_wall_ns'          => (int) $row['total_wall_ns'],
        'dropped_records'        => (int) $row['dropped_records'],
        'anomaly_count'          => (int) $row['anomaly_count'],
        'cpu_snapshot_available' => ((int) $row['cpu_snapshot_available']) === 1,
    ];
}
