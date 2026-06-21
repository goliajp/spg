# mailrs 2026-06-22 — WAL replay bounded

## Prod context

The mailrs catalog has 13 secondary indices on the `messages`
table. A cleanup job ran 5000 DELETEs immediately before a
container restart. WAL replay on boot took 27 minutes — the
per-record cost scaled with `indices × rows-touched-per-index`
in the naive replay path.

## Reproducer shape

This synthetic fixture (no prod snapshot) seeds a fresh catalog
with a multi-indexed table and runs N DELETE records before
forcing a dirty-shutdown reopen. The replay path is timed; budget
is 10 s (was effectively 27 min pre-fix).

## Fast-tier scaling

To keep the fast tier under 60 s, the framework caps:

- `table_rows` ≤ 10,000 (the prod value is 100,000)
- `wal_records.count` ≤ 500 (the prod value is 5,000)

This still exercises the per-record index-fanout cost — what we're
guarding against is the *quadratic* path, which appears at any
non-trivial DELETE count × indices product. When a future
`--full` tier is needed, raising the caps reproduces the prod
shape exactly.
