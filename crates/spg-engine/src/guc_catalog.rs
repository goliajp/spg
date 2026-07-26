//! v7.39 (round 501) — PG18's configuration-parameter names and the
//! context each one may be changed in.
//!
//! This is a VALIDATION list, not a reporting list. Round 474 decided
//! deliberately that `pg_settings` lists only the parameters SPG actually
//! reads, on the grounds that reporting a knob tells a tuning tool that
//! turning it does something — and that decision stands. But the same
//! comment said "SET validates it", and round 500 measured that it does
//! not: `SET nonexistent_knob = 3` answered `SET`, where PG18 answers
//! `ERROR: unrecognized configuration parameter`. A typo'd parameter name
//! was accepted silently.
//!
//! So the names live here, used only to decide whether a `SET` is
//! addressing something real and whether the session is allowed to change
//! it. Nothing here is reported.
//!
//! Contexts, and what PG does when a session tries to SET one:
//!   user / superuser / backend / superuser-backend  — allowed
//!   sighup      — `parameter "x" cannot be changed now`
//!   postmaster  — `parameter "x" cannot be changed without restarting the server`
//!   internal    — `parameter "x" cannot be changed`
//!
//! Extracted from a live PG 18 (`select name, vartype, context from
//! pg_settings`), 398 rows: user 151, sighup 104, postmaster 69, superuser 48, internal 20, superuser-backend 4, backend 2.

