# SPG SQL baseline corpus

This corpus is the **systematic baseline** of every SQL feature SPG has
shipped through v7.37. Unlike the vendored corpora (`duckdb/`,
`mysql/`, `pg_regress/`, `pgvector/`), every file here is authored
against SPG's own surface and is expected to pass 100%.

## Layout

```
01_basic_dml/        SELECT / INSERT / UPDATE / DELETE / UPSERT / RETURNING
02_data_types/       Every shipped scalar + array + composite-typed column
03_composite_domain/ CREATE TYPE composite, CREATE DOMAIN with CHECK
04_joins/            INNER / LEFT / RIGHT / FULL / CROSS / USING / NATURAL / LATERAL
05_aggregates/       aggregates, GROUP BY / HAVING / DISTINCT / window functions
06_subqueries/       scalar / IN / EXISTS / correlated / ANY / ALL
07_cte/              WITH, multi-CTE, writable CTE, CTE chains
08_partition/        PARTITION BY RANGE, routing, drop
09_indexes/          btree, expression, partial, GIN-jsonb / tsvector / trgm
10_constraints/      PK / FK / CASCADE / CHECK / NOT NULL
11_dialect/          PG-specific + MySQL-specific + MariaDB-specific surfaces
12_explain/          EXPLAIN basic / ANALYZE / COSTS OFF
13_recovery/         restart smoke, WAL replay, ROLLBACK, isolation level
14_dialect_compat/   pg_dump / mysqldump / mariadb-dump round-trip smokes
```

Each `NN_<category>/` may have a brief `_README.md`.

## Format

sqllogictest standard:

```
# header comment: what this file tests + which SPG version

statement ok
CREATE TABLE t (a INT)

statement ok
INSERT INTO t VALUES (1)

query I rowsort
SELECT * FROM t
----
1
```

The runner walks `corpus/<group>/*.test` so files use the `.test`
extension (not `.slt`) and live under `corpus/spg_baseline/<NN_*>/`.
The runner discovers nested groups automatically.

## How to extend

When a v7.X feature train ships a new SQL surface, the train's PR
must add the corresponding `.test` here:

1. Identify the right `NN_<category>/` directory.
2. Create `<feature_name>.test`.
3. Header comment lists which v7.X feature train shipped it.
4. Use `statement ok` / `statement error` / `query <types> rowsort`
   blocks (see existing files for examples).
5. Run `scripts/test-on-mini.sh biz` to verify integration locally
   before pushing. (biz uses Docker; run it on the dev box, not on
   the mini testbed mirror.)

## Coverage philosophy

This corpus is **shape coverage**, not exhaustive edge-case coverage.
Each file demonstrates 5-30 statements showing the feature works on
minimal data. Stress tests, edge cases, and perf regressions live in:

- `tests/` (e2e + integration)
- perf gates (`#[ignore]` benches)
- the dropin panel (`scripts/dropin-acceptance.sh`)
- the vendored sqllogictest corpora (`duckdb/`, `mysql/`, ...)

## Pass rate target

100%. Every file in `spg_baseline/` is by definition something SPG
already supports. A failure here is a regression, not a baseline
issue. The biz gate refuses to ship if any baseline `.test` fails.
