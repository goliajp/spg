# MariaDB drop-in support — current state + roadmap

MariaDB shares MySQL's wire protocol (with extensions);
SPG's `mysqlwire` shim accepts MariaDB clients out of the
box. The dialect surface is broadly the same as
[MYSQL_DROPIN.md](./MYSQL_DROPIN.md), with a few MariaDB-only
features called out here.

## Current state — wire protocol (v7.17)

Verified clients:

- `mariadb` CLI 11.x
- MariaDB Connector/J (JDBC)
- MariaDB Connector/Node.js
- Standard MariaDB-compatible drivers in Python (`mariadb`,
  `mysql.connector`) and Go (`go-sql-driver/mysql`).

Start the listener identically to MySQL mode:

```sh
docker run -d \
    -p 3306:3306 \
    -e SPG_MYSQLWIRE_ADDR=0.0.0.0:3306 \
    goliakk/spg:7.17.0
```

## MariaDB-specific dialect notes

MariaDB has a few extensions beyond MySQL that need separate
attention:

| Feature | Status | Notes |
|---|---|---|
| `SEQUENCE` objects + `NEXTVAL` / `LASTVAL` / `SETVAL` | ✅ | SPG ships PG-flavoured sequences (v7.17), MariaDB syntax accepted. |
| MariaDB JSON functions (`JSON_EXTRACT`, `JSON_VALUE`) | partial | PG JSONB operator surface (`->`, `->>`, `#>`, `@>`, `?`) covers most usage; the MariaDB function spellings need parser arms case-by-case. |
| `WITH RECURSIVE` CTE | ✅ | PG-shape implementation works. |
| MariaDB-specific storage engines (`Aria`, `InnoDB`, etc.) | pending | The wire-level statement is parsed; the engine-level storage choice is a no-op (SPG has one engine). MariaDB schema dumps that name an engine load cleanly. |
| `pg_dump` equivalent (`mariadb-dump`) output as-is | partial | v7.14.0+ accepts most preamble; specific edge cases (regex character classes inside `WHERE`, vendor-specific hint comments) need fixtures to find. |

## Roadmap

Same as MYSQL_DROPIN.md — when a real MariaDB customer brings
a schema, drop it into `scripts/fixtures/` as
`mariadb-<customer>-init.sql`, run
`scripts/dropin-acceptance.sh --fixture …` to find the gaps,
and patch SPG dialect coverage on the gaps that show up.
Regressions become release-blocking via the CI gate the same
way the mailrs PG fixture does today.

## Verify it yourself

```sh
docker run -d --name spg-mariadb-trial \
    -p 3306:3306 -e SPG_MYSQLWIRE_ADDR=0.0.0.0:3306 \
    goliakk/spg:7.17.0
mariadb -h 127.0.0.1 -P 3306 -u spg -e "SHOW DATABASES"
mariadb -h 127.0.0.1 -P 3306 -u spg < your-init-schema.sql
```

Same shape as the MySQL flow. File the schema as a fixture in
`scripts/fixtures/` and PR it — the moment it's there, SPG
treats it as load-bearing.
