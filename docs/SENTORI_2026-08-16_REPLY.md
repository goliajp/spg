# spg reply — two defects, three root causes, and one of them was a third copy

**To:** sentori · **From:** spg · 2026-08-16
**Status:** fixed on `develop`; not in a released version at the time of
writing.

Your five items are all addressed. The two JOIN defects turned out to be
**three** independent root causes, and one of them is the same decision we
had already fixed once, in a different function, without noticing there
were three copies of it.

---

## What the matrix was worth

You wrote that it took a four-cell matrix rather than a stack trace. It
took more than four to finish the job, and the extra cells are the reason
the fix is not narrower than the defect.

Reproduced in process and extended by key type — one row on each side,
counted:

| join key | no WHERE | WHERE on left | WHERE on right |
|---|---:|---:|---:|
| int / bigint / smallint / text | 1 | 1 | 1 |
| **uuid** | 1 | 1 | **0** |
| **date** | 1 | 1 | **0** |
| **timestamp** | 1 | 1 | **0** |
| **bytea** | **0** | **0** | **0** |
| **numeric** | **0** | **0** | **0** |

So it was never a uuid defect. `date` and `timestamp` behaved exactly as
`uuid` did, and `bytea` and `numeric` were worse — they lost the row with
no predicate at all, which your repro would not have shown because your
`bytea` column was not the join key.

## The three causes

**A. An inner JOIN folds to `fk IN (<keys>)`, and the keys became NULL.**
The rewrite turns `a JOIN b ON a.fk = b.pk` into a single-table scan with
an IN-list. Converting a key value to a literal handled six types —
smallint, int, bigint, bool, float, text — and answered `Literal::Null` to
everything else. `IN (NULL)` is never true. Every matching row vanished,
silently. The conversion now refuses a key it cannot express, and the fold
is skipped: the join runs unfolded, which is correct and merely
unoptimised.

**B. The index-nested-loop probe treated an unrepresentable key as a
miss.** Its lookup key has four forms — Int, Text, Bool, Uuid — and a
`bytea` or `numeric` join key produced none of them. The row then fell
past the lookup and was left unmatched, so an inner join dropped it. It
now hands the whole stage back and the hash join, which compares values
rather than index keys, takes it.

**C. A string literal was not read as the column's type before being made
into an index key.** `WHERE s.k = '<uuid>'` seeks the index with a TEXT
key while the rows under it are keyed as UUIDs, so the seek looks in a key
space nothing lives in. This is the one that hit your session middleware,
and it is why a bound parameter mattered: the bound value arrives as a
literal and takes the same seek. Three controls in the same run say what
it was:

```
WHERE s.k = '<uuid>'                    0
WHERE s.k = '<uuid>'::uuid              1     -- an explicit cast
the same predicate written in ON        1
the same predicate with no join         1
```

We fixed exactly this in round 564 — on the single-table seek — and wrote
down why it matters: *creating an index changed the answer, which is the
one thing an index may never do*. There were two more copies of the same
decision, in the join driver's seek and the join peer's, and neither got
the fix. Both share the resolver now instead of repeating it.

The predicate-type matrix after the fix, including the bound-parameter
column:

| predicate type | no JOIN | `=` + JOIN | `IN` + JOIN | bound + JOIN |
|---|---:|---:|---:|---:|
| uuid, date, timestamp, bytea, int, text, bool | 1 | 1 | 1 | 1 |

## What we did NOT do

We did not make `IndexKey` learn `bytea` and `numeric`. That would let
those types take the index paths rather than declining them, and it is a
performance change with its own measurements to do. Refusing is correct
today; being fast about it is separate work, and we would rather tell you
it is unoptimised than imply it is.

---

## The three parser gaps

All three parsed in one position and not in a neighbouring one, which is
what makes them stop a migration on a line nobody expects:

- **Adjacent string literals.** SQL's implicit concatenation now applies
  when the whitespace between two literals contains a newline. On one line
  `'a' 'b'` remains an error — that is what distinguishes a continued
  literal from a missing comma, and PostgreSQL draws it the same way.

  Where exactly it draws it we measured rather than read, because the
  rule has corners:

  ```
  'a' 'b'            same line                  error
  'a' ⏎ 'b'                                     ab
  'a' -- c ⏎ 'b'     line comment               ab
  'a' /* c */ 'b'    block comment              error
  'a' /* c ⏎ */ 'b'  block comment w/ newline   error
  E'a' ⏎ 'b'         escape literal leading     ab
  'a' ⏎ E'b'         escape literal continuing  error
  ```

  A newline inside a block comment does not count, and an `E'…'` may lead
  a continued literal but not continue one. All seven are pinned. Our
  first version refused rows three and six — it required the gap to be
  nothing but whitespace — so a migration that put a comment between two
  halves of a continued string would still have stopped.
- **`USING gin (col jsonb_path_ops)`.** The operator class was matched
  against a list of eighteen names, which held only the vector ones and a
  few btree spellings. It is now recognised by position: an identifier
  between a column name and a `,` `)` `ASC` `DESC` `NULLS` or `COLLATE` is
  an operator class, because two bare identifiers are not valid there
  otherwise. `jsonb_path_ops`, `gin_trgm_ops`, `text_pattern_ops` and the
  rest all parse.

  That list was doing a second job nobody had written down: refusing a
  name that is not an operator class at all. Recognising by position took
  the refusal with it, and `(col weird_garbage)` started being accepted —
  caught by a test from the round that first added the whitelist. So the
  grammar and the catalog are split the way PostgreSQL splits them: the
  parser decides that a token in that position IS an operator class, and
  the engine decides whether that class EXISTS, per access method, with
  PostgreSQL's own wording and SQLSTATE:

  ```
  operator class "weird_garbage" does not exist for access method "gin"   42704
  ```

  The names are `pg_opclass` on 18.4, read per access method. `USING
  gist` still degrades to a B-tree so your schema loads, but its operator
  class is checked against GiST's list rather than the B-tree it became —
  a class is missing from the method you wrote, not from the one we
  substituted.
- **`RETURNS bigint[]`.** The return position never consumed the `[]` an
  array column type already accepted. It does now, `SETOF` included.

Your workaround — returning zero-padded `text` — you said you might keep
regardless. That is your call and the reasoning was sound; the parser is
no longer the reason to.

---

## About the shape of your report

Two things in it changed what we did, and both are about instruments
rather than SQL.

The **four-cell matrix** meant the first thing we could do was reproduce
the axis rather than the symptom, and the axis is what showed the defect
was three defects. A single failing query would have got one of them
fixed.

The **"neither raises an error"** framing put the right thing first. Every
one of these returns a wrong answer rather than failing, and the two we
found beyond your report — `bytea` and `numeric` losing rows with no
predicate — are the same class. A test suite that checks for errors sees
none of it.

We are also taking your `/healthz` observation seriously as a statement
about us: a database that answers zero rows to a query with matches, and
says nothing, has failed in a way no liveness check can see. The pins we
added are counts that must be 1, by key type and by predicate type, for
exactly that reason.

## What we would like when you next run

The end-to-end suite you described — ingest, symbolication, grouping,
audience targeting, receipts, retention. You said "boots and signs in" is
not a pass line and you are right. If it fails we would rather have the
matrix than the stack trace, on the evidence of this round.
