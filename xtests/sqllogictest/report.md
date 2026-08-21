# SPG conformance baseline

Per-corpus pass / fail / skip:

| corpus | pass | fail | skip | % pass |
|---|---|---|---|---|
| `duckdb` | 170 | 0 | 0 | 100.0% |
| `mysql` | 170 | 0 | 0 | 100.0% |
| `pg_regress` | 1506 | 0 | 0 | 100.0% |
| `pgvector` | 76 | 0 | 0 | 100.0% |
| `spg_baseline/01_basic_dml` | 127 | 0 | 0 | 100.0% |
| `spg_baseline/02_data_types` | 116 | 0 | 0 | 100.0% |
| `spg_baseline/03_composite_domain` | 6 | 0 | 0 | 100.0% |
| `spg_baseline/04_joins` | 50 | 0 | 0 | 100.0% |
| `spg_baseline/05_aggregates` | 72 | 0 | 0 | 100.0% |
| `spg_baseline/06_subqueries` | 34 | 0 | 0 | 100.0% |
| `spg_baseline/07_cte` | 16 | 0 | 0 | 100.0% |
| `spg_baseline/08_partition` | 60 | 0 | 0 | 100.0% |
| `spg_baseline/09_indexes` | 32 | 0 | 0 | 100.0% |
| `spg_baseline/10_constraints` | 39 | 0 | 0 | 100.0% |
| `spg_baseline/11_dialect` | 62 | 0 | 0 | 100.0% |
| `spg_baseline/12_explain` | 12 | 0 | 0 | 100.0% |
| `spg_baseline/13_recovery` | 31 | 0 | 0 | 100.0% |
| `spg_baseline/14_dialect_compat` | 0 | 0 | 0 | 0.0% |
| `spg_baseline/15_regressions` | 554 | 2 | 0 | 99.6% |
| `spg_baseline/16_isolation` | 18 | 0 | 0 | 100.0% |

## Top fail patterns

| count | pattern |
|---|---|
| 1 | `record 10: row mismatch | expected:` |
| 1 | `record 8: row mismatch | expected:` |

## Per-file detail

### `duckdb/`

| file | pass | fail | skip |
|---|---|---|---|
| `01_select_basic.test` | 8 | 0 | 0 |
| `02_where_basic.test` | 8 | 0 | 0 |
| `03_order_by_limit.test` | 8 | 0 | 0 |
| `04_arith_in_where.test` | 8 | 0 | 0 |
| `05_boolean_logic.test` | 9 | 0 | 0 |
| `06_aliases.test` | 7 | 0 | 0 |
| `07_transactions.test` | 15 | 0 | 0 |
| `08_create_index_seek.test` | 10 | 0 | 0 |
| `09_multi_value_insert.test` | 5 | 0 | 0 |
| `10_is_null_predicates.test` | 7 | 0 | 0 |
| `11_column_list_insert.test` | 4 | 0 | 0 |
| `12_string_concat.test` | 4 | 0 | 0 |
| `13_between_in_like.test` | 9 | 0 | 0 |
| `14_aggregates.test` | 10 | 0 | 0 |
| `15_joins.test` | 11 | 0 | 0 |
| `16_distinct_union.test` | 8 | 0 | 0 |
| `17_functions.test` | 8 | 0 | 0 |
| `18_cast_expr.test` | 6 | 0 | 0 |
| `19_having_and_show.test` | 8 | 0 | 0 |
| `20_offset_orderby_position.test` | 8 | 0 | 0 |
| `21_order_by_desc.test` | 9 | 0 | 0 |

### `mysql/`

| file | pass | fail | skip |
|---|---|---|---|
| `01_dialect.test` | 15 | 0 | 0 |
| `02_int_types.test` | 7 | 0 | 0 |
| `03_inline_key.test` | 17 | 0 | 0 |
| `05_delimiter.test` | 8 | 0 | 0 |
| `06_tinyint_bool.test` | 12 | 0 | 0 |
| `07_unsigned.test` | 16 | 0 | 0 |
| `08_create_procedure.test` | 5 | 0 | 0 |
| `09_if_ifnull.test` | 16 | 0 | 0 |
| `10_tinyint1_int_coerce.test` | 21 | 0 | 0 |
| `11_fulltext_gin_seek.test` | 9 | 0 | 0 |
| `12_unique_collation.test` | 17 | 0 | 0 |
| `13_show_databases.test` | 2 | 0 | 0 |
| `14_show_create_table.test` | 3 | 0 | 0 |
| `15_show_indexes_status_processlist.test` | 9 | 0 | 0 |
| `16_info_schema_mysql.test` | 13 | 0 | 0 |

