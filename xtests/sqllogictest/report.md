# SPG conformance baseline

Per-corpus pass / fail / skip:

| corpus | pass | fail | skip | % pass | ran in |
|---|---|---|---|---|---|
| `15_regressions` | 104 | 0 | 0 | 100.0% | postgres × 7 |
| `duckdb` | 170 | 0 | 0 | 100.0% | postgres × 21 |
| `mysql` | 387 | 4 | 0 | 99.0% | mysql × 24 |
| `pg_regress` | 1506 | 0 | 0 | 100.0% | postgres × 89 |
| `pgvector` | 76 | 0 | 0 | 100.0% | postgres × 9 |
| `spg_baseline/01_basic_dml` | 127 | 0 | 0 | 100.0% | postgres × 15 |
| `spg_baseline/02_data_types` | 116 | 0 | 0 | 100.0% | postgres × 18 |
| `spg_baseline/03_composite_domain` | 6 | 0 | 0 | 100.0% | postgres × 1 |
| `spg_baseline/04_joins` | 50 | 0 | 0 | 100.0% | postgres × 7 |
| `spg_baseline/05_aggregates` | 72 | 0 | 0 | 100.0% | postgres × 15 |
| `spg_baseline/06_subqueries` | 34 | 0 | 0 | 100.0% | postgres × 5 |
| `spg_baseline/07_cte` | 16 | 0 | 0 | 100.0% | postgres × 4 |
| `spg_baseline/08_partition` | 60 | 0 | 0 | 100.0% | postgres × 4 |
| `spg_baseline/09_indexes` | 32 | 0 | 0 | 100.0% | postgres × 6 |
| `spg_baseline/10_constraints` | 39 | 0 | 0 | 100.0% | postgres × 7 |
| `spg_baseline/11_dialect` | 62 | 0 | 0 | 100.0% | postgres × 11 |
| `spg_baseline/12_explain` | 12 | 0 | 0 | 100.0% | postgres × 3 |
| `spg_baseline/13_recovery` | 31 | 0 | 0 | 100.0% | postgres × 4 |
| `spg_baseline/14_dialect_compat` | 0 | 0 | 0 | 0.0% |  |
| `spg_baseline/15_regressions` | 556 | 0 | 0 | 100.0% | postgres × 33 |
| `spg_baseline/16_isolation` | 18 | 0 | 0 | 100.0% | postgres × 1 |

## Top fail patterns

| count | pattern |
|---|---|
| 1 | `record 11: row mismatch | expected:` |
| 1 | `record 12: row mismatch | expected:` |
| 1 | `record 13: row mismatch | expected:` |

## Per-file detail

### `15_regressions/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `v73813_bare_group_by.test` | 14 | 0 | 0 | postgres |
| `v73814_explain_names_the_index.test` | 7 | 0 | 0 | postgres |
| `v73814_insert_select_tsvector.test` | 7 | 0 | 0 | postgres |
| `v73814_temp_namespace.test` | 10 | 0 | 0 | postgres |
| `v73816_expression_index.test` | 37 | 0 | 0 | postgres |
| `v73816_gin_expression.test` | 21 | 0 | 0 | postgres |
| `v73818_scalar_subquery_types.test` | 8 | 0 | 0 | postgres |

### `duckdb/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `01_select_basic.test` | 8 | 0 | 0 | postgres |
| `02_where_basic.test` | 8 | 0 | 0 | postgres |
| `03_order_by_limit.test` | 8 | 0 | 0 | postgres |
| `04_arith_in_where.test` | 8 | 0 | 0 | postgres |
| `05_boolean_logic.test` | 9 | 0 | 0 | postgres |
| `06_aliases.test` | 7 | 0 | 0 | postgres |
| `07_transactions.test` | 15 | 0 | 0 | postgres |
| `08_create_index_seek.test` | 10 | 0 | 0 | postgres |
| `09_multi_value_insert.test` | 5 | 0 | 0 | postgres |
| `10_is_null_predicates.test` | 7 | 0 | 0 | postgres |
| `11_column_list_insert.test` | 4 | 0 | 0 | postgres |
| `12_string_concat.test` | 4 | 0 | 0 | postgres |
| `13_between_in_like.test` | 9 | 0 | 0 | postgres |
| `14_aggregates.test` | 10 | 0 | 0 | postgres |
| `15_joins.test` | 11 | 0 | 0 | postgres |
| `16_distinct_union.test` | 8 | 0 | 0 | postgres |
| `17_functions.test` | 8 | 0 | 0 | postgres |
| `18_cast_expr.test` | 6 | 0 | 0 | postgres |
| `19_having_and_show.test` | 8 | 0 | 0 | postgres |
| `20_offset_orderby_position.test` | 8 | 0 | 0 | postgres |
| `21_order_by_desc.test` | 9 | 0 | 0 | postgres |

