//! 9.0.0 (N19) — the one list of catalog relations SPG answers for.
//!
//! It used to be written in six places: the parser gate that decides
//! whether `pg_catalog.x` is rewritten at all (107 names), the
//! `pg_class` row builder (43), the empty-catalog table (30), the
//! meta-view dispatch (17), a catalog-oid table (64) and a
//! hand-copied 13-name subset in the evaluator. They disagreed, and
//! the disagreement was observable: **64 names answered a query and
//! had no `pg_class` row** — `SELECT count(*) FROM pg_database`
//! answered 1 while `SELECT count(*) FROM pg_class WHERE
//! relname = 'pg_database'` answered 0. PostgreSQL 18.6 answers 7
//! and 1.
//!
//! This lives in `spg-sql` because the parser is the lowest layer
//! that needs it, and the dependency only points down. The engine
//! reads it for `pg_class`, for `::regclass`, and to decide which
//! relations `information_schema` lists; a test asserts every table
//! that HOLDS a relation's columns names a relation that is here.
//!
//! Each row is `(name, oid, relkind)`, read off PostgreSQL 18.6's
//! own `pg_class`. The oids are a contract: `'pg_type'::regclass`
//! answering 1247 is something any client can observe.

/// `(name, oid, relkind, rewrite)` for every catalog relation SPG
/// answers for.
///
/// `rewrite` is the parser's question and not the catalog's: whether
/// `pg_catalog.x` is rewritten to the synthetic `__spg_pg_x` name. Four
/// relations answer through `meta_view_result` under their own bare
/// name instead, and rewriting them would mis-target the lookup — they
/// are `false` here and still get a `pg_class` row, which is the half
/// that was missing.
pub const CATALOG_RELATIONS: &[(&str, i64, &str, bool)] = &[
    ("pg_am", 2601, "r", true),
    ("pg_amop", 2602, "r", true),
    ("pg_amproc", 2603, "r", true),
    ("pg_attrdef", 2604, "r", true),
    ("pg_attribute", 1249, "r", true),
    ("pg_auth_members", 1261, "r", true),
    ("pg_authid", 1260, "r", true),
    ("pg_cast", 2605, "r", true),
    ("pg_class", 1259, "r", true),
    ("pg_collation", 3456, "r", true),
    ("pg_constraint", 2606, "r", true),
    ("pg_conversion", 2607, "r", true),
    ("pg_database", 1262, "r", true),
    ("pg_db_role_setting", 2964, "r", true),
    ("pg_default_acl", 826, "r", true),
    ("pg_depend", 2608, "r", true),
    ("pg_description", 2609, "r", true),
    ("pg_enum", 3501, "r", true),
    ("pg_event_trigger", 3466, "r", true),
    ("pg_extension", 3079, "r", true),
    ("pg_file_settings", 12110, "v", true),
    ("pg_foreign_data_wrapper", 2328, "r", true),
    ("pg_foreign_server", 1417, "r", true),
    ("pg_foreign_table", 3118, "r", true),
    ("pg_group", 12010, "v", true),
    ("pg_hba_file_rules", 12114, "v", true),
    ("pg_ident_file_mappings", 12118, "v", true),
    ("pg_index", 2610, "r", true),
    ("pg_indexes", 12043, "v", true),
    ("pg_inherits", 2611, "r", true),
    ("pg_init_privs", 3394, "r", true),
    ("pg_language", 2612, "r", true),
    ("pg_largeobject", 2613, "r", true),
    ("pg_largeobject_metadata", 2995, "r", true),
    ("pg_matviews", 12038, "v", true),
    ("pg_namespace", 2615, "r", true),
    ("pg_opclass", 2616, "r", true),
    ("pg_operator", 2617, "r", true),
    ("pg_opfamily", 2753, "r", true),
    ("pg_parameter_acl", 6243, "r", true),
    ("pg_partitioned_table", 3350, "r", true),
    ("pg_policies", 12018, "v", true),
    ("pg_policy", 3256, "r", true),
    ("pg_prepared_statements", 12095, "v", true),
    ("pg_prepared_xacts", 12090, "v", true),
    ("pg_proc", 1255, "r", true),
    ("pg_publication", 6104, "r", true),
    ("pg_publication_namespace", 6237, "r", true),
    ("pg_publication_rel", 6106, "r", true),
    ("pg_publication_tables", 12068, "v", true),
    ("pg_range", 3541, "r", true),
    ("pg_replication_origin", 6000, "r", true),
    ("pg_replication_origin_status", 12343, "v", true),
    ("pg_replication_slots", 12261, "v", true),
    ("pg_rewrite", 2618, "r", true),
    ("pg_roles", 12000, "v", true),
    ("pg_rules", 12023, "v", true),
    ("pg_seclabel", 3596, "r", true),
    ("pg_seclabels", 12099, "v", true),
    ("pg_sequence", 2224, "r", true),
    ("pg_sequences", 12048, "v", true),
    ("pg_settings", 12104, "v", true),
    ("pg_shadow", 12005, "v", true),
    ("pg_shdepend", 1214, "r", true),
    ("pg_shdescription", 2396, "r", true),
    ("pg_shmem_allocations", 12134, "v", true),
    ("pg_shmem_allocations_numa", 12138, "v", true),
    ("pg_shseclabel", 3592, "r", true),
    ("pg_stat_archiver", 12289, "v", true),
    ("pg_stat_bgwriter", 12293, "v", true),
    ("pg_stat_checkpointer", 12297, "v", true),
    ("pg_stat_database", 12270, "v", true),
    ("pg_stat_io", 12301, "v", true),
    ("pg_stat_progress_analyze", 12309, "v", true),
    ("pg_stat_progress_create_index", 12324, "v", true),
    ("pg_stat_progress_vacuum", 12314, "v", true),
    ("pg_stat_replication", 12231, "v", true),
    ("pg_stat_slru", 12236, "v", true),
    ("pg_stat_subscription_stats", 12347, "v", true),
    ("pg_stat_user_functions", 12279, "v", true),
    ("pg_stat_user_indexes", 12196, "v", true),
    ("pg_stat_user_tables", 12165, "v", true),
    ("pg_stat_wal", 12305, "v", true),
    ("pg_statistic", 2619, "r", true),
    ("pg_statistic_ext", 3381, "r", true),
    ("pg_statistic_ext_data", 3429, "r", true),
    ("pg_stats", 12053, "v", true),
    ("pg_stats_ext", 12058, "v", true),
    ("pg_stats_ext_exprs", 12063, "v", true),
    ("pg_subscription", 6100, "r", true),
    ("pg_subscription_rel", 6102, "r", true),
    ("pg_tables", 12033, "v", true),
    ("pg_tablespace", 1213, "r", true),
    ("pg_timezone_abbrevs", 12122, "v", true),
    ("pg_timezone_names", 12126, "v", true),
    ("pg_transform", 3576, "r", true),
    ("pg_trigger", 2620, "r", true),
    ("pg_ts_config", 3602, "r", true),
    ("pg_ts_config_map", 3603, "r", true),
    ("pg_ts_dict", 3600, "r", true),
    ("pg_ts_parser", 3601, "r", true),
    ("pg_ts_template", 3764, "r", true),
    ("pg_type", 1247, "r", true),
    ("pg_user", 12014, "v", true),
    ("pg_user_mapping", 1418, "r", true),
    ("pg_user_mappings", 12338, "v", true),
    ("pg_views", 12028, "v", true),
    // 9.0.0 — the relations that answer through `meta_view_result`
    // under their own bare name. PostgreSQL has a `pg_class` row for
    // each (measured on 18.6); SPG answered the query and had none, so
    // `information_schema` could not list them.
    ("pg_locks", 12073, "v", false),
    ("pg_stat_activity", 12226, "v", false),
    ("pg_statio_user_tables", 12183, "v", false),
];

/// The oid PostgreSQL 18.6 gives this catalog relation, if SPG
/// answers for it.
#[must_use]
pub fn catalog_relation_oid(name: &str) -> Option<i64> {
    CATALOG_RELATIONS
        .iter()
        .find(|(n, _, _, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, oid, _, _)| *oid)
}

/// Whether `pg_catalog.<name>` is rewritten to SPG's synthetic name —
/// the parser's gate. A relation that answers under its own bare name
/// is `false`: rewriting it would mis-target the lookup.
#[must_use]
pub fn is_rewritten_catalog(name: &str) -> bool {
    CATALOG_RELATIONS
        .iter()
        .any(|(n, _, _, rewrite)| *rewrite && n.eq_ignore_ascii_case(name))
}

/// Whether SPG answers for this catalog relation at all, however it is
/// reached. `pg_class` lists every one of them.
#[must_use]
pub fn is_catalog_relation(name: &str) -> bool {
    CATALOG_RELATIONS
        .iter()
        .any(|(n, _, _, _)| n.eq_ignore_ascii_case(name))
}
