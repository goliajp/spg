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

## Expected post-fix

| Window   | Warm median | p95   | Cold first iter |
| -------- | ----------- | ----- | --------------- |
| Post-fix | ≤ 10 ms     | ≤ 15 ms | ≤ 100 ms        |

These are the numbers pinned in `fixture.json.expected`.

## Snapshot

| Field   | Value                                                            |
| ------- | ---------------------------------------------------------------- |
| size    | 246 MB                                                           |
| sha256  | `f0ad88ba16b11ec2f7b53a0e50da59e20099d0ed472d061edb6bc80c195a609e` |
| origin  | mailrs prod 2026-06-22 incident                                  |

The tarball is gitignored. Fetch from the URL in `fixture.json` (or
out-of-band) and drop at `snapshot.tar.gz` to enable this fixture.
