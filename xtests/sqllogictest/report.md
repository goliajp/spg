# SPG conformance baseline

Per-corpus pass / fail / skip:

| corpus | pass | fail | skip | % pass |
|---|---|---|---|---|
| `duckdb` | 117 | 9 | 0 | 92.9% |
| `pg_regress` | 26 | 8 | 0 | 76.5% |
| `pgvector` | 42 | 0 | 0 | 100.0% |

## Top fail patterns

| count | pattern |
|---|---|
| 3 | `record 5: parse: parse error at` |
| 1 | `record 0: parse: parse error at` |
| 1 | `record 1: parse: parse error at` |
| 1 | `record 2: parse: parse error at` |
| 1 | `record 3: parse: parse error at` |
| 1 | `record 4: eval: column not found:` |
| 1 | `record 6: eval: type mismatch: unknown` |
| 1 | `record 6: parse: parse error at` |
| 1 | `record 7: eval: type mismatch: unknown` |
| 1 | `record 7: parse: parse error at` |
| 1 | `record 7: row mismatch | expected:` |
| 1 | `record 8: parse: parse error at` |

## Per-file detail

### `duckdb/`

| file | pass | fail | skip |
|---|---|---|---|
| `01_select_basic.test` | 7 | 0 | 0 |
| `02_where_basic.test` | 7 | 0 | 0 |
| `03_order_by_limit.test` | 7 | 0 | 0 |
| `04_arith_in_where.test` | 7 | 0 | 0 |
| `05_boolean_logic.test` | 8 | 0 | 0 |
| `06_aliases.test` | 6 | 0 | 0 |
| `07_transactions.test` | 14 | 0 | 0 |
| `08_create_index_seek.test` | 9 | 0 | 0 |
| `09_multi_value_insert.test` | 4 | 0 | 0 |
| `10_is_null_predicates.test` | 6 | 0 | 0 |
| `11_column_list_insert.test` | 3 | 0 | 0 |
| `12_string_concat.test` | 3 | 0 | 0 |
| `13_between_in_like.test` | 8 | 0 | 0 |
| `14_aggregates.test` | 5 | 4 | 0 |
| `15_joins.test` | 7 | 2 | 0 |
| `16_distinct_union.test` | 4 | 3 | 0 |
| `17_functions.test` | 7 | 0 | 0 |
| `18_cast_expr.test` | 5 | 0 | 0 |

<details><summary>`14_aggregates.test` fail snippets</summary>

- record 5: parse: parse error at token #3: unexpected token Star in expression
- record 6: eval: type mismatch: unknown function `sum`
- record 7: eval: type mismatch: unknown function `max`
</details>

<details><summary>`15_joins.test` fail snippets</summary>

- record 7: parse: parse error at token #10: expected end of input, got Comma
- record 8: parse: parse error at token #11: expected end of input, got Ident("join")
</details>

<details><summary>`16_distinct_union.test` fail snippets</summary>

- record 4: eval: column not found: distinct
- record 5: parse: parse error at token #3: expected end of input, got Select
- record 6: parse: parse error at token #3: expected end of input, got Ident("all")
</details>

### `pg_regress/`

| file | pass | fail | skip |
|---|---|---|---|
| `01_create_table_shapes.test` | 6 | 0 | 0 |
| `02_insert_shapes.test` | 9 | 0 | 0 |
| `03_dml_v2_unsupported.test` | 6 | 0 | 0 |
| `04_pg_types.test` | 0 | 5 | 0 |
| `05_savepoints.test` | 5 | 3 | 0 |

<details><summary>`04_pg_types.test` fail snippets</summary>

- record 0: parse: parse error at token #5: unsupported column type "smallint"
- record 1: parse: parse error at token #5: unsupported column type "numeric"
- record 2: parse: parse error at token #5: unsupported column type "varchar"
</details>

<details><summary>`05_savepoints.test` fail snippets</summary>

- record 3: parse: parse error at token #0: expected SELECT / CREATE / INSERT / BEGIN / COMMIT / ROLLBACK at start of statement, got Ident("savepoint")
- record 5: parse: parse error at token #1: expected end of input, got Ident("to")
- record 7: row mismatch |   expected: ["1"] |   actual:   ["1", "2"]
</details>

### `pgvector/`

| file | pass | fail | skip |
|---|---|---|---|
| `01_create_vector_column.test` | 5 | 0 | 0 |
| `02_insert_vector_literal.test` | 6 | 0 | 0 |
| `03_dim_mismatch.test` | 4 | 0 | 0 |
| `04_l2_distance_order_limit.test` | 8 | 0 | 0 |
| `05_vector_in_transaction.test` | 10 | 0 | 0 |
| `06_distance_variants.test` | 6 | 0 | 0 |
| `07_cast_vector_literal.test` | 3 | 0 | 0 |
