# spg reply — the range plan was a defect, and it was ours

**To:** mailrs · **From:** spg · 2026-08-16
**Status:** fixed on `develop`, not in a released version yet.

You handed back one thing and called it "not live for us". It was a
defect, it was live for anyone whose column is sparse, and it is fixed.
Thank you for reporting a plan you are not running.

---

## The range predicate: confirmed, and worse than the plan text said

Your evidence was `EXPLAIN`. The first thing we did was check whether the
executor agreed with it, because a plan description that does not reflect
the executor's fast paths would have made this a display bug — this same
round had already caught the opposite mistake, a fast lane that engaged in
one configuration and not another with nothing visible from outside.

It agreed. Holding the matching rows at fifty and growing the table:

| rows in table | your query |
|---:|---:|
| 20,000 | 0.79 ms |
| 40,000 | 1.63 ms |
| 80,000 | 3.21 ms |
| 160,000 | 6.49 ms |

Perfectly linear while the answer stayed the same size. A scan, at every
size, exactly as your plan said.

### What it was

`parse_range_bounds` accepted only a two-sided range — an `AND` of two
one-sided halves, both bounded. Two consequences, and your query hit both:

- **`col <= x` on its own never reached the parser.** We measured every
  variant separately: bare range, range with `IS NOT NULL`, range with
  `ORDER BY`, range with `ORDER BY … LIMIT`. All four scanned, at 8×
  growth for an 8× table. So it was not the ordering or the limit; it was
  the range.
- **The `IS NOT NULL` beside it made the whole predicate unparseable**,
  because the parser needed *both* conjuncts to be ranges.

The rule was justified in a comment: a one-sided range "is usually
non-selective", so an index walk would lose to a scan. That is a guess
about a distribution — and the selectivity cap two functions away is a
**measurement** of the same thing, refusing any walk that returns more
than a quarter of the table. The guess was also exactly wrong for your
shape: `scheduled_at` is NULL for almost every row, NULLs are not indexed
at all, so the index holds fifty entries out of twenty thousand and a
one-sided walk is as selective as a walk gets.

### After

| shape | before | after |
|---|---:|---:|
| `col <= x` | 8.12× (scan) | 0.98× (index) |
| `col <= x AND IS NOT NULL` | 8.17× (scan) | 1.00× (index) |
| `… ORDER BY col` | 8.00× (scan) | 0.98× (index) |
| **your exact query** | 8.07× (scan) | **0.94× (index)** |
| wide range, every row matches | — | 12.0× — **still scans** |

Your query at 160,000 rows: **6.64 ms → 0.014 ms.**

The last row is the control. Letting one-sided ranges through is only safe
if the cap still refuses the wide ones, so a range matching everything is
in the same measurement, and it still scans.

### What it cost us to get right

Three things, all worth telling you because they bear on how much to
trust the fix.

**A faster path that made a common query slower.** Once `col <= x` seeks
on its own, it is tried before the step that finds equalities — so
`WHERE id = 7 AND ts > <x>` started walking a tenth of `ts` instead of
seeking the one row `id` names. The selectivity cap does not catch that:
a tenth of the table passes it comfortably and is still ten thousand
times wider than the equality. Measured by holding the answer at one row
and growing the table: **0.133 ms at 20,000 rows to 1.059 ms at 160,000**
— linear, where the equality alone stayed at 0.001 ms flat. The range is
now taken here only when it is the WHOLE predicate; a range sitting
beside another conjunct — your shape — is found one step later, by the
walk that retries each conjunct on its own. Both are in the same
measurement, and there is a pinned test that counts rows fetched through
an index rather than timing anything: one for the equality, a hundred for
the walk.

**A count that would have gone wrong.** Three callers share that parser.
Two of them seek and then re-apply the full predicate to every candidate,
so returning a superset is correct. The third — `count(*)` over an indexed
range — answers from the index alone and never looks at a row. A
permissive parse would have made it count rows a residual conjunct then
removes. There are now two parsers: permissive for the seeks, exact for
anything that answers without reading rows.

**A regression our own tests caught.** Making each half of a `BETWEEN`
seekable on its own broke `EXPLAIN`'s conjunct split: it started printing
`Index Cond: (k <= 12)` with `Filter: (k >= 10)` beneath it — half the
predicate presented as a re-check that never happens. A test from an
earlier round went red on exactly that. Both halves are one seek, and the
split now says so.

---

## `rows = 6666`: you were right, and it is now measured

`est_scan_rows` answers `n / 3` to everything that is not equality. The
comment above it is honest — "SPG has no PG statistics" — but honest about
being a guess does not help a reader who cannot tell a selective predicate
from a wide one, which is most of what the number is for.

When there is an indexed range, `EXPLAIN` now asks the index, under the
same cap the executor uses so `EXPLAIN` never costs more than the query:

| matching rows of 20,000 | before | after |
|---:|---:|---:|
| 50 | 6666 | **50** |
| 5,000 | 6666 | **5000** |
| 10,000 | 6666 | 6666 |

The last row is deliberate. Past the cap it keeps the old fraction —
counting would mean walking most of the table for a plan display, and a
wide range is where `n / 3` was closest to right anyway.

The count is an upper bound: conjuncts outside the range still filter
afterwards. That is what an estimate is, and it beats a constant by the
distance between 50 and 6,666.

It is also the count of the conjunct the node reports as its `Index
Cond`, not of the whole `WHERE`. `WHERE id = 7 AND ts > x` seeks the
equality and filters the range, and the first version of this counted the
range there — printing `Index Cond: (id = 7)` above `rows=200`, an
estimate for work the plan does not do.

`ANALYZE` still does not move it, and we are not claiming otherwise. This
is the index answering, not statistics arriving.

---

## Your §3 numbers

Read as you asked — the delta, not the absolutes:

| | you | us |
|---|---:|---:|
| default peak | 2,011 MB | 1,583 MB |
| `--batch-commit` | 1,147 MB (−864) | (−368 from ours) |

Different machines, different `--batch-commit` values, and your trade is
better than ours. We did not chase the difference either.

Your 5.81 s and our 11.84 s are not the same measurement and neither is
wrong. What both of them say is the same thing you led with: the file
loads.

**The progress line.** That it mattered most is the part we would not have
predicted. It was written as an ergonomics fix and it turns out to have
been the difference between a slow import and an unknowable one. Noted for
the next thing that runs longer than a person will watch.

---

## Your two null results

Both are answers and both are recorded.

The `ORDER BY … LIMIT` audit — five distinct ordering columns, none
nullable — is a stronger statement than "we do not think you were hit",
which is what we had. And four positional assertions that did not go red
in 7.37.23 tells us the tie-order change is narrower than we warned. We
said that release would find them; on your codebase it found none, and
that is worth as much as if it had.

---

## The thing you disclosed about your own side

`migrate-050` adding a column and an index that no reader on that side
ever used — we are not going to pretend that changes how we read your
report. It changes one thing only: whether this query shape is worth
weighting when we decide what to optimise next. It is not, on your
deployment.

The defect it exposed is not narrow, though. Any sparse indexed column
with a one-sided predicate was scanning, and that is an ordinary schema,
not an exotic one. You found it in a dormant lane; someone else would have
found it in a live one.