/// `(name, context)` for every PG18 configuration parameter.
///
/// Names are LOWERCASED and sorted that way: PG spells a few of them in
/// mixed case (`TimeZone`, `DateStyle`, `IntervalStyle`) while matching
/// case-insensitively, and a table sorted by the raw spelling cannot be
/// binary-searched with a lowercased key — the timezone tests caught
/// exactly that.
pub(crate) const PG_GUC_CONTEXTS: &[(&str, &str, &str)] = &[
    ("allow_alter_system", "sighup", "on"),
    ("allow_in_place_tablespaces", "superuser", "off"),
    ("allow_system_table_mods", "superuser", "off"),
    ("application_name", "user", ""),
    ("archive_cleanup_command", "sighup", ""),
    ("archive_command", "sighup", ""),
    ("archive_library", "sighup", ""),
    ("archive_mode", "postmaster", "off"),
    ("archive_timeout", "sighup", "0"),
    ("array_nulls", "user", "on"),
    ("authentication_timeout", "sighup", "1min"),
    ("autovacuum", "sighup", "on"),
    ("autovacuum_analyze_scale_factor", "sighup", "0.1"),
    ("autovacuum_analyze_threshold", "sighup", "50"),
    ("autovacuum_freeze_max_age", "postmaster", "200000000"),
    ("autovacuum_max_workers", "sighup", "3"),
    ("autovacuum_multixact_freeze_max_age", "postmaster", "400000000"),
    ("autovacuum_naptime", "sighup", "1min"),
    ("autovacuum_vacuum_cost_delay", "sighup", "2ms"),
    ("autovacuum_vacuum_cost_limit", "sighup", "-1"),
    ("autovacuum_vacuum_insert_scale_factor", "sighup", "0.2"),
    ("autovacuum_vacuum_insert_threshold", "sighup", "1000"),
    ("autovacuum_vacuum_max_threshold", "sighup", "100000000"),
    ("autovacuum_vacuum_scale_factor", "sighup", "0.2"),
    ("autovacuum_vacuum_threshold", "sighup", "50"),
    ("autovacuum_work_mem", "sighup", "-1"),
    ("autovacuum_worker_slots", "postmaster", "16"),
    ("backend_flush_after", "user", "0"),
    ("backslash_quote", "user", "safe_encoding"),
    ("backtrace_functions", "superuser", ""),
    ("bgwriter_delay", "sighup", "200ms"),
    ("bgwriter_flush_after", "sighup", "512kB"),
    ("bgwriter_lru_maxpages", "sighup", "100"),
    ("bgwriter_lru_multiplier", "sighup", "2"),
    ("block_size", "internal", "8192"),
    ("bonjour", "postmaster", "off"),
    ("bonjour_name", "postmaster", ""),
    ("bytea_output", "user", "hex"),
    ("check_function_bodies", "user", "on"),
    ("checkpoint_completion_target", "sighup", "0.9"),
    ("checkpoint_flush_after", "sighup", "256kB"),
    ("checkpoint_timeout", "sighup", "5min"),
    ("checkpoint_warning", "sighup", "30s"),
    ("client_connection_check_interval", "user", "0"),
    ("client_encoding", "user", "SQL_ASCII"),
    ("client_min_messages", "user", "notice"),
    ("cluster_name", "postmaster", ""),
    ("commit_delay", "superuser", "0"),
    ("commit_siblings", "user", "5"),
    ("commit_timestamp_buffers", "postmaster", "0"),
    ("compute_query_id", "superuser", "auto"),
    ("config_file", "postmaster", ""),
    ("constraint_exclusion", "user", "partition"),
    ("cpu_index_tuple_cost", "user", "0.005"),
    ("cpu_operator_cost", "user", "0.0025"),
    ("cpu_tuple_cost", "user", "0.01"),
    ("createrole_self_grant", "user", ""),
    ("cursor_tuple_fraction", "user", "0.1"),
    ("data_checksums", "internal", "off"),
    ("data_directory", "postmaster", ""),
    ("data_directory_mode", "internal", "448"),
    ("data_sync_retry", "postmaster", "off"),
    ("datestyle", "user", "ISO, MDY"),
    ("deadlock_timeout", "superuser", "1s"),
    ("debug_assertions", "internal", "off"),
    ("debug_discard_caches", "superuser", "0"),
    ("debug_io_direct", "postmaster", ""),
    ("debug_logical_replication_streaming", "user", "buffered"),
    ("debug_parallel_query", "user", "off"),
    ("debug_pretty_print", "user", "on"),
    ("debug_print_parse", "user", "off"),
    ("debug_print_plan", "user", "off"),
    ("debug_print_rewritten", "user", "off"),
    ("default_statistics_target", "user", "100"),
    ("default_table_access_method", "user", "heap"),
    ("default_tablespace", "user", ""),
    ("default_text_search_config", "user", "pg_catalog.simple"),
    ("default_toast_compression", "user", "pglz"),
    ("default_transaction_deferrable", "user", "off"),
    ("default_transaction_isolation", "user", "read committed"),
    ("default_transaction_read_only", "user", "off"),
    ("dynamic_library_path", "superuser", "$libdir"),
    ("dynamic_shared_memory_type", "postmaster", "posix"),
    ("effective_cache_size", "user", "4GB"),
    ("effective_io_concurrency", "user", "16"),
    ("enable_async_append", "user", "on"),
    ("enable_bitmapscan", "user", "on"),
    ("enable_distinct_reordering", "user", "on"),
    ("enable_gathermerge", "user", "on"),
    ("enable_group_by_reordering", "user", "on"),
    ("enable_hashagg", "user", "on"),
    ("enable_hashjoin", "user", "on"),
    ("enable_incremental_sort", "user", "on"),
    ("enable_indexonlyscan", "user", "on"),
    ("enable_indexscan", "user", "on"),
    ("enable_material", "user", "on"),
    ("enable_memoize", "user", "on"),
    ("enable_mergejoin", "user", "on"),
    ("enable_nestloop", "user", "on"),
    ("enable_parallel_append", "user", "on"),
    ("enable_parallel_hash", "user", "on"),
    ("enable_partition_pruning", "user", "on"),
    ("enable_partitionwise_aggregate", "user", "off"),
    ("enable_partitionwise_join", "user", "off"),
    ("enable_presorted_aggregate", "user", "on"),
    ("enable_self_join_elimination", "user", "on"),
    ("enable_seqscan", "user", "on"),
    ("enable_sort", "user", "on"),
    ("enable_tidscan", "user", "on"),
    ("escape_string_warning", "user", "on"),
    ("event_source", "postmaster", "PostgreSQL"),
    ("event_triggers", "superuser", "on"),
    ("exit_on_error", "user", "off"),
    ("extension_control_path", "superuser", "$system"),
    ("external_pid_file", "postmaster", ""),
    ("extra_float_digits", "user", "1"),
    ("file_copy_method", "user", "copy"),
    ("file_extend_method", "sighup", "posix_fallocate"),
    ("from_collapse_limit", "user", "8"),
    ("fsync", "sighup", "on"),
    ("full_page_writes", "sighup", "on"),
    ("geqo", "user", "on"),
    ("geqo_effort", "user", "5"),
    ("geqo_generations", "user", "0"),
    ("geqo_pool_size", "user", "0"),
    ("geqo_seed", "user", "0"),
    ("geqo_selection_bias", "user", "2"),
    ("geqo_threshold", "user", "12"),
    ("gin_fuzzy_search_limit", "user", "0"),
    ("gin_pending_list_limit", "user", "4MB"),
    ("gss_accept_delegation", "sighup", "off"),
    ("hash_mem_multiplier", "user", "2"),
    ("hba_file", "postmaster", ""),
    ("hot_standby", "postmaster", "on"),
    ("hot_standby_feedback", "sighup", "off"),
    ("huge_page_size", "postmaster", "0"),
    ("huge_pages", "postmaster", "try"),
    ("huge_pages_status", "internal", "unknown"),
    ("icu_validation_level", "user", "warning"),
    ("ident_file", "postmaster", ""),
    ("idle_in_transaction_session_timeout", "user", "0"),
    ("idle_replication_slot_timeout", "sighup", "0"),
    ("idle_session_timeout", "user", "0"),
    ("ignore_checksum_failure", "superuser", "off"),
    ("ignore_invalid_pages", "postmaster", "off"),
    ("ignore_system_indexes", "backend", "off"),
    ("in_hot_standby", "internal", "off"),
    ("integer_datetimes", "internal", "on"),
    ("intervalstyle", "user", "postgres"),
    ("io_combine_limit", "user", "128kB"),
    ("io_max_combine_limit", "postmaster", "128kB"),
    ("io_max_concurrency", "postmaster", "-1"),
    ("io_method", "postmaster", "worker"),
    ("io_workers", "sighup", "3"),
    ("jit", "user", "on"),
    ("jit_above_cost", "user", "100000"),
    ("jit_debugging_support", "superuser-backend", "off"),
    ("jit_dump_bitcode", "superuser", "off"),
    ("jit_expressions", "user", "on"),
    ("jit_inline_above_cost", "user", "500000"),
    ("jit_optimize_above_cost", "user", "500000"),
    ("jit_profiling_support", "superuser-backend", "off"),
    ("jit_provider", "postmaster", "llvmjit"),
    ("jit_tuple_deforming", "user", "on"),
    ("join_collapse_limit", "user", "8"),
    ("krb_caseins_users", "sighup", "off"),
    ("krb_server_keyfile", "sighup", "FILE:/etc/postgresql-common/krb5.keytab"),
    ("lc_messages", "superuser", ""),
    ("lc_monetary", "user", "C"),
    ("lc_numeric", "user", "C"),
    ("lc_time", "user", "C"),
    ("listen_addresses", "postmaster", "localhost"),
    ("lo_compat_privileges", "superuser", "off"),
    ("local_preload_libraries", "user", ""),
    ("lock_timeout", "user", "0"),
    ("log_autovacuum_min_duration", "sighup", "10min"),
    ("log_checkpoints", "sighup", "on"),
    ("log_connections", "superuser-backend", ""),
    ("log_destination", "sighup", "stderr"),
    ("log_directory", "sighup", "log"),
    ("log_disconnections", "superuser-backend", "off"),
    ("log_duration", "superuser", "off"),
    ("log_error_verbosity", "superuser", "default"),
    ("log_executor_stats", "superuser", "off"),
    ("log_file_mode", "sighup", "384"),
    ("log_filename", "sighup", "postgresql-%Y-%m-%d_%H%M%S.log"),
    ("log_hostname", "sighup", "off"),
    ("log_line_prefix", "sighup", "%m [%p] "),
    ("log_lock_failures", "superuser", "off"),
    ("log_lock_waits", "superuser", "off"),
    ("log_min_duration_sample", "superuser", "-1"),
    ("log_min_duration_statement", "superuser", "-1"),
    ("log_min_error_statement", "superuser", "error"),
    ("log_min_messages", "superuser", "warning"),
    ("log_parameter_max_length", "superuser", "-1"),
    ("log_parameter_max_length_on_error", "user", "0"),
    ("log_parser_stats", "superuser", "off"),
    ("log_planner_stats", "superuser", "off"),
    ("log_recovery_conflict_waits", "sighup", "off"),
    ("log_replication_commands", "superuser", "off"),
    ("log_rotation_age", "sighup", "1d"),
    ("log_rotation_size", "sighup", "10MB"),
    ("log_startup_progress_interval", "sighup", "10s"),
    ("log_statement", "superuser", "none"),
    ("log_statement_sample_rate", "superuser", "1"),
    ("log_statement_stats", "superuser", "off"),
    ("log_temp_files", "superuser", "-1"),
    ("log_timezone", "sighup", "GMT"),
    ("log_transaction_sample_rate", "superuser", "0"),
    ("log_truncate_on_rotation", "sighup", "off"),
    ("logging_collector", "postmaster", "off"),
    ("logical_decoding_work_mem", "user", "64MB"),
    ("maintenance_io_concurrency", "user", "16"),
    ("maintenance_work_mem", "user", "64MB"),
    ("max_active_replication_origins", "postmaster", "10"),
    ("max_connections", "postmaster", "100"),
    ("max_files_per_process", "postmaster", "1000"),
    ("max_function_args", "internal", "100"),
    ("max_identifier_length", "internal", "63"),
    ("max_index_keys", "internal", "32"),
    ("max_locks_per_transaction", "postmaster", "64"),
    ("max_logical_replication_workers", "postmaster", "4"),
    ("max_notify_queue_pages", "postmaster", "1048576"),
    ("max_parallel_apply_workers_per_subscription", "sighup", "2"),
    ("max_parallel_maintenance_workers", "user", "2"),
    ("max_parallel_workers", "user", "8"),
    ("max_parallel_workers_per_gather", "user", "2"),
    ("max_pred_locks_per_page", "sighup", "2"),
    ("max_pred_locks_per_relation", "sighup", "-2"),
    ("max_pred_locks_per_transaction", "postmaster", "64"),
    ("max_prepared_transactions", "postmaster", "0"),
    ("max_replication_slots", "postmaster", "10"),
    ("max_slot_wal_keep_size", "sighup", "-1"),
    ("max_stack_depth", "superuser", "100kB"),
    ("max_standby_archive_delay", "sighup", "30s"),
    ("max_standby_streaming_delay", "sighup", "30s"),
    ("max_sync_workers_per_subscription", "sighup", "2"),
    ("max_wal_senders", "postmaster", "10"),
    ("max_wal_size", "sighup", "1GB"),
    ("max_worker_processes", "postmaster", "8"),
    ("md5_password_warnings", "user", "on"),
    ("min_dynamic_shared_memory", "postmaster", "0"),
    ("min_parallel_index_scan_size", "user", "512kB"),
    ("min_parallel_table_scan_size", "user", "8MB"),
    ("min_wal_size", "sighup", "80MB"),
    ("multixact_member_buffers", "postmaster", "256kB"),
    ("multixact_offset_buffers", "postmaster", "128kB"),
    ("notify_buffers", "postmaster", "128kB"),
    ("num_os_semaphores", "internal", "0"),
    ("oauth_validator_libraries", "sighup", ""),
    ("parallel_leader_participation", "user", "on"),
    ("parallel_setup_cost", "user", "1000"),
    ("parallel_tuple_cost", "user", "0.1"),
    ("password_encryption", "user", "scram-sha-256"),
    ("plan_cache_mode", "user", "auto"),
    ("port", "postmaster", "5432"),
    ("post_auth_delay", "backend", "0"),
    ("pre_auth_delay", "sighup", "0"),
    ("primary_conninfo", "sighup", ""),
    ("primary_slot_name", "sighup", ""),
    ("quote_all_identifiers", "user", "off"),
    ("random_page_cost", "user", "4"),
    ("recovery_end_command", "sighup", ""),
    ("recovery_init_sync_method", "sighup", "fsync"),
    ("recovery_min_apply_delay", "sighup", "0"),
    ("recovery_prefetch", "sighup", "try"),
    ("recovery_target", "postmaster", ""),
    ("recovery_target_action", "postmaster", "pause"),
    ("recovery_target_inclusive", "postmaster", "on"),
    ("recovery_target_lsn", "postmaster", ""),
    ("recovery_target_name", "postmaster", ""),
    ("recovery_target_time", "postmaster", ""),
    ("recovery_target_timeline", "postmaster", "latest"),
    ("recovery_target_xid", "postmaster", ""),
    ("recursive_worktable_factor", "user", "10"),
    ("remove_temp_files_after_crash", "sighup", "on"),
    ("reserved_connections", "postmaster", "0"),
    ("restart_after_crash", "sighup", "on"),
    ("restore_command", "sighup", ""),
    ("restrict_nonsystem_relation_kind", "user", ""),
    ("row_security", "user", "on"),
    ("scram_iterations", "user", "4096"),
    ("search_path", "user", "\"$user\", public"),
    ("segment_size", "internal", "1GB"),
    ("send_abort_for_crash", "sighup", "off"),
    ("send_abort_for_kill", "sighup", "off"),
    ("seq_page_cost", "user", "1"),
    ("serializable_buffers", "postmaster", "256kB"),
    ("server_encoding", "internal", "SQL_ASCII"),
    ("server_version", "internal", "18.4 (Debian 18.4-1.pgdg13+1)"),
    ("server_version_num", "internal", "180004"),
    ("session_preload_libraries", "superuser", ""),
    ("session_replication_role", "superuser", "origin"),
    ("shared_buffers", "postmaster", "128MB"),
    ("shared_memory_size", "internal", "0"),
    ("shared_memory_size_in_huge_pages", "internal", "-1"),
    ("shared_memory_type", "postmaster", "mmap"),
    ("shared_preload_libraries", "postmaster", ""),
    ("ssl", "sighup", "off"),
    ("ssl_ca_file", "sighup", ""),
    ("ssl_cert_file", "sighup", "server.crt"),
    ("ssl_ciphers", "sighup", "HIGH:MEDIUM:+3DES:!aNULL"),
    ("ssl_crl_dir", "sighup", ""),
    ("ssl_crl_file", "sighup", ""),
    ("ssl_dh_params_file", "sighup", ""),
    ("ssl_groups", "sighup", "X25519:prime256v1"),
    ("ssl_key_file", "sighup", "server.key"),
    ("ssl_library", "internal", "OpenSSL"),
    ("ssl_max_protocol_version", "sighup", ""),
    ("ssl_min_protocol_version", "sighup", "TLSv1.2"),
    ("ssl_passphrase_command", "sighup", ""),
    ("ssl_passphrase_command_supports_reload", "sighup", "off"),
    ("ssl_prefer_server_ciphers", "sighup", "on"),
    ("ssl_tls13_ciphers", "sighup", ""),
    ("standard_conforming_strings", "user", "on"),
    ("statement_timeout", "user", "0"),
    ("stats_fetch_consistency", "user", "cache"),
    ("subtransaction_buffers", "postmaster", "0"),
    ("summarize_wal", "sighup", "off"),
    ("superuser_reserved_connections", "postmaster", "3"),
    ("sync_replication_slots", "sighup", "off"),
    ("synchronize_seqscans", "user", "on"),
    ("synchronized_standby_slots", "sighup", ""),
    ("synchronous_commit", "user", "on"),
    ("synchronous_standby_names", "sighup", ""),
    ("syslog_facility", "sighup", "local0"),
    ("syslog_ident", "sighup", "postgres"),
    ("syslog_sequence_numbers", "sighup", "on"),
    ("syslog_split_messages", "sighup", "on"),
    ("tcp_keepalives_count", "user", "0"),
    ("tcp_keepalives_idle", "user", "0"),
    ("tcp_keepalives_interval", "user", "0"),
    ("tcp_user_timeout", "user", "0"),
    ("temp_buffers", "user", "8MB"),
    ("temp_file_limit", "superuser", "-1"),
    ("temp_tablespaces", "user", ""),
    ("timezone", "user", "GMT"),
    ("timezone_abbreviations", "user", ""),
    ("trace_connection_negotiation", "postmaster", "off"),
    ("trace_notify", "user", "off"),
    ("trace_sort", "user", "off"),
    ("track_activities", "superuser", "on"),
    ("track_activity_query_size", "postmaster", "1kB"),
    ("track_commit_timestamp", "postmaster", "off"),
    ("track_cost_delay_timing", "superuser", "off"),
    ("track_counts", "superuser", "on"),
    ("track_functions", "superuser", "none"),
    ("track_io_timing", "superuser", "off"),
    ("track_wal_io_timing", "superuser", "off"),
    ("transaction_buffers", "postmaster", "0"),
    ("transaction_deferrable", "user", "off"),
    ("transaction_isolation", "user", "read committed"),
    ("transaction_read_only", "user", "off"),
    ("transaction_timeout", "user", "0"),
    ("transform_null_equals", "user", "off"),
    ("unix_socket_directories", "postmaster", "/var/run/postgresql"),
    ("unix_socket_group", "postmaster", ""),
    ("unix_socket_permissions", "postmaster", "511"),
    ("update_process_title", "superuser", "on"),
    ("vacuum_buffer_usage_limit", "user", "2MB"),
    ("vacuum_cost_delay", "user", "0"),
    ("vacuum_cost_limit", "user", "200"),
    ("vacuum_cost_page_dirty", "user", "20"),
    ("vacuum_cost_page_hit", "user", "1"),
    ("vacuum_cost_page_miss", "user", "2"),
    ("vacuum_failsafe_age", "user", "1600000000"),
    ("vacuum_freeze_min_age", "user", "50000000"),
    ("vacuum_freeze_table_age", "user", "150000000"),
    ("vacuum_max_eager_freeze_failure_rate", "user", "0.03"),
    ("vacuum_multixact_failsafe_age", "user", "1600000000"),
    ("vacuum_multixact_freeze_min_age", "user", "5000000"),
    ("vacuum_multixact_freeze_table_age", "user", "150000000"),
    ("vacuum_truncate", "user", "on"),
    ("wal_block_size", "internal", "8192"),
    ("wal_buffers", "postmaster", "-1"),
    ("wal_compression", "superuser", "off"),
    ("wal_consistency_checking", "superuser", ""),
    ("wal_decode_buffer_size", "postmaster", "512kB"),
    ("wal_init_zero", "superuser", "on"),
    ("wal_keep_size", "sighup", "0"),
    ("wal_level", "postmaster", "replica"),
    ("wal_log_hints", "postmaster", "off"),
    ("wal_receiver_create_temp_slot", "sighup", "off"),
    ("wal_receiver_status_interval", "sighup", "10s"),
    ("wal_receiver_timeout", "sighup", "1min"),
    ("wal_recycle", "superuser", "on"),
    ("wal_retrieve_retry_interval", "sighup", "5s"),
    ("wal_segment_size", "internal", "16MB"),
    ("wal_sender_timeout", "user", "1min"),
    ("wal_skip_threshold", "user", "2MB"),
    ("wal_summary_keep_time", "sighup", "10d"),
    ("wal_sync_method", "sighup", "fdatasync"),
    ("wal_writer_delay", "sighup", "200ms"),
    ("wal_writer_flush_after", "sighup", "1MB"),
    ("work_mem", "user", "4MB"),
    ("xmlbinary", "user", "base64"),
    ("xmloption", "user", "content"),
    ("zero_damaged_pages", "superuser", "off"),
];

