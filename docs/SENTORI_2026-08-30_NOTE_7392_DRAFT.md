# spg → sentori — 7.39.2, and the schema a restart quietly rewrote

**Image:** `goliakk/spg:7.39.2`
**Manifest digest:** `sha256:8e0c86434655f60166f1dd9054e348404391af5eecfa1a21a81e1470f2ea44a1`
**Battery:** `gate.sh all` — lint, unit, e2e, gates, biz, dogfood — plus
the release-blocking comparison against PostgreSQL 18.6: **64 cells, no
losses**, worst sort ratio 1.30x against the 2.0x ceiling, with the locale
panel (16 cells) and the shipped-default panel (16 cells) also clean.
Then drop-in acceptance against the pushed image: **71 of 71**.

Twenty-six defects. Three of them are things you would feel, and the
first one has been in every version you have run.

## A restart rewrote your MySQL tables' declared types

This is the one to read first, and the one to check for.

Recovery replays the SQL text of what was written, and the engine doing
that replaying speaks PostgreSQL. So every MySQL-only marker a column
carries was parsed away on the way back in. Measured on 7.39.1, before
and after a bounce that replayed the WAL:

```
declared        before the restart      after
TINYINT         tinyint                 smallint
MEDIUMINT       mediumint               int
DATETIME(3)     truncates to .123       stores .123456
```

`SHOW CREATE TABLE`, `DESCRIBE` and `information_schema` all changed
their answer together, so **a `mysqldump` taken after a restart wrote a
different schema from the one you created** — and nothing said so.

**What to check:** if you took a dump after a restart and used it to
create a table elsewhere, compare the integer columns. A `TINYINT` that
came back `SMALLINT` still holds your data; a `DATETIME(3)` that came
back without its precision has been storing microseconds since that
bounce, where MySQL would have dropped them.

The WAL carries the dialect now, in a record type of its own. Nothing
changes for a PostgreSQL-only deployment — those records are written
exactly as before.

Underneath that one sat a second: SPG stored `TIMESTAMP` and `DATETIME`
as a single type and reported `datetime` for both. They are not the same
type on MySQL — a different range, and conversion to and from UTC — and
the declared spelling is recorded now, so a dump reproduces what you
wrote.

## The standard reflection query found none of your tables

`SELECT … FROM information_schema.tables WHERE table_schema = DATABASE()`
is how Django's introspection, Rails' schema dumper and every JDBC
browser ask what a database holds. SPG answered PostgreSQL's `public` in
that column, so the query matched nothing and reported an empty
database.

The three catalog views a reflection reads — `COLUMNS`, `TABLES`,
`SCHEMATA` — answer MySQL's shape and MySQL's values on a MySQL session
now. `COLUMNS` had six of MySQL's columns missing outright
(`CHARACTER_OCTET_LENGTH`, `COLUMN_KEY`, `EXTRA`, `PRIVILEGES`,
`COLUMN_COMMENT`, `SRS_ID`), and where the names did agree the values
did not: an `INT` reported precision 32 (bits) where MySQL reports 10
(decimal digits), and an `AUTO_INCREMENT` column's default read
`nextval('t_id_seq'::regclass)` — a PostgreSQL expression, which a
reflection copying it into MySQL DDL cannot replay.

`DESCRIBE` is in the same family: it answered three columns of SPG's own
where MySQL answers six, so a tool reading `row['Default']` or
`row['Extra']` found no such key.

## A typo in a WHERE clause was accepted, if the table was empty

```sql
SELECT a FROM t WHERE nosuch = 1
```

answered zero rows and no error while `t` was empty, and raised the
moment `t` had one row in it. PostgreSQL and MySQL both refuse it
whatever the row count.

The shape that hides: a query written against an empty fixture passes
its test, and a nightly job over an empty window reports nothing rather
than failing. It is refused before the scan now, in `WHERE`, `ORDER BY`,
`GROUP BY`, `HAVING` and `ON` alike.

## The rest

`DOUBLE(10,2)` — how a legacy MySQL schema spells money — failed the
whole `CREATE TABLE` with a syntax error. A MySQL `FLOAT` was widened to
eight bytes, so it kept precision MySQL discards, and printed with
PostgreSQL's digits. Wrong-argument-count calls answered SPG's own
sentence rather than either engine's, and on the MySQL wire carried the
wrong errno. `ONLY_FULL_GROUP_BY` refusals were worded as PostgreSQL's
and numbered 1055 where MySQL has three different numbers. An unknown
column did not say which clause it failed in. The full list is in the
CHANGELOG under 7.39.2.

## Still open, and measured

The wrong-argument-count check runs per row, so a call with the wrong
number of arguments over an **empty** table still answers zero rows and
no error — the same shape as the WHERE-clause defect above, in a
different place. Closing it needs a description of each built-in's
accepted argument counts that SPG does not yet keep; two ways of
deriving one were tried and both were wrong in ways that would have
refused valid queries.
