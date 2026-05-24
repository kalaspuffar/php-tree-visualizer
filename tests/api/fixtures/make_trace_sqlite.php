<?php

declare(strict_types=1);

/**
 * Build a fixture per-trace `<key>.sqlite` matching SPECIFICATION.md
 * §4.3.
 *
 *   make_trace_sqlite(
 *       string $path,
 *       array  $meta,        // trace_meta column overrides
 *       array  $dict,        // [fn_id => [fqn, file, line, kind]]
 *       array  $nodes,       // see below
 *       array  $anomalies,   // optional: [[node_id, kind, count, detail], ...]
 *       int    $userVersion = 1
 *   ): void
 *
 * The synthetic root (node_id=1, parent_node_id=NULL, fn_id=0,
 * fqn='<root>') is inserted automatically with values derived from
 * the supplied $nodes children — its total_wall_ns is the sum of its
 * direct children's total_wall_ns; children_total_wall_ns is also
 * that sum (DI-3 holds: self_wall_ns of the root is 0).
 *
 * `$nodes` shape: a list of arrays each with keys:
 *   node_id          int  (auto-assigned if omitted; sequential from 2)
 *   parent_node_id   int  (1 = synthetic root)
 *   fn_id            int  (must exist in $dict)
 *   depth            int  (cached, the fixture trusts the caller)
 *   call_count       int
 *   total_wall_ns    int
 *   children_total_wall_ns int  (auto-derived if omitted: sum of
 *                                children's total_wall_ns)
 *   total_cpu_u_ns   int
 *   total_cpu_s_ns   int
 *   total_mem_delta_bytes int
 *   abnormal_exit_count int
 *
 * `$dict` is keyed by fn_id; the synthetic fn_id=0 entry is added
 * automatically.
 *
 * Test authors describe a tree by writing nodes in any order and
 * letting the builder fill defaults; the only required keys are
 * `parent_node_id` and `fn_id`.
 */

/**
 * @param array<string, mixed>                                            $meta
 * @param array<int, array{fqn: string, file?: string, line?: int, kind?: int}> $dict
 * @param list<array<string, mixed>>                                      $nodes
 * @param list<array<string, mixed>>                                      $anomalies
 */
