# php-tree-visualizer

A collector and web visualizer for the [`php-analyze`](https://github.com/kalaspuffar/php-analyze) PHP profiling extension: ingest MessagePack call-tree batches, reconstruct the per-trace tree, and render flame-graph-style views in a browser.

## Architecture

```
   php-analyze extension
   (PHP process, in-tree)
            │
            │  HTTP POST /ingest/v1
            │  (MessagePack, schema v1)
            ▼
   Rust collector ────────► <data_dir>/traces/<key>.sqlite
   (this repo, async)       <data_dir>/traces/<key>.raw/
            │               <data_dir>/index.sqlite
            │
            │  read-only PDO
            ▼
   PHP API  ◄─── reverse proxy (Apache 2.4+ or nginx) ───►  Static frontend
   /api/*                                                     /viz/*
   (this repo, api/)                                          (this repo, viz/)
```

See [SPECIFICATION.md §2.1](./SPECIFICATION.md) for the canonical architecture view.

## Prerequisites

- **Rust toolchain** — `stable`, pinned via [`rust-toolchain.toml`](./rust-toolchain.toml). Includes `rustfmt` and `clippy`. The first `cargo build` will install the pinned components via `rustup`.
- **PHP 8.1 or newer** with PHP-FPM. The API code uses typed properties and `declare(strict_types=1)`.
- **A reverse proxy** — Apache 2.4+ or nginx. Illustrative snippets live at [`etc/apache-example.conf`](./etc/apache-example.conf) and [`etc/nginx-example.conf`](./etc/nginx-example.conf).
- **The upstream `php-analyze` PHP extension** — installed in the PHP build whose traces you want to see. The extension is in a separate repository: <https://github.com/kalaspuffar/php-analyze>.
- **Linux x86_64.** The collector and the extension are both tested on Linux; other platforms are not covered.

## Quickstart

The steps below take an operator from `git clone` to a finalized trace visible in the UI on a Linux host that already has PHP-FPM and Apache (or nginx) running.

### 1. Clone and build the collector

```bash
git clone https://github.com/kalaspuffar/php-tree-visualizer.git
cd php-tree-visualizer
cargo build --release
```

The compiled binary lands at `./target/release/php-tree-viz-collector`.

### 2. Prepare the data directory

The collector writes per-trace SQLite databases under a single data directory. Both the collector AND the PHP-FPM user need access — the collector writes, the PHP API reads via a shared group. SQLite WAL mode also requires shared readers to be able to update the `*-shm` file, so "read-only via group" is not enough; the data dir must be group-writable.

Create the directory, set ownership to your user with `www-data` as the shared group, and turn on the setgid bit so new files inherit the group:

```bash
sudo mkdir -p /var/lib/php-tree-viz
sudo chown "$USER":www-data /var/lib/php-tree-viz
sudo chmod 2770 /var/lib/php-tree-viz
```

The `2770` is intentional: `2` = setgid (new files inherit `www-data`), `770` = owner + group full access, no world access. Substitute `www-data` for whatever group your PHP-FPM pool runs as.

Setting `umask 0007` before starting the collector also helps — the collector inherits the umask and new SQLite files land at mode `0660` instead of the default `0664`. See step 4 below.

### 3. Write the collector config

Copy the example file, set up shared-group ownership so the PHP API can read it too, then replace both `REPLACE_ME` placeholders with two distinct ≥32-character strings:

```bash
sudo mkdir -p /etc/php-tree-viz
sudo cp etc/collector.toml.example /etc/php-tree-viz/collector.toml
sudo chown "$USER":www-data /etc/php-tree-viz/collector.toml
sudo chmod 0640 /etc/php-tree-viz/collector.toml
$EDITOR /etc/php-tree-viz/collector.toml
```

Mode `0640` means owner-write + group-read — the collector (running as you) can edit; the PHP API (running as `www-data`) can read; nothing outside the shared group sees the secrets.

The shape of [`etc/collector.toml.example`](./etc/collector.toml.example) mirrors [SPECIFICATION.md §7.3](./SPECIFICATION.md). Pay attention to:

- `[server].bind` — must be a loopback address (`127.0.0.1:<port>` or `[::1]:<port>`). The collector refuses to bind anything else.
- `[auth].token` and `[auth].session_salt` — each at least 32 characters, distinct from each other. Generate with `openssl rand -base64 33 | tr -d +/= | head -c 40`.
- `[storage].data_dir` — the directory you created in step 2.

### 4. Run the collector

Set `umask 0007` so the collector creates SQLite files at mode `0660` (group-readable+writable), then start the binary:

```bash
umask 0007
./target/release/php-tree-viz-collector --config /etc/php-tree-viz/collector.toml
```

The collector emits structured `tracing` events on stdout. The first events tell you the config loaded, the listener bound, and the periodic disk-usage gauge started. See [Expected output](#expected-output) for a sample.

For a long-running deployment, run under `systemd` (the spec includes a sample unit at [SPECIFICATION.md §3.6](./SPECIFICATION.md)) so journald captures the stream. Set `UMask=0007` in the service unit's `[Service]` section so the same file modes apply.

### 5. Front it with Apache or nginx

The collector binds to localhost only. The web stack (PHP API + static frontend) sits behind your existing reverse proxy. Paste the snippet that matches your proxy into the vhost that already fronts PHP-FPM:

- Apache: [`etc/apache-example.conf`](./etc/apache-example.conf)
- nginx: [`etc/nginx-example.conf`](./etc/nginx-example.conf)

Both snippets map `/api/*` to the PHP files in [`api/`](./api/) and `/viz/*` to the static frontend in [`viz/`](./viz/). They also refuse `/api/internal/*` (those are PHP includes, never HTTP-reachable).

A few things worth checking on your reverse proxy before reloading:

- **`pdo_sqlite` MUST be loaded in the PHP-FPM build** that handles `/api/*.php`. On Debian, this is the `php8.x-sqlite3` package — installed for PHP 8.4 on a default install, but not always for 8.3 or older. Verify with `php-fpm8.x -m | grep pdo_sqlite`. The vhost's `<FilesMatch>` socket path is where you select the PHP version.
- **If you symlink `/var/www/<your-site>/{api,viz}` to this repo's `api/` and `viz/` directories** (rather than copying them), Apache 2.4 requires `Options +FollowSymLinks` inside the `<Directory>` block. The example snippet does not include this — add it if you use symlinks.
- **If the symlink target sits inside your home directory** (`/home/$USER/...`), make sure `www-data` can traverse it. Debian's default `/home/$USER` is mode `0700`; widen it to `0701` (`chmod o+x /home/$USER`) or move the files into a www-data-readable path. Mode `0701` keeps your files private from other users while letting daemons enter to follow symlinks.

Reload the reverse proxy after editing the config.

### 6. Send a probe batch

The collector is now up but has no traces yet. Send a single well-formed v1 batch with `curl` to confirm the wiring:

```bash
TOKEN=$(awk -F'"' '/^token/ {print $2}' /etc/php-tree-viz/collector.toml)
PORT=$(awk -F'"' '/^bind/ {print $2}' /etc/php-tree-viz/collector.toml | awk -F: '{print $NF}')

# Probe body: any MessagePack-encoded v1 batch. The integration test
# helper at crates/php-tree-viz-collector/tests/support/batch.rs has
# `build_test_batch_with_chain(...)` for a synthetic one-call-plus-child
# batch. You can also point a real `php-analyze`-instrumented PHP
# process at the collector and skip this curl step entirely.
curl -i \
    --data-binary @/path/to/your/probe.msgpack \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/vnd.php-analyze.v1+msgpack" \
    "http://127.0.0.1:${PORT}/ingest/v1"
```

A successful probe returns `HTTP/1.1 200 OK`. The collector then logs one `batch accepted` event (see [Expected output](#expected-output)) and writes the trace into `<data_dir>/traces/<key>.sqlite`.

A trace becomes "finalized" once no new batches arrive for 30 seconds (configurable). The retention sweeper then handles aging.

### 7. Open the UI

Browse to `http://<your-host>/viz/login.html`, sign in with the same bearer token from `[auth].token`, and the trace list at `/viz/index.html` shows the trace you just ingested. Click into a row for the call-tree view.

### Expected output

A successful run of steps 4–6 above produces a `tracing` event stream that starts like this. Timestamps, ports, byte counts, PIDs, and trace identifiers vary run-to-run; `<placeholder>` tokens mark the parts that change. The structure (event message, field names, level prefix) is what to verify.

```text
<timestamp>  INFO config: configuration loaded path=/etc/php-tree-viz/collector.toml bind=127.0.0.1:<port> data_dir=/var/lib/php-tree-viz retention_days=30 queue_capacity=256 max_body_bytes=67108864 log_level=info log_format=text
<timestamp>  INFO php_tree_viz_collector::http::server: listening addr=127.0.0.1:<port>
<timestamp>  INFO php_tree_viz_collector::observability::disk_usage: disk usage data_dir_bytes=<N> trace_count=0 threshold_pct=80 over_threshold=false
<timestamp>  INFO php_tree_viz_collector::http::logging: request method=POST path=/ingest/v1 remote_addr=127.0.0.1:<port> status=200 body_bytes=<N>
<timestamp>  INFO php_tree_viz_collector::http::server: batch accepted trace_key=<32hex> trace_id=00000000-0000-0000-0000-000000000000 host=<host> pid=<pid> body_bytes=<N> dict_entries=<N> call_count=<N> nodes=<N> pending=<N> anomalies=0
<timestamp>  INFO php_tree_viz_collector::finalize: trace finalized trace_key=<32hex> pending_dq2=0 cpu_snapshot_available=true
```

After 30 s of no further batches, the `trace finalized` event appears for the same `trace_key`. The `disk usage` event repeats once per hour by default — adjust `[observability].disk_usage_tick_seconds` in the config for a snappier cadence during testing.

If you ran with `log.format = "json"` (the production default) instead of `"text"`, each line is a JSON object with the same fields flattened to the top level.

If you don't see the `listening` event within a second of starting the binary, look for a `config error:` or `observability error:` line on stderr — those are the only places the collector writes outside of the `tracing` subscriber, and they signal a startup failure.

## Logs and observability

The collector's runtime is observable through one structured `tracing` event stream. Subscriber install is documented in the `collector-observability` capability under [`openspec/specs/`](./openspec/) (gitignored locally; consult [SPECIFICATION.md §10.8](./SPECIFICATION.md) for the operator-facing summary).

The signals an operator usually cares about:

- **`batch accepted`** — one event per ingested batch, with the F-1.10 field set: `trace_key`, `trace_id`, `host`, `pid`, `body_bytes`, `dict_entries`, `call_count`, `nodes`, `pending`, `anomalies`. Grep this to confirm the extension is reaching the collector.
- **`disk usage`** — hourly gauge by default. Carries `data_dir_bytes`, `trace_count`, `threshold_pct`, `over_threshold`. The event level rises to `warn` once `data_dir_bytes / storage.disk_capacity_bytes >= observability.disk_usage_warn_pct%`. Set `storage.disk_capacity_bytes` in the config to make `over_threshold` meaningful; leave it unset to disable the threshold check.
- **`trace finalized`** — one event per trace that crosses the idle window. Carries `trace_key`, `pending_dq2`, `cpu_snapshot_available`.
- **`retention swept`** — one summary event per retention tick that pruned anything. Carries `removed_traces` and `freed_bytes`. Silent on ticks that pruned nothing.
- **`request`** — one event per HTTP request (auth failures included), with `method`, `path`, `remote_addr`, `status`, `body_bytes`. Never carries `Authorization` content (verified by the S-8 regression test).

Filter level is `log.level` in the config (`trace|debug|info|warn|error`, default `info`). Override at runtime with the `RUST_LOG` environment variable; a malformed `RUST_LOG` falls back to the config level with a `warn` event explaining what happened.

Output format is `log.format` (`text` for the layout shown under [Expected output](#expected-output), `json` for one JSON object per event with all fields flattened — what journald wants).

## Project status

**Pre-1.0.** All eight phases in [SPECIFICATION.md §10](./SPECIFICATION.md) are implemented; the operator-visible surface is stable. There is no semantic-versioning guarantee yet — internal APIs (PHP API endpoints, SQLite schemas, log event names and fields) may change between commits.

The wire format the collector accepts is `php-analyze` schema **v1**. The contract is at `handover/WIRE_FORMAT.md` (in this repo) and authoritative at the upstream `php-analyze` project. The collector rejects anything other than `schema_version = 1` with a `422`.

**Deploy on a trusted internal network only.** The collector binds loopback; the reverse proxy is the only thing that should reach the bearer-token-protected endpoints. There is no TLS termination inside the collector. See [SECURITY.md](./SECURITY.md) for the threat model and [SPECIFICATION.md §6](./SPECIFICATION.md) for depth.

## Documentation

- [SPECIFICATION.md](./SPECIFICATION.md) — full architecture, requirements, data model, and acceptance criteria.
- [CONTRIBUTING.md](./CONTRIBUTING.md) — toolchain, gates, branch / commit / review handoff, and the OpenSpec workflow this repository uses.
- [SECURITY.md](./SECURITY.md) — threat model, token rotation, and how to report a vulnerability.
- [LICENSE](./LICENSE) — `MIT`.

## License

`MIT`. See [LICENSE](./LICENSE).
