# v7.37 ship readiness — what's in, what's queued

> Authoritative snapshot of v7.37's ship surface vs the items deferred
> to v7.38+. The complete-roadmap file
> (`.claude/notes/v7.37.x-complete-roadmap.md`, gitignored) is an
> EXHAUSTIVE PG-completeness audit covering v7.37 through v7.51+;
> this doc is the SHIPPING subset. Re-read after every release.

## v7.37 — what ships

### Catalog completeness

37 PG-shape catalog views land in v7.37, each with the
PG-canonical column set so dashboards / ORMs / pg_dump round-trip
without errors:

`pg_class`, `pg_attribute`, `pg_index`, `pg_constraint`, `pg_proc`,
`pg_type`, `pg_enum`, `pg_namespace`, `pg_database`, `pg_roles`,
`pg_am`, `pg_collation`, `pg_inherits`, `pg_depend`, `pg_publication`,
`pg_subscription`, `pg_replication_slots`, `pg_statistic_ext`,
`pg_statistic`, `pg_stat_statements`, `pg_stat_database`,
`pg_stat_user_tables`, `pg_stat_user_indexes`, `pg_stat_user_functions`,
`pg_stat_io`, `pg_stat_bgwriter`, `pg_stat_archiver`,
`pg_stat_replication`, `pg_stat_progress_vacuum`,
`pg_stat_progress_create_index`, `pg_stat_progress_analyze`,
`pg_tablespace`, `pg_statio_user_tables`, +
`information_schema.{schemata, views, table_constraints, domains,
attributes}`.

### Type completeness

PG 18 builtin scalar coverage: 100% (v7.37.5 milestone). UUID +
INTERVAL + 13 array-of-scalar + 6 multirange + 7 geometry + INET /
CIDR / MACADDR / MACADDR8 / BIT / VARBIT / XML / "char" / MONEY[] all
shipped. Composite + domain meta-types queue with ζ-B (v7.37 SHIP
not gated on those).

### SQL surface

- ALTER TABLE 70+ sub-commands accept-or-execute (18.1-18.18 all
  closed; PG-only forms accept-and-no-op for pg_dump round-trip).
- EXPLAIN (ANALYZE / BUFFERS / TIMING / SETTINGS / WAL / SUGGEST /
  VERBOSE / FORMAT text|json|xml|yaml) all live.
- Partition DDL: LIST + RANGE + HASH strategies; ATTACH / DETACH /
  DETACH CONCURRENTLY all live; pruning on `=` predicates.
- CREATE STATISTICS parse-accepted (v7.17.0 Phase 8 + 23.7
  rationale).
- pg_dump-compat ALTER TABLE residuals (18.18): RESET / OF /
  NOT OF / FORCE RLS / ENABLE/DISABLE ROW LEVEL SECURITY all
  accept-and-no-op.

### PL/pgSQL surface (v7.37.20 slice, autorun-shipped)

The DO block and trigger body executor now cover a substantial
chunk of PL/pgSQL:

- Control flow: IF / ELSIF / ELSE / END IF (v7.12.6); bare LOOP +
  EXIT [WHEN] + CONTINUE [WHEN]; WHILE LOOP; FOR i IN start..end
  LOOP (via Token::DotDot); FOR var IN SELECT LOOP (scalar-column
  binding via for_query_resolver); FOR var IN EXECUTE LOOP
  (runtime-computed SELECT).
- Diagnostics: RAISE NOTICE/WARNING/INFO/LOG/DEBUG/EXCEPTION with
  `%` positional substitution; ASSERT <cond> [, <msg>];
  EXCEPTION WHEN <cond> [OR <cond>]* THEN <handler> catches
  RaiseException (OTHERS + named condition substring match);
  sqlerrm + sqlstate locals auto-populated inside handlers.
- Data: DECLARE with type inference (`DECLARE x := 42;`);
  %TYPE / %ROWTYPE parse-accept; SELECT INTO with FOUND local
  auto-set; PERFORM <select>; EXECUTE <string_expr>; RETURN NEW /
  OLD / NULL / <expr>; RETURN QUERY <select>; RETURN QUERY
  EXECUTE <string>.

Cursors, RETURN NEXT accumulator, RECORD types, and full
PG-canonical GET DIAGNOSTICS syntax queue with v7.40.

### Observability + operator surface

- `spg_stat_activity` with `application_name` + `wait_event_type` +
  `wait_event` populated.
- `pg_stat_statements` PG-shape 38-column view; query stats
  normalization (whitespace + literals → `$N`); per-template row
  tracking; pg_amcheck heap + index corruption checks.
- `spgctl top` real-time query watcher.
- `spgctl` psql meta-commands: `\d`, `\dt`, `\di`, `\dv`, `\df`,
  `\du`, `\l`, `\dn` + `describe-*` longhand.
- Scripts: `audit-pg-builtins.sh`, `dump-roundtrip-oss-schemas.sh`,
  `diff-with-pg.sh`, `migrate-from-pg.sh`, `run-pg18-regression.sh`,
  `perf-endpoint-sweep.sh`.

### Documentation surface

13 docs in `docs/` consolidate the native-equivalent commitments
and design constraints that pg_dump / monitoring / migration
operators need to know:

