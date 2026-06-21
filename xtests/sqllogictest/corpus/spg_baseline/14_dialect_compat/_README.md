# 14 — Dialect dump round-trip

pg_dump → SPG → SELECT byte-equal smokes for PG / MySQL / MariaDB.
These tests can't live in sqllogictest's in-process runner — they
need the actual `pg_dump` / `mysqldump` binaries.
The full harness for this category lives in:

- `xtests/dump_compat/run.sh` (PG-side)
- `scripts/dropin-acceptance.sh` (cross-dialect panel)

Files here would be smoke probes against a pre-restored fixture —
they're tracked as a future train. Until then, this directory is
intentionally empty.