### `mysql/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `01_dialect.test` | 16 | 0 | 0 | mysql |
| `02_int_types.test` | 8 | 0 | 0 | mysql |
| `03_inline_key.test` | 18 | 0 | 0 | mysql |
| `05_delimiter.test` | 9 | 0 | 0 | mysql |
| `06_tinyint_bool.test` | 13 | 0 | 0 | mysql |
| `07_unsigned.test` | 17 | 0 | 0 | mysql |
| `08_create_procedure.test` | 6 | 0 | 0 | mysql |
| `09_if_ifnull.test` | 17 | 0 | 0 | mysql |
| `10_tinyint1_int_coerce.test` | 22 | 0 | 0 | mysql |
| `11_fulltext_gin_seek.test` | 10 | 0 | 0 | mysql |
| `12_collation_index_agreement.test` | 38 | 0 | 0 | mysql |
| `12_unique_collation.test` | 22 | 0 | 0 | mysql |
| `13_pad_semantics.test` | 20 | 0 | 0 | mysql |
| `13_show_databases.test` | 3 | 0 | 0 | mysql |
| `14_mixed_type_compare.test` | 16 | 0 | 0 | mysql |
| `14_show_create_table.test` | 4 | 0 | 0 | mysql |
| `15_pad_by_collation_name.test` | 17 | 4 | 0 | mysql |
| `15_show_indexes_status_processlist.test` | 10 | 0 | 0 | mysql |
| `16_info_schema_mysql.test` | 14 | 0 | 0 | mysql |
| `v73813_distinct_binary_collation.test` | 32 | 0 | 0 | mysql |
| `v73814_distinct_sources.test` | 15 | 0 | 0 | mysql |
| `v73814_expr_collation.test` | 16 | 0 | 0 | mysql |
| `v73814_join_collation.test` | 23 | 0 | 0 | mysql |
| `v73814_setop_collation.test` | 21 | 0 | 0 | mysql |

<details><summary>`15_pad_by_collation_name.test` fail snippets</summary>

- record 11: row mismatch |   expected: ["3"] |   actual:   ["4"]
- record 12: row mismatch |   expected: ["2"] |   actual:   ["1"]
- record 13: row mismatch |   expected: ["2"] |   actual:   ["3"]
</details>