`PG_DROPIN.md`, `MYSQL_DROPIN.md`, `MARIADB_DROPIN.md`,
`SPG_TUNABLES.md`, `WIRE_FORMAT_PROMISE.md`, `TABLESPACES.md`,
`FLAMEGRAPHS.md`, `LONG_RUN_VERIFICATION.md`, `PITR_TARGETS.md`,
`WAL_SYNC_INVARIANTS.md`, `DEAD_CODE_AUDIT.md`,
`REPLICATION_PROTOCOLS_RFC.md`, `STORAGE_FORMAT_RETIREMENT.md`,
`INJECTION_POINTS.md`, `LSP_IDE_SETUP.md`,
`CONCURRENCY-INVARIANTS.md`, `EMBEDDED_VS_SERVER.md`,
`PERF_METHODOLOGY_VS_FOSS.md`, `TESTING.md`,
`WAL-QUARANTINE-RECOVERY.md`, `TESTING_V2_SKELETON.md`,
`TESTING_V2_DASHBOARD.md`.

### CI surface

5-category gate (lint / unit / e2e / gates / biz) green;
`perf_gate` job fanned into an 8-crate matrix (27.8);
`gate.sh all` is the release.sh preflight (27.7); never-die gate
runs as part of the existing `perf_gate` job (27.2).

## v7.38+ — what's queued (sized & sequenced)

### v7.37.15 → v7.38 MVCC epic (24 items)

Per-row `RowHeader { xmin, xmax }` + `Snapshot { version, in_progress }`
infrastructure (Phase A/B done) + the remaining Phases C-F (writer
concurrency / vacuum / isolation levels / regression). Multi-week
epic. Gates Hot Standby (21.15) + the per-MVCC dependent items
(22.11 N-hop wait chain, 19.22 EXPLAIN ANALYZE lock-wait, etc.).

### v7.37.17 → v7.39 indexes (9 items)

Hash / GiST / SPGiST / GIN posting tree / GIN fast-update +
CREATE INDEX CONCURRENTLY. Each AM is a 1-2 week epic.

### v7.37.19 → v7.39 query (24 items)

GROUPING SETS / ROLLUP / CUBE / JSON_TABLE / XMLTABLE / CREATE
MATERIALIZED VIEW + REFRESH / window RANGE explicit offset / window
GROUPS / Exclusion constraint USING gist / view auto-updatable /
INSTEAD OF trigger / sort tape merge / merge join / bushy join /
parameterized nested-loop / sorted-vs-hashed aggregate /
EquivalenceClass refinement.

### v7.37.20 → v7.40 PL/pgSQL (19 items)

Current 163 LOC subset → full SQL standard surface. GET
DIAGNOSTICS / cursors / RAISE NOTICE chain / line-level coverage
report.

### v7.37.21 → v7.39 replication tail (10+ items)

Logical replication cross-version compat / publication row filter /
column list / ALTER PUBLICATION / replication slot persistence /
Hot Standby physical / `spgctl basebackup`.

### v7.37.27 → v7.38 monster splits (4 items)

storage `lib.rs` 12k / parser.rs 10k / aggregate
`accumulate_groups` 354 LOC / pgwire `handle_conn` 335 LOC. Each
split is a 1-2 day focused refactor with the differential bench
running before/after to catch perf regressions per
PERF_METHODOLOGY_VS_FOSS.md.

### v7.37.23.1 → v7.38 spgctl REPL

`reedline` adoption + tab completion + history (`~/.spg_history`)
follow as the REPL surface lands.

### v7.37.16.8 / 16.9 → v7.39 partition-wise execution

Partition-wise join + aggregate join the planner once 17.2 GiST AM
ships (partition pruning on geometric predicates needs an AM).

### v7.37.26.5 / 26.6 → operational, not release-blocking

TPC-C custom workload + decomposition-agent loss attack run as
operator actions against losing endpoints from
`scripts/perf-endpoint-sweep.sh`. No release blocks on a
to-be-determined LOSS that no customer reports.

## Why this split is honest

The v7.37 ship surface delivers every committed v7.37 milestone:
- mailrs / sentori cascade closures (P0 lock-hang fixes through
  v7.37.12)
- PG 18 builtin scalar 100% coverage (v7.37.5)
- partition completeness (v7.37.16)
- catalog completeness (v7.37.24)
- observability completeness (v7.37.22)
- SPG-specific 收敛 + docs (v7.37.25)
- pipeline governance (v7.37.27.8 perf_gate matrix)

The deferred items are multi-week epics that the v7.37 train was
never the right ship vehicle for. They land on their own ship
schedules.

## Release checklist

When v7.37 is ready to tag:

1. `git status` — clean against feature/v7.37.16-partition-completeness
2. `bash scripts/test-on-mini.sh all` (mini offload per
   `feedback-all-build-test-mini.md`)
3. `bash scripts/dropin-acceptance.sh` (G4 dump-compat)
4. `bash scripts/gate.sh all` (G1-G5 gate, called by release.sh)
5. `bash scripts/release.sh 7.37.13` — tag + crate publish + docker
   build / push (see [reference-cargo-publish-order](../../../.claude-profile-2/projects/-Users-doracawl-workspace-goliajp-spg/memory/reference-cargo-publish-order.md))
6. Customer ack files: mailrs + sentori under
   `/Users/doracawl/workspace/stables/mailrs/.claude/notes/spg-7.37.X-shipped-2026-MM-DD.md`

## See also

- `.claude/notes/v7.37.x-complete-roadmap.md` — full audit roadmap
  (gitignored, longer than this; this is the ship-curated extract)
- `memory/vision-spg-ge-pg-everywhere.md` — the multi-version
  vision this train serves
- `docs/PERF_METHODOLOGY_VS_FOSS.md` — how the deferred perf
  attacks (26.5 / 26.6 / 27.5) are organized when they're run
