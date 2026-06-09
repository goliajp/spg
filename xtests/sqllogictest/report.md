# SPG conformance baseline

Per-corpus pass / fail / skip:

| corpus | pass | fail | skip | % pass |
|---|---|---|---|---|
| `duckdb` | 148 | 0 | 0 | 100.0% |
| `mysql` | 119 | 0 | 0 | 100.0% |
| `pg_regress` | 1160 | 0 | 0 | 100.0% |
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
| `03_inline_key.test` | 17 | 0 | 0 |
| `05_delimiter.test` | 7 | 0 | 0 |
| `06_tinyint_bool.test` | 9 | 0 | 0 |
| `07_unsigned.test` | 12 | 0 | 0 |
| `08_create_procedure.test` | 5 | 0 | 0 |
| `09_if_ifnull.test` | 15 | 0 | 0 |
| `10_tinyint1_int_coerce.test` | 16 | 0 | 0 |
| `11_fulltext_gin_seek.test` | 8 | 0 | 0 |
| `12_unique_collation.test` | 13 | 0 | 0 |

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
| `13_timestamptz_offset.test` | 11 | 0 | 0 |
| `14_disable_trigger_tsvector.test` | 23 | 0 | 0 |
| `15_do_block_information_schema.test` | 21 | 0 | 0 |
| `16_sequences.test` | 50 | 0 | 0 |
| `17_views.test` | 28 | 0 | 0 |
| `18_materialized_views.test` | 27 | 0 | 0 |
| `19_enum_types.test` | 17 | 0 | 0 |
| `20_domain_types.test` | 28 | 0 | 0 |
| `21_schemas.test` | 18 | 0 | 0 |
| `22_fulltext_index.test` | 16 | 0 | 0 |
| `23_statement_timeout.test` | 12 | 0 | 0 |
| `24_application_name.test` | 7 | 0 | 0 |
| `25_collate.test` | 22 | 0 | 0 |
| `26_mysql_view_algorithm.test` | 16 | 0 | 0 |
| `27_for_update.test` | 15 | 0 | 0 |
| `28_deferrable.test` | 14 | 0 | 0 |
| `29_generate_series.test` | 10 | 0 | 0 |
| `30_limit_extras.test` | 9 | 0 | 0 |
| `31_regtype_regclass.test` | 5 | 0 | 0 |
| `32_format.test` | 10 | 0 | 0 |
| `33_regexp_family.test` | 13 | 0 | 0 |
| `34_jsonb_path_query.test` | 12 | 0 | 0 |
| `35_inet_types.test` | 15 | 0 | 0 |
| `36_concat.test` | 25 | 0 | 0 |
| `37_concat_ws.test` | 24 | 0 | 0 |
| `38_now_bare_call.test` | 16 | 0 | 0 |
| `39_trim_family.test` | 22 | 0 | 0 |
| `40_replace.test` | 17 | 0 | 0 |
| `41_split_part.test` | 25 | 0 | 0 |
| `42_repeat.test` | 13 | 0 | 0 |
| `43_lpad_rpad.test` | 22 | 0 | 0 |
| `44_strpos.test` | 20 | 0 | 0 |
| `45_left_right.test` | 26 | 0 | 0 |
| `46_floor.test` | 14 | 0 | 0 |
| `47_ceil.test` | 13 | 0 | 0 |
| `48_round.test` | 18 | 0 | 0 |
| `49_trunc.test` | 16 | 0 | 0 |
| `50_nullif.test` | 16 | 0 | 0 |
| `51_greatest_least.test` | 18 | 0 | 0 |
| `52_mod.test` | 14 | 0 | 0 |
| `53_power.test` | 13 | 0 | 0 |
| `54_sqrt.test` | 8 | 0 | 0 |
| `55_sign.test` | 10 | 0 | 0 |
| `56_random.test` | 5 | 0 | 0 |
| `57_translate.test` | 12 | 0 | 0 |
| `58_uuid.test` | 20 | 0 | 0 |
| `59_string_agg.test` | 12 | 0 | 0 |
| `60_bool_agg.test` | 11 | 0 | 0 |
| `61_json_build.test` | 25 | 0 | 0 |
| `62_mysql_time_alias.test` | 15 | 0 | 0 |
| `63_session_funcs.test` | 8 | 0 | 0 |
| `64_pg_typeof.test` | 15 | 0 | 0 |
| `65_pg_time.test` | 10 | 0 | 0 |
| `66_mysql_year.test` | 13 | 0 | 0 |
| `67_pg_timetz.test` | 11 | 0 | 0 |
| `68_pg_money.test` | 10 | 0 | 0 |
| `69_mysql_inline_enum.test` | 11 | 0 | 0 |
| `70_mysql_inline_set.test` | 10 | 0 | 0 |
| `71_pg_range.test` | 12 | 0 | 0 |
| `72_pg_hstore.test` | 9 | 0 | 0 |
| `73_pg_array_2d.test` | 9 | 0 | 0 |
| `74_inet_contains.test` | 17 | 0 | 0 |

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
