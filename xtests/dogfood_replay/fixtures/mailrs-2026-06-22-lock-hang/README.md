# mailrs 2026-06-22 — lock-hang on dirty restart

## Prod context

`docker stop -t 10` on the mailrs SPG container delivered SIGKILL
mid-checkpoint. On restart the lock dir survived, and `open_path`
hung waiting for the (already dead) holder. Total boot delay
exceeded the orchestrator's readiness timeout, marking the pod
permanently unhealthy.

## Reproducer shape

This synthetic fixture (no prod snapshot required — uses a small
in-process catalog so it's safe on the fast tier) drives:

1. `open_path` on a fresh catalog.
2. Two writes to dirty the WAL.
3. `inject_kill_9_mid_checkpoint` — the framework drops the
   `Database` handle without a clean shutdown, leaving the lock
   dir behind.
4. `reopen_path` — must succeed within `total_recovery_ms_max`
   (5 s budget — was effectively unbounded pre-fix).

## Why synthetic

The lock-hang root cause is in the lock-dir clear path, which is
independent of catalog shape. A 100 MB prod snapshot doesn't
add signal — but it makes the fixture too slow for fast-tier CI.

When a prod-shape reproducer is needed (e.g. the WAL-replay path
*does* scale with catalog shape — see
`mailrs-2026-06-22-wal-replay-bounded`), the synthetic fixture
covers the lock-clear logic and the prod-shape fixture covers
replay shape.
