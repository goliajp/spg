# MySQL drop-in support — current state

> **Re-measured 2026-08-23 against v7.38.18.** This page said "partial
> as of v7.17.0 and a roadmap item for v7.18+" for twenty-one versions
> after that stopped being true. Every `partial` and `pending` cell in
> the table below was executed against the current build and **all ten
> now work**, and both items the "what needs to happen" section asked
> for shipped in v7.38.16 and v7.38.17. The old text is replaced rather
> than annotated, because a page a MySQL user reads to decide whether to
> evaluate SPG is not the place for a historical note.

## Current state — `mysqlwire` protocol (v7.17)

The v7.17 epic shipped a full MySQL 8 / MariaDB 11 wire
protocol shim:

- Handshake protocol — `caching_sha2_password` (the MySQL 8
  default, the fast-path SHA256 XOR proof) and
  `mysql_native_password` legacy auth.
- Query path — `COM_QUERY` simple query, `COM_STMT_PREPARE` /
  `COM_STMT_EXECUTE` / `COM_STMT_RESET` / `COM_STMT_CLOSE` for
  prepared statements, binary-format row data.
- Admin commands — `COM_PING`, `COM_QUIT`, `COM_INIT_DB`,
  `COM_STATISTICS`, `COM_FIELD_LIST`, the SHOW family
  (`SHOW DATABASES` / `SHOW CREATE TABLE` / `SHOW INDEXES` /
  `SHOW STATUS` / `SHOW VARIABLES` / `SHOW PROCESSLIST`).
- SSL upgrade via `CLIENT_SSL` capability bit.

Confirmed-working clients (each verified by an e2e test):

- `mysql` CLI 8.0 / 8.4
- `mariadb` CLI 11
- JDBC (`mysql-connector-j` / MariaDB Connector/J)
- Python `mysql.connector` / `pymysql`
- Go `go-sql-driver/mysql`
- Rust `sqlx` MySQL backend

Enable with the `SPG_MYSQLWIRE_ADDR` env var on the SPG
container:

```sh
docker run -d \
    -p 3306:3306 \
    -e SPG_MYSQLWIRE_ADDR=0.0.0.0:3306 \
    goliakk/spg:7.17.0
```

The image's PG-wire on `5432` and SPG-native on `5544` stay up
in parallel; one server speaks all three protocols.

## MySQL dialect coverage — measured v7.38.18

The wire shim accepts MySQL clients and the engine speaks the MySQL
dialect when the session is in it. Each row below was executed against
v7.38.18 on 2026-08-23.

| Area | Status | Notes |
|---|---|---|
| `CAST(x AS UNSIGNED INTEGER)` / `SIGNED` / `UNSIGNED` | ✅ | All three spellings (was `partial`) |
| `AUTO_INCREMENT` inline | ✅ | |
| `INSERT … ON DUPLICATE KEY UPDATE` | ✅ | The MySQL spelling, not only PG's `ON CONFLICT` (was `partial`) |
| `REPLACE INTO` | ✅ | (was `pending`) |
| `LIMIT N OFFSET M` / `LIMIT M, N` | ✅ | Both spellings |
| `IFNULL` / `IF()` / `DATE_FORMAT` | ✅ | `DATE_FORMAT('2026-08-23'::date, '%Y/%m/%d')` → `2026/08/23` (was `partial`, "spotty") |
| `mysqldump` output as-is | ✅ | The dump-compat gate runs MySQL and MariaDB dumps of four apps each on every release |
| Collation semantics (`utf8mb4_*`) | ✅ | v7.38.13–18: case folding, `PAD SPACE` vs `NO PAD`, and index keys that agree with the scan |
| `SHOW WARNINGS` / `SHOW COUNT(*) WARNINGS` / `@@warning_count` | ✅ | v7.38.17–18, including the diagnostics area's lifetime |

### What the previous version of this section asked for

All of it shipped:

1. **A MySQL fixture in `scripts/dropin-acceptance.sh`** — landed. The
   script runs a MySQL panel and a MariaDB panel, each with its own
   expectations, against a MySQL client image.
2. **A `--dialect mysql` panel** — landed in v7.38.17. Before it, the
   `corpus/mysql/` fixtures had been running in **PostgreSQL dialect**
   since they were created: the runner had no notion of a dialect, so
   those files asserted that MySQL *syntax* parses and nothing about
   MySQL *semantics*. A `dialect` directive and an axis registry now
   fail the run if the MySQL-semantics × index-present intersection is
   ever empty again.
3. **A MySQL acceptance customer** — still open, and the only one. It is
   a request for a real application, not an engineering task.

If you are a MySQL user evaluating SPG, file an issue with your schema —
`scripts/dropin-acceptance.sh --fixture your-schema.sql --dialect mysql`
produces a yes/no.

## Verify it yourself

```sh
# Start SPG with the mysql wire listener enabled.
docker run -d \
    --name spg-mysql-trial \
    -p 3306:3306 \
    -e SPG_MYSQLWIRE_ADDR=0.0.0.0:3306 \
    goliakk/spg:7.17.0

# Connect with any MySQL client.
mysql -h 127.0.0.1 -P 3306 -u spg -e "SHOW DATABASES"

# Run your app's init schema.
mysql -h 127.0.0.1 -P 3306 -u spg < your-init-schema.sql
```

## Migrating data: `spg import` takes stock mysqldump output

v7.22 — an **unmodified** `mysqldump` (or `mariadb-dump`) file,
schema AND data sections, loads straight into an embedded catalog:

```sh
mysqldump -u root -p mydb > dump.sql
spg import --db ./mydb.spgdb --file dump.sql
```

The data sections work as emitted: `/*!40000 …*/` executable
conditional comments (FOREIGN_KEY_CHECKS off, DISABLE/ENABLE KEYS),
`LOCK TABLES` wrappers, multi-row INSERT packing, and — the subtle
one — MySQL backslash string escapes (`\'`). SPG switches its
string-literal lexing per session on the dump's own `SET sql_mode`
preamble, so MySQL escapes and PG's literal-backslash semantics
coexist without flags. The dump-compat gate loads mysql:8.4 and
mariadb:11.4 fixtures (schema and with-data) through `spg import`
on every release.

Note: feeding mysqldump DATA through **psql** does not work — psql
itself splits statements with PG string rules and shreds
backslash-escaped INSERTs before they reach any server. That's a
transport property, not an SPG gap; use `spg import` or the mysql
wire listener.

If it loads, you're drop-in. If it doesn't, the harness will
soon (T11+ MySQL panel) tell you which clause SPG needs to
add — and a fixture PR keeps the coverage from regressing once
we ship it.
