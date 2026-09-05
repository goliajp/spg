# What the MySQL corpus found — v7.40.0, MySQL 9.7.2

Every line quoted here is a differing line in `out/*.diff`. `-` is
MySQL 9.7.2, `+` is SPG. Classified per `RUNNER.md`.

The first run scored **85** differing lines across thirteen files, with
three of them already identical. What `baseline.tsv` records now is
**2**, with twelve identical — and those two lines are the single
difference this version keeps as a DECISION (D1 below).

Four "findings" along the way were the harness's own, and each is
written up in `RUNNER.md` rather than left to be rediscovered: a
missing `--force` turned one missing function into fifteen lines of
truncation; an unnamed connection charset made `CHAR_LENGTH('日本')`
disagree; two merged streams raced an error against its own marker row;
and two corpus statements ordered by a marker literal, which ties every
row and leaves the order unspecified on both engines.

One run graded a STALE BINARY — `rsync -a` preserves mtime, cargo
skipped the rebuild, and nine and five differing lines were recorded as
a baseline before the cause was known. The runner asks the SPG leg for
`spg_version()` now and refuses to score when it is not this tree's.

---

## Fixed in v7.40.0 — the first pass

### F1 — a composite PRIMARY KEY was two fabricated indexes on the MySQL surface

`CREATE TABLE mc_i (a INT, b VARCHAR(32), c INT, PRIMARY KEY (a,b), KEY kc (c))`

```text
                    MySQL 9.7.2          SPG 7.39.13
  index_name        PRIMARY (both rows)  mc_i_a_pkey_0_0
                                         mc_i_b_pkey_0_1
  seq_in_index      1, 2                 1, 1
  non_unique        0                    1
```

`information_schema.statistics` and `SHOW INDEX` each walked
`t.indices()` themselves, so a key SPG stores as two single-column
indexes reached the reader as two unrelated indexes, both claiming not
to be unique, and nothing said `PRIMARY`. That is the v7.39.10 defect
(`SHOW INDEX` said the primary key was not unique) and the v7.39.12 one
(composite keys missing on 20 of 27 tables) arriving through the
composite door.

Both surfaces read `catalog_indexes` now — the table the PostgreSQL
side already used, which knows how to tell a constraint's index from a
declared one and how to synthesise the row for a constraint SPG
enforces without one covering index.

### F2 — `SHOW CREATE TABLE` printed the internal indexes, and three spellings were wrong

```text
-  PRIMARY KEY (`a`,`b`),                  MySQL
-  KEY `kb` (`b`(4)),
+  PRIMARY KEY (`a`, `b`),                 SPG 7.39.13
+  KEY `mc_i_a_pkey_0_0` (`a`),
+  KEY `mc_i_b_pkey_0_1` (`b`),
-) … DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci
+) … DEFAULT CHARSET=utf8mb4
```

Four things, all measured on 9.7.2: a KEY's column list has no space
after the comma while a FOREIGN KEY's does (MySQL's own
inconsistency); a `UNIQUE KEY` carries its name; a foreign key is
always written `CONSTRAINT <name> FOREIGN KEY …`, generated as
`<table>_ibfk_<n>` when the user named none; and the table options end
with the collation.

The internal-index test compared a one-element slice against the
constraint's column list, so a composite key's backing indexes matched
nothing and were printed as ordinary `KEY` lines.

### F3 — PostgreSQL sentences on the MySQL wire

```text
-ERROR 1062 (23000): Duplicate entry '1-x' for key 'dk.PRIMARY'
+ERROR 1062 (23000): duplicate key value violates unique constraint
                     "dk_pkey" on table "dk" DETAIL: Key (a, b)=(1, x)…
-ERROR 1050 (42S01): Table 'mc_t' already exists
+ERROR 1050 (42S01): relation "mc_t" already exists
-ERROR 1051 (42S02): Unknown table '<db>.no_such_table_here'
+ERROR 1146 (42S02): table "no_such_table_here" does not exist
-ERROR 1292 (22007): Incorrect datetime value: '2020-99-99' for column 'made' at row 1
+ERROR 1064 (42000): date/time field value out of range: "2020-99-99"
+HINT:  Perhaps you need a different "DateStyle" setting.
```

v7.39 (round 429) gave each failure MySQL's errno and stopped there;
the words stayed PostgreSQL's. The last pair also shows a PostgreSQL
`HINT:` line inside a MySQL error packet, naming a PostgreSQL GUC —
every message on this wire loses that tail now, so a future error that
grows one cannot leak it either.

`DROP TABLE` on a missing table also had the wrong number: MySQL uses
1051 for the DROP and 1146 for a read, both at 42S02.

Pinned in `e2e_mysqlwire_messages_v7400`.

### F4 — `ISNULL()` did not exist

`ISNULL(expr)` is not `IS NULL` spelled as a call: it takes one
argument and returns an integer, which is what a query written against
MySQL compares and sums.

