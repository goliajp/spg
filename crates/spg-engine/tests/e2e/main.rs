//! v7.20 test-speed A (part 2) — the 161 standalone spg-engine
//! integration-test binaries merged into ONE target: one link
//! instead of 161 (each link cost ~10 s — the dominant share of
//! the serial workspace gate), and libtest parallelises all
//! modules in-process. Pure in-memory engine tests: no servers,
//! no file locks, no env mutation — fully concurrency-safe.
//! perf_* targets stay standalone (timing-sensitive).

mod e2e;
mod e2e_agg_subquery_pullup;
mod e2e_alter_add_column;
mod e2e_array_agg_argmax;
mod e2e_array_family;
mod e2e_array_ops;
mod e2e_as_of_segment;
mod e2e_audit_n6_remainder;
mod e2e_bool_agg;
mod e2e_brin;
mod e2e_bytea;
mod e2e_bytea_ops;
mod e2e_cast_targets;
mod e2e_ceil;
mod e2e_cold_rows_per_table;
mod e2e_collate;
mod e2e_collate_order_group;
mod e2e_compaction;
mod e2e_compiled_expr;
mod e2e_concat;
mod e2e_concat_ws;
mod e2e_corr_limit1_pullup;
mod e2e_correlated_subquery_batch;
mod e2e_count_short_circuit;
mod e2e_create_extension;
mod e2e_deferrable;
mod e2e_delimiter;
mod e2e_do_block;
mod e2e_domain_type;
mod e2e_enum_type;
mod e2e_exists_decorrelation;
#[allow(
    clippy::cast_lossless,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::uninlined_format_args
)]
mod e2e_explain_analyze;
mod e2e_expression_index;
mod e2e_fetch_with_ties;
mod e2e_fk_advanced;
mod e2e_fk_alter;
mod e2e_fk_catalog;
mod e2e_fk_chaos;
mod e2e_fk_delete_cascade;
mod e2e_fk_delete_restrict;
mod e2e_fk_delete_set;
mod e2e_fk_insert;
mod e2e_fk_update;
mod e2e_floor;
mod e2e_for_update;
mod e2e_format;
mod e2e_fts;
mod e2e_fulltext_gin_seek;
mod e2e_fulltext_index;
mod e2e_fulltext_planner;
mod e2e_generate_series;
mod e2e_generated_stored;
mod e2e_geometry;
mod e2e_get_ddl;
mod e2e_gin_trgm_partial;
mod e2e_greatest_least;
mod e2e_group_by_all;
mod e2e_hnsw_opclass;
mod e2e_in_list_depth;
mod e2e_in_list_index_seek;
mod e2e_include;
mod e2e_index_advisor;
mod e2e_inet_contains;
mod e2e_inet_types;
mod e2e_info_mysql_views;
mod e2e_inline_column_constraints;
mod e2e_inline_pk;
mod e2e_insert_select;
mod e2e_int_array;
mod e2e_interval_array;
mod e2e_interval_cast;
mod e2e_interval_column_storage;
mod e2e_join_peer_predicate;
mod e2e_json_build;
mod e2e_json_path;
mod e2e_jsonb;
mod e2e_jsonb_epic6;
mod e2e_jsonb_path_query;
mod e2e_key_column;
mod e2e_lateral_join;
mod e2e_left_right;
mod e2e_limit_placeholder;
mod e2e_lpad_rpad;
mod e2e_materialized_view;
#[allow(
    clippy::cast_lossless,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::uninlined_format_args
)]
mod e2e_memoize;
mod e2e_memory_stats;
mod e2e_merge;
mod e2e_mod;
mod e2e_multi_col_exists_pullup;
mod e2e_multi_col_index;
mod e2e_multirange;
mod e2e_mysql_conditional;
mod e2e_mysql_inline_enum;
mod e2e_mysql_inline_set;
mod e2e_mysql_procedure;
mod e2e_mysql_time_alias;
mod e2e_mysql_tinyint1_coerce;
mod e2e_mysql_user_db;
mod e2e_mysql_view_algorithm;
mod e2e_mysql_year;
mod e2e_network_bit_xml;
mod e2e_now_bare_call;
mod e2e_nullif;
mod e2e_on_conflict_composite;
mod e2e_on_conflict_nothing;
mod e2e_on_conflict_update;
mod e2e_on_update_current_timestamp;
mod e2e_order_by_multi;
mod e2e_partial_index;
mod e2e_partition_by_range;
mod e2e_per_table_budget;
mod e2e_pg_array_2d;
mod e2e_pg_constraint_view;
mod e2e_pg_customer_parity;
mod e2e_pg_database_roles_view;
mod e2e_pg_hstore;
mod e2e_pg_indexes_view;
mod e2e_pg_money;
mod e2e_pg_namespace_view;
mod e2e_pg_proc_view;
mod e2e_pg_range;
mod e2e_pg_settings_view;
mod e2e_pg_time;
mod e2e_pg_timetz;
mod e2e_pg_type_view;
mod e2e_pg_typeof;
mod e2e_pg_views_view;
mod e2e_pgdump_compat;
mod e2e_plan_cache;
mod e2e_plan_cache_invalidation;
mod e2e_power;
mod e2e_random;
mod e2e_redo_capture;
mod e2e_regexp_family;
mod e2e_repeat;
mod e2e_replace;
mod e2e_returning;
mod e2e_round;
mod e2e_round5_alter_and_trigger;
mod e2e_round5_misc;
mod e2e_round6_surfaces;
mod e2e_round7_surfaces;
mod e2e_runtime_default;
mod e2e_schema;
mod e2e_select_star_agg;
mod e2e_sequence;
mod e2e_serial;
mod e2e_session_funcs;
mod e2e_setof_aggregate;
mod e2e_show_create_table;
mod e2e_show_databases;
mod e2e_show_misc_mysql;
mod e2e_sign;
mod e2e_slow_query;
mod e2e_snapshot;
mod e2e_spg_stat_query;
mod e2e_spg_stat_views;
mod e2e_split_part;
mod e2e_sql_funcs;
mod e2e_sqrt;
mod e2e_srf_unnest_projection;
mod e2e_string_agg;
mod e2e_strpos;
mod e2e_table_constraints_engine;
mod e2e_text_array;
mod e2e_timestamptz;
mod e2e_tinyint_bool;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::uninlined_format_args
)]
mod e2e_tpch;
mod e2e_transactional_ddl;
mod e2e_translate;
mod e2e_trigger;
mod e2e_trim;
mod e2e_trunc;
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]
mod e2e_two_tier;
mod e2e_unique_collation;
mod e2e_unique_index;
mod e2e_unique_nulls_not_distinct;
mod e2e_unsigned;
mod e2e_update_correlated;
mod e2e_uuid;
mod e2e_view;
mod e2e_window_in_join;
mod e2e_window_null_treatment;
mod e2e_window_with_join;
mod mailrs_round26;
mod mailrs_round30;
mod mailrs_round31;
