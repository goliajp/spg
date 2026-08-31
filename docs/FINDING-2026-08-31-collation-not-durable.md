# A collation was not durable, so a restart reordered every text column

*2026-08-31, during v7.39.4. Measured on the published `goliakk/spg:7.39.3`
image, not on a build.*

## What it looked like

Same container, same `SPG_LC_COLLATE=en_US.utf8`, same table, same query,
either side of a `docker restart`:

```text
before   a,A,b,B          the collator's order
after    A,B,a,b          byte order
log      spg-server: database collation "C"
```

No error, no warning, no row moved. What changed is the order rows come
back in.

## Why

A database's collation lives in the catalog. A catalog reaches disk only
at a checkpoint, so every crash and every plain `kill` starts from an
EMPTY catalog and rebuilds itself from the WAL — and the WAL says nothing
about a collation.

v7.38.19 moved `apply_database_collation` to run AFTER replay, on
purpose, and its comment records the defect it closed: a database created
under `C`, restarted with `SPG_LC_COLLATE=en_US.utf8`, used to come back
declaring the locale and answering in a different order. Running after
replay means the guard sees the tables and refuses.

That guard is right. It is also, from where it stands, unable to tell two
situations apart:

* a database created under `C` being asked to become a locale — refuse
* a database created under a locale asking to be itself again — allow

Both look identical after a WAL-only restart, because in neither case does
the catalog hold a collation. Refusing is the safe answer to the first and
the wrong answer to the second, and the second is what every locale
database hits on every bounce.

Discriminating measurement: with NO tables created, the collation survives
the restart (replay puts nothing in the catalog, so the guard does not
fire). With one table, it does not. An explicit `CHECKPOINT` before the
restart did not help either — the `db` file was not written at all; the
directory held only `wal`, `audit` and `wal.cluster_id`.

## The fix

`<wal>.collation`, the same sidecar shape as `.cluster_id` and for the
same reason: a fact about this database that has to survive a restart
that never checkpointed. Written when a collation is established, read
BEFORE replay, so the catalog carries the database's own collation into
recovery and the guard then compares like with like.

`C` writes no file — it is the default, and a second place to record it
is a second place for the two to disagree.

## Pins

`e2e_collation_survives_restart` now holds both directions:

* `an_existing_database_keeps_its_collation` (v7.38.19) — a `C` database
  is not upgraded by an environment variable
* `a_locale_database_keeps_its_collation_across_a_restart` (v7.39.4) — a
  locale database comes back as itself

Ablation (removing the sidecar read) reddens exactly the second and
leaves the first alone.

## How it was found

Not by a gate. It surfaced while chasing an unrelated question — one
cell of the perf sweep's locale panel — where two containers of the
7.39.3 image were being used as control legs. `docker restart` was used
between panels, and the restarted leg reported `database collation "C"`
while its environment still said `en_US.utf8`.

The lesson is the one this whole version is about: **the shipped
configuration is where these live.** Nothing in the suite restarts a
server that collates, because nothing in the suite collates —
`proclib` declares `SPG_LC_COLLATE=C` for every server it starts, at
all six spawn sites, deliberately (v7.38.19: every fixture was authored
under `C`, and inheriting the machine's locale had already produced a
panel that compared `en_US` against itself).

That default is right for the fixtures that exist. What is missing is a
second panel beside it, the way the sqllogictest corpus now has one.
The pin added here does not depend on the default — it names
`en_US.utf8` itself, because a pin for a collation defect that inherits
a byte-ordering default cannot see the defect. Three versions have now
been caught by that same rule.

**Closed in v7.39.5, and the sentence above was wrong about how.**

The wire panel did not run under `C`. It ran under whatever the person
typing the command had in their shell. `proclib` declares
`SPG_LC_COLLATE=C` for the servers IT starts — the sweep legs, the dump
round-trip, the released-directory open — but the spg-server test
harness is a different one: it clears three variables and inherits the
rest. Both machines here export `LANG=en_US.UTF-8`, so the panel had
been ordering text by a locale, silently, while every fixture in it was
authored under `C`; a CI runner with `LANG` unset was running a
different panel from the one anybody had looked at. Reading the
declaration in one harness and writing it down as if it were the
other's is what put the wrong sentence here.

Measured before changing anything: 734 wire tests under
`SPG_LC_COLLATE=C`, then all 734 under `en_US.utf8` — green both ways,
no fixture disagreeing. So the `skipif`-per-fixture judgement the
sentence above anticipated was not needed at all. What was needed was
the opposite: something that CAN tell the panels apart. A second panel
no fixture can distinguish is theatre.

v7.39.5 therefore does three things. `ServerBuilder` DECLARES the
collation (`C` by default, the same reasoning `proclib` wrote down),
with `SPG_E2E_DB_COLLATION` to switch the panel. `gate.sh e2e` runs the
whole spg-server surface a second time under `en_US.utf8`. And
`e2e_panel_collation_v7395` asks for an ordering the two collations
answer differently — `Bob,Zebra,apple` by bytes, `apple,Bob,Zebra` by
`en_US` — and checks the server's own `pg_database.datcollate` first,
so a declaration that fails to reach the child reds rather than being
judged against a panel it is not in. Removing the declaration reds it
with exactly that sentence.
