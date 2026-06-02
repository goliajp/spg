# SPG v6.1 design — logical replication + PG-driver polish

> Drafted 2026-06-03 after v6.0 series shipped (final tag pending v6.1.0
> graph compaction + v6.1.1 Extended Query roll-up).
> Scope: v6.1 series (v6.1.0 → v6.1.10).
> Companion research:
>   `.claude/researches/spg-vs-pg19-comparison.md` §1
>   `.claude/researches/spg-v6-roadmap-from-pg19.md` §1.9.1 / §1.9.4 / §1.6.1 / §1.7.3 / §1.11.1
> Predecessor design: `V6_DESIGN.md` (v6.0 — vector advancement).

## L1 — Roadmap

v6.1 closes the **second biggest gap** found in the PG 19 audit:
**logical replication** (Publication / Subscription). SPG ships
physical streaming today (`replication.rs` — master WAL byte
replay on follower), which covers HA + read-scaling but cannot:

- Replicate a subset of tables (per-publication filtering)
- Replicate to a different schema / encoding
- Cascade beyond 1 hop (followers reject downstream peers)
- Coordinate cross-replica reads via "WAIT FOR WAL POSITION"

PG 19 has all four. SPG's WAL is already SQL bytes (not page
images), so the implementation cost is **far lower than PG's** —
no `wal2json`-style decoder needed. v6.1 capitalises on that.

Theme:

1. **Publications + Subscriptions** — catalog + DDL + worker.
   Publisher filters WAL stream by publication before sending.
2. **Cascading replication** — follower exposes v2 replication
   endpoint to sub-followers. Same protocol, no new format.
3. **Cross-replica wait** — `WAIT FOR WAL POSITION` SQL command
   + timeout. Read scaling without stale-read surprises.
4. **`effective_wal_level` dynamic switch** — flip between
   `replica` and `logical` at runtime so a fresh cluster doesn't
   pay the logical-decoder overhead until a subscription exists.

Two **non-replication preludes** already shipped under the v6.1
banner because they were ready while logical-replication design
was being scoped:

- **v6.1.0** — HNSW graph adjacency `Vec<u32>` (was `Vec<usize>`).
  -78 MiB resident at 1M dim-128 SQ8. Storage-only, no API.
- **v6.1.1** — PG-wire Extended Query Protocol (Parse / Bind /
  Execute, real AST cached prepared statements). PG-driver
  compatibility unlocked (JDBC / asyncpg / psycopg3 default
  path). Modest p50 perf win (3-5%).

These two are kept in the v6.1 series rather than spun out to
v6.0.x because they were not in the original v6.0 ship-gate
(see `V6_DESIGN.md` §"Out of v6.0").

Hard rule unchanged: **0 external deps, no `unsafe` (except the
already-scoped aarch64 NEON intrinsics block in `spg-storage`)**.
Existing axioms A1–A11 (see `spg-v6-roadmap-from-pg19.md` §0)
all hold. In particular: WAL format stays human-readable SQL.

### Goal numbers (v6.1 ship-gate definition)

| metric | v6.0.5 baseline | v6.1 target | PG 19 reference |
|--------|----------------:|------------:|-----------------|
| Publisher + 2 subscribers, 1M row consistency | n/a (no logical repl) | **100 %** identical rows | PG `pgoutput` parity |
| Subscriber initial sync (1M rows) | n/a | **≤ 60 s** | PG sync_worker similar order |
| Cascading follower lag (primary → follower → sub) | n/a (rejected) | **< 2 s** at 50K w/s primary load | PG cascade ~1-3 s |
| `WAIT FOR WAL POSITION` accuracy | n/a | **monotone**, no false-positive resolve | PG `pg_wal_replay_wait` (PG 19) |
| Physical-stream throughput regression vs v6.0.5 | (baseline) | **≤ 2 %** at p50, p99 | n/a |
| Publication-filtered stream overhead per record | n/a | **≤ 200 ns** publisher-side | n/a |

### Out of v6.1 (carved out)

- **DDL replication** — `CREATE TABLE`, `ALTER TABLE`, `CREATE
  INDEX` on the publisher do NOT auto-propagate. Subscriber-side
  schema drift is the operator's problem (v6.1 documents the
  policy; v6.6 may revisit). Documented at: §1.9.1 design
  point 3.
