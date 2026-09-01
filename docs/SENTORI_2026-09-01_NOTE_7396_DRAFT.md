# spg → sentori — 7.39.6, two indexes that were charging you for nothing

**Image:** `goliakk/spg:7.39.6`
**Manifest digest:** `sha256:0fa8677fed60bb800ad9025cdb4183489f52ac51ee1c05f0d5bb64e0e2672e5f`
**Battery:** `gate.sh all` — lint, unit, e2e (now run twice, once under
each collation), gates, the corpus twice, dogfood, and the
release-blocking comparison against PostgreSQL 18.6 — then drop-in
acceptance against the pushed image: **71 of 71 cases pass**. The
PostgreSQL comparison: **sixty-four cells, no losses**.

Nothing in this version changes an answer. Both items below are the
same shape: an index that took its cost on every write and gave nothing
back on reads, so no test that checks answers could ever have seen it.
We found them by asking a different question — for each kind of index,
does the same query get faster when the index is there? — against the
image you pull, on the configuration it ships.

If you have added either kind of index to an existing table, you were
paying for it and not being served by it.

## A BRIN index added to a table that already had rows

`CREATE INDEX … USING brin (col)` summarises the column so a scan can
skip ranges that cannot match. Those summaries were only ever built as
rows were INSERTED, so an index created afterwards — which is how an
index is normally added — had none, and an index with no summaries
skips nothing. It stayed that way: a `CHECKPOINT` did not build them,
and neither did a restart.

Measured on 7.39.5, 200,000 rows, a predicate matching none of them:

```text
SELECT count(*) FROM t WHERE v > 999999999

                                    7.39.5    7.39.6     PG 18.6
  no index                          7.55 ms   6.53 ms    4.57 ms
  BRIN, created after the rows      7.53 ms   0.21 ms    0.35 ms
  a tail predicate, 1000 rows       7.48 ms   0.30 ms    1.20 ms
```

**What to check.** If you have BRIN indexes, they now work; nothing is
needed from you. If you dropped one because it did not seem to help,
that is why.

## A composite index over a text column, on the collation we ship

`CREATE INDEX … (a, b)` with `a` a text column: on a database that
orders text by a locale — which every published image does,
`LANG=en_US.utf8` — the planner declined to use it. Every write paid
for it and no read used it.

```text
SELECT count(*) FROM c WHERE a = 'k00012345' AND b = 45

                                    7.39.5    7.39.6     PG 18.6
  no index                          7.53 ms   5.33 ms    4.59 ms
  with (a, b)                       7.49 ms   0.21 ms    0.39 ms
```

The reason is worth one paragraph, because it explains why this only
ever showed up on a collated database. A single-column B-tree over a
collated text column stores ICU sort keys, so its probes are encoded
the same way. A composite index does not: it keys on the raw cell. The
probe was being encoded as a sort key anyway, so it looked for
something the tree does not contain and found nothing — and the code
avoided the wrong answer by refusing to use the index at all. It now
builds the probe the way the entries were built. Range scans over such
an index still decline, and must: an interval in collated order is not
an interval in byte order.

**What to check.** Composite indexes whose FIRST column is text, on a
database that collates. `SELECT datcollate FROM pg_database WHERE
datname = current_database()` tells you which you are on; anything but
`C` is affected. Nothing is needed from you beyond taking the version —
but if you removed such an index for not earning its keep, it does now.

## Also in this version

The wire test panel declared no collation, so it ran under whatever
`LANG` the machine had. It declares one now, and the whole panel runs
twice — once by bytes, once under the collation the image ships. That
found nothing, which is the honest thing to report about it; it means
the next collation defect has somewhere to be caught.

## What we would like from you

Nothing to run this time. If either index kind is in your schema, the
version is the whole of it.