/// The context `name` may be changed in, or `None` when PG18 has no such
/// parameter.
pub(crate) fn guc_context(name: &str) -> Option<&'static str> {
    guc_entry(name).map(|e| e.1)
}

/// v7.39 (round 534) — the compiled-in default PG18 reports for a
/// parameter.
///
/// Round 474 decided that `pg_settings` lists only the parameters SPG
/// actually reads, so that reporting a knob does not tell a tuning tool
/// that turning it does something. That decision stands. What it left
/// open is the READ path: `current_setting('block_size')` answered an
/// EMPTY STRING where PG answers `8192`, and `SHOW random_page_cost`
/// printed nothing at all.
///
/// An empty string is the worst of the three possible answers. It is
/// not the value, and it is not an error a caller can branch on — a
/// tool doing `current_setting('block_size')::int` gets a cast failure
/// or silently reads zero.
///
/// SPG already ACCEPTS `SET random_page_cost = 3` (round 501), so the
/// session has a value for these names whether or not anything reads
/// them; reporting the stored configuration is what PG-compat asks for
/// and says nothing about behaviour that accepting the SET did not
/// already say.
pub(crate) fn guc_boot_value(name: &str) -> Option<&'static str> {
    guc_entry(name).map(|e| e.2)
}

fn guc_entry(name: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    let lower = name.to_ascii_lowercase();
    PG_GUC_CONTEXTS
        .binary_search_by(|(n, ..)| (*n).cmp(lower.as_str()))
        .ok()
        .map(|i| &PG_GUC_CONTEXTS[i])
}

