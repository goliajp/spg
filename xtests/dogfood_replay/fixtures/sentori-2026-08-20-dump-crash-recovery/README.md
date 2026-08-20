# sentori — a real dump, killed mid-write

Filed 2026-08-20. sentori asked for exactly this, twice, and then sent
the data for it: their own database, dumped after their 87-step suite
had written it, `kill -9` against the `events` insert path, and the
directory reopened and checked row-for-row.

## What it holds

`pg_dump --no-owner --no-privileges` from PostgreSQL 18 — 27 tables, 40
indexes (btree, partial, GIN `jsonb_path_ops`, one BRIN), 66 constraints,
one user-defined function, ~66,000 rows. The two large tables were grown
by cloning rows the suite had written, keeping every foreign key and
check satisfied and spreading timestamps over 90 days so the BRIN index
has more than one block range to have an opinion about.

Stored gzipped (2.4 MB) and committed, rather than fetched out-of-band
like the mailrs snapshots. A gate that is skipped by default is not a
gate, and this one is small enough to keep.

## What it checks, and why each is the shape it is

**The dump restores first.** Row counts are compared to the fixture's own
record before the crash test may start. A fixture whose data did not load
reports agreement on emptiness — this repository has made that mistake
twice, in the divergence harness and again in a describe differ, and both
times the output looked exactly like a finding.

**The child dies by `SIGKILL`, not by abort or panic.** Those run handlers
a power cut does not. The child prints how many writes were acknowledged
and flushes before killing itself, so the number is a contract: every
write the client was told had committed must be there afterwards. A clean
exit fails the fixture — it would mean the kill never landed and
everything after it measured an orderly shutdown.

**The index probes compare two answers from the same database.** Each
predicate is asked once so an index can serve it and once phrased so none
can, and the two must agree. There is no expectation file to go stale,
and the check catches an index that came back from the unclean stop
subtly WRONG, which a row count cannot. sentori named the two they would
least expect a from-scratch engine to rebuild identically — the GIN
`jsonb_path_ops` and the BRIN — and both are probes here.