### `pg_regress/`

| file | pass | fail | skip |
|---|---|---|---|
| `01_create_table_shapes.test` | 13 | 0 | 0 |
| `02_insert_shapes.test` | 10 | 0 | 0 |
| `03_dml_v2_unsupported.test` | 25 | 0 | 0 |
| `04_pg_types.test` | 29 | 0 | 0 |
| `05_savepoints.test` | 9 | 0 | 0 |
| `06_date_time.test` | 15 | 0 | 0 |
| `07_date_functions.test` | 16 | 0 | 0 |
| `08_now_and_date_arith.test` | 10 | 0 | 0 |
| `09_bare_current.test` | 4 | 0 | 0 |
| `10_interval.test` | 23 | 0 | 0 |
| `11_date_functions_part2.test` | 25 | 0 | 0 |
| `12_pg_trgm.test` | 16 | 0 | 0 |
| `13_timestamptz_offset.test` | 11 | 0 | 0 |
| `14_disable_trigger_tsvector.test` | 23 | 0 | 0 |
| `15_do_block_information_schema.test` | 22 | 0 | 0 |
| `16_sequences.test` | 52 | 0 | 0 |
| `17_views.test` | 36 | 0 | 0 |
| `18_materialized_views.test` | 31 | 0 | 0 |
| `19_enum_types.test` | 19 | 0 | 0 |
| `20_domain_types.test` | 33 | 0 | 0 |
| `21_schemas.test` | 19 | 0 | 0 |
| `22_fulltext_index.test` | 19 | 0 | 0 |
| `23_statement_timeout.test` | 12 | 0 | 0 |
| `24_application_name.test` | 7 | 0 | 0 |
| `25_collate.test` | 29 | 0 | 0 |
| `26_mysql_view_algorithm.test` | 27 | 0 | 0 |
| `27_for_update.test` | 17 | 0 | 0 |
| `28_deferrable.test` | 24 | 0 | 0 |
| `29_generate_series.test` | 11 | 0 | 0 |
| `30_limit_extras.test` | 10 | 0 | 0 |
| `31_regtype_regclass.test` | 10 | 0 | 0 |
| `32_format.test` | 10 | 0 | 0 |
| `33_regexp_family.test` | 13 | 0 | 0 |
| `34_jsonb_path_query.test` | 12 | 0 | 0 |
| `35_inet_types.test` | 18 | 0 | 0 |
| `36_concat.test` | 28 | 0 | 0 |
| `37_concat_ws.test` | 26 | 0 | 0 |
| `38_now_bare_call.test` | 18 | 0 | 0 |
| `39_trim_family.test` | 23 | 0 | 0 |
| `40_replace.test` | 18 | 0 | 0 |
| `41_split_part.test` | 26 | 0 | 0 |
| `42_repeat.test` | 13 | 0 | 0 |
| `43_lpad_rpad.test` | 23 | 0 | 0 |
| `44_strpos.test` | 21 | 0 | 0 |
| `45_left_right.test` | 28 | 0 | 0 |
| `46_floor.test` | 15 | 0 | 0 |
| `47_ceil.test` | 13 | 0 | 0 |
| `48_round.test` | 19 | 0 | 0 |
| `49_trunc.test` | 16 | 0 | 0 |
| `50_nullif.test` | 18 | 0 | 0 |
| `51_greatest_least.test` | 19 | 0 | 0 |
| `52_mod.test` | 15 | 0 | 0 |
| `53_power.test` | 13 | 0 | 0 |
| `54_sqrt.test` | 8 | 0 | 0 |
| `55_sign.test` | 11 | 0 | 0 |
| `56_random.test` | 6 | 0 | 0 |
| `57_translate.test` | 12 | 0 | 0 |
| `58_uuid.test` | 23 | 0 | 0 |
| `59_string_agg.test` | 14 | 0 | 0 |
| `60_bool_agg.test` | 12 | 0 | 0 |
| `61_json_build.test` | 25 | 0 | 0 |
| `62_mysql_time_alias.test` | 15 | 0 | 0 |
| `63_session_funcs.test` | 9 | 0 | 0 |
| `64_pg_typeof.test` | 16 | 0 | 0 |
| `65_pg_time.test` | 11 | 0 | 0 |
| `66_mysql_year.test` | 14 | 0 | 0 |
| `67_pg_timetz.test` | 13 | 0 | 0 |
| `68_pg_money.test` | 11 | 0 | 0 |
| `69_mysql_inline_enum.test` | 13 | 0 | 0 |
| `70_mysql_inline_set.test` | 11 | 0 | 0 |
| `71_pg_range.test` | 13 | 0 | 0 |
| `72_pg_hstore.test` | 10 | 0 | 0 |
| `73_pg_array_2d.test` | 10 | 0 | 0 |
| `74_inet_contains.test` | 18 | 0 | 0 |
| `75_fetch_with_ties.test` | 11 | 0 | 0 |
| `76_setof_aggregate.test` | 9 | 0 | 0 |
| `77_window_join.test` | 11 | 0 | 0 |
| `78_merge_statement.test` | 32 | 0 | 0 |
| `79_lateral_join.test` | 11 | 0 | 0 |
| `80_pg_type_view.test` | 6 | 0 | 0 |
| `81_pg_proc_view.test` | 4 | 0 | 0 |
| `82_pg_namespace_view.test` | 5 | 0 | 0 |
| `83_pg_indexes_view.test` | 7 | 0 | 0 |
| `84_pg_constraint_view.test` | 6 | 0 | 0 |
| `85_pg_database_roles_view.test` | 3 | 0 | 0 |
| `86_pg_views_view.test` | 6 | 0 | 0 |
| `87_pg_settings_view.test` | 6 | 0 | 0 |
| `88_boolean.test` | 80 | 0 | 0 |
| `90_wire_text_forms.test` | 22 | 0 | 0 |