- **Conflict resolution / multi-master** — v6.1 is single-
  publisher per subscription. Multi-publisher conflict policy is
  v7 territory.
- **`pgoutput` wire-format compatibility** — SPG's logical stream
  is still SQL bytes. A future v6.x can add a `pgoutput`-encoded
  variant for cross-product subscribers (PG subscribing from
  SPG). Out of scope for v6.1.
- **Sequence sync** — SPG has no SEQUENCE type, so PG 19's
  publisher → subscriber sequence sync is N/A. (See roadmap §1.9.2.)
- **Cold-tier segment forward** (V5_DESIGN.md carve-out) — v6.7
  topic. Not blended into v6.1.

## L2 — Version boundaries (v6.1.0 → v6.1.10)

Each row is one shippable commit with its own perf gate and chaos
coverage. Ordered by dependency.

| ver | scope (work units) | ship-gate | depends on |
|-----|--------------------|-----------|------------|
| **v6.1.0** (shipped) | HNSW adjacency `Vec<u32>` — see `V6_DESIGN.md` epilogue. | `tests/perf_gate_hnsw_compact::rss_1m_dim128_sq8_under_410mib` | v6.0.5 |
| **v6.1.1** (shipped) | PG-wire Extended Query Protocol. AST-cached prepared statements; vector text-format Bind; `Token::Placeholder` / `Expr::Placeholder` end-to-end. | `tests/e2e_pg_extended` 3/3 + `tests/perf_prepared_vs_simple` measurements | v6.0.5 |
| **v6.1.2** | `CREATE PUBLICATION <name> FOR …` / `DROP PUBLICATION <name>` DDL + lexer / parser / AST. Internal catalog table `spg_publications(name TEXT PRIMARY KEY, definition TEXT NOT NULL, created_at TIMESTAMP)`. No publisher-side WAL filtering yet (that's v6.1.5). | `tests/e2e_publication_ddl::create_drop_roundtrip` + `tests/e2e_publication_ddl::duplicate_name_errors` + `tests/e2e_publication_ddl::publications_persist_across_restart` | v6.1.1 |
| **v6.1.3** | `FOR ALL TABLES` / `FOR TABLE t1, t2, …` / `FOR ALL TABLES EXCEPT t3` filter expressions. Stored as the parsed AST shape in `spg_publications.definition`. Implicit `FOR ALL TABLES` if omitted. | `tests/e2e_publication_ddl::all_tables_default` + `…::for_table_list` + `…::all_tables_except_subset` | v6.1.2 |
| **v6.1.4** | `CREATE SUBSCRIPTION <name> CONNECTION '…' PUBLICATION <name>` + `DROP SUBSCRIPTION <name>`. Subscriber-side background worker (thread per active subscription) that opens a v2 replication connection, requests the publication, replays. Catalog `spg_subscriptions(name PK, conn_str TEXT, publication TEXT, enabled BOOL, last_received_pos BIGINT)`. | `tests/e2e_subscription::create_and_replays_inserts` + `tests/e2e_subscription::disabled_stops_worker` | v6.1.2 |
| **v6.1.5** | Publisher-side filter: replication endpoint accepts a `PUBLICATION = <name>` header and walks the publication's table set when sourcing WAL records. SQL records get a per-table `OWNER` field at parse time (lexed once) so filtering is a `HashSet<&str>::contains` per record. | `tests/e2e_replication_filter::only_published_tables_replicated` + `tests/perf_gate_replication::publisher_filter_overhead_under_200ns_per_record` | v6.1.3, v6.1.4 |
| **v6.1.6** | Cascading replication: follower exposes its own v2 replication endpoint to downstream peers. Same wire-frame protocol; the follower replays from its own applied position. Loop detection: each replication-handshake carries a `cluster_id`; receiving a record originating from your own cluster_id aborts the link. | `tests/e2e_cascade::three_node_chain_replays_correctly` + `tests/e2e_cascade::cycle_detection_aborts_loop` | v6.1.5 |
| **v6.1.7** | `WAIT FOR WAL POSITION <pos>` SQL command. Blocks until `state.applied_pos >= <pos>` or `WITH TIMEOUT <ms>` fires. Implementation reuses `lag_state.applied_pos` (v6.0.x cross-process sidecar work). | `tests/e2e_wait_pos::resolves_when_applied` + `…::timeout_returns_status` | v6.1.4 |
| **v6.1.8** | `effective_wal_level` dynamic switch — `replica` ↔ `logical`. `logical` adds per-record OWNER hashing (v6.1.5 work). `SET effective_wal_level = logical` flips global atomic; subsequent records carry OWNER hashing. Fresh cluster boots in `replica` so the publisher-filter cost only applies when needed. | `tests/e2e_wal_level::flip_at_runtime_no_disruption` + `tests/perf_gate_wal_level::replica_mode_baseline_unchanged` | v6.1.5 |
| **v6.1.9** | e2e chaos: publisher + 2 subscribers + 1 cascading sub-follower. 100K rows inserted on publisher under chaos (netsplit between primary↔follower, follower↔sub-follower); final consistency 100%; observed lag < 2 s p99. | `tests/e2e_chaos_logical::full_topology_under_netsplit` | v6.1.6, v6.1.7 |
| **v6.1.10** | v6.1 ship rollup: CHANGELOG v6.1 entry, PROD_READY rows 7.x (logical replication), STABILITY.md addition (Publication / Subscription as frozen surface), bench sweep against PG 19 logical replication. Tag `v6.1.0`. | Sweep `logical_repl_sweep` matches or beats PG 19 latency at 100K w/s; CHANGELOG + PROD_READY merged. | v6.1.0 → v6.1.9 all |

### Estimated effort

| sub-version | est. days | running total |
|-------------|----------:|--------------:|
| v6.1.0 (shipped) | 0.5 | 0.5 |
| v6.1.1 (shipped) | 1.5 | 2.0 |
| v6.1.2 | 1.0 | 3.0 |
| v6.1.3 | 1.0 | 4.0 |
| v6.1.4 | 2.0 | 6.0 |
| v6.1.5 | 1.5 | 7.5 |
| v6.1.6 | 1.5 | 9.0 |
| v6.1.7 | 1.0 | 10.0 |
| v6.1.8 | 0.5 | 10.5 |
| v6.1.9 | 1.0 | 11.5 |
| v6.1.10 | 0.5 | 12.0 |

Total **12 d** vs roadmap's 10.5 d — extra 1.5 d folded in for
v6.1.0 + v6.1.1 (the perf preludes) which were not in the
original v6.1 scope.

## Architectural deliberations

### 1 — Publication catalog: internal table vs new file format

Two options:

  a) Treat `spg_publications` as a regular catalog table (one row
     per publication), backed by the existing storage layer.
     Replication is a property *of the catalog state*.
  b) Add a sidecar file `.spg/publications.json` outside the
     normal storage layer.

