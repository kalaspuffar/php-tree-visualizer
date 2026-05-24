<?php

declare(strict_types=1);

/**
 * Build a fixture `index.sqlite` matching SPECIFICATION.md §4.2.
 *
 *   make_index_sqlite(string $path, array $rows, int $userVersion = 1): void
 *
 * `$rows` is a list of arrays whose keys map onto the `traces`
 * columns. Missing optional fields get sensible defaults so test
 * authors don't have to spell out every column. The file is recreated
 * if it already exists.
 *
 * The schema mirrors the production DDL byte-for-byte (modulo
 * whitespace). If the Rust collector ever evolves it, this fixture
 * builder needs the same edit — that's the price of decoupling.
 */

/**
 * @param list<array<string,mixed>> $rows
 */
function make_index_sqlite(string $path, array $rows = [], int $userVersion = 1): void
{
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

    $pdo->exec(
        'CREATE TABLE traces (
            trace_key              TEXT    PRIMARY KEY,
            trace_id               TEXT    NOT NULL,
            host                   TEXT    NOT NULL,
            pid                    INTEGER NOT NULL,
            start_time_ns          INTEGER NOT NULL,
            sapi                   TEXT    NOT NULL
                                   CHECK (sapi IN (\'cli\', \'fpm-fcgi\')),
            uri_or_script          TEXT    NOT NULL,
            state                  TEXT    NOT NULL
                                   CHECK (state IN (\'active\', \'finalized\'))
                                   DEFAULT \'active\',
            first_batch_at_ns      INTEGER NOT NULL,
            last_batch_at_ns       INTEGER NOT NULL,
            batch_count            INTEGER NOT NULL DEFAULT 0,
            call_count             INTEGER NOT NULL DEFAULT 0,
            total_wall_ns          INTEGER NOT NULL DEFAULT 0,
            dropped_records        INTEGER NOT NULL DEFAULT 0,
            anomaly_count          INTEGER NOT NULL DEFAULT 0,
            cpu_snapshot_available INTEGER NOT NULL DEFAULT 1
        )'
    );
    $pdo->exec('CREATE INDEX idx_traces_start_time     ON traces (start_time_ns DESC)');
    $pdo->exec('CREATE INDEX idx_traces_uri            ON traces (uri_or_script)');
    $pdo->exec('CREATE INDEX idx_traces_state_lastbatch ON traces (state, last_batch_at_ns)');

    $defaults = [
        'trace_id'              => '00000000-0000-0000-0000-000000000000',
        'host'                  => 'test-host',
        'pid'                   => 1,
        'start_time_ns'         => 1_700_000_000_000_000_000,
        'sapi'                  => 'cli',
        'uri_or_script'         => '/srv/app/test.php',
        'state'                 => 'finalized',
        'first_batch_at_ns'     => 1_700_000_000_000_000_000,
        'last_batch_at_ns'      => 1_700_000_000_000_000_000,
        'batch_count'           => 1,
        'call_count'            => 100,
        'total_wall_ns'         => 1_000_000_000,
        'dropped_records'       => 0,
        'anomaly_count'         => 0,
        'cpu_snapshot_available'=> 1,
    ];

    $stmt = $pdo->prepare(
        'INSERT INTO traces (
            trace_key, trace_id, host, pid, start_time_ns, sapi,
            uri_or_script, state, first_batch_at_ns, last_batch_at_ns,
            batch_count, call_count, total_wall_ns, dropped_records,
            anomaly_count, cpu_snapshot_available
        ) VALUES (
            :trace_key, :trace_id, :host, :pid, :start_time_ns, :sapi,
            :uri_or_script, :state, :first_batch_at_ns, :last_batch_at_ns,
            :batch_count, :call_count, :total_wall_ns, :dropped_records,
            :anomaly_count, :cpu_snapshot_available
        )'
    );

    foreach ($rows as $row) {
        $merged = array_merge($defaults, $row);
        if (!isset($merged['trace_key'])) {
            throw new \InvalidArgumentException(
                'fixture row must supply trace_key'
            );
        }
        foreach ($merged as $col => $val) {
            $stmt->bindValue(':' . $col, $val);
        }
        $stmt->execute();
    }

    // Ensure the WAL is checkpointed into the main file so a
    // read-only opener (which won't write the WAL back) sees every row.
    $pdo->exec('PRAGMA wal_checkpoint(TRUNCATE)');
    $pdo = null;
}