function make_trace_sqlite(
    string $path,
    array $meta = [],
    array $dict = [],
    array $nodes = [],
    array $anomalies = [],
    int $userVersion = 1
): void {
    $dir = dirname($path);
    if (!is_dir($dir)) {
        mkdir($dir, 0700, true);
    }
    foreach ([$path, $path . '-wal', $path . '-shm'] as $stale) {
        if (is_file($stale)) {
            unlink($stale);
        }
    }

    $pdo = new \PDO('sqlite:' . $path, null, null, [
        \PDO::ATTR_ERRMODE => \PDO::ERRMODE_EXCEPTION,
    ]);
    $pdo->exec('PRAGMA journal_mode = WAL');
    $pdo->exec('PRAGMA synchronous = NORMAL');
    $pdo->exec('PRAGMA foreign_keys = ON');
    $pdo->exec("PRAGMA user_version = {$userVersion}");

    // --- DDL exactly mirrors SPECIFICATION.md §4.3 ----------------

    $pdo->exec(
        'CREATE TABLE trace_meta (
            trace_key              TEXT    PRIMARY KEY,
            trace_id               TEXT    NOT NULL,
            host                   TEXT    NOT NULL,
            pid                    INTEGER NOT NULL,
            start_time_ns          INTEGER NOT NULL,
            sapi                   TEXT    NOT NULL,
            uri_or_script          TEXT    NOT NULL,
            state                  TEXT    NOT NULL,
            dropped_records        INTEGER NOT NULL DEFAULT 0,
            cpu_snapshot_available INTEGER NOT NULL DEFAULT 1
        )'
    );
    $pdo->exec(
        'CREATE TABLE dict (
            fn_id   INTEGER PRIMARY KEY,
            fqn     TEXT    NOT NULL,
            file    TEXT    NOT NULL,
            line    INTEGER NOT NULL,
            kind    INTEGER NOT NULL CHECK (kind BETWEEN 0 AND 3)
        )'
    );
    $pdo->exec(
        'CREATE TABLE nodes (
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
        )'
    );
    $pdo->exec('CREATE INDEX idx_nodes_parent ON nodes (parent_node_id)');
    $pdo->exec('CREATE INDEX idx_nodes_fn     ON nodes (fn_id)');

    $pdo->exec(
        'CREATE TABLE anomalies (
            rowid          INTEGER PRIMARY KEY AUTOINCREMENT,
            node_id        INTEGER REFERENCES nodes(node_id),
            kind           TEXT    NOT NULL,
            count          INTEGER NOT NULL DEFAULT 1,
            sample_call_id INTEGER,
            detail         TEXT
        )'
    );
    $pdo->exec('CREATE INDEX idx_anomalies_node ON anomalies (node_id)');

    // --- trace_meta -----------------------------------------------

    $metaDefaults = [
        'trace_key'              => str_repeat('a', 32),
        'trace_id'               => '00000000-0000-0000-0000-000000000000',
        'host'                   => 'test-host',
        'pid'                    => 1,
        'start_time_ns'          => 1_700_000_000_000_000_000,
        'sapi'                   => 'cli',
        'uri_or_script'          => '/srv/app/test.php',
        'state'                  => 'finalized',
        'dropped_records'        => 0,
        'cpu_snapshot_available' => 1,
    ];
    $metaRow = array_merge($metaDefaults, $meta);

    $stmt = $pdo->prepare(
        'INSERT INTO trace_meta (
            trace_key, trace_id, host, pid, start_time_ns, sapi,
            uri_or_script, state, dropped_records, cpu_snapshot_available
         ) VALUES (
            :trace_key, :trace_id, :host, :pid, :start_time_ns, :sapi,
            :uri_or_script, :state, :dropped_records, :cpu_snapshot_available
         )'
    );
    foreach ($metaRow as $col => $val) {
        $stmt->bindValue(':' . $col, $val);
    }
    $stmt->execute();

    // --- dict -----------------------------------------------------

    // The synthetic root's dict entry.
    $dict[0] = $dict[0] ?? [
        'fqn'  => '<root>',
        'file' => '',
        'line' => 0,
        'kind' => 0,
    ];

    $dictStmt = $pdo->prepare(
        'INSERT INTO dict (fn_id, fqn, file, line, kind) VALUES (:fn_id, :fqn, :file, :line, :kind)'
    );
    foreach ($dict as $fnId => $entry) {
        $dictStmt->bindValue(':fn_id', $fnId, \PDO::PARAM_INT);
        $dictStmt->bindValue(':fqn',   (string) $entry['fqn'], \PDO::PARAM_STR);
        $dictStmt->bindValue(':file',  (string) ($entry['file'] ?? ''), \PDO::PARAM_STR);
        $dictStmt->bindValue(':line',  (int) ($entry['line'] ?? 0), \PDO::PARAM_INT);
        $dictStmt->bindValue(':kind',  (int) ($entry['kind'] ?? 0), \PDO::PARAM_INT);
        $dictStmt->execute();
    }

    // --- nodes ----------------------------------------------------

    // First, pre-process the caller's $nodes list so every entry has
    // an explicit node_id, and so we can compute auto-derived
    // children_total_wall_ns by summing children before insert.

    // Auto-assign node_ids: synthetic root is 1; subsequent rows
    // start at 2 in the order the caller supplied them.
    $nextId = 2;
    $byId = [];
    foreach ($nodes as $i => $node) {
        if (!isset($node['parent_node_id'])) {
            throw new \InvalidArgumentException(
                "node[{$i}] requires parent_node_id"
            );
        }
        if (!isset($node['fn_id'])) {
            throw new \InvalidArgumentException(
                "node[{$i}] requires fn_id"
            );
        }
        if (!isset($node['node_id'])) {
            $node['node_id'] = $nextId++;
        }
        $byId[$node['node_id']] = $node;
        $nodes[$i] = $node;
    }

    // Compute children_total_wall_ns for any node that didn't set it.
    foreach ($byId as $id => $node) {
        if (isset($node['children_total_wall_ns'])) {
            continue;
        }
        $sum = 0;
        foreach ($byId as $other) {
            if (($other['parent_node_id'] ?? null) === $id) {
                $sum += (int) ($other['total_wall_ns'] ?? 0);
            }
        }
        $byId[$id]['children_total_wall_ns'] = $sum;
    }

    // Insert the synthetic root first so its node_id=1 is allocated
    // before any descendant references it.
    $rootChildrenSum = 0;
    foreach ($byId as $node) {
        if (($node['parent_node_id'] ?? null) === 1) {
            $rootChildrenSum += (int) ($node['total_wall_ns'] ?? 0);
        }
    }

    // The root's total_wall_ns equals the sum of its children's
    // total_wall_ns by definition (self time at the root is 0 for
    // typical fixtures; tests can override via $meta if they need
    // non-zero root self time).
    $rootTotalWall = $meta['root_total_wall_ns'] ?? $rootChildrenSum;

    $pdo->exec(
        'INSERT INTO nodes (node_id, parent_node_id, fn_id, depth,'
        . ' call_count, total_wall_ns, children_total_wall_ns,'
        . ' total_cpu_u_ns, total_cpu_s_ns, total_mem_delta_bytes,'
        . ' abnormal_exit_count) VALUES (1, NULL, 0, 0,'
        . ' 1, ' . $rootTotalWall . ', ' . $rootChildrenSum . ', 0, 0, 0, 0)'
    );

    // Insert descendants in parent-before-child order. Sort by
    // walking BFS from the root. Pull from $byId (which has the
    // auto-derived children_total_wall_ns) rather than the original
    // $nodes array.
    $insertedIds = [1 => true];
    $insertOrder = [];
    $pending = $byId;
    // Bounded by node count squared; fine for fixtures.
    while ($pending) {
        $progress = false;
        foreach ($pending as $key => $node) {
            $parentId = (int) $node['parent_node_id'];
            if (isset($insertedIds[$parentId])) {
                $insertOrder[] = $node;
                $insertedIds[(int) $node['node_id']] = true;
                unset($pending[$key]);
                $progress = true;
            }
        }
        if (!$progress) {
            throw new \InvalidArgumentException(
                'fixture has an orphan node (parent_node_id not in tree)'
            );
        }
    }

    $insertStmt = $pdo->prepare(
        'INSERT INTO nodes (
            node_id, parent_node_id, fn_id, depth, call_count,
            total_wall_ns, children_total_wall_ns,
            total_cpu_u_ns, total_cpu_s_ns, total_mem_delta_bytes,
            abnormal_exit_count
         ) VALUES (
            :node_id, :parent_node_id, :fn_id, :depth, :call_count,
            :total_wall_ns, :children_total_wall_ns,
            :total_cpu_u_ns, :total_cpu_s_ns, :total_mem_delta_bytes,
            :abnormal_exit_count
         )'
    );
    foreach ($insertOrder as $node) {
        $insertStmt->bindValue(':node_id',                (int) $node['node_id'],                              \PDO::PARAM_INT);
        $insertStmt->bindValue(':parent_node_id',         (int) $node['parent_node_id'],                       \PDO::PARAM_INT);
        $insertStmt->bindValue(':fn_id',                  (int) $node['fn_id'],                                \PDO::PARAM_INT);
        $insertStmt->bindValue(':depth',                  (int) ($node['depth'] ?? 1),                         \PDO::PARAM_INT);
        $insertStmt->bindValue(':call_count',             (int) ($node['call_count'] ?? 1),                    \PDO::PARAM_INT);
        $insertStmt->bindValue(':total_wall_ns',          (int) ($node['total_wall_ns'] ?? 0),                 \PDO::PARAM_INT);
        $insertStmt->bindValue(':children_total_wall_ns', (int) ($node['children_total_wall_ns'] ?? 0),        \PDO::PARAM_INT);
        $insertStmt->bindValue(':total_cpu_u_ns',         (int) ($node['total_cpu_u_ns'] ?? 0),                \PDO::PARAM_INT);
        $insertStmt->bindValue(':total_cpu_s_ns',         (int) ($node['total_cpu_s_ns'] ?? 0),                \PDO::PARAM_INT);
        $insertStmt->bindValue(':total_mem_delta_bytes',  (int) ($node['total_mem_delta_bytes'] ?? 0),         \PDO::PARAM_INT);
        $insertStmt->bindValue(':abnormal_exit_count',    (int) ($node['abnormal_exit_count'] ?? 0),           \PDO::PARAM_INT);
        $insertStmt->execute();
    }

    // --- anomalies ------------------------------------------------

    $anomalyStmt = $pdo->prepare(
        'INSERT INTO anomalies (node_id, kind, count, sample_call_id, detail) VALUES (:node_id, :kind, :count, :sample_call_id, :detail)'
    );
    foreach ($anomalies as $a) {
        $anomalyStmt->bindValue(':node_id',        $a['node_id']        ?? null);
        $anomalyStmt->bindValue(':kind',           $a['kind']           ?? 'unresolved_fn');
        $anomalyStmt->bindValue(':count',          (int) ($a['count']   ?? 1), \PDO::PARAM_INT);
        $anomalyStmt->bindValue(':sample_call_id', $a['sample_call_id'] ?? null);
        $anomalyStmt->bindValue(':detail',         $a['detail']         ?? null);
        $anomalyStmt->execute();
    }

    // Checkpoint so read-only opens see every row immediately.
    $pdo->exec('PRAGMA wal_checkpoint(TRUNCATE)');
    $pdo = null;
}
