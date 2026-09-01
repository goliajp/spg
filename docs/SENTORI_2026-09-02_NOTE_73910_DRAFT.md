# spg → sentori — 7.39.10, SHOW INDEX said your primary key was not unique

**Image:** `goliakk/spg:7.39.10`
**Manifest digest:** `sha256:9dbc5df537ce543f3ca76221bc07dfcdc8f2419be17667a622e0fbfbe309916c`
**Drop-in acceptance against the pushed image:** 71 of 71 cases pass.

Small version, one thing worth your attention if any tooling reads
`SHOW INDEX`.

## `SHOW INDEX` reported the primary key wrong, three ways

Measured against MySQL 9.7.2 on the image we had just published, same
table, same client:

```text
                             MySQL 9.7.2      spg 7.39.9
  Key_name of the PK           PRIMARY          f1_pkey
  Non_unique of the PK           0                1
  columns returned              15                7
```

- **`Non_unique = 1` on a primary key** is a wrong value, not a
  spelling. Anything that reads that column concludes the key is not
  unique. It came from the index's own flag, and SPG records the
  primary key's uniqueness on the table's constraint, not on the index.
- **MySQL names every primary key `PRIMARY`.** Tooling that looks for
  that name found nothing on SPG, which called it `<table>_pkey`.
- **`SHOW INDEX` has a fixed fifteen-column shape** and clients read it
  by position. Seven columns is not a subset — it is a different
  result.

All three are fixed. The eight columns that were missing carry MySQL's
own values for a table it has not analysed, copied from a 9.7.2 run
rather than invented. A composite index now gets one row per column,
numbered from 1, where it had named only the leading column; and
`PRIMARY` is listed first, as MySQL lists it.

**What to check.** Anything that inspects indexes through `SHOW INDEX`
— schema-diff tools, ORM introspection, migration guards that assert a
key is unique. On 7.39.9 and earlier they saw a primary key that looked
non-unique and was named something MySQL never names it.

## Also

`ALTER TABLE t ENGINE=NoSuchEng` quoted `'nosucheng'` — a word your
migration does not contain, which is the one thing that message is for.
It now names the engine back exactly as written, the way `CREATE TABLE`
has since 7.39.3.

## What we would like from you

Nothing to run. If you have index-introspection tooling, this version
is the whole of it.
