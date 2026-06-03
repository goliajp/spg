# SPG v6.4 design — SQL polish

> Drafted 2026-06-03 after v6.3 series shipped (PG-wire extended
> query finish; tag `v6.3.6` rolled the series up at commit
> `28aabbc`).
> Scope: v6.4 series (v6.4.0 → v6.4.8).
> Companion research:
>   `.claude/researches/spg-vs-pg19-comparison.md` §1.12
>   `.claude/researches/spg-v6-roadmap-from-pg19.md` §3.v6.4
> Predecessor designs: `V6_DESIGN.md`, `V6_1_DESIGN.md`,
> `V6_2_DESIGN.md`, `V6_3_DESIGN.md`.

## L0 — v7.0 discipline (inherited from V6_2_DESIGN)

Same rule:

> **NO ITEM in any v6.x sub-version design may be deferred to a
> later minor without an explicit user-level "OK to defer".**

Deferrals must target a later same-minor sub-version in this
file. Future means a STABILITY §"Out of scope" entry.

## L1 — Roadmap

v6.4 closes the **twelfth-gap cluster** from the PG-19 audit:
all the small-to-medium SQL surface improvements PG 19 ships
plus the JSON path operators that everyone wants. Also picks up
two SQL-surface gaps the v6.2 series carved as "follow up in
v6.4": multi-column ORDER BY and SELECT-list alias in ORDER BY.

v6.4 lands:

1. **Multi-column `ORDER BY a, b DESC, c`** — the parser already
   accepts the comma list but the engine only sorts by the first
   key.
2. **SELECT-list alias in ORDER BY** — `SELECT a + b AS sum
   FROM t ORDER BY sum` — currently errors because `sum` doesn't
   resolve to a base column.
3. **`GROUP BY ALL`** — PG 19 shortcut: group by every non-
   aggregate SELECT-list item.
4. **`IGNORE NULLS` / `RESPECT NULLS`** on window functions
   `LAG` / `LEAD` / `FIRST_VALUE` / `LAST_VALUE`.
5. **`encode/decode` + `random(date,date)` + `random(ts,ts)` +
   `error_on_null(v)`** — small SQL-function bundle.
6. **`INSERT ... ON CONFLICT DO SELECT [FOR UPDATE]`** — PG 19's
   atomic upsert-or-pick variant.
7. **JSON path operators** — `json -> key` / `json ->> key` /
   `json #> path` / `json #>> path` / `json @> json`
   (containment).
8. **Transactional DDL hardening** — explicit-TX `CREATE TABLE +
   INSERT + COMMIT` is atomic; `ROLLBACK` discards the new
   table.
9. **COPY enhancements** — `SKIP N` (skip first N rows of CSV),
   `ON_ERROR SET_NULL` (replace parse-failed cell with NULL),
   `FORMAT JSON` (each row is one JSON object).

Hard rules unchanged: **0 external dependencies, no `unsafe`
(aarch64 NEON carve-out only), WAL on-disk format frozen,
sqllogictest 100% pass rate maintained**.

### Goal numbers (v6.4 ship-gate definition)

| metric | v6.3.6 baseline | v6.4 target | competitor reference |
|--------|-----------------|------------:|----------------------|
| Multi-column ORDER BY correctness | first-key-only | **PG-byte-correct on all asc/desc combinations** | PG ANSI |
| SELECT-list alias resolution in ORDER BY | errors | **resolves alias to projected expr** | PG-compatible |
| GROUP BY ALL coverage | unsupported | **groups every non-aggregate item in SELECT list** | PG 19 |
| Window IGNORE NULLS / RESPECT NULLS | unsupported | **LAG / LEAD / FIRST_VALUE / LAST_VALUE** | PG 19 |
| JSON path operators | unsupported | **`-> ->> #> #>> @>` byte-correct on PG-style payloads** | PG 19 |
| Transactional DDL atomicity | not chaos-tested | **ROLLBACK undoes prior CREATE/ALTER inside TX** | PG ANSI |
| sqllogictest 4-corpus regression | 100 % | **100 %** | unchanged |

### Out of v6.4 (carved out)

- **`json_path_exists` / `json_path_query` / `jsonb_path_query_array`** — PG
  has SQL/JSON path with a full path-expression grammar (the
  `jsonpath` opaque type). v6.4 ships the bare-key/path-array
  operators; full SQL/JSON path is a separate large surface,
  out of v6.x.
- **`@?` (JSON path exists short form)** — depends on the full
  jsonpath grammar. Out of v6.4.
- **PG 19 `MERGE ... WHEN NOT MATCHED BY SOURCE`** — full MERGE
  is a separate verb; INSERT ON CONFLICT DO SELECT covers the
  common upsert. Out of v6.4.
- **COPY `FORMAT BINARY`** — PG's binary COPY format is a
  separate spec. Out of v6.x (text + CSV + JSON are the
  practically-needed formats).
- **`DDL within an implicit transaction` autocommit semantics
  divergence from PG** — SPG v6.4 makes explicit-TX DDL
  atomic; implicit-TX DDL stays auto-commit, same as v6.3.
  Documented in STABILITY.
