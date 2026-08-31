# The corpus had never run under the collation the product ships with

*2026-08-31, during v7.39.4.*

## What was true

`docker image inspect goliakk/spg:7.39.3` carries `LANG=en_US.utf8`, and
the server says so on the way up:

```text
spg-server: database collation "en_US.utf8"
```

Every record of the sqllogictest corpus — 3,889 of them — ran on a
catalog that orders by **bytes**. The suite's servers declare
`SPG_LC_COLLATE=C` in `proclib`, deliberately and with a comment
explaining why; the in-process runner never set a collation at all. The
only leg anywhere in the suite that used the shipped locale was the perf
sweep's second panel, which measures **time**, not answers.

So a defect that requires the database to name a collation could not
appear in the correctness gates no matter how many records were added.
That is how the ordering half of 7.39.3's session-collation fix shipped
past nine green gate steps, and it is the third time a collation defect
has reached a release this way.

## What the second panel found on its first run

Running the same corpus with the database collated as the image collates
it gave **13 failures out of 3,889**. Two of them were defects, and
neither was about ordering:

1. **A `UNIQUE` text column admitted a duplicate.**

   ```text
   CREATE TABLE t (id INT NOT NULL, code TEXT NOT NULL UNIQUE);
   INSERT INTO t VALUES (1, 'a');
   INSERT INTO t VALUES (2, 'a');   -- accepted
   SELECT code FROM t;              -- a, a
   ```

   The constraint descends the column's B-tree when one discriminates.
   A locale-collated tree is keyed by ICU sort keys the engine supplies
   and is empty until a refresh fills it, so a lookup by the RAW value
   answers "no locators" — which the chooser reads as the most selective
   candidate. It takes it, and the conflicting row is never compared.

   `uc_probe_choice` now declines a collated tree and folds the table,
   which is the path a byte-ordering database has been taking all along.

2. **Every GIN over a text column answered nothing.**

   MySQL `MATCH … AGAINST` over a `FULLTEXT KEY`, and `LIKE '%…%'` over
   a `gin_trgm_ops` index, both returned the empty set. `index_collation_in`
   asked the column's type and the database's collation without asking
   what KIND of index it was, so a GIN — whose keys are lexemes and
   trigrams the engine derives, in a different keyspace with a different
   key type — was reported as taking a supplied key and routed into the
   ICU path.

   It now answers `None` for anything that is not a plain B-tree, which
   is the same exclusion already made for expression and composite
   indexes one screen above.

The other eleven records were four assertions of byte order in files
that are about collation, and seven that fell out once the GIN
classification was fixed. The four say `skipif spg-collated`.

## What is in place now

- `SPG_SLT_DB_COLLATION` installs a database collation in the in-process
  runner. Unset is the byte-ordering default the corpus was authored
  under.
- `gate.sh biz` runs the corpus twice, and the precommit `slt-smoke`
  step runs its subset twice.
- `skipif spg-collated` marks a record that asserts byte order. Four do.
  Everything else in those files runs in both panels.

## The other half, still open

The **server** e2e tests and every harness that spawns a server still
run under `SPG_LC_COLLATE=C`. That default is deliberate and every
fixture was authored under it; a second panel there is a bigger piece of
work than this one, because those harnesses assert wire output rather
than answers. What is closed here is the ENGINE's answers. The pins for
the collation defects themselves set the shipped collation directly, so
they do not depend on either default.

## A separate thing the panel work uncovered

`PRECOMMIT.list`'s header says "all of 15_regressions". There are two
directories by that name and it enumerated one: **forty-three fixtures,
every regression written since v7.38.13 including v7.39.4's own, had
never run at precommit.** The header also said the tier warned on
strays; nothing read the file except the runner. The runner now fails on
a `15_regressions` fixture missing from the list, and the subset went
from 508 records to 1,116.
