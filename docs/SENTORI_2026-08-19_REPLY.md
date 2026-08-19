# Re: drop-in status — both open items are closed in 7.38.2

**To:** sentori · **Date:** 2026-08-19 · **Against:** your consolidated
status doc (2026-08-19, suite 39/86, two open on our side)

Both open items are fixed. The four batteries were re-run here on the
7.38.2 head before this was written; numbers below are from that run,
not from memory.

---

## 2.1 `DROP COLUMN` leaves its CHECK behind — fixed

Your six-line repro, verbatim, on 7.38.2:

```
CREATE TABLE ck4 (id int PRIMARY KEY, name text NOT NULL,
                  fp bytea CHECK (fp IS NULL OR octet_length(fp) = 32));
INSERT INTO ck4 (id, name) VALUES (1, 'a');     -- INSERT 0 1
ALTER TABLE ck4 DROP COLUMN fp;                 -- ALTER TABLE
INSERT INTO ck4 (id, name) VALUES (2, 'b');     -- INSERT 0 1   ← was ColumnNotFound
SELECT count(*) … contype='c';                  -- 0            ← orphan gone
```

The rule implemented is PG's: a constraint **involving** the column is
dropped with it, no CASCADE needed; a constraint that does not
reference the column survives with teeth. Differential-anchored against
PG18 for both halves, including the multi-column case — `CHECK (a < b)`
goes when either `a` or `b` goes, exactly as PG does it.

Two notes on the things that cost you time:

- The error naming a column that no longer exists cannot recur for this
  cause: there is no surviving constraint to evaluate.
- The unnamed orphan coming back as the table-level `device_tokens_check`
  was a **consequence** of the same bug, not a separate naming defect.
  Our synthesised name is `<table>_<column>_check` when the expression
  references exactly one column — but attributing it needs that column
  to still exist, and yours had been dropped, so the name fell back to
  the table-level form. On 7.38.2 your table's constraint reads
  `device_tokens_fp_check` for as long as the column is there, and is
  gone afterwards. We checked ours against PG18's naming on the same
  DDL (`nm4_fp_check` / `nm4_env_check` / `nm4_id_check`, and
  `<table>_check` only for a genuinely multi-column table constraint).

Your migration chain's single `DROP COLUMN` should now pass, and step 39
with it.

## 2.2 `xmax` does not exist — it does now, and your statement is unchanged

You asked which shape we would prefer. The answer is: **yours**. Both
call sites keep `RETURNING id, (xmax = 0) AS is_new` with no edit.

Anchored against PG18 on all four shapes:

| statement | `xmax` |
|---|---|
| plain `INSERT` | `0` → `is_new` true |
| `ON CONFLICT DO UPDATE`, the updated row | nonzero → `is_new` false |
| `UPDATE … RETURNING` (new tuple) | `0` |
| `DELETE … RETURNING` (old tuple) | nonzero |

A bare `RETURNING xmax` keeps PG's column name and numeric type. If a
table has a real column called `xmax`, that column wins — the same rule
our scan path already used.

One implementation fact you should have, since it decides whether this
is safe for you long-term: SPG's in-place MVCC does not keep a
deleting-transaction id on a live row header, so the value here is
**synthesised per statement path**. The nonzero value is our writer
version, not a PG txid — the numbers differ. `= 0` and `<> 0`, which is
what both of your call sites test, are exact. If you ever need to
compare `xmax` values to each other or to a txid, tell us first; that
would need a different mechanism and we would rather hear it than have
you discover it.

## The batteries, on the 7.38.2 head

| | result |
|---|---|
| `join-bugs.sql` | `1 1 1 1 1 1 1` |
| `bind/` | 14 of 14 ok (every type, binary Bind) |
| `describe/` | 0 statements described no columns |
| `divergence/` | 8 of 18 vs `postgres:18`, 18 of 18 agreeing vs `postgres:18-alpine` — we ran it ourselves once the image was published; same eight as your 7.38.1 numbers, unchanged and unsurprising (see below) |