### `pg_regress/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `01_create_table_shapes.test` | 13 | 0 | 0 | postgres |
| `02_insert_shapes.test` | 10 | 0 | 0 | postgres |
| `03_dml_v2_unsupported.test` | 25 | 0 | 0 | postgres |
| `04_pg_types.test` | 29 | 0 | 0 | postgres |
| `05_savepoints.test` | 9 | 0 | 0 | postgres |
| `06_date_time.test` | 15 | 0 | 0 | postgres |
| `07_date_functions.test` | 16 | 0 | 0 | postgres |
| `08_now_and_date_arith.test` | 10 | 0 | 0 | postgres |
| `09_bare_current.test` | 4 | 0 | 0 | postgres |
| `10_interval.test` | 23 | 0 | 0 | postgres |
| `11_date_functions_part2.test` | 25 | 0 | 0 | postgres |
| `12_pg_trgm.test` | 16 | 0 | 0 | postgres |
| `13_timestamptz_offset.test` | 11 | 0 | 0 | postgres |
| `14_disable_trigger_tsvector.test` | 23 | 0 | 0 | postgres |
| `15_do_block_information_schema.test` | 22 | 0 | 0 | postgres |
| `16_sequences.test` | 52 | 0 | 0 | postgres |
| `17_views.test` | 36 | 0 | 0 | postgres |
| `18_materialized_views.test` | 31 | 0 | 0 | postgres |
| `19_enum_types.test` | 19 | 0 | 0 | postgres |
| `20_domain_types.test` | 33 | 0 | 0 | postgres |
| `21_schemas.test` | 19 | 0 | 0 | postgres |
| `22_fulltext_index.test` | 19 | 0 | 0 | postgres |
| `23_statement_timeout.test` | 12 | 0 | 0 | postgres |
| `24_application_name.test` | 7 | 0 | 0 | postgres |
| `25_collate.test` | 29 | 0 | 0 | postgres |
| `26_mysql_view_algorithm.test` | 27 | 0 | 0 | postgres |
| `27_for_update.test` | 17 | 0 | 0 | postgres |
| `28_deferrable.test` | 24 | 0 | 0 | postgres |
| `29_generate_series.test` | 11 | 0 | 0 | postgres |
| `30_limit_extras.test` | 10 | 0 | 0 | postgres |
| `31_regtype_regclass.test` | 10 | 0 | 0 | postgres |
| `32_format.test` | 10 | 0 | 0 | postgres |
| `33_regexp_family.test` | 13 | 0 | 0 | postgres |
| `34_jsonb_path_query.test` | 12 | 0 | 0 | postgres |
| `35_inet_types.test` | 18 | 0 | 0 | postgres |
| `36_concat.test` | 28 | 0 | 0 | postgres |
| `37_concat_ws.test` | 26 | 0 | 0 | postgres |
| `38_now_bare_call.test` | 18 | 0 | 0 | postgres |
| `39_trim_family.test` | 23 | 0 | 0 | postgres |
| `40_replace.test` | 18 | 0 | 0 | postgres |
| `41_split_part.test` | 26 | 0 | 0 | postgres |
| `42_repeat.test` | 13 | 0 | 0 | postgres |
| `43_lpad_rpad.test` | 23 | 0 | 0 | postgres |
| `44_strpos.test` | 21 | 0 | 0 | postgres |
| `45_left_right.test` | 28 | 0 | 0 | postgres |
| `46_floor.test` | 15 | 0 | 0 | postgres |
| `47_ceil.test` | 13 | 0 | 0 | postgres |
| `48_round.test` | 19 | 0 | 0 | postgres |
| `49_trunc.test` | 16 | 0 | 0 | postgres |
| `50_nullif.test` | 18 | 0 | 0 | postgres |
| `51_greatest_least.test` | 19 | 0 | 0 | postgres |
| `52_mod.test` | 15 | 0 | 0 | postgres |
| `53_power.test` | 13 | 0 | 0 | postgres |
| `54_sqrt.test` | 8 | 0 | 0 | postgres |
| `55_sign.test` | 11 | 0 | 0 | postgres |
| `56_random.test` | 6 | 0 | 0 | postgres |
| `57_translate.test` | 12 | 0 | 0 | postgres |
| `58_uuid.test` | 23 | 0 | 0 | postgres |
| `59_string_agg.test` | 14 | 0 | 0 | postgres |
| `60_bool_agg.test` | 12 | 0 | 0 | postgres |
| `61_json_build.test` | 25 | 0 | 0 | postgres |
| `62_mysql_time_alias.test` | 15 | 0 | 0 | postgres |
| `63_session_funcs.test` | 9 | 0 | 0 | postgres |
| `64_pg_typeof.test` | 16 | 0 | 0 | postgres |
| `65_pg_time.test` | 11 | 0 | 0 | postgres |
| `66_mysql_year.test` | 14 | 0 | 0 | postgres |
| `67_pg_timetz.test` | 13 | 0 | 0 | postgres |
| `68_pg_money.test` | 11 | 0 | 0 | postgres |
| `69_mysql_inline_enum.test` | 13 | 0 | 0 | postgres |
| `70_mysql_inline_set.test` | 11 | 0 | 0 | postgres |
| `71_pg_range.test` | 13 | 0 | 0 | postgres |
| `72_pg_hstore.test` | 10 | 0 | 0 | postgres |
| `73_pg_array_2d.test` | 10 | 0 | 0 | postgres |
| `74_inet_contains.test` | 18 | 0 | 0 | postgres |
| `75_fetch_with_ties.test` | 11 | 0 | 0 | postgres |
| `76_setof_aggregate.test` | 9 | 0 | 0 | postgres |
| `77_window_join.test` | 11 | 0 | 0 | postgres |
| `78_merge_statement.test` | 32 | 0 | 0 | postgres |
| `79_lateral_join.test` | 11 | 0 | 0 | postgres |
| `80_pg_type_view.test` | 6 | 0 | 0 | postgres |
| `81_pg_proc_view.test` | 4 | 0 | 0 | postgres |
| `82_pg_namespace_view.test` | 5 | 0 | 0 | postgres |
| `83_pg_indexes_view.test` | 7 | 0 | 0 | postgres |
| `84_pg_constraint_view.test` | 6 | 0 | 0 | postgres |
| `85_pg_database_roles_view.test` | 3 | 0 | 0 | postgres |
| `86_pg_views_view.test` | 6 | 0 | 0 | postgres |
| `87_pg_settings_view.test` | 6 | 0 | 0 | postgres |
| `88_boolean.test` | 80 | 0 | 0 | postgres |
| `90_wire_text_forms.test` | 22 | 0 | 0 | postgres |

