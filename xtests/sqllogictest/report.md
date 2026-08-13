# SPG conformance baseline

Per-corpus pass / fail / skip:

| corpus | pass | fail | skip | % pass |
|---|---|---|---|---|
| `duckdb` | 148 | 0 | 0 | 100.0% |
| `mysql` | 142 | 0 | 0 | 100.0% |
| `pg_regress` | 1371 | 0 | 0 | 100.0% |
| `pgvector` | 63 | 0 | 0 | 100.0% |
| `spg_baseline/01_basic_dml` | 110 | 0 | 0 | 100.0% |
| `spg_baseline/02_data_types` | 97 | 0 | 0 | 100.0% |
| `spg_baseline/03_composite_domain` | 5 | 0 | 0 | 100.0% |
| `spg_baseline/04_joins` | 36 | 0 | 0 | 100.0% |
| `spg_baseline/05_aggregates` | 57 | 0 | 0 | 100.0% |
| `spg_baseline/06_subqueries` | 25 | 0 | 0 | 100.0% |
| `spg_baseline/07_cte` | 12 | 0 | 0 | 100.0% |
| `spg_baseline/08_partition` | 45 | 0 | 0 | 100.0% |
| `spg_baseline/09_indexes` | 26 | 0 | 0 | 100.0% |
| `spg_baseline/10_constraints` | 30 | 0 | 0 | 100.0% |
| `spg_baseline/11_dialect` | 53 | 0 | 0 | 100.0% |
| `spg_baseline/12_explain` | 9 | 0 | 0 | 100.0% |
| `spg_baseline/13_recovery` | 27 | 0 | 0 | 100.0% |
| `spg_baseline/14_dialect_compat` | 0 | 0 | 0 | 0.0% |
| `spg_baseline/15_regressions` | 69 | 0 | 0 | 100.0% |
| `spg_baseline/16_isolation` | 18 | 0 | 0 | 100.0% |

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
| `02_int_types.test` | 6 | 0 | 0 |
| `03_inline_key.test` | 17 | 0 | 0 |
| `05_delimiter.test` | 7 | 0 | 0 |
| `06_tinyint_bool.test` | 9 | 0 | 0 |
| `07_unsigned.test` | 12 | 0 | 0 |
| `08_create_procedure.test` | 5 | 0 | 0 |
| `09_if_ifnull.test` | 15 | 0 | 0 |
| `10_tinyint1_int_coerce.test` | 16 | 0 | 0 |
| `11_fulltext_gin_seek.test` | 8 | 0 | 0 |
| `12_unique_collation.test` | 13 | 0 | 0 |
| `13_show_databases.test` | 2 | 0 | 0 |
| `14_show_create_table.test` | 2 | 0 | 0 |
| `15_show_indexes_status_processlist.test` | 8 | 0 | 0 |
| `16_info_schema_mysql.test` | 10 | 0 | 0 |

### `pg_regress/`

| file | pass | fail | skip |
|---|---|---|---|
| `01_create_table_shapes.test` | 8 | 0 | 0 |
| `02_insert_shapes.test` | 9 | 0 | 0 |
| `03_dml_v2_unsupported.test` | 25 | 0 | 0 |
| `04_pg_types.test` | 24 | 0 | 0 |
| `05_savepoints.test` | 8 | 0 | 0 |
| `06_date_time.test` | 13 | 0 | 0 |
| `07_date_functions.test` | 15 | 0 | 0 |
| `08_now_and_date_arith.test` | 10 | 0 | 0 |
| `09_bare_current.test` | 4 | 0 | 0 |
| `10_interval.test` | 23 | 0 | 0 |
| `11_date_functions_part2.test` | 25 | 0 | 0 |
| `12_pg_trgm.test` | 16 | 0 | 0 |
| `13_timestamptz_offset.test` | 11 | 0 | 0 |
| `14_disable_trigger_tsvector.test` | 23 | 0 | 0 |
| `15_do_block_information_schema.test` | 22 | 0 | 0 |
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
| `31_regtype_regclass.test` | 9 | 0 | 0 |
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
| `67_pg_timetz.test` | 12 | 0 | 0 |
| `68_pg_money.test` | 10 | 0 | 0 |
| `69_mysql_inline_enum.test` | 11 | 0 | 0 |
| `70_mysql_inline_set.test` | 10 | 0 | 0 |
| `71_pg_range.test` | 12 | 0 | 0 |
| `72_pg_hstore.test` | 9 | 0 | 0 |
| `73_pg_array_2d.test` | 9 | 0 | 0 |
| `74_inet_contains.test` | 17 | 0 | 0 |
| `75_fetch_with_ties.test` | 10 | 0 | 0 |
| `76_setof_aggregate.test` | 9 | 0 | 0 |
| `77_window_join.test` | 9 | 0 | 0 |
| `78_merge_statement.test` | 24 | 0 | 0 |
| `79_lateral_join.test` | 9 | 0 | 0 |
| `80_pg_type_view.test` | 6 | 0 | 0 |
| `81_pg_proc_view.test` | 4 | 0 | 0 |
| `82_pg_namespace_view.test` | 5 | 0 | 0 |
| `83_pg_indexes_view.test` | 6 | 0 | 0 |
| `84_pg_constraint_view.test` | 4 | 0 | 0 |
| `85_pg_database_roles_view.test` | 3 | 0 | 0 |
| `86_pg_views_view.test` | 4 | 0 | 0 |
| `87_pg_settings_view.test` | 6 | 0 | 0 |
| `88_boolean.test` | 80 | 0 | 0 |
| `90_wire_text_forms.test` | 22 | 0 | 0 |

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

