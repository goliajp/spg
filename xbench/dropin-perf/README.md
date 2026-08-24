# dropin-perf — measure a drop-in candidate on *your* statements

Two files and a profile directory. Copy the whole `dropin-perf/` folder
anywhere; it does not read anything outside itself.

```sh
PROFILE=profiles/sentori ./run.sh goliakk/spg:7.38.18 postgres:18-alpine
```

Needs `docker`, `psql` and `awk`, plus the POSIX text tools (`grep`,
`sed`, `sort`, `head`, `tail`, `printf`). No checkout of ours.

The first version of this line said "three commands"; listing what the
script actually calls found ten. `uptime` was one of them, is not on
every container's PATH, and printed context rather than a measurement —
it now degrades to a note instead of failing the run.

## What it refuses to do

The comparison a vendor's own benchmark makes — this build against the
vendor's previous build — measures whether the vendor is moving. It says
nothing about whether you should switch. So this harness only knows how
to compare a **candidate image against the image your compose ships
today**, on **your** statements, on **your** box.

- Both databases run as containers here, started the same way, seeded by
  the same SQL, **in the same run**.
- The legs **interleave at the cell level** and the starting leg rotates
  every round. Running all of A and then all of B puts one leg always
  second while the machine drifts underneath it; we have twice had that
  bias invent a result, once in each direction.
- Every run carries a **control leg** — the reference image served a
  second time from a second container. Cells where the control differs
  from the reference are that run's own noise, and **no verdict in the
  table is worth more than that count**.
- Overlapping ranges print `unresolved`, never "a small win". A
  disjoint-but-adjacent pair prints its gap, because a 0.4 % separation
  and a 40 % one should not read the same.

## Writing a profile

Three files in `profiles/<name>/`:

| file | what it is |
|---|---|
| `schema.sql` | DDL and seed. Must be valid on **both** engines. |
| `shapes.tsv` | `name<TAB>SQL`, one single statement per line. |
| `rows` | `table<TAB>count`, asserted after seeding. |

`profiles/sentori/` is a worked example. Two things it gets right that
are easy to get wrong:

- **The seed's data distribution decides what you measure.** Its first
  version cycled timestamps so that every block range spanned nearly the
  whole window, which no BRIN index can prune — so the profile measured a
  shape in which the customer's own index does nothing.
- **Repeated values hide behind themselves.** A column of twenty-six
  distinct values repeated across 400,000 rows is decided by its first
  byte; a key that looks at a prefix scores spectacularly on it and tells
  you nothing. Include varied data, or know which of the two you are
  looking at.

## Reading the output

```
SHAPE                 candidate      reference      control        verdict     gap    control
ingest: one row       5.2-9.3        0.9-3.8        1.0-3.2        SLOWER      37.8%  clean
window: count over…   0.4-1.1        1.1-3.8        1.0-2.9        unresolved  -      clean
…
cells=8 candidate_slower=4 candidate_faster=1 control_false_differences=0
```

Read `control_false_differences` first. If it is not zero the box was too
noisy for the table above it, whatever the verdicts say.

Exit codes: `0` ran with a clean control, `1` ran but the control found a
false difference, `2` the harness could not measure at all (an image
would not boot, a seed landed the wrong row count, a profile is missing).
`2` is deliberately unreachable from the same path as a clean run.

## A note on collation, added in v7.38.19

The row order a database gives depends on the collation it was created
under, and that is decided by the image's environment, not by the engine.
`postgres:18` starts a database under `en_US.utf8`; `postgres:18-alpine`
declares the same and sorts by bytes anyway, because musl carries no
locale data; `goliakk/spg` starts under `C` unless `LANG`, `LC_ALL` or
`SPG_LC_COLLATE` says otherwise.

If you are comparing sort-heavy shapes, **check what each leg is actually
serving** —

```sql
SELECT datcollate FROM pg_database;
```

— because two legs under different collations are not doing the same
work, and the difference will read as a performance result. We found this
in our own release gate, where the baseline leg had been inheriting the
testbed's `LANG` for months while every comment about it said `C`.
