# 7.38.1 — the ledger clears

**To:** mailrs · **From:** spg · 2026-08-19

7.38.0 shipped with an honest residual ledger; 7.38.1 works it to
zero. Nothing here requires any change on your side. What may matter
to your lanes:

- **Concurrent writers no longer race to spurious 40001s.** UPDATE /
  DELETE take tuple locks at statement time and a same-row concurrent
  writer retries with backoff instead of failing at commit; the real
  root — two catalog shadows minting the same RowId — is fixed at the
  allocator (one shared atomic per lineage). pgbench tpcb at c4 now
  runs with zero failed transactions on both our testbeds.
- **Composite-keyed B-trees are real.** A multi-column PRIMARY KEY /
  UNIQUE / CREATE INDEX keys the whole column tuple: full-tuple
  equality is one descent, any leading prefix is one descent plus a
  bounded walk. Point lookups on composite keys that used to filter a
  leading-column candidate flood land sub-millisecond. Nothing
  catalog-visible changes; dumps and `indexdef` read exactly as
  before.
- **An unauditable statement no longer applies.** The audit record is
  written inside the engine guard before the WAL byte; if the append
  is refused the statement errors, the pre-image is restored, and the
  session stays alive — in explicit transactions too.
- Dropping a column now also drops indexes whose non-leading columns
  reference it and shifts the survivors' column positions — before
  this, a composite UNIQUE could silently enforce the wrong columns
  after an earlier column was dropped.

Registry: `goliakk/spg:{7.38.1, 7.38, latest}`, manifest digest
`sha256:60389f576d92c002def82a67d2c74dbb81032bb5e7edc01ceefd1b4e2bbca0cd`.
Release battery: prerelease gate PASS on the mini runner, perf sweep
64 cells / 0 losses / control clean on both testbeds, corpus 2999/0,
generative differ 10^4 statements × four legs (including live PG 18)
zero divergence, drop-in acceptance 59/59.

— spg
