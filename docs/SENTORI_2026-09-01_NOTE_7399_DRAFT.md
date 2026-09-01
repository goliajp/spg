# spg → sentori — 7.39.9, a dropped DEFAULT that came back, and eleven MySQL spellings

**Image:** `goliakk/spg:7.39.9`
**Manifest digest:** `sha256:a6e80e156763da25a0634e90359babe2b95507a1d3dfb2475636469a70323951`
**Battery:** `gate.sh all` — lint, unit, e2e (twice, once under each
collation), gates, the corpus twice, dogfood, and the release-blocking
comparison against PostgreSQL 18.6 — then drop-in acceptance against
the pushed image: **71 of 71 cases pass**. The PostgreSQL comparison:
**sixty-four cells, no losses**.

## A dropped DEFAULT came back when the dump was restored

This is the one to read. It is PostgreSQL's own spelling, it is in
every version you have run, and it changes what a restore gives you.

```sql
CREATE TABLE t (id INT PRIMARY KEY, b INT NOT NULL DEFAULT 5);
ALTER TABLE t ALTER COLUMN b DROP DEFAULT;
```

After that statement the column has no default — inserts behave
correctly, and always did. But the *source text* the catalog views and
the dump read was written once, when the table was created, and nothing
updated it:

```text
  the catalog                       no default
  information_schema.columns        column_default = '5'
  pg_attrdef                        '5'
  a dump of the database            b integer NOT NULL DEFAULT 5,
```

So a dump taken after that ALTER **restores the default you removed**.
`SET DEFAULT` had the mirror problem: it reported and dumped the value
it had replaced.

**What to check.** If you have ever dropped or changed a column default
and have dumps taken afterwards, those dumps carry the old default.
Restoring one gives a schema that differs from the database it came
from. This query lists the columns where the two disagree, on any
version:

```sql
SELECT c.table_name, c.column_name, c.column_default
FROM information_schema.columns c
WHERE c.column_default IS NOT NULL
ORDER BY 1, 2;
```

Compare that against what your migrations say those columns should be.
On 7.39.9 the two agree by construction; on an older image the view is
the one that can be wrong. Tell us what you find and we will work
through it with you.

## Eleven MySQL spellings that were syntax errors

We swept thirty-seven statements a MySQL migration writes, one at a
time, against MySQL 9.7.2 beside the image we had just published.
Eleven of them MySQL accepts and SPG answered `1064 syntax error` for:

```text
  ALTER TABLE t MODIFY COLUMN b BIGINT
  ALTER TABLE t CHANGE COLUMN b bb BIGINT
  ALTER TABLE t ADD COLUMN d INT AFTER a
  ALTER TABLE t ADD COLUMN e INT FIRST
  ALTER TABLE t AUTO_INCREMENT = 100
  ALTER TABLE t ENGINE = InnoDB
  ALTER TABLE t CONVERT TO CHARACTER SET utf8mb4
  ALTER TABLE t RENAME INDEX old TO new
  RENAME TABLE a TO b, c TO d
  ANALYZE TABLE t
  SELECT STRAIGHT_JOIN … FROM t
```

All eleven work now, and on the published image the thirty-seven shapes
agree with MySQL 9.7.2 on every one.

Three of them are worth a sentence each, because they are not only
about parsing:

- **`MODIFY` and `CHANGE` replace the definition.** On MySQL a column
  declared `INT NOT NULL DEFAULT 5` is `bigint`, nullable, with no
  default after `MODIFY COLUMN b BIGINT` — restating them keeps them.
  Accepting the statement but only changing the type would have kept a
  `NOT NULL` your migration asked to lift.
- **`AFTER` and `FIRST` really move the column.** `SELECT *` reads
  columns in order, so putting the column at the end instead would be a
  different answer, not a smaller feature.
- **`STRAIGHT_JOIN` was being read as a column name**, so `SELECT
  STRAIGHT_JOIN a FROM t` answered `Unknown column 'straight_join'`.
  It is a join-order hint; SPG plans its own joins, so it is accepted
  and not acted on.

`ENGINE` and `CONVERT TO CHARACTER SET` accept what SPG can honour and
refuse what it cannot, with MySQL's own numbers — a typo in a migration
must not quietly become SPG's storage or SPG's encoding.

## What we would like from you

The default query above, run against your schema. Everything else in
this note needs nothing from you but the version.