### `pgvector/`

| file | pass | fail | skip |
|---|---|---|---|
| `01_create_vector_column.test` | 10 | 0 | 0 |
| `02_insert_vector_literal.test` | 7 | 0 | 0 |
| `03_dim_mismatch.test` | 5 | 0 | 0 |
| `04_l2_distance_order_limit.test` | 9 | 0 | 0 |
| `05_vector_in_transaction.test` | 11 | 0 | 0 |
| `06_distance_variants.test` | 7 | 0 | 0 |
| `07_cast_vector_literal.test` | 4 | 0 | 0 |
| `08_hnsw_knn.test` | 12 | 0 | 0 |
| `09_hnsw_metrics_and_filter.test` | 11 | 0 | 0 |

### `spg_baseline/01_basic_dml/`

| file | pass | fail | skip |
|---|---|---|---|
| `case_when.test` | 4 | 0 | 0 |
| `coalesce_nullif.test` | 5 | 0 | 0 |
| `delete_basic.test` | 9 | 0 | 0 |
| `in_list.test` | 5 | 0 | 0 |
| `insert_basic.test` | 11 | 0 | 0 |
| `returning.test` | 7 | 0 | 0 |
| `select_alias.test` | 5 | 0 | 0 |
| `select_basic.test` | 10 | 0 | 0 |
| `select_distinct.test` | 5 | 0 | 0 |
| `select_order_limit.test` | 14 | 0 | 0 |
| `select_where.test` | 20 | 0 | 0 |
| `union_intersect_except.test` | 8 | 0 | 0 |
| `update_basic.test` | 11 | 0 | 0 |
| `upsert_on_conflict.test` | 9 | 0 | 0 |
| `values_constructor.test` | 4 | 0 | 0 |

### `spg_baseline/02_data_types/`

| file | pass | fail | skip |
|---|---|---|---|
| `array_ops.test` | 4 | 0 | 0 |
| `array_scalar.test` | 5 | 0 | 0 |
| `bool.test` | 6 | 0 | 0 |
| `bpchar.test` | 22 | 0 | 0 |
| `bytea.test` | 4 | 0 | 0 |
| `date_functions.test` | 7 | 0 | 0 |
| `date_time.test` | 6 | 0 | 0 |
| `decimal_numeric.test` | 6 | 0 | 0 |
| `enum.test` | 5 | 0 | 0 |
| `float_types.test` | 5 | 0 | 0 |
| `integer_arith.test` | 7 | 0 | 0 |
| `integer_types.test` | 7 | 0 | 0 |
| `interval.test` | 5 | 0 | 0 |
| `json_jsonb.test` | 6 | 0 | 0 |
| `now_current.test` | 2 | 0 | 0 |
| `text_funcs.test` | 6 | 0 | 0 |
| `text_varchar_char.test` | 7 | 0 | 0 |
| `uuid.test` | 6 | 0 | 0 |