- **`xmlforest` / `xmlagg` / XML functions** — SPG has no XML
  type. Out of v6.x.

## L2 — Version boundaries (v6.4.0 → v6.4.8)

| ver | scope | ship-gate | depends on |
|-----|-------|-----------|------------|
| **v6.4.0** | Multi-column ORDER BY + SELECT-list alias resolution. Engine sorts on the full `Vec<OrderBy>` (currently uses only index 0). Comparator chains keys left-to-right, each independently asc/desc. ORDER BY items reference SELECT-list aliases by name lookup against the projection before falling through to the FROM schema. | `tests/e2e_order_by_multi::two_key_asc_desc_correct` + `…::three_key_nulls_first_last` + `…::alias_resolves_to_projection` + `…::position_ref_still_works` | v6.3.6 |
| **v6.4.1** | `GROUP BY ALL` parser + planner pass. Parser accepts the literal `ALL` in place of expression list. Planner walks SELECT items, collects every non-aggregate, and rewrites the GROUP BY to that list. Behaviour matches DuckDB/PG 19. | `tests/e2e_group_by_all::sums_with_group_by_all` + `…::no_aggregate_errors_clear` | v6.4.0 (uses projection walk) |
| **v6.4.2** | Window function `IGNORE NULLS` / `RESPECT NULLS` on LAG/LEAD/FIRST_VALUE/LAST_VALUE. AST grows a `null_treatment: NullTreatment` field on `WindowFunction`; executor consults it during the partition walk. | `tests/e2e_window_null_treatment::lag_ignore_nulls_skips_nulls` + `…::first_value_respect_nulls_default` + `…::last_value_ignore_nulls` | v6.4.0 |
| **v6.4.3** | SQL function bundle: `encode(bytes, format)` adds `base64url` and `base32hex`; `random(date, date)` returns uniform Date in [lo, hi]; `random(ts, ts)` returns uniform Timestamp in [lo, hi]; `error_on_null(v)` returns v but raises if v is NULL. All four are parse-time function calls already (no new AST nodes). | `tests/e2e_sql_funcs::encode_base64url_round_trip` + `…::encode_base32hex` + `…::random_date_bounds` + `…::random_timestamp_bounds` + `…::error_on_null_panics_on_null` | v6.3.6 |
| **v6.4.4** | **DROPPED — design error**. Original claim: "INSERT ON CONFLICT DO SELECT [FOR UPDATE], already supported in v5.x". Audit during v6.4.4 work found SPG has NO PRIMARY KEY / UNIQUE constraint enforcement at all (`grep Primary` returns nothing across storage + engine). ON CONFLICT has nothing to detect. Implementing the prerequisites (PK/UNIQUE syntax + storage + enforcement) is foundational DML work, out of scope for the v6.4 SQL-polish theme. Carved out to STABILITY §"Out of v6.4"; revisited as a dedicated v6.x feature (likely v6.6 or later). | n/a — slot intentionally empty | n/a |
| **v6.4.5** | JSON path operators. Five new BinOps: `JsonGet` (`->`), `JsonGetText` (`->>`), `JsonGetPath` (`#>`), `JsonGetPathText` (`#>>`), `JsonContains` (`@>`). Walks JSON with array-index / string-key semantics matching PG. | `tests/e2e_json_path::arrow_get_object_key` + `…::arrow_arrow_get_text` + `…::hash_arrow_path_walk` + `…::contains_predicate` + `…::null_propagation_on_missing_key` | v6.3.6 |
| **v6.4.6** | Transactional DDL hardening. Engine `tx_catalog` shadow already holds DDL inside a TX; v6.4.6 chaos-tests `BEGIN; CREATE TABLE; INSERT; ROLLBACK` actually leaves the catalog unchanged + `BEGIN; CREATE TABLE; INSERT; COMMIT` flips both atomically. | `tests/e2e_transactional_ddl::rollback_drops_table_created_in_tx` + `…::commit_persists_table_and_rows_atomically` + `…::ddl_inside_tx_invisible_to_other_session_before_commit` | v6.4.0 |
| **v6.4.7** | COPY enhancements: `WITH (SKIP 1)`, `WITH (ON_ERROR SET_NULL)`, `WITH (FORMAT JSON)`. SKIP N drops first N data rows; ON_ERROR SET_NULL replaces a parse-failed cell with NULL instead of aborting; FORMAT JSON parses each input line as a JSON object whose keys match column names. | `tests/e2e_copy_options::skip_header_row` + `…::on_error_set_null_replaces_bad_cell` + `…::format_json_one_row_per_line` | v6.3.6 |
| **v6.4.8** | v6.4 series ship rollup — CHANGELOG header, PROD_READY rows 7.26 – 7.32, STABILITY §"SQL polish (v6.4 series)" + carve-outs. | rollup-only; CHANGELOG / PROD_READY / STABILITY merged; 4-corpus 100 %; every v6.4.x e2e from rows above passes. | v6.4.0 → v6.4.7 all |

