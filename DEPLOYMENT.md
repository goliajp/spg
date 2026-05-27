# SPG deployment

## Install

### From source

```bash
git clone https://github.com/<...>/lab22-spg.git
cd lab22-spg
cargo build --release -p spg-server
# binary at target/release/spg-server (or $CARGO_TARGET_DIR/release/spg-server)
```

Requires Rust 1.95.0 (`rust-toolchain.toml` pins it).

### From release artifact

CI produces a Linux x86_64 binary on every push to main
(`.github/workflows/ci.yml` → `release build` job). Grab it from
the workflow's `spg-server-linux-amd64` artifact.

## Minimal run

```bash
spg-server 127.0.0.1:5544 /var/lib/spg/db /var/lib/spg/audit /var/lib/spg/wal
```

Positional args: `<addr> [db_path] [audit_path] [wal_path]`.
Pass `-` to skip any positional after the first.

## File layout

| path                       | purpose                                       | grows? |
|----------------------------|-----------------------------------------------|--------|
| `db_path` (e.g. `db`)      | catalog snapshot. With WAL on: written at restart-checkpoint time only. With WAL off: rewritten atomically after every successful DDL/DML. | bounded by data size |
| `audit_path` (e.g. `audit`)| BLAKE3 hash-chain audit log (append-only)     | unbounded — rotate via operator policy |
| `wal_path` (e.g. `wal`)    | write-ahead log (length-prefixed SQL, fsync'd)| unbounded — checkpoint to truncate (manual: `cp db db.new && rm wal && rename`) |

Recommend a single dedicated directory per node; SPG never
modifies anything outside the three explicit paths.

## Environment variables

| env var                       | purpose                                              | default       |
|-------------------------------|------------------------------------------------------|---------------|
| `SPG_ADDR`                    | native listener bind                                 | `127.0.0.1:5544` |
| `SPG_DB` / `SPG_AUDIT` / `SPG_WAL` | path fallbacks for the three positional args   | unset         |
| `SPG_PG_ADDR`                 | PostgreSQL-wire listener bind                        | disabled      |
| `SPG_HTTP_ADDR`               | `/healthz` + `/metrics` HTTP listener bind           | disabled      |
| `SPG_REPL_ADDR`               | replication listener bind (this node is a primary)   | disabled      |
| `SPG_FOLLOW_OF`               | replication primary to follow (this node is a follower) | disabled   |
| `SPG_PASSWORD`                | legacy single-password AUTH (Redis style)            | open mode     |
| `SPG_ADMIN_PASSWORD`          | bootstrap admin user on first run                    | open mode     |
| `SPG_ADMIN_USER`              | admin username for bootstrap                         | `admin`       |
| `SPG_MAX_CONNECTIONS`         | concurrent client connection cap                     | unlimited     |
| `SPG_MAX_QUERY_ROWS`          | per-SELECT row count cap                             | unlimited     |
| `SPG_QUERY_TIMEOUT_MS`        | per-query wall-clock budget                          | unlimited     |
| `SPG_IDLE_TIMEOUT_SEC`        | connection idle close                                | unlimited     |
| `SPG_LOG_FORMAT`              | `json` to switch stderr to single-line JSON          | text          |
| `SPG_REPLAY_UPTO`             | PITR: cap WAL replay at byte offset N                | unset         |
| `SPG_FAIL_WAL_QUOTA_BYTES`    | chaos knob: refuse WAL append past N bytes (test only) | unset       |
| `SPG_SHUTDOWN_DEADLINE_SEC`   | v4.33: SIGTERM/SIGINT drain budget before `exit(0)`  | `30`          |
| `SPG_SLOW_QUERY_LOG_MS`       | v4.33: log queries slower than N ms (one JSON line/stderr) | unset    |
| `SPG_WAL_MIN_FREE_BYTES`      | v4.33: refuse WAL append when volume free space < N bytes | unset      |

## Recommended host setup

- **Filesystem**: ext4 / APFS / ZFS — anything with reliable
  `fsync`. NFS is not recommended (silent fsync corner cases).
- **Disk**: SSD strongly preferred. WAL fsync per commit is the
  bottleneck on HDD.
- **CPU**: any 64-bit Rust target. ARM64 (Apple Silicon, AWS
  Graviton) tested in CI.
- **Memory**: 100 MiB resident is enough for small workloads;
  scale with table size (no buffer pool cap, everything lives in
  the catalog `Vec`s).
- **Open file descriptors**: bump `ulimit -n` to ≥ 4× expected
  concurrent connections.

## Ports

| port | bind env       | exposed to                            |
|------|----------------|---------------------------------------|
| 5544 | `SPG_ADDR`     | application clients (native wire)     |
| ?    | `SPG_PG_ADDR`  | psql / drivers (PG-wire)              |
| ?    | `SPG_HTTP_ADDR`| Prometheus scraper + k8s liveness     |
| ?    | `SPG_REPL_ADDR`| followers (binary handshake)          |

There is no admin port. Privileged operations are over the same
native or PG-wire port, gated by the RBAC `admin` role.

## Replication

See RUNBOOK §replication and `xtests/v4_24_repl_report.md` for
attach cost / lag numbers.

## Backup

See RESTORE_DRILL.md.

## Shutdown

`spg-server` exits cleanly on `SIGINT` / `SIGTERM` — closes the
listener, drains in-flight queries up to the OS shutdown
deadline, exits 0. **No explicit graceful-shutdown deadline knob
yet** (PROD_READY row 2.7); for now rely on `SPG_QUERY_TIMEOUT_MS`
to bound how long the longest in-flight query can hold up exit.

## Upgrades

- Patch / minor versions: stop server, replace binary, start
  with same db+wal+audit paths. WAL replay handles version skew
  within v4.x.
- Major versions (when v5 lands): see UPGRADE.md (does not exist
  yet — v5 hasn't been cut).

## Verifying a node is healthy

```bash
curl http://${SPG_HTTP_ADDR}/healthz
# {"status":"ok",...}
curl http://${SPG_HTTP_ADDR}/metrics | head
# spg_server_info{version="4.30.0",...} 1
# spg_connections_active 3
# ...
```