### `pgvector/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `01_create_vector_column.test` | 10 | 0 | 0 | postgres |
| `02_insert_vector_literal.test` | 7 | 0 | 0 | postgres |
| `03_dim_mismatch.test` | 5 | 0 | 0 | postgres |
| `04_l2_distance_order_limit.test` | 9 | 0 | 0 | postgres |
| `05_vector_in_transaction.test` | 11 | 0 | 0 | postgres |
| `06_distance_variants.test` | 7 | 0 | 0 | postgres |
| `07_cast_vector_literal.test` | 4 | 0 | 0 | postgres |
| `08_hnsw_knn.test` | 12 | 0 | 0 | postgres |
| `09_hnsw_metrics_and_filter.test` | 11 | 0 | 0 | postgres |

### `spg_baseline/01_basic_dml/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `case_when.test` | 4 | 0 | 0 | postgres |
| `coalesce_nullif.test` | 5 | 0 | 0 | postgres |
| `delete_basic.test` | 9 | 0 | 0 | postgres |
| `in_list.test` | 5 | 0 | 0 | postgres |
| `insert_basic.test` | 11 | 0 | 0 | postgres |
| `returning.test` | 7 | 0 | 0 | postgres |
| `select_alias.test` | 5 | 0 | 0 | postgres |
| `select_basic.test` | 10 | 0 | 0 | postgres |
| `select_distinct.test` | 5 | 0 | 0 | postgres |
| `select_order_limit.test` | 14 | 0 | 0 | postgres |
| `select_where.test` | 20 | 0 | 0 | postgres |
| `union_intersect_except.test` | 8 | 0 | 0 | postgres |
| `update_basic.test` | 11 | 0 | 0 | postgres |
| `upsert_on_conflict.test` | 9 | 0 | 0 | postgres |
| `values_constructor.test` | 4 | 0 | 0 | postgres |

### `spg_baseline/02_data_types/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `array_ops.test` | 4 | 0 | 0 | postgres |
| `array_scalar.test` | 5 | 0 | 0 | postgres |
| `bool.test` | 6 | 0 | 0 | postgres |
| `bpchar.test` | 22 | 0 | 0 | postgres |
| `bytea.test` | 4 | 0 | 0 | postgres |
| `date_functions.test` | 7 | 0 | 0 | postgres |
| `date_time.test` | 6 | 0 | 0 | postgres |
| `decimal_numeric.test` | 6 | 0 | 0 | postgres |
| `enum.test` | 5 | 0 | 0 | postgres |
| `float_types.test` | 5 | 0 | 0 | postgres |
| `integer_arith.test` | 7 | 0 | 0 | postgres |
| `integer_types.test` | 7 | 0 | 0 | postgres |
| `interval.test` | 5 | 0 | 0 | postgres |
| `json_jsonb.test` | 6 | 0 | 0 | postgres |
| `now_current.test` | 2 | 0 | 0 | postgres |
| `text_funcs.test` | 6 | 0 | 0 | postgres |
| `text_varchar_char.test` | 7 | 0 | 0 | postgres |
| `uuid.test` | 6 | 0 | 0 | postgres |

