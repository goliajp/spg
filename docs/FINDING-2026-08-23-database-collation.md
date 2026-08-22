# The database collation is `C`, and nothing can say otherwise

*Measured 2026-08-23 against PostgreSQL 18.4 (`spg-bench-postgres`).*

## What was measured

`CREATE TABLE cd(x text COLLATE "en_US.utf8", y text)`, four values,
`ORDER BY` each column.

| | `x` (declared `en_US.utf8`) | `y` (undeclared) |
|---|---|---|
| PostgreSQL 18.4 | `apple client DateStyle Zebra` | `apple client DateStyle Zebra` |
| SPG | `apple client DateStyle Zebra` | `DateStyle Zebra apple client` |

The declared column agrees exactly — SPG carries a full ICU collator
calibrated against PostgreSQL over all 880 of its collation names, and
`<`, `min()` and `information_schema.columns` all agree with the sort.

The undeclared column does not agree, and the reason is not a defect in
the collator. That oracle's database collates as `en_US.utf8`, which is
the stock Debian default; SPG's collates as `C`:

```
SELECT datname, datcollate FROM pg_database   ->  spg | C
CREATE DATABASE d2 LC_COLLATE 'en_US.utf8'    ->  accepted, creates nothing
```

`pg_database.datcollate` is fixed at `C` and the `CREATE DATABASE` form
that would change it is accepted and ignored. So a deployer cannot say
"my database collates as en_US.utf8", and every text column that does
not carry its own `COLLATE` clause sorts by bytes.

## Why it matters

A customer moving off a stock PostgreSQL gets a different row order from
every `ORDER BY <text>`, every `min`/`max` over text, and every range
comparison on an undeclared column — silently, with no error and no
warning. `Zebra` before `apple` instead of after it. That is the
zero-customer-change line, and it is the widest single divergence found
in this file's series.

It is also invisible to the release gates. The differential corpus
compares SPG against that oracle, and the two differ on exactly this
(`15-catalog` T12, `ORDER BY 1` over three parameter names) — the
difference has been sitting in `baseline.tsv` as an unexplained count
since the file was written.

## Closed the same day

`docs/DESIGN-2026-08-23-collation.md` has the decision and the build
order. The short version: **PostgreSQL does not allow a live database's
collation to change**, which removes the hazard the three options below
were weighed against, and makes the answer a fourth thing — set it at
creation from the environment, persist it, never move it.

Absent on disk means `C`, so every database written by every earlier
version keeps exactly the answers it had and rebuilds nothing.

The three options below are left as written, because the reasoning that
rejected them is the reasoning that found the fourth.

## Why it was not fixed in the same breath

Changing the default changes **index key order on disk**. Every B-tree
in every existing database was built byte-ordered; a database that
starts collating as `en_US.utf8` would read them with the wrong
comparator, and the answers would be wrong rather than merely
differently ordered. Closing this means one of:

1. **Make it declarable, default unchanged.** `CREATE DATABASE ...
   LC_COLLATE` starts meaning something, and a database created that way
   collates that way from the first row. Existing databases keep `C` and
   keep their indexes. Additive, and it does not help a customer who has
   already restored into an SPG database.

2. **Make it settable on an existing database**, which requires
   rebuilding every text index at the point of the change — the same
   work PostgreSQL's own `REINDEX` after a collation-version change
   does, and the same failure mode if it is skipped.

3. **Change the default to match a stock PostgreSQL**, which is the
   answer that makes a drop-in behave like the thing it drops into, and
   the one that invalidates every index in every database written by
   an earlier version unless the upgrade rebuilds them.

The three differ in what they do to data already on disk, so the choice
is not mine to make quietly. This file exists so the choice is made
against measurements rather than found by a customer.

## What WAS fixed alongside it

- The dump did not emit `COLLATE` at all, so even a column that
  correctly declared `en_US.utf8` came back byte-ordered after a
  dump/restore. Emitted now; confirmed by ablation. Pinned in
  `15_regressions/v73818_collation_survives_dump.test`.
- Two statements in the source had gone false. `synth_pg_collation`'s
  comment said "column-level COLLATE clauses parse but don't alter sort
  order" and "v7.37.x doesn't yet support per-locale ICU collations";
  the parser's error said "SPG orders text by bytes (the C collation);
  locale collations are not supported yet". The table above is what the
  build actually does.
- The parser's message also gave a nonexistent collation name the same
  answer as a real one. PostgreSQL says `collation "x" for encoding
  "UTF8" does not exist`; SPG cannot tell the two apart, because the
  parser has no collator to ask. Recorded, not fixed.

## Still open

- `pg_collation` lists 3 rows where PostgreSQL lists 880, so a client
  that looks a collation up by name is told it does not exist while the
  engine performs it. Same disagreement `pg_settings` had before
  v7.38.18. PostgreSQL's set is its host's locales, so listing that
  container's 880 would be claiming SPG has exactly those; naming what
  this build can perform needs a source for the candidate names.
- `COLLATE` on an arbitrary expression (`'a' < 'B' COLLATE "en_US.utf8"`)
  is refused. It works on a column declaration and in an `ORDER BY` key;
  carrying it on an expression needs an `Expr::Collate` node and every
  walker taught about it.