## 5 — on the divergence instrument

The third row of your table is the finding, and it is a good one:
`postgres:18` and `postgres:18-alpine` differ from each other by the
same eight probes. That is worth more than the SPG comparison, because
it means the exposure was already inside your product — between your
own compose and a customer's own Postgres — before we were in the
picture. We would not have found it; you did, with an instrument you
built for a different purpose.

Two things we are taking from it on our side:

- The self-checks you describe — reading `datcollate` *and* measuring
  `'a' < 'A'`, and making `count(*)` the first probe so a corpus that
  did not load exits before printing agreement — are the same two
  failure modes we keep hitting in our own measurement work (an
  instrument that cannot express the defect; an empty result read as a
  zero). We have written both into our methodology notes with your
  case as the citation.
- `COLLATE "C"` working identically on both, so it can serve as a
  probe, is load-bearing for you now, so we stopped leaving it to
  chance: it is pinned in the corpus that gates every release, against
  measured PG18 answers for both an ordering and a text `BETWEEN` —
  the probe whose divergence changes *which rows* come back rather
  than only their order. If that ever stops matching, the release does
  not go out.

Our `COLLATION_RFC.md` stands as the accurate description of where we
are. What it does not yet say, and now will, is that the same
divergence exists between two builds of PostgreSQL itself.

To be explicit about what 7.38.2 does NOT change: we ran your
`divergence/run.sh` against the published `goliakk/spg:7.38.2` on both
oracles, and the numbers are exactly yours — 8 of 18 against
`postgres:18`, 18 of 18 agreeing against `postgres:18-alpine`. The
eight are the shapes §4 of the RFC enumerates, and the cause is that
our database default collation is C. The RFC's §5 recommendation —
thread a collation name from the parser into the catalog, wire an ICU
collator behind it, and build the index-rebuild path a collation change
demands — is a piece of work in its own right and is not in this
release. We would rather say that plainly than let a green battery in
§4 imply otherwise.

## 6 — the list you say is yours

Noted as yours, not requests. Two of them we do test on our side and
can hand you evidence for whenever it is useful, rather than have you
build it: crash recovery (kill -9 mid-write, WAL replay, verified every
release) and in-place upgrade of a populated database (our release gate
restores data directories captured by earlier versions and diffs the
answers). If you want those pointed at a Sentori-shaped database rather
than ours, send us a dump and we will run them against it.

Concurrency we would treat differently: our own concurrent-write work
this release changed the read-committed rebase substantially, and 23
`ON CONFLICT` sites with a 3–6 connection pool is exactly the shape it
affects. We would rather you did not take that on faith. If you write
the concurrent registration test you describe in §6, we will run it
here on every release alongside our own.

## 7 — next

Re-run the 86 against 7.38.2 and send us wherever it stops. If it stops
somewhere new, the same treatment: a repro this small is worth more
than a description, and yours have been unusually good ones.

---

**7.38.2 is published.**

* image `goliakk/spg:7.38.2` — manifest digest
  `sha256:5fccada7be1dcbb9df11f551a6116eb788bd24d890fefad82e5b6711e49a6266`
* crates.io: all 12 crates at `7.38.2`
* release gates on this build: drop-in acceptance panel 59/59, the
  constant-answer sweep 64 cells with 0 losses and 0 false differences
  from its control, sqllogictest 3024/0 (the two fixes above are in
  that corpus now and gate every future release), and the previous
  release's data directory re-opened by this binary and verified
  row-for-row.

One number from our own side, since your §6 names concurrency as the
thing you have not tested and we changed exactly that this release:
pgbench tpcb against PostgreSQL 18 in a container, interleaved on one
box, medians of five — 2.65x at 1 connection, 1.89x at 2, 1.70x at 4,
1.39x at 8. Two of those were losses when this release opened.
