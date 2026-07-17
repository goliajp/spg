//! v7.20 test-speed A — the 90 standalone e2e_* spg-server test
//! binaries merged into ONE integration-test target. One link
//! instead of 90, and the libtest runner parallelises across all
//! modules in-process. Each module still spawns its own
//! spg-server child on a kernel-assigned port (common::ServerBuilder
//! passes 127.0.0.1:0) and uses nanos-stamped scratch dirs, so
//! in-process concurrency is safe. See .claude notes 2026-06-10.

#[path = "../common/mod.rs"]
mod common;

mod e2e_alter_rebuild;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args
)]
mod e2e_application_name;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables
)]
mod e2e_async_commit;
mod e2e_autovacuum_worker_round173;
mod e2e_nowal_returning_round182;
mod e2e_pgwire_group_commit_round178;
mod e2e_session_sync_commit_round172;
mod e2e_tx_single_fsync_round177;
#[allow(unused_mut, unused_variables)]
mod e2e_audit;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args
)]
mod e2e_audit_verify;
#[allow(unused_mut, unused_variables)]
mod e2e_auth;
#[allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
mod e2e_auto_analyze;
#[allow(
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables
)]
mod e2e_backup;
#[allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
mod e2e_cascade;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]
mod e2e_chaos;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables
)]
mod e2e_chaos_async_commit;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]
mod e2e_chaos_freeze;
#[allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
mod e2e_chaos_logical;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::similar_names,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables
)]
mod e2e_chaos_netsplit;
#[allow(unsafe_code)]
mod e2e_compression_metrics;
mod e2e_copy_errors_round91;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args
)]
mod e2e_copy_options;
mod e2e_copy_query_to_stdout_round94;
#[allow(clippy::doc_markdown, clippy::uninlined_format_args)]
mod e2e_correlated;
#[allow(clippy::doc_markdown, clippy::uninlined_format_args)]
mod e2e_cte;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables
)]
mod e2e_disk_watermark;
mod e2e_error_position_round95;
#[allow(clippy::doc_markdown, clippy::uninlined_format_args)]
mod e2e_explain;
#[allow(
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables
)]
mod e2e_flusher;
mod e2e_dml_kill_restart_matrix_round179;
mod e2e_flusher_idle_gate_round176;
mod e2e_fsync_fail_round190;
#[allow(
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables
)]
mod e2e_freezer;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]
mod e2e_fuzz;
mod e2e_gen_series_tstz_round119;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]
mod e2e_graceful_shutdown;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]
mod e2e_group_commit;
mod e2e_half;
#[allow(unused_mut, unused_variables)]
mod e2e_index;
#[allow(clippy::doc_markdown, clippy::uninlined_format_args)]
mod e2e_json;
#[allow(clippy::doc_markdown, clippy::uninlined_format_args)]
mod e2e_json_path;
#[allow(unused_mut, unused_variables)]
mod e2e_limits;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables,
    clippy::needless_borrow,
    clippy::needless_pass_by_value,
    clippy::empty_line_after_doc_comments
)]
mod e2e_manifest;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]
mod e2e_mysqlwire_admin;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]
mod e2e_mysqlwire_auth;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]
mod e2e_mysqlwire_binary_rows;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]
mod e2e_mysqlwire_caching_sha2;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]
mod e2e_mysqlwire_handshake;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]
mod e2e_mysqlwire_query;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]
mod e2e_mysqlwire_ssl;
mod e2e_mysqlwire_returning_durability_round181;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]
mod e2e_mysqlwire_stmt;
mod e2e_not_null_detail_round117;
#[allow(
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables
)]
mod e2e_observability;
mod e2e_parallel_freezer;
#[allow(unused_mut, unused_variables)]
mod e2e_persistence;
mod e2e_pg_abort_firewall_round86;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]
mod e2e_pg_catalog;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]
mod e2e_pg_copy;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::naive_bytecount,
    clippy::similar_names,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]
mod e2e_pg_extended;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]
mod e2e_pg_scram;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]
mod e2e_pg_session_vars;
mod e2e_pg_tx_visibility_round84;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]
mod e2e_pg_vacuum;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables
)]
mod e2e_pgbouncer_compat;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]
mod e2e_pgwire;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::uninlined_format_args
)]
mod e2e_pgwire_binary_params;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::uninlined_format_args
)]
mod e2e_pgwire_client_compat;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::uninlined_format_args
)]
mod e2e_pgwire_describe;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::uninlined_format_args
)]
mod e2e_pgwire_pipelined;
mod e2e_pgwire_ssl;
mod e2e_ping;
#[allow(clippy::uninlined_format_args)]
mod e2e_prefetch;
#[allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
mod e2e_prepared_wal_durability;
#[allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
mod e2e_publication_ddl;
mod e2e_query;
#[allow(unused_mut, unused_variables)]
mod e2e_query_budget;
mod e2e_query_cancel;
#[allow(clippy::uninlined_format_args)]
mod e2e_query_ns_budget;
#[allow(unused_mut, unused_variables)]
mod e2e_rbac;
#[allow(clippy::doc_markdown, clippy::uninlined_format_args)]
mod e2e_recursive_cte;
#[allow(clippy::uninlined_format_args, unsafe_code)]
mod e2e_replay_only;
#[allow(
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables,
    clippy::needless_borrow,
    clippy::needless_pass_by_value,
    clippy::empty_line_after_doc_comments
)]
mod e2e_replication;
#[allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
mod e2e_replication_filter;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]
mod e2e_restore_drill;
mod e2e_returning_command_tag_round131;
#[allow(clippy::uninlined_format_args)]
mod e2e_segment_forward;
mod e2e_show_isolation_round118;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables
)]
mod e2e_slow_query_log;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args
)]
mod e2e_spg_stat_activity;
#[allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
mod e2e_spg_statistic;
mod e2e_sq8;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]
mod e2e_statement_timeout;
#[allow(clippy::doc_markdown, clippy::uninlined_format_args)]
mod e2e_subqueries;
#[allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
mod e2e_subscription;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables,
    clippy::needless_borrow,
    clippy::needless_pass_by_value,
    clippy::empty_line_after_doc_comments
)]
mod e2e_table_metrics;
#[allow(
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables
)]
mod e2e_timeouts;
#[allow(unused_mut, unused_variables)]
mod e2e_two_tier_server;
#[allow(unused_mut, unused_variables)]
mod e2e_tx;
#[allow(clippy::doc_markdown, clippy::uninlined_format_args)]
mod e2e_update_delete;
mod e2e_vector;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args
)]
mod e2e_wait_events;
#[allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
mod e2e_wait_pos;
#[allow(unused_mut, unused_variables)]
mod e2e_wal;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::uninlined_format_args,
    clippy::doc_markdown,
    unused_mut,
    unused_variables
)]
mod e2e_wal_binary;
#[allow(unsafe_code)]
mod e2e_wal_compression;
#[allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
mod e2e_wal_level;
#[allow(clippy::uninlined_format_args, unsafe_code)]
mod e2e_wal_tee;
#[allow(clippy::doc_markdown, clippy::uninlined_format_args)]
mod e2e_window;
#[allow(clippy::doc_markdown, clippy::float_cmp, clippy::uninlined_format_args)]
mod e2e_window_ext;
#[allow(clippy::doc_markdown, clippy::float_cmp, clippy::uninlined_format_args)]
mod e2e_window_frames;

#[allow(unsafe_code)]
mod chaos_compact_atomic;
#[allow(unsafe_code)]
mod chaos_wal_compression_torn_write;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::doc_overindented_list_items,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unnecessary_debug_formatting,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]
mod cross_version_compat;