### `spg_baseline/03_composite_domain/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `domain_types.test` | 6 | 0 | 0 | postgres |

### `spg_baseline/04_joins/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `cross_join.test` | 7 | 0 | 0 | postgres |
| `inner_join.test` | 7 | 0 | 0 | postgres |
| `left_join.test` | 7 | 0 | 0 | postgres |
| `left_join_filter.test` | 8 | 0 | 0 | postgres |
| `multi_join.test` | 10 | 0 | 0 | postgres |
| `self_join.test` | 4 | 0 | 0 | postgres |
| `using_clause.test` | 7 | 0 | 0 | postgres |

### `spg_baseline/05_aggregates/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `agg_empty.test` | 5 | 0 | 0 | postgres |
| `agg_no_group.test` | 6 | 0 | 0 | postgres |
| `array_agg.test` | 4 | 0 | 0 | postgres |
| `bool_agg.test` | 5 | 0 | 0 | postgres |
| `count_sum_avg.test` | 7 | 0 | 0 | postgres |
| `distinct.test` | 5 | 0 | 0 | postgres |
| `distinct_in_agg.test` | 6 | 0 | 0 | postgres |
| `filter_clause.test` | 4 | 0 | 0 | postgres |
| `group_by.test` | 5 | 0 | 0 | postgres |
| `group_by_all.test` | 4 | 0 | 0 | postgres |
| `having.test` | 4 | 0 | 0 | postgres |
| `min_max.test` | 5 | 0 | 0 | postgres |
| `string_agg.test` | 4 | 0 | 0 | postgres |
| `window_basic.test` | 4 | 0 | 0 | postgres |
| `window_frame.test` | 4 | 0 | 0 | postgres |

### `spg_baseline/06_subqueries/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `any_all.test` | 4 | 0 | 0 | postgres |
| `correlated_subq.test` | 7 | 0 | 0 | postgres |
| `exists.test` | 8 | 0 | 0 | postgres |
| `in_subq.test` | 8 | 0 | 0 | postgres |
| `scalar_subq.test` | 7 | 0 | 0 | postgres |

### `spg_baseline/07_cte/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `basic_cte.test` | 4 | 0 | 0 | postgres |
| `cte_chain.test` | 4 | 0 | 0 | postgres |
| `multi_cte.test` | 4 | 0 | 0 | postgres |
| `writable_cte.test` | 4 | 0 | 0 | postgres |

### `spg_baseline/08_partition/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `partition_attach_detach.test` | 18 | 0 | 0 | postgres |
| `partition_explain_kept_names.test` | 11 | 0 | 0 | postgres |
| `partition_hash_basic.test` | 15 | 0 | 0 | postgres |
| `partition_list_basic.test` | 16 | 0 | 0 | postgres |

### `spg_baseline/09_indexes/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `btree_basic.test` | 6 | 0 | 0 | postgres |
| `btree_unique.test` | 5 | 0 | 0 | postgres |
| `expression_index.test` | 5 | 0 | 0 | postgres |
| `index_drop.test` | 6 | 0 | 0 | postgres |
| `multi_column_index.test` | 5 | 0 | 0 | postgres |
| `partial_index.test` | 5 | 0 | 0 | postgres |

### `spg_baseline/10_constraints/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `check_constraint.test` | 5 | 0 | 0 | postgres |
| `fk_cascade.test` | 8 | 0 | 0 | postgres |
| `foreign_key.test` | 7 | 0 | 0 | postgres |
| `multi_pk.test` | 5 | 0 | 0 | postgres |
| `not_null.test` | 4 | 0 | 0 | postgres |
| `primary_key.test` | 6 | 0 | 0 | postgres |
| `serial.test` | 4 | 0 | 0 | postgres |