### `spg_baseline/03_composite_domain/`

| file | pass | fail | skip |
|---|---|---|---|
| `domain_types.test` | 6 | 0 | 0 |

### `spg_baseline/04_joins/`

| file | pass | fail | skip |
|---|---|---|---|
| `cross_join.test` | 7 | 0 | 0 |
| `inner_join.test` | 7 | 0 | 0 |
| `left_join.test` | 7 | 0 | 0 |
| `left_join_filter.test` | 8 | 0 | 0 |
| `multi_join.test` | 10 | 0 | 0 |
| `self_join.test` | 4 | 0 | 0 |
| `using_clause.test` | 7 | 0 | 0 |

### `spg_baseline/05_aggregates/`

| file | pass | fail | skip |
|---|---|---|---|
| `agg_empty.test` | 5 | 0 | 0 |
| `agg_no_group.test` | 6 | 0 | 0 |
| `array_agg.test` | 4 | 0 | 0 |
| `bool_agg.test` | 5 | 0 | 0 |
| `count_sum_avg.test` | 7 | 0 | 0 |
| `distinct.test` | 5 | 0 | 0 |
| `distinct_in_agg.test` | 6 | 0 | 0 |
| `filter_clause.test` | 4 | 0 | 0 |
| `group_by.test` | 5 | 0 | 0 |
| `group_by_all.test` | 4 | 0 | 0 |
| `having.test` | 4 | 0 | 0 |
| `min_max.test` | 5 | 0 | 0 |
| `string_agg.test` | 4 | 0 | 0 |
| `window_basic.test` | 4 | 0 | 0 |
| `window_frame.test` | 4 | 0 | 0 |

### `spg_baseline/06_subqueries/`

| file | pass | fail | skip |
|---|---|---|---|
| `any_all.test` | 4 | 0 | 0 |
| `correlated_subq.test` | 7 | 0 | 0 |
| `exists.test` | 8 | 0 | 0 |
| `in_subq.test` | 8 | 0 | 0 |
| `scalar_subq.test` | 7 | 0 | 0 |

### `spg_baseline/07_cte/`

| file | pass | fail | skip |
|---|---|---|---|
| `basic_cte.test` | 4 | 0 | 0 |
| `cte_chain.test` | 4 | 0 | 0 |
| `multi_cte.test` | 4 | 0 | 0 |
| `writable_cte.test` | 4 | 0 | 0 |

### `spg_baseline/08_partition/`

| file | pass | fail | skip |
|---|---|---|---|
| `partition_attach_detach.test` | 18 | 0 | 0 |
| `partition_explain_kept_names.test` | 11 | 0 | 0 |
| `partition_hash_basic.test` | 15 | 0 | 0 |
| `partition_list_basic.test` | 16 | 0 | 0 |

### `spg_baseline/09_indexes/`

| file | pass | fail | skip |
|---|---|---|---|
| `btree_basic.test` | 6 | 0 | 0 |
| `btree_unique.test` | 5 | 0 | 0 |
| `expression_index.test` | 5 | 0 | 0 |
| `index_drop.test` | 6 | 0 | 0 |
| `multi_column_index.test` | 5 | 0 | 0 |
| `partial_index.test` | 5 | 0 | 0 |

### `spg_baseline/10_constraints/`

| file | pass | fail | skip |
|---|---|---|---|
| `check_constraint.test` | 5 | 0 | 0 |
| `fk_cascade.test` | 8 | 0 | 0 |
| `foreign_key.test` | 7 | 0 | 0 |
| `multi_pk.test` | 5 | 0 | 0 |
| `not_null.test` | 4 | 0 | 0 |
| `primary_key.test` | 6 | 0 | 0 |
| `serial.test` | 4 | 0 | 0 |

### `spg_baseline/11_dialect/`

