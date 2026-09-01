# spg → sentori — 7.39.7, MySQL's own DROP INDEX, and what a missing table says

**Image:** `goliakk/spg:7.39.7`
**Manifest digest:** `sha256:2e1db3294d072088cc5ec5b189add00d94c724f2c32feeecf5dbf11c8e3dc74c`
**Battery:** `gate.sh all` — lint, unit, e2e (twice, once under each
collation), gates, the corpus twice, dogfood, and the release-blocking
comparison against PostgreSQL 18.6 — then drop-in acceptance against
the pushed image: **71 of 71 cases pass**. The PostgreSQL comparison:
**sixty-four cells, no losses**.

Two items, both on the MySQL wire, both found by running the same
statements against MySQL 9.7.2 and against the image we had just
published.

## `DROP INDEX i ON t` was a syntax error

That is how MySQL drops an index. We had the two dialects exactly
backwards:

```text
                                  MySQL 9.7.2       spg 7.39.6
  DROP INDEX ix ON ci                works       syntax error 1064
  ALTER TABLE ci DROP INDEX ix       works            works
  DROP INDEX ix                   syntax error       accepted
```

So a migration that drops an index failed against the drop-in and not
against the thing it replaces — and a script written against SPG would
have failed on the MySQL it claims to be.

MySQL keys an index name inside its table, which is why its statement
names one; PostgreSQL keys it in the schema and has no `ON` clause at
all. Each dialect now takes its own form and refuses the other's. The
`ALTER TABLE t DROP INDEX i` spelling, which both accept, now scopes to
the table `ALTER` named — before this it could drop an index of the
same name belonging to a different table.

The failure modes are MySQL's too, measured rather than assumed:

```text
  DROP INDEX ix ON c2        (ix is on c1)   1091 Can't DROP 'ix'; …
  DROP INDEX nosuch ON c1                    1091 Can't DROP 'nosuch'; …
  DROP INDEX IF EXISTS ix ON c1              1064 syntax error
  DROP INDEX ix ON nosuchtable               1146 Table '…' doesn't exist
```

The 1091s used to be 1064 — a syntax error, which is what an unmapped
failure falls back to and is wrong about what happened.

**What to check.** Any migration or tooling that drops an index by
MySQL's syntax. If you have been working around this with `ALTER TABLE
… DROP INDEX`, that keeps working and is now scoped correctly.

## A missing table said what PostgreSQL says

The number has been right since 7.39 — 1146, which is what a client
branches on — but the sentence was PostgreSQL's, on every statement:

```text
                            MySQL 9.7.2                        spg 7.39.6
  SELECT * FROM nosuch   Table 'bench.nosuch' doesn't exist   relation "nosuch" does not exist
  INSERT INTO nosuch …   Table 'bench.nosuch' doesn't exist   relation "nosuch" does not exist
  ALTER TABLE nosuch …   Table 'bench.nosuch' doesn't exist   relation "nosuch" does not exist
```

It now says what MySQL says, prefixed with the database, which is the
name `DATABASE()` already answered on this connection.

**What to check.** Anything that matches on the text of that error
rather than on 1146 — log scrapers, retry predicates, test fixtures. If
you match on the number, nothing changes for you.

## What we would like from you

Nothing to run. If you have index-dropping migrations, this version is
the whole of it.
