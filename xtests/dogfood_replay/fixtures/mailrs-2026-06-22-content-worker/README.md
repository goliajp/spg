# mailrs 2026-06-22 — content_worker anti-join cold-tier read amplification

## Prod context

Filed by: `stables/mailrs/.claude/notes/spg-7.37.4-prod-deploy-2026-06-22.md`

The content_worker hot loop runs the SQL in `queries.sql` once per
attachment-eligible message. On the 825 MB cold-tier catalog the
walker degraded from "anti-join + index probe on `attachment_content`"
to "full table scan + per-row NOT EXISTS lookup", driving wall time
from the expected sub-10 ms range to 3–5 s warm with a ~19 s tail.

## Measured baseline

| Window  | Warm median | p95     | max     |
| ------- | ----------- | ------- | ------- |
| Pre-fix | 3–5 s       | 5.3 s   | ~19 s   |

## Budgets

`f8669eca` tightened these after attack #1 cut warm from 86 ms to 7.9 ms.
Generated from `fixture.json` and kept honest by
`every_query_fixture_readme_carries_the_budget_the_gate_reads`.

<!-- BUDGETS: generated from fixture.json — the gate reads the JSON, not this table -->
| Window | Budget |
| --- | --- |
| Cold (first iter) | ≤ 15 ms |
| Warm median (p50) | ≤ 12 ms |
| p95 | ≤ 14 ms |
<!-- /BUDGETS -->

## Snapshot

| Field   | Value                                                            |
| ------- | ---------------------------------------------------------------- |
| size    | 246 MB                                                           |
| sha256  | `f0ad88ba16b11ec2f7b53a0e50da59e20099d0ed472d061edb6bc80c195a609e` |
| origin  | mailrs prod 2026-06-22 incident                                  |

The tarball is gitignored. Fetch from the URL in `fixture.json` (or
out-of-band) and drop at `snapshot.tar.gz` to enable this fixture.