#[cfg(test)]
mod tests {
    use super::{PG_GUC_CONTEXTS, guc_boot_value, guc_context};

    /// v7.39 (round 534) — the table is binary-searched, so an
    /// out-of-order row makes names below it invisible and `SET` starts
    /// answering "unrecognized configuration parameter" for parameters
    /// that are right there. Regenerating it from a query ordered by the
    /// CONCATENATED row rather than by name did exactly that: `~`
    /// outranks `_`, so `lock_timeout` sorted after `lock_timeout_*`.
    #[test]
    fn table_is_strictly_sorted_by_name() {
        for w in PG_GUC_CONTEXTS.windows(2) {
            assert!(
                w[0].0 < w[1].0,
                "out of order: {:?} then {:?}",
                w[0].0,
                w[1].0
            );
        }
    }

    /// Every name is lowercase, which is what the lookup folds to.
    #[test]
    fn every_name_is_lowercase() {
        for (n, ..) in PG_GUC_CONTEXTS {
            assert_eq!(*n, n.to_ascii_lowercase(), "{n} is not lowercased");
        }
    }

    /// A spot check that both accessors reach the same row, including
    /// the mixed-case spellings PG uses.
    #[test]
    fn lookups_agree_and_fold_case() {
        assert_eq!(guc_context("TimeZone"), Some("user"));
        assert_eq!(guc_boot_value("block_size"), Some("8192"));
        assert_eq!(guc_boot_value("BLOCK_SIZE"), Some("8192"));
        assert_eq!(guc_context("nosuchknob"), None);
        assert_eq!(guc_boot_value("nosuchknob"), None);
    }
}