### F5 — `AVG` returned PostgreSQL's scale

```text
-T02	0	3	6	1.5000                  MySQL: argument scale + 4
+T02	0	3	6	1.5000000000000000      PostgreSQL's display scale
```

Measured on 9.7.2: `AVG` over an integer has four decimals, over
`DECIMAL(10,2)` six. SPG answered PostgreSQL's sixteen on the MySQL
wire. The scale is now chosen by dialect.

---

## Also fixed in v7.40.0, after the first pass

The first pass fixed five and recorded fourteen. The instruction that
followed was `不允许有任何形式的 defer`, so the fourteen were worked
through too. Each is measured against MySQL 9.7.2; the two at the end
are recorded as DECISIONS rather than defects, with the measurement.

### F6 — a declared index disappeared when the column already had one

```text
  CREATE TABLE t (a INT, b VARCHAR(32), PRIMARY KEY (a,b), KEY kb (b(4)))
    SHOW INDEX FROM t     MySQL  lists kb, Sub_part 4    SPG  not listed
    DROP INDEX kb ON t    MySQL  succeeds                SPG  ERROR 1091
```

Two losses on one declaration: the composite key had already put a
probe B-tree on `b` and the inline installer skipped any column that
already carried one, so `kb` was swallowed and its name never
registered; and the `(4)` was parsed and thrown away. The catalog
records the prefix now (`FILE_VERSION` 99).

A prefixed **UNIQUE** key is a different constraint — MySQL rejects two
rows sharing the first four characters — and is enforced as a unique
expression index over `left(b, 4)`. Measured: the same insert is
refused with the same sentence, `Duplicate entry 'abcd' for key
'u.uq'`.

### F7 — `COLLATE utf8mb4_bin` stopped comparing trailing spaces

```text
                                        MySQL 9.7.2   SPG 7.39.13
  'a ' = 'a' COLLATE utf8mb4_bin             1             0
  'a ' = 'a' COLLATE utf8mb4_0900_bin        0             0
```

Byte-wise is two properties and SPG carried one: a `_bin` collation
does not fold case AND it pads, while the `BINARY` cast the parser
lowered it onto carries both.

### F8 — `ANY_VALUE` is not an aggregate on MySQL

`SELECT any_value(x) FROM t` is one row on PostgreSQL 18.6 and one row
per input row on MySQL 9.7.2. Both measured.

### F9 — `information_schema.table_constraints` answered the wrong engine

PostgreSQL 18.6 lists a NOT NULL column as a CHECK constraint and names
the key `<table>_pkey`; MySQL has neither. The view answers the engine
the session speaks now.

### F10 — `CREATE TABLE b LIKE a`, and what a copy carries

The MySQL spelling was a syntax error. And SPG's `LIKE` copied the
plain indexes and left the PRIMARY KEY behind, on both spellings —
measured, both engines copy it.

### F11 — `AUTO_INCREMENT` on `BIGINT UNSIGNED`

Refused with "applies to integer columns only", about a column that is
one: SPG models the type as a scale-0 NUMERIC because its range does
not fit an i64.

### F12 — six scalar answers

`-1e308` (`operator does not exist: - numeric`, **wrong on both
faces**), `TIME('<datetime>')` (an error; both engines extract the time
— also both faces), `CAST('2020-99-99' AS DATE)` (raised where MySQL
answers NULL), `0b101` (read as 5, not as the binary string), and the
missing `COERCIBILITY()` and `STD()`. `JSON_PRETTY` indented four
spaces where MySQL indents two.

### F13 — `WITH ROLLUP`, `STD` / `VARIANCE` and the UNION errno

The rollup total was NULL; the statistical aggregates were missing
`STD` entirely, and MySQL answers them as a DOUBLE (`1.25`) where
PostgreSQL answers a NUMERIC (`1.2500000000000000`); a `UNION` arity
mismatch carried 1064 — a PARSE error, which is not what happened —
where MySQL uses 1222 / 21000.

### F14 — `Incorrect datetime value`, with its column and row

The number and SQLSTATE were right and the sentence was PostgreSQL's.
The column and the row number are known only at the INSERT site, which
is where the sentence is written now.

---

## Decisions, not defects

### D1 — `->` and `->>` accept a literal left operand

MySQL requires a column and raises a syntax error; SPG answers NULL.
SPG is the more permissive of the two, and the same shape of superset
as bare `ARRAY[]` answering `text[]` where PostgreSQL refuses. Making
SPG refuse a form that works today would be a regression for anything
relying on it.

### D2 — `0o17` is an octal literal

SPG and PostgreSQL 18.6 read it as 15; MySQL 9.7.2 has no such literal
and answers `Unknown column '0o17' in 'field list'`. Matching MySQL
here would mean removing a PostgreSQL form.
