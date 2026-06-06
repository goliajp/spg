# SPG conformance baseline

Per-corpus pass / fail / skip:

| corpus | pass | fail | skip | % pass |
|---|---|---|---|---|
| `duckdb` | 148 | 0 | 0 | 100.0% |
| `mysql` | 17 | 0 | 0 | 100.0% |
| `pg_regress` | 176 | 0 | 0 | 100.0% |
| `pgvector` | 63 | 0 | 0 | 100.0% |

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
| `14_aggregates.test` | 9 | 0 | 0 |
| `15_joins.test` | 9 | 0 | 0 |
| `16_distinct_union.test` | 7 | 0 | 0 |
| `17_functions.test` | 7 | 0 | 0 |
| `18_cast_expr.test` | 5 | 0 | 0 |
| `19_having_and_show.test` | 7 | 0 | 0 |
| `20_offset_orderby_position.test` | 7 | 0 | 0 |
| `21_order_by_desc.test` | 8 | 0 | 0 |

### `mysql/`

| file | pass | fail | skip |
|---|---|---|---|
| `01_dialect.test` | 12 | 0 | 0 |
| `02_int_types.test` | 5 | 0 | 0 |

### `pg_regress/`

| file | pass | fail | skip |
|---|---|---|---|
| `01_create_table_shapes.test` | 6 | 0 | 0 |
| `02_insert_shapes.test` | 9 | 0 | 0 |
| `03_dml_v2_unsupported.test` | 25 | 0 | 0 |
| `04_pg_types.test` | 24 | 0 | 0 |
| `05_savepoints.test` | 8 | 0 | 0 |
| `06_date_time.test` | 13 | 0 | 0 |
| `07_date_functions.test` | 13 | 0 | 0 |
| `08_now_and_date_arith.test` | 10 | 0 | 0 |
| `09_bare_current.test` | 4 | 0 | 0 |
| `10_interval.test` | 23 | 0 | 0 |
| `11_date_functions_part2.test` | 25 | 0 | 0 |
| `12_pg_trgm.test` | 16 | 0 | 0 |

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
| `08_hnsw_knn.test` | 11 | 0 | 0 |
| `09_hnsw_metrics_and_filter.test` | 10 | 0 | 0 |
