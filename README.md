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

- **Rust toolchain via `rustup`** — `stable`, pinned by [`rust-toolchain.toml`](./rust-toolchain.toml). `rustup` itself is **not in apt** on Debian; install it once with the canonical shell installer:

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
  . "$HOME/.cargo/env"
  ```

  The first `cargo build` after that picks up the pinned components (`rustfmt`, `clippy`) automatically. The install script (Quick install below) does this for you when `cargo` isn't already on PATH.
- **PHP 8.1 or newer** with PHP-FPM. The API code uses typed properties and `declare(strict_types=1)`.
- **`pdo_sqlite` loaded in PHP-FPM.** On Debian this is the `php8.4-sqlite3` apt package — installed alongside `php8.4-fpm` does *not* pull it in. Without it, every `/api/*` request returns `500 internal_error`.
- **`libclang-dev`** (build-time only). `cargo build` invokes `bindgen` via the `libsqlite3-sys` build script, which needs `libclang.so`. Missing this package makes the initial build fail with a non-obvious bindgen error.
- **A reverse proxy** — Apache 2.4+ or nginx. Illustrative snippets live at [`etc/apache-example.conf`](./etc/apache-example.conf) and [`etc/nginx-example.conf`](./etc/nginx-example.conf).
- **The upstream `php-analyze` PHP extension** — installed in the PHP build whose traces you want to see. The extension is in a separate repository: <https://github.com/kalaspuffar/php-analyze>.
- **Linux x86_64.** The collector and the extension are both tested on Linux; other platforms are not covered.

On a fresh Debian 13 box, this one-liner installs everything in the list above (substitute your distro's package manager as needed):

```bash
sudo apt-get install -y apache2 php8.4-fpm php8.4-sqlite3 libclang-dev curl openssl
```

The Rust toolchain itself comes from `rustup`. If you don't have it, install via the rustup script and accept defaults.

## Quickstart

The steps below take an operator from `git clone` to a finalized trace visible in the UI on a Linux host that already has PHP-FPM and Apache (or nginx) running.

### Quick install (Debian 13)

On a freshly-imaged Debian 13 box, every step in the rest of this Quickstart is bundled into one idempotent script:

```bash
git clone https://github.com/kalaspuffar/php-tree-visualizer.git
cd php-tree-visualizer
sudo bash bin/install-debian.sh <your-hostname>                  # Apache (default)
# or
sudo bash bin/install-debian.sh <your-hostname> --proxy=nginx    # nginx
```

The script installs apt packages, builds the collector, deploys the config + systemd unit + reverse-proxy vhost, starts everything, and runs a smoke test that POSTs a probe batch and confirms it lands in `/api/traces`. The reverse proxy is `apache` by default; pass `--proxy=nginx` to install nginx instead. Re-running on a working install is a no-op — secrets are preserved, packages are skipped, the vhost is overwritten from the tracked template.

The script is Debian-13-only; the hand-curated steps below describe what it does, in case you're on a different distro or want to understand the deployment shape before you trust the script.

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

- `[server].bind` — must be a loopback address (`127.0.0.1:<port>` or `[::1]:<port>`). The collector refuses to bind anything else and exits with a clear error on startup. **Why loopback?** The spec ([§3.4 / NF-4.4](./SPECIFICATION.md)) assumes a reverse proxy in front of the collector that terminates TLS and gates the public-internet edge. The collector itself doesn't speak TLS and isn't hardened for direct public exposure; `0.0.0.0` would expose the bearer-token endpoint on every interface, contradicting the threat model. Keep it loopback and let Apache/nginx do the public-facing work.
- `[auth].token` and `[auth].session_salt` — each at least 32 characters, distinct from each other. Generate with `openssl rand -base64 33 | tr -d +/= | head -c 40`.
- `[storage].data_dir` — the directory you created in step 2.

### 4. Run the collector

The recommended path is via systemd. A tracked, working unit lives at [`etc/php-tree-viz-collector.service.example`](./etc/php-tree-viz-collector.service.example) — copy it to `/etc/systemd/system/`, reload, enable:

```bash
sudo install -o root -g root -m 0644 etc/php-tree-viz-collector.service.example \
    /etc/systemd/system/php-tree-viz-collector.service
sudo systemctl daemon-reload
sudo systemctl enable --now php-tree-viz-collector
sudo systemctl status php-tree-viz-collector
```

The tracked unit is `Type=simple` (not `Type=notify` like [SPECIFICATION.md §3.6](./SPECIFICATION.md) shows — the binary doesn't call `sd_notify` yet, so the spec's sample would hang on startup and time out; the example unit's top comment explains the deviation). It sets `UMask=0007` so SQLite files land at mode `0660` and the PHP API can read them via the shared `www-data` group.

For ad-hoc verification without systemd, just run the binary in the foreground:

```bash
umask 0007
./target/release/php-tree-viz-collector --config /etc/php-tree-viz/collector.toml
```

The collector emits structured `tracing` events on stdout (or journald, via systemd). The first events tell you the config loaded, the listener bound, and the periodic disk-usage gauge started. See [Expected output](#expected-output) for a sample.

The example config defaults to `log.format = "json"` — the [Expected output](#expected-output) sample below shows that shape. If you'd rather eyeball plain text during initial wiring, flip the value to `"text"` and `sudo systemctl restart php-tree-viz-collector`; the same events come out as `<timestamp>  INFO target: <message> field=value …` lines. The field set is identical between the two formats.

### 5. Front it with Apache or nginx

The collector binds to localhost only. The web stack (PHP API + static frontend) sits behind your existing reverse proxy. Pick whichever you already run (or apt-install fresh):

- Apache: [`etc/apache-example.conf`](./etc/apache-example.conf)
- nginx: [`etc/nginx-example.conf`](./etc/nginx-example.conf)

Both snippets cover the same routes: `/` redirects to `/viz/login.html`; `/api/*` rewrites to the PHP handlers in [`api/`](./api/); `/api/internal/*` is denied (those are PHP includes, never HTTP-reachable); `/viz/*` serves the static frontend in [`viz/`](./viz/) with `X-Content-Type-Options` + `Referrer-Policy`; `/ingest/v1` reverse-proxies to the collector on `127.0.0.1:8088`. The snippets are *fragments* — paste them inside an existing vhost on your machine, or let `bin/install-debian.sh` wrap them into a complete vhost for you.

**For Apache**, enable the full set of modules the snippet uses **before** you reload — note that `a2enmod proxy_fcgi` does NOT auto-enable `proxy` or `proxy_http`; you have to name them all:

```bash
sudo a2enmod proxy proxy_fcgi proxy_http rewrite headers
sudo a2enconf php8.4-fpm
sudo systemctl reload apache2
```

**For nginx**, apt-installing `nginx-light` is sufficient — every directive the snippet uses ships in the base package. There is no `a2enmod` equivalent step; just paste the snippet and reload:

```bash
sudo systemctl reload nginx
```

A few more things worth checking before reloading either proxy:

- **`pdo_sqlite` MUST be loaded in the PHP-FPM build** that handles `/api/*.php`. On Debian this is `php8.4-sqlite3` (named in the Prerequisites apt one-liner above). Verify with `php-fpm8.4 -m | grep pdo_sqlite` — if it returns nothing, the API will 500 on every `/api/*` request.
- **If you symlink `/var/www/<your-site>/{api,viz}` to this repo's `api/` and `viz/` directories** (rather than copying them), Apache 2.4 requires `Options +FollowSymLinks` inside the relevant `<Directory>` block. The Apache example snippet explains this in its leading comments. nginx follows symlinks by default — no extra directive needed.
- **If the symlink target sits inside your home directory** (`/home/$USER/...`), make sure `www-data` can traverse it. Debian's default `/home/$USER` is mode `0700`; widen it to `0701` (`chmod o+x /home/$USER`) or move the files into a www-data-readable path. Mode `0701` keeps your files private from other users while letting daemons enter to follow symlinks. (Same constraint applies to both proxies — it's a filesystem-perm issue, not a proxy-config one.)

Reload the reverse proxy after editing the config (the `systemctl reload …` above does it).

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

A successful run of steps 4–6 above produces a `tracing` event stream in the journal. The example config ships `log.format = "json"`, so each event is one JSON object on its own line. Timestamps, byte counts, PIDs, and trace identifiers vary run-to-run; `<placeholder>` tokens mark the parts that change. The structure — event message, field names, level — is what to verify against.

```json
{"timestamp":"<timestamp>","level":"INFO","message":"configuration loaded","path":"/etc/php-tree-viz/collector.toml","bind":"127.0.0.1:8088","data_dir":"/var/lib/php-tree-viz","retention_days":30,"queue_capacity":256,"max_body_bytes":67108864,"log_level":"info","log_format":"json","target":"config"}
{"timestamp":"<timestamp>","level":"INFO","message":"listening","addr":"127.0.0.1:8088","target":"php_tree_viz_collector::http::server"}
{"timestamp":"<timestamp>","level":"INFO","message":"disk usage","data_dir_bytes":<N>,"trace_count":0,"threshold_pct":80,"over_threshold":false,"target":"php_tree_viz_collector::observability::disk_usage"}
{"timestamp":"<timestamp>","level":"INFO","message":"batch accepted","trace_key":"<32hex>","trace_id":"00000000-0000-0000-0000-000000000000","host":"<host>","pid":<pid>,"body_bytes":<N>,"dict_entries":<N>,"call_count":<N>,"nodes":<N>,"pending":0,"anomalies":0,"target":"php_tree_viz_collector::http::server"}
{"timestamp":"<timestamp>","level":"INFO","message":"trace finalized","trace_key":"<32hex>","pending_dq2":0,"cpu_snapshot_available":true,"target":"php_tree_viz_collector::finalize"}
```

View it directly with `sudo journalctl -u php-tree-viz-collector --output cat --no-pager`. Feed it through `jq` to filter — for example, `jq -r 'select(.message == "batch accepted") | .trace_key'` lists every accepted trace's key. The `disk usage` event repeats once per hour by default; adjust `[observability].disk_usage_tick_seconds` in the config for a snappier cadence during testing.

If you prefer the human-readable line layout for an interactive verification, flip `log.format` to `"text"` in `/etc/php-tree-viz/collector.toml` and `sudo systemctl restart php-tree-viz-collector`. The same events come out shaped like `<timestamp>  INFO target: <message> field=value …` on a single line per event — easier to eyeball, harder to filter by program.

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

Output format is `log.format`. Two shapes:

- **`json`** (the production default; what [Expected output](#expected-output) shows) — one JSON object per event, with every field flattened to the top level. journald reads this natively; `journalctl -u php-tree-viz-collector --output cat` prints one object per line; `jq` filters across them cleanly.
- **`text`** — `<timestamp>  INFO target: <message> field=value …` on a single line per event. Easier to eyeball, harder to filter by program. Flip `log.format = "text"` in the config and `sudo systemctl restart php-tree-viz-collector` to switch.

## Scripts

Operator and developer helper scripts live in [`bin/`](./bin/). They are POSIX-bash, resolve the repo root from their own location (so they work from any working directory), and carry `--help`/usage headers.

| Script | Purpose |
| --- | --- |
| `bin/install-debian.sh` | One-shot Debian-13 installer: apt packages, builds the collector, deploys config + systemd unit + reverse-proxy vhost, starts everything, runs a smoke test. |
| `bin/reset-data.sh` | Reset the data state to clean/empty for test loops — wipe all traces and rebuild an empty, correctly-permissioned data layout, without re-running the installer or the retention sweeper. |
| `bin/validate-proxy-configs.sh` | Syntax-check the Apache/nginx example fragments under `etc/` with the proxy's native configtest. Also run in CI. |

### `bin/install-debian.sh`

```bash
sudo bash bin/install-debian.sh <public-hostname> [data-dir-owner] [--proxy=apache|nginx]
```

Idempotent — re-running preserves secrets, skips installed packages, and overwrites the vhost from the tracked template. Debian-13-only; the [Quickstart](#quickstart) above documents the equivalent manual steps for other distros.

### `bin/reset-data.sh`

```bash
sudo bin/reset-data.sh [--config PATH] [--no-restart] [--yes]
```

For fast test/eval loops on limited hardware: instead of re-running the installer, this resets only the data. It stops the collector, deletes the known artifacts under `[storage].data_dir` (`index.sqlite{,-wal,-shm}`, `traces/`, `tmp/`), recreates the empty layout `2770` setgid preserving the data dir's existing owner:group, then restarts the collector so it rebuilds a fresh empty `index.sqlite`. It does not touch the binary, config, vhost, or unit, and does not invoke the retention sweeper.

- Deletes only the named artifacts — never `rm -rf` the data dir itself.
- Refuses system paths, relative/missing `data_dir`, and (unless `--yes`) non-interactive runs.
- `--no-restart` skips all `systemctl` calls — use it when you run the collector manually rather than via systemd.
- Reads `[storage].data_dir` from `$PHPTV_CONFIG` or `/etc/php-tree-viz/collector.toml`; override with `--config`.

### `bin/validate-proxy-configs.sh`

```bash
bash bin/validate-proxy-configs.sh [--proxy=apache|nginx|both]
```

Wraps the `etc/*-example.conf` fragments in the `etc/*-validate.conf.in` scaffolding and runs `apache2ctl -t` / `nginx -t`. Exits `0` iff the selected configtest(s) pass. Needs `apache2` and/or `nginx` on `PATH` for the chosen proxy.

> Test-suite smoke runners (`tests/api/smoke/*.sh`) intentionally stay with the tests — they are coupled to the test layout, not operator tooling.

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