| file | pass | fail | skip |
|---|---|---|---|
| `pg_array_ops.test` | 4 | 0 | 0 |
| `pg_cast_expr.test` | 5 | 0 | 0 |
| `pg_concat_op.test` | 4 | 0 | 0 |
| `pg_default_clause.test` | 5 | 0 | 0 |
| `pg_information_schema.test` | 3 | 0 | 0 |
| `pg_jsonb_ops.test` | 5 | 0 | 0 |
| `pg_like_ilike.test` | 7 | 0 | 0 |
| `pg_returning.test` | 3 | 0 | 0 |
| `pg_sequence.test` | 4 | 0 | 0 |
| `pg_show_session_params.test` | 16 | 0 | 0 |
| `pg_string_funcs.test` | 6 | 0 | 0 |

### `spg_baseline/12_explain/`

| file | pass | fail | skip |
|---|---|---|---|
| `explain_analyze.test` | 4 | 0 | 0 |
| `explain_basic.test` | 4 | 0 | 0 |
| `explain_costs_off.test` | 4 | 0 | 0 |

### `spg_baseline/13_recovery/`

| file | pass | fail | skip |
|---|---|---|---|
| `commit.test` | 7 | 0 | 0 |
| `rollback.test` | 7 | 0 | 0 |
| `savepoint.test` | 9 | 0 | 0 |
| `tx_visibility.test` | 8 | 0 | 0 |

### `spg_baseline/14_dialect_compat/`

| file | pass | fail | skip |
|---|---|---|---|

### `spg_baseline/15_regressions/`

| file | pass | fail | skip |
|---|---|---|---|
| `distinct_index_walk.test` | 16 | 0 | 0 |
| `index_order_stream.test` | 18 | 0 | 0 |
| `indexkey_numeric_bytea.test` | 45 | 0 | 0 |
| `int_key_order_lane.test` | 20 | 0 | 0 |
| `join_key_type_matrix.test` | 45 | 0 | 0 |
| `join_predicate_type_seek.test` | 29 | 0 | 0 |
| `k02_in_list_visitor.test` | 8 | 0 | 0 |
| `orderby_null_index_walk.test` | 14 | 0 | 0 |
| `parser_gaps_sentori.test` | 31 | 0 | 0 |
| `posting_list_blocks.test` | 17 | 0 | 0 |
| `pred_int_arith_lane.test` | 17 | 0 | 0 |
| `range_index_one_sided.test` | 28 | 0 | 0 |
| `round_14_text_jumbo.test` | 8 | 0 | 0 |
| `round_20_typed_agg_columns.test` | 8 | 0 | 0 |
| `round_25_in_list_flat.test` | 9 | 0 | 0 |
| `round_27_returning_type.test` | 6 | 0 | 0 |
| `round_28_update_correlated.test` | 11 | 0 | 0 |
| `round_29_filter_clause.test` | 6 | 0 | 0 |
| `text_concat_real.test` | 12 | 0 | 0 |
| `v73811_brin_prune.test` | 17 | 0 | 0 |
| `v73812_gin_brin_intersect.test` | 11 | 2 | 0 |
| `v7381_ledger_fixes.test` | 31 | 0 | 0 |
| `v7382_collate_c_escape_hatch.test` | 6 | 0 | 0 |
| `v7382_drop_column_checks.test` | 12 | 0 | 0 |
| `v7382_returning_xmax_is_new.test` | 7 | 0 | 0 |
| `v7383_add_column_inline_check.test` | 13 | 0 | 0 |
| `v7384_array_returning_function.test` | 15 | 0 | 0 |
| `v7385_on_conflict_partial_index.test` | 23 | 0 | 0 |
| `v7388_jsonb_column_validates.test` | 16 | 0 | 0 |
| `v7388_qual_order.test` | 8 | 0 | 0 |
| `v7389_jsonb_containment.test` | 14 | 0 | 0 |
| `v7389_jsonb_string_result.test` | 10 | 0 | 0 |
| `wire_streaming_declines.test` | 23 | 0 | 0 |

<details><summary>`v73812_gin_brin_intersect.test` fail snippets</summary>

- record 8: row mismatch |   expected: ["1000"] |   actual:   ["999"]
- record 10: row mismatch |   expected: ["1"] |   actual:   ["0"]
</details>

### `spg_baseline/16_isolation/`

| file | pass | fail | skip |
|---|---|---|---|
| `set_transaction_isolation_level.test` | 18 | 0 | 0 |
