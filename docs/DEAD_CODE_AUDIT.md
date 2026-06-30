# Dead-code audit (v7.37.25.10)

> Verdict snapshot of every `#[allow(dead_code)]` site in `crates/spg-*/src/`.
> Re-run with `grep -rn '#\[allow(dead_code)\]' crates/spg-*/src/` and compare
> against this table whenever new sites land or existing ones turn live.

## Audit table

| # | File:line | Item | Verdict | Why kept |
|---|-----------|------|---------|----------|
| D01 | `crates/spg-embedded/src/lib.rs:2000` | `V6RecordView.prev_lsn` field | KEEP | Read-only diag field; populated from WAL record header so a future `verify-pitr` or replay-debug surface can walk it without re-parsing |
| D02 | `crates/spg-embedded/src/lib.rs:2594` | `wal_dir` / `current_chunk_path` | KEEP | Read at boot, retained for Drop / diag introspection (comment in source) |
| D03 | `crates/spg-embedded/src/lib.rs:4935` | `_database_is_send()` | KEEP — invariant assertion | Compile-time guard that `Database: Send`; reading no longer would silently regress thread-safety |
| D04 | `crates/spg-engine/src/partition.rs:142` | partition route helper | KEEP | Wired by commit #4 (INSERT routing); comment in source |
| D05 | `crates/spg-engine/src/partition.rs:194` | partition route helper | KEEP | Tests call via the public route; comment in source |
| D06 | `crates/spg-engine/src/select.rs:796` | `materialise_ctes` | KEEP | Pre-staged for nested-CTE materialisation; lands when CTE epic is opened |
| D07 | `crates/spg-server/src/observability.rs:149` | `log_event` JSON-line emitter | KEEP | Wire-up to startup / auth events queued for follow-up; surface stable so call sites don't churn when wired |
| D08 | `crates/spg-server/src/wal.rs:184` | `encode_wal_auto_commit_sql` non-prod caller path | KEEP | Production commit-queue uses the metric-emitting variant; this is the no-metrics path for tests / migration tooling |
| D09 | `crates/spg-server/src/backup.rs:54` | `ChecksumState::Corrupt` variant | KEEP | v4.37 v2-bundle CRC32 mismatch outcome — restored bundles still hit this arm; pattern-match completeness |
| D10 | `crates/spg-server/src/backup.rs:182` | `inspect_bundle()` | KEEP | Exposed for operator tooling + e2e PITR test (comment in source) |
| D11 | `crates/spg-server/src/backup.rs:230` | `BundleHeader` struct | KEEP | Public return type of D10; struct must exist even if no in-crate caller |
| D12 | `crates/spg-server/src/scram.rs:29` | `ScramErr::NonceMismatch` variant | KEEP | Reachable via the nonce-check arm in the helper (comment in source) |
| D13 | `crates/spg-sqlx/src/error.rs:88` | `_ArcMarker` type alias | KEEP — import shim | Holds `Arc` reachable so future kind-mapping work can reach for it without re-importing |
| D14 | `crates/spg-sqlx/src/value.rs:105` | `engine_value_kind()` | KEEP | Exposed for adapters whose column metadata isn't yet known; sqlx-side optional dispatch |
| D15 | `crates/spg-sqlx/src/value.rs:143` | `_box_dyn_marker()` | KEEP — import shim | Keeps `BoxDynError` import path so trait impls can return unboxed errors without losing the import |
| D16 | `crates/spg-sqlx/src/connection.rs:494` | `affected_from()` | KEEP | Pre-staged for v7.37.20 PL/pgSQL `GET DIAGNOSTICS ROW_COUNT` — sqlx connection layer needs the same shape as the engine layer |
| D17 | `crates/spg-sqlx/src/types/chrono.rs:143` | `_timelike_marker()` | KEEP — import shim | Keeps `Timelike` reached only via `format!()` chain on `NaiveDateTime`; loose without this |

## Verdict summary

- **KEEP: 17 sites.** Every `#[allow(dead_code)]` site has a documented reason
  (either a comment in source or a clear semantic role above): import shims,
  invariant assertions, pre-staged plumbing for an open queue item,
  diagnostic-only fields, or pattern-match completeness variants.
- **REMOVE: 0 sites.**

## Why this audit exists

`#[allow(dead_code)]` accumulates entropy quickly: each site is a deliberate
exception, but the next reader has to re-derive *why* from grep + git-blame.
This file makes the verdict cheap to look up. When a new `#[allow(dead_code)]`
lands, add a row here; when one becomes reachable, drop both the allow and
the row.

## Re-audit cadence

- Each v7.37.X+ minor release: `grep | diff` against this table, walk any
  new rows, drop any rows whose underlying item went live.
- Any standalone dead-code-sweep refactor commit: cite this file in the
  commit message + update the table in the same commit.
