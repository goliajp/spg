# SPG — project notes for Claude sessions

## Competitor bench stack lives in docker-compose

PG / MySQL / MariaDB are NOT installed as host binaries. Don't go
looking for `psql` / `mysql` on PATH — they're not there and won't
be added. The competitor side of `xbench/competitor/` runs against
three loopback-only containers:

  xbench/competitor/docker-compose.yml
    postgres (pgvector/pgvector:pg18) → 127.0.0.1:25432
    mysql    (mysql:9)                → 127.0.0.1:23306
    mariadb  (mariadb:11)             → 127.0.0.1:23307

  All three use bench/bench credentials + bench database/schema.

Helper scripts:

  xbench/competitor/scripts/up.sh    # docker compose up -d + wait healthy
  xbench/competitor/scripts/down.sh  # docker compose down -v (drops data)

Connection strings are centralized in
`xbench/competitor/src/lib.rs::connection_strings()`. Bench
binaries (`latency.rs`, `throughput.rs`, `vector_knn.rs`,
`concurrent.rs`, `large_data.rs`, `memory.rs`, etc.) all read from
there — no port literals scattered around.

Before any competitor bench: check `docker ps --filter
name=spg-bench` for `(healthy)`. If down, run `up.sh`.

## Conformance corpus lives in xtests/sqllogictest

`xtests/sqllogictest/corpus/{duckdb,mysql,pg_regress,pgvector}/`
hold the 4-corpus .test files. `cargo run -q -p sqllogictest
--release` writes `xtests/sqllogictest/report.{md,json}`. The
4-corpus 100% pass rate is PROD_READY row 6.1.

## Cargo target dir lives on /Volumes/INTEL2T

Workspace cargo target is shared at
`/Volumes/INTEL2T/workspace-cache/cargo-target`. Don't drop your
own local `target/` here — read `~/.claude-shared/global/cargo-
target-dir.md` for the full story (in particular: never bare
`cargo clean` from this workspace — it would wipe every other
project's cache too; the wrapper auto-scopes to current workspace
members).
