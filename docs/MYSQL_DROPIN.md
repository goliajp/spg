# MySQL drop-in support — current state + roadmap

SPG's MySQL drop-in story is **partial as of v7.17.0** and a
roadmap item for v7.18+. This page lays out the current state
honestly so MySQL users know what works and what's pending.

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

## What's pending — full MySQL dialect coverage

The wire shim accepts MySQL clients, but the engine's SQL
dialect is PG-flavoured. MySQL-specific surface that has
divergent semantics needs case-by-case work:

| Area | Status | Notes |
|---|---|---|
| Type casts (`CAST(x AS UNSIGNED INTEGER)`) | partial | PG `::TYPE` form + `CAST(x AS type)` SQL standard form covered; some MySQL-only spellings (`UNSIGNED INTEGER`, `SIGNED`) pending. |
| MySQL `AUTO_INCREMENT` inline | ✅ | Maps to SPG's `BIGSERIAL` PK path. |
| MySQL `INSERT ... ON DUPLICATE KEY UPDATE` | partial | PG `INSERT ... ON CONFLICT (col) DO UPDATE SET` covers the semantic; the MySQL spelling needs a parser arm. |
| MySQL `REPLACE INTO` | pending | maps to PG `DELETE` + `INSERT`. |
| MySQL `LIMIT N OFFSET M` / `LIMIT M, N` two-arg form | ✅ | both spellings accepted. |
| MySQL specific functions (`IFNULL` / `IF()` / `DATE_FORMAT`) | partial | some covered as aliases for PG `COALESCE` / `CASE` / `to_char`; spotty. |
| `mysqldump` output as-is | partial | v7.14.0+ accepts the preamble (`SET FOREIGN_KEY_CHECKS=0`, conditional comments, etc.). DDL coverage matches the type table above. |

## What needs to happen

1. **MySQL fixture in `scripts/dropin-acceptance.sh`** — same
   shape as the mailrs PG fixture: a real MySQL app's
   `init.sql` checked in under `scripts/fixtures/`, the
   `--fixture FILE` option already supports it.
2. **`--dialect mysql` panel in the harness** — MySQL-specific
   probe cases (the table above's "pending" cells), so a
   regression is caught in CI.
3. **MySQL acceptance customer** — analog to mailrs for PG. We
   need a real MySQL application to be the first regression
   target.

If you're a MySQL user evaluating SPG, file an issue with your
schema — we'll point `scripts/dropin-acceptance.sh --fixture
your-schema.sql --dialect mysql` at it and produce a yes/no.

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