### Estimated effort

| sub-version | est. days | running total |
|-------------|----------:|--------------:|
| v6.4.0 | 1.5 | 1.5 |
| v6.4.1 | 0.5 | 2.0 |
| v6.4.2 | 1.0 | 3.0 |
| v6.4.3 | 1.0 | 4.0 |
| v6.4.4 | 1.0 | 5.0 |
| v6.4.5 | 3.0 | 8.0 |
| v6.4.6 | 1.0 | 9.0 |
| v6.4.7 | 2.0 | 11.0 |
| v6.4.8 | 0.5 | 11.5 |

Roadmap estimate was 12.5 d (incl. 3 d optional COPY); v6.4
adopts the COPY work and ships 11.5 d total.

## Architectural deliberations

### 1 — ORDER BY alias resolution: when does the alias bind?

PG: SELECT-list aliases shadow base-column names within
ORDER BY and GROUP BY (same set of clauses). SPG follows that
rule:
  1. Try to resolve the ORDER BY identifier against SELECT-list
     output names (post-alias).
  2. Fall through to FROM schema.
  3. Error if neither match.

Position references (`ORDER BY 2`) continue to bind to the
1-based projection index (already supported via v6.2.4
`resolve_order_by_position`).

### 2 — GROUP BY ALL: rewrite vs new executor mode

PG 19 implements GROUP BY ALL as a parser-level rewrite to the
explicit list. SPG follows the same path — v6.4.1's planner
pass walks SELECT items, collects every non-aggregate
expression, and replaces the AST's `Statement::Select.group_by`
with that vector before any other planner work runs. Keeps the
executor unchanged.

### 3 — JSON path operators: storage representation

SPG's `Value::Json(String)` holds the canonical JSON text
(parsed at write time for validity, stored as text for
round-trip stability). v6.4.5 operators parse the text on
demand via a small zero-alloc walker — no in-memory JSON
representation cached. For repeated paths on the same value
this is O(n²) but n stays small for typical JSON payloads.
v6.5 Observability v2 might tier toward a parsed cache; out of
v6.4.

### 4 — Transactional DDL: ROLLBACK semantics

v4.41 introduced `tx_catalog` — a shadow `Catalog` keyed on
`TxId`. DDL inside a TX writes to the shadow; COMMIT atomically
swaps it into the engine's main catalog; ROLLBACK drops the
shadow. v6.4.6 adds explicit e2e coverage for this path (the
mechanism is already there; the test surface formally locks
the invariant).

### 5 — COPY ON_ERROR SET_NULL: how aggressive

PG 17 added `ON_ERROR SET_NULL` to skip per-cell parse failures
without aborting the COPY. v6.4.7 implements the same: each
input cell goes through `coerce_value`; on failure, if
`ON_ERROR SET_NULL` was specified, the cell becomes `NULL`
(must be a nullable column or the row is still rejected with
a clear "ON_ERROR SET_NULL but target column not nullable"
message). The default (no `ON_ERROR`) aborts the COPY on any
parse failure, matching PG.

## L3a — Hot plan for v6.4.0 (the only sub-version that's "next")

Goal: implement multi-column ORDER BY + SELECT-list alias
resolution in ORDER BY. No GROUP BY ALL yet (v6.4.1), no window
NULL treatment yet (v6.4.2).

### Step 1 — AST shape audit

`SelectStatement.order_by` is currently `Option<OrderBy>` — only
holds ONE key. v6.4.0 changes it to `Option<Vec<OrderBy>>`. Each
`OrderBy { expr, desc }` continues to carry its own asc/desc
flag.

### Step 2 — Parser update

Already accepts the comma list per the existing `ORDER BY a,
b DESC, c` shape? Verify. If not, extend `parse_order_by` to
return `Vec<OrderBy>`.

### Step 3 — Executor sort

`exec_select` sorts via a `cmp_rows` closure. v6.4.0's
comparator chains comparisons across every `OrderBy` key in
order: first key decides; tie → second; tie → third; …

### Step 4 — Alias resolution

`resolve_order_by_alias(stmt)` runs before
`resolve_order_by_position` and tries each ORDER BY key's
`Expr::Column(c)` against `stmt.items`'s aliases. On match,
replace the `Expr::Column` with a fresh `Expr` referencing the
projected expression (or — simpler — leave the column name as
is and let the existing post-aggregate fallback resolve it
against the projected schema).

### Step 5 — Test surface

```text
crates/spg-engine/tests/e2e_order_by_multi.rs
  ├── two_key_asc_desc_correct
  ├── three_key_nulls_first_last
  ├── alias_resolves_to_projection
  └── position_ref_still_works   (regression: v6.2.4 surface preserved)
```

### Step 6 — Acceptance

- `cargo test -p spg-engine` green
- `cargo run -q -p sqllogictest --release` → 4-corpus 100%
- Existing position-ref ORDER BY tests untouched

Commit message: `v6.4.0: multi-column ORDER BY + SELECT-list alias resolution`.