### `spg_baseline/01_basic_dml/`

| file | pass | fail | skip |
|---|---|---|---|
| `case_when.test` | 3 | 0 | 0 |
| `coalesce_nullif.test` | 4 | 0 | 0 |
| `delete_basic.test` | 8 | 0 | 0 |
| `in_list.test` | 4 | 0 | 0 |
| `insert_basic.test` | 9 | 0 | 0 |
| `returning.test` | 6 | 0 | 0 |
| `select_alias.test` | 4 | 0 | 0 |
| `select_basic.test` | 9 | 0 | 0 |
| `select_distinct.test` | 4 | 0 | 0 |
| `select_order_limit.test` | 13 | 0 | 0 |
| `select_where.test` | 19 | 0 | 0 |
| `union_intersect_except.test` | 6 | 0 | 0 |
| `update_basic.test` | 10 | 0 | 0 |
| `upsert_on_conflict.test` | 8 | 0 | 0 |
| `values_constructor.test` | 3 | 0 | 0 |

### `spg_baseline/02_data_types/`

| file | pass | fail | skip |
|---|---|---|---|
| `array_ops.test` | 3 | 0 | 0 |
| `array_scalar.test` | 4 | 0 | 0 |
| `bool.test` | 5 | 0 | 0 |
| `bpchar.test` | 19 | 0 | 0 |
| `bytea.test` | 3 | 0 | 0 |
| `date_functions.test` | 6 | 0 | 0 |
| `date_time.test` | 5 | 0 | 0 |
| `decimal_numeric.test` | 5 | 0 | 0 |
| `enum.test` | 4 | 0 | 0 |
| `float_types.test` | 4 | 0 | 0 |
| `integer_arith.test` | 6 | 0 | 0 |
| `integer_types.test` | 6 | 0 | 0 |
| `interval.test` | 4 | 0 | 0 |
| `json_jsonb.test` | 5 | 0 | 0 |
| `now_current.test` | 2 | 0 | 0 |
| `text_funcs.test` | 5 | 0 | 0 |
| `text_varchar_char.test` | 6 | 0 | 0 |
| `uuid.test` | 5 | 0 | 0 |

### `spg_baseline/03_composite_domain/`

| file | pass | fail | skip |
|---|---|---|---|
| `domain_types.test` | 5 | 0 | 0 |

### `spg_baseline/04_joins/`

| file | pass | fail | skip |
|---|---|---|---|
| `cross_join.test` | 5 | 0 | 0 |
| `inner_join.test` | 5 | 0 | 0 |
| `left_join.test` | 5 | 0 | 0 |
| `left_join_filter.test` | 6 | 0 | 0 |
| `multi_join.test` | 7 | 0 | 0 |
| `self_join.test` | 3 | 0 | 0 |
| `using_clause.test` | 5 | 0 | 0 |

### `spg_baseline/05_aggregates/`

| file | pass | fail | skip |
|---|---|---|---|
| `agg_empty.test` | 4 | 0 | 0 |
| `agg_no_group.test` | 5 | 0 | 0 |
| `array_agg.test` | 3 | 0 | 0 |
| `bool_agg.test` | 4 | 0 | 0 |
| `count_sum_avg.test` | 6 | 0 | 0 |
| `distinct.test` | 4 | 0 | 0 |
| `distinct_in_agg.test` | 5 | 0 | 0 |
| `filter_clause.test` | 3 | 0 | 0 |
| `group_by.test` | 4 | 0 | 0 |
| `group_by_all.test` | 3 | 0 | 0 |
| `having.test` | 3 | 0 | 0 |
| `min_max.test` | 4 | 0 | 0 |
| `string_agg.test` | 3 | 0 | 0 |
| `window_basic.test` | 3 | 0 | 0 |
| `window_frame.test` | 3 | 0 | 0 |