**Decided: (a)**. Reasons:
- Replication of replication-state is its own can of worms.
  Carrying `spg_publications` in the WAL means a follower
  *already* knows the publication set when it becomes a
  cascade primary (v6.1.6). No special bootstrapping.
- Backup/restore (v4.42 + v5 carve-out) covers it for free —
  the snapshot captures `spg_publications` like any other table.
- The cost is a tiny `system_table = true` flag on `TableSchema`
  so admin DDL (DROP DATABASE-equivalent) treats it as protected.

### 2 — Filter granularity: per-table vs per-row

Per-table only. SPG's WAL records are auto-commit SQL; the table
name is already in the parsed AST. Filtering is essentially:

```rust
let owner = parse_owner_from_sql(&sql_bytes); // cached at lex
publication.tables.contains(&owner)
```

Per-row predicates (PG's `WHERE` clause on publications) are
out of scope. Documented in §"Out of v6.1".

### 3 — Subscriber schema drift policy

The subscriber's `CREATE SUBSCRIPTION` does NOT auto-create
target tables. Operator runs the same `CREATE TABLE` DDL on the
subscriber first. Replication starts from the named publication's
current WAL position; any DDL-vs-data mismatch errors out the
subscriber worker and surfaces in `spg_subscriptions.last_error`.

Rationale: PG follows the same policy (subscriber tables must
exist before subscription replays). SPG sticks with PG to avoid
"hidden DDL execution" being a surprise.

### 4 — Cycle detection in cascading

Each cluster boots with a stable random `cluster_id` (u64, stored
in a `.spg/cluster_id` sidecar — generated on first boot, immutable
after). Replication handshake (`H` frame) carries the source
cluster's id. If a receiving cluster sees a record whose
*originator* cluster_id matches its own, the link aborts with
`REPLICATION_LOOP` and surfaces in the subscription's last-error.

This catches the common operator mistake (subscribe back to
yourself via a cascading peer) without needing a full topology
gossip.

## L3a — Hot plan for v6.1.2 (the only sub-version that's "next")

Goal: introduce `CREATE PUBLICATION` and `DROP PUBLICATION` DDL +
internal `spg_publications` catalog table. No publisher filtering
(that's v6.1.5). Drop a small but real end-to-end so v6.1.3+ has
concrete state to extend.

### Step 1 — Lexer additions

Tokens added: `PUBLICATION`, `SUBSCRIPTION` (reserved early for
v6.1.4), `FOR`, `ALL`, `EXCEPT`. `TABLES` is already covered by
the existing `TABLE` token + suffix matching, but a dedicated
`TABLES` keyword is cleaner — added.

### Step 2 — AST nodes

```rust
pub enum Statement {
    …,
    CreatePublication(CreatePublicationStatement),
    DropPublication(String),
}

pub struct CreatePublicationStatement {
    pub name: String,
    /// v6.1.2 ships only the implicit `FOR ALL TABLES`. The
    /// `tables` field is `None` here; v6.1.3 expands it to
    /// `Option<PublicationScope>` with the FOR-clause variants.
    pub tables: Option<PublicationScope>,
}

pub enum PublicationScope {
    AllTables,
    ForTables(Vec<String>),
    AllTablesExcept(Vec<String>),
}
```

`PublicationScope` is introduced now so v6.1.3 only has to wire
up parser + Display; no AST migration.

### Step 3 — Parser rules

```
CREATE PUBLICATION <ident> [FOR ALL TABLES]
DROP PUBLICATION <ident>
```

v6.1.2 accepts the optional `FOR ALL TABLES` clause (no-op:
always-all). The `FOR TABLE … list` and `FOR ALL TABLES EXCEPT …`
forms parse-error here with "v6.1.3" in the message so future
diff blame is clean.

### Step 4 — `spg_publications` catalog table

Created lazily on first `CREATE PUBLICATION` (or up-front at
`Engine::new()` — design choice; lean toward up-front so a
`SHOW PUBLICATIONS` query against an empty database returns 0
rows rather than "table not found").

Schema:
```
spg_publications (
    name TEXT NOT NULL PRIMARY KEY,
    scope TEXT NOT NULL,           -- 'all_tables' for v6.1.2; v6.1.3 widens
    created_at TIMESTAMP NOT NULL
)
```

`TableSchema` gains `system_table: bool` flag (false for user
tables, true for `spg_publications` + future system tables). DROP
TABLE on a system table errors with "system table protected".

### Step 5 — Engine execute dispatch

`Engine::execute_stmt_with_cancel` adds two new arms:

```rust
Statement::CreatePublication(c) => self.exec_create_publication(c),
Statement::DropPublication(name) => self.exec_drop_publication(name),
```

`exec_create_publication`: validates name uniqueness, inserts row
into `spg_publications`. Wraps in an internal transaction so a
duplicate name doesn't leave half-state.

`exec_drop_publication`: deletes row. No-op + warning if not
present (PG-compatible).

### Step 6 — Replication catalog visibility

`spg_publications` writes go through the same WAL path as user
tables — so followers automatically see publications as they're
created. No special handling needed (the v6.1.6 cascade work
relies on this).

### Step 7 — Tests

```
tests/e2e_publication_ddl.rs:
  - create_drop_roundtrip
  - duplicate_name_errors
  - drop_nonexistent_errors_silently
  - publications_persist_across_restart
  - system_table_drop_protected
  - for_table_list_errors_with_v6_1_3_note
```

### Step 8 — STABILITY.md update

Add Publication DDL to the frozen-surface list. Documented as
"v6.1.2 introduced; backward-compatible additions allowed in v6.1.x;
no removal before v7".

### Step 9 — CHANGELOG entry under v6.1

Add v6.1.2 row to the v6.1 section.

### Step 10 — fmt + clippy + workspace test + 4-corpus

Standard pre-commit gates. No regression in any of the 372 + 4-
corpus tests; new e2e_publication_ddl tests pass.

### Step 11 — Commit

```
v6.1.2: CREATE PUBLICATION / DROP PUBLICATION DDL + catalog
```