### `spg_baseline/11_dialect/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `pg_array_ops.test` | 4 | 0 | 0 | postgres |
| `pg_cast_expr.test` | 5 | 0 | 0 | postgres |
| `pg_concat_op.test` | 4 | 0 | 0 | postgres |
| `pg_default_clause.test` | 5 | 0 | 0 | postgres |
| `pg_information_schema.test` | 3 | 0 | 0 | postgres |
| `pg_jsonb_ops.test` | 5 | 0 | 0 | postgres |
| `pg_like_ilike.test` | 7 | 0 | 0 | postgres |
| `pg_returning.test` | 3 | 0 | 0 | postgres |
| `pg_sequence.test` | 4 | 0 | 0 | postgres |
| `pg_show_session_params.test` | 16 | 0 | 0 | postgres |
| `pg_string_funcs.test` | 6 | 0 | 0 | postgres |

### `spg_baseline/12_explain/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `explain_analyze.test` | 4 | 0 | 0 | postgres |
| `explain_basic.test` | 4 | 0 | 0 | postgres |
| `explain_costs_off.test` | 4 | 0 | 0 | postgres |

### `spg_baseline/13_recovery/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `commit.test` | 7 | 0 | 0 | postgres |
| `rollback.test` | 7 | 0 | 0 | postgres |
| `savepoint.test` | 9 | 0 | 0 | postgres |
| `tx_visibility.test` | 8 | 0 | 0 | postgres |

### `spg_baseline/14_dialect_compat/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|

### `spg_baseline/15_regressions/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `distinct_index_walk.test` | 16 | 0 | 0 | postgres |
| `index_order_stream.test` | 18 | 0 | 0 | postgres |
| `indexkey_numeric_bytea.test` | 45 | 0 | 0 | postgres |
| `int_key_order_lane.test` | 20 | 0 | 0 | postgres |
| `join_key_type_matrix.test` | 45 | 0 | 0 | postgres |
| `join_predicate_type_seek.test` | 29 | 0 | 0 | postgres |
| `k02_in_list_visitor.test` | 8 | 0 | 0 | postgres |
| `orderby_null_index_walk.test` | 14 | 0 | 0 | postgres |
| `parser_gaps_sentori.test` | 31 | 0 | 0 | postgres |
| `posting_list_blocks.test` | 17 | 0 | 0 | postgres |
| `pred_int_arith_lane.test` | 17 | 0 | 0 | postgres |
| `range_index_one_sided.test` | 28 | 0 | 0 | postgres |
| `round_14_text_jumbo.test` | 8 | 0 | 0 | postgres |
| `round_20_typed_agg_columns.test` | 8 | 0 | 0 | postgres |
| `round_25_in_list_flat.test` | 9 | 0 | 0 | postgres |
| `round_27_returning_type.test` | 6 | 0 | 0 | postgres |
| `round_28_update_correlated.test` | 11 | 0 | 0 | postgres |
| `round_29_filter_clause.test` | 6 | 0 | 0 | postgres |
| `text_concat_real.test` | 12 | 0 | 0 | postgres |
| `v73811_brin_prune.test` | 17 | 0 | 0 | postgres |
| `v73812_gin_brin_intersect.test` | 13 | 0 | 0 | postgres |
| `v7381_ledger_fixes.test` | 31 | 0 | 0 | postgres |
| `v7382_collate_c_escape_hatch.test` | 6 | 0 | 0 | postgres |
| `v7382_drop_column_checks.test` | 12 | 0 | 0 | postgres |
| `v7382_returning_xmax_is_new.test` | 7 | 0 | 0 | postgres |
| `v7383_add_column_inline_check.test` | 13 | 0 | 0 | postgres |
| `v7384_array_returning_function.test` | 15 | 0 | 0 | postgres |
| `v7385_on_conflict_partial_index.test` | 23 | 0 | 0 | postgres |
| `v7388_jsonb_column_validates.test` | 16 | 0 | 0 | postgres |
| `v7388_qual_order.test` | 8 | 0 | 0 | postgres |
| `v7389_jsonb_containment.test` | 14 | 0 | 0 | postgres |
| `v7389_jsonb_string_result.test` | 10 | 0 | 0 | postgres |
| `wire_streaming_declines.test` | 23 | 0 | 0 | postgres |

### `spg_baseline/16_isolation/`

| file | pass | fail | skip | ran in |
|---|---|---|---|---|
| `set_transaction_isolation_level.test` | 18 | 0 | 0 | postgres |