### `spg_baseline/06_subqueries/`

| file | pass | fail | skip |
|---|---|---|---|
| `any_all.test` | 3 | 0 | 0 |
| `correlated_subq.test` | 5 | 0 | 0 |
| `exists.test` | 6 | 0 | 0 |
| `in_subq.test` | 6 | 0 | 0 |
| `scalar_subq.test` | 5 | 0 | 0 |

### `spg_baseline/07_cte/`

| file | pass | fail | skip |
|---|---|---|---|
| `basic_cte.test` | 3 | 0 | 0 |
| `cte_chain.test` | 3 | 0 | 0 |
| `multi_cte.test` | 3 | 0 | 0 |
| `writable_cte.test` | 3 | 0 | 0 |

### `spg_baseline/08_partition/`

| file | pass | fail | skip |
|---|---|---|---|
| `partition_attach_detach.test` | 16 | 0 | 0 |
| `partition_explain_kept_names.test` | 7 | 0 | 0 |
| `partition_hash_basic.test` | 10 | 0 | 0 |
| `partition_list_basic.test` | 12 | 0 | 0 |

### `spg_baseline/09_indexes/`

| file | pass | fail | skip |
|---|---|---|---|
| `btree_basic.test` | 5 | 0 | 0 |
| `btree_unique.test` | 4 | 0 | 0 |
| `expression_index.test` | 4 | 0 | 0 |
| `index_drop.test` | 5 | 0 | 0 |
| `multi_column_index.test` | 4 | 0 | 0 |
| `partial_index.test` | 4 | 0 | 0 |

### `spg_baseline/10_constraints/`

| file | pass | fail | skip |
|---|---|---|---|
| `check_constraint.test` | 4 | 0 | 0 |
| `fk_cascade.test` | 6 | 0 | 0 |
| `foreign_key.test` | 5 | 0 | 0 |
| `multi_pk.test` | 4 | 0 | 0 |
| `not_null.test` | 3 | 0 | 0 |
| `primary_key.test` | 5 | 0 | 0 |
| `serial.test` | 3 | 0 | 0 |

### `spg_baseline/11_dialect/`

| file | pass | fail | skip |
|---|---|---|---|
| `pg_array_ops.test` | 3 | 0 | 0 |
| `pg_cast_expr.test` | 4 | 0 | 0 |
| `pg_concat_op.test` | 3 | 0 | 0 |
| `pg_default_clause.test` | 4 | 0 | 0 |
| `pg_information_schema.test` | 2 | 0 | 0 |
| `pg_jsonb_ops.test` | 4 | 0 | 0 |
| `pg_like_ilike.test` | 6 | 0 | 0 |
| `pg_returning.test` | 2 | 0 | 0 |
| `pg_sequence.test` | 4 | 0 | 0 |
| `pg_show_session_params.test` | 16 | 0 | 0 |
| `pg_string_funcs.test` | 5 | 0 | 0 |

### `spg_baseline/12_explain/`

| file | pass | fail | skip |
|---|---|---|---|
| `explain_analyze.test` | 3 | 0 | 0 |
| `explain_basic.test` | 3 | 0 | 0 |
| `explain_costs_off.test` | 3 | 0 | 0 |

### `spg_baseline/13_recovery/`

| file | pass | fail | skip |
|---|---|---|---|
| `commit.test` | 6 | 0 | 0 |
| `rollback.test` | 6 | 0 | 0 |
| `savepoint.test` | 8 | 0 | 0 |
| `tx_visibility.test` | 7 | 0 | 0 |

### `spg_baseline/14_dialect_compat/`

| file | pass | fail | skip |
|---|---|---|---|

### `spg_baseline/15_regressions/`

| file | pass | fail | skip |
|---|---|---|---|
| `k02_in_list_visitor.test` | 6 | 0 | 0 |
| `orderby_null_index_walk.test` | 12 | 0 | 0 |
| `round_14_text_jumbo.test` | 7 | 0 | 0 |
| `round_20_typed_agg_columns.test` | 7 | 0 | 0 |
| `round_25_in_list_flat.test` | 7 | 0 | 0 |
| `round_27_returning_type.test` | 5 | 0 | 0 |
| `round_28_update_correlated.test` | 9 | 0 | 0 |
| `round_29_filter_clause.test` | 5 | 0 | 0 |
| `text_concat_real.test` | 11 | 0 | 0 |

### `spg_baseline/16_isolation/`

| file | pass | fail | skip |
|---|---|---|---|
| `set_transaction_isolation_level.test` | 18 | 0 | 0 |
