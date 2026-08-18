//! r1038 — which operator classes exist, per access method.
//!
//! PG resolves `CREATE INDEX … (col <opclass>)` against `pg_opclass` and
//! refuses a name that is not there:
//!
//! ```text
//! ERROR:  operator class "weird_garbage" does not exist for access method "gin"
//! ```
//!
//! SPG used to get that refusal for free, by accident: the parser matched
//! the opclass against a whitelist of eighteen names, and anything else
//! failed to parse. That whitelist is why `USING gin (doc jsonb_path_ops)`
//! — ordinary PG — was a syntax error, which is the gap sentori reported.
//! Recognising an opclass by its POSITION fixed that, and took the refusal
//! with it: every identifier in that position parsed, including nonsense.
//!
//! So the grammar and the catalog split the way they do in PG. The parser
//! decides that a token is an operator class; this decides whether that
//! class exists.
//!
//! The names are `pg_opclass` on PostgreSQL 18.4, read per access method,
//! plus the extension classes SPG implements or accepts natively (pg_trgm,
//! pgvector). SPG has no `pg_opclass` relation to answer from, so the lists
//! are transcribed; a class PG grows later is a name SPG will refuse until
//! it is added here, which is the same failure mode as any other
//! transcribed catalog in this codebase.

/// Whether `name` is an operator class of the access method `am` — both
/// lower-cased, `am` as the user wrote it in `USING`.
///
/// `None` for `am` means there was no `USING` clause, i.e. btree.
pub(crate) fn exists_for_access_method(name: &str, am: Option<&str>) -> bool {
    let am = am.unwrap_or("btree");
    let list: &[&str] = match am {
        "btree" => BTREE,
        "gin" => GIN,
        "brin" => BRIN,
        "gist" => GIST,
        "spgist" => SPGIST,
        "hash" => HASH,
        "hnsw" | "ivfflat" => VECTOR,
        // An access method SPG does not know is refused by the parser
        // before this runs, so anything arriving here is one of the above.
        // Being permissive rather than wrong is the right way to be
        // surprised.
        _ => return true,
    };
    list.contains(&name)
}

/// pg_opclass ∩ btree on PG18.4.
const BTREE: &[&str] = &[
    "array_ops",
    "bit_ops",
    "bool_ops",
    "bpchar_ops",
    "bpchar_pattern_ops",
    "bytea_ops",
    "char_ops",
    "cidr_ops",
    "date_ops",
    "enum_ops",
    "float4_ops",
    "float8_ops",
    "inet_ops",
    "int2_ops",
    "int4_ops",
    "int8_ops",
    "interval_ops",
    "jsonb_ops",
    "macaddr8_ops",
    "macaddr_ops",
    "money_ops",
    "multirange_ops",
    "name_ops",
    "numeric_ops",
    "oid_ops",
    "oidvector_ops",
    "pg_lsn_ops",
    "range_ops",
    "record_image_ops",
    "record_ops",
    "text_ops",
    "text_pattern_ops",
    "tid_ops",
    "time_ops",
    "timestamp_ops",
    "timestamptz_ops",
    "timetz_ops",
    "tsquery_ops",
    "tsvector_ops",
    "uuid_ops",
    "varbit_ops",
    "varchar_ops",
    "varchar_pattern_ops",
    "xid8_ops",
];

/// pg_opclass ∩ gin, plus `gin_trgm_ops` — pg_trgm's, which SPG builds a
/// real trigram-shingle GIN for (`add_gin_trgm_index`), so refusing the
/// name would refuse an index SPG actually has.
const GIN: &[&str] = &[
    "array_ops",
    "jsonb_ops",
    "jsonb_path_ops",
    "tsvector_ops",
    "gin_trgm_ops",
];

/// pg_opclass ∩ brin on PG18.4.
const BRIN: &[&str] = &[
    "bit_minmax_ops",
    "box_inclusion_ops",
    "bpchar_bloom_ops",
    "bpchar_minmax_ops",
    "bytea_bloom_ops",
    "bytea_minmax_ops",
    "char_bloom_ops",
    "char_minmax_ops",
    "date_bloom_ops",
    "date_minmax_multi_ops",
    "date_minmax_ops",
    "float4_bloom_ops",
    "float4_minmax_multi_ops",
    "float4_minmax_ops",
    "float8_bloom_ops",
    "float8_minmax_multi_ops",
    "float8_minmax_ops",
    "inet_bloom_ops",
    "inet_inclusion_ops",
    "inet_minmax_multi_ops",
    "inet_minmax_ops",
    "int2_bloom_ops",
    "int2_minmax_multi_ops",
    "int2_minmax_ops",
    "int4_bloom_ops",
    "int4_minmax_multi_ops",
    "int4_minmax_ops",
    "int8_bloom_ops",
    "int8_minmax_multi_ops",
    "int8_minmax_ops",
    "interval_bloom_ops",
    "interval_minmax_multi_ops",
    "interval_minmax_ops",
    "macaddr8_bloom_ops",
    "macaddr8_minmax_multi_ops",
    "macaddr8_minmax_ops",
    "macaddr_bloom_ops",
    "macaddr_minmax_multi_ops",
    "macaddr_minmax_ops",
    "name_bloom_ops",
    "name_minmax_ops",
    "numeric_bloom_ops",
    "numeric_minmax_multi_ops",
    "numeric_minmax_ops",
    "oid_bloom_ops",
    "oid_minmax_multi_ops",
    "oid_minmax_ops",
    "pg_lsn_bloom_ops",
    "pg_lsn_minmax_multi_ops",
    "pg_lsn_minmax_ops",
    "range_inclusion_ops",
    "text_bloom_ops",
    "text_minmax_ops",
    "tid_bloom_ops",
    "tid_minmax_multi_ops",
    "tid_minmax_ops",
    "time_bloom_ops",
    "time_minmax_multi_ops",
    "time_minmax_ops",
    "timestamp_bloom_ops",
    "timestamp_minmax_multi_ops",
    "timestamp_minmax_ops",
    "timestamptz_bloom_ops",
    "timestamptz_minmax_multi_ops",
    "timestamptz_minmax_ops",
    "timetz_bloom_ops",
    "timetz_minmax_multi_ops",
    "timetz_minmax_ops",
    "uuid_bloom_ops",
    "uuid_minmax_multi_ops",
    "uuid_minmax_ops",
    "varbit_minmax_ops",
];

/// pg_opclass ∩ gist, plus pg_trgm's `gist_trgm_ops`.
///
/// SPG has no GiST; `USING gist` degrades to a BTree on the leading column
/// so PG schemas load. The opclass still has to be one GiST really has,
/// because the user wrote `gist` and that is the AM the name is checked
/// against — and reported against, if it is missing.
const GIST: &[&str] = &[
    "box_ops",
    "circle_ops",
    "inet_ops",
    "multirange_ops",
    "point_ops",
    "poly_ops",
    "range_ops",
    "tsquery_ops",
    "tsvector_ops",
    "gist_trgm_ops",
];

/// pg_opclass ∩ spgist on PG18.4. Degrades to BTree, as GiST does.
const SPGIST: &[&str] = &[
    "box_ops",
    "inet_ops",
    "kd_point_ops",
    "poly_ops",
    "quad_point_ops",
    "range_ops",
    "text_ops",
];

/// pg_opclass ∩ hash on PG18.4. Degrades to BTree, as GiST does.
const HASH: &[&str] = &[
    "aclitem_ops",
    "array_ops",
    "bool_ops",
    "bpchar_ops",
    "bpchar_pattern_ops",
    "bytea_ops",
    "char_ops",
    "cid_ops",
    "cidr_ops",
    "date_ops",
    "enum_ops",
    "float4_ops",
    "float8_ops",
    "inet_ops",
    "int2_ops",
    "int4_ops",
    "int8_ops",
    "interval_ops",
    "jsonb_ops",
    "macaddr8_ops",
    "macaddr_ops",
    "multirange_ops",
    "name_ops",
    "numeric_ops",
    "oid_ops",
    "oidvector_ops",
    "pg_lsn_ops",
    "range_ops",
    "record_ops",
    "text_ops",
    "text_pattern_ops",
    "tid_ops",
    "time_ops",
    "timestamp_ops",
    "timestamptz_ops",
    "timetz_ops",
    "uuid_ops",
    "varchar_ops",
    "varchar_pattern_ops",
    "xid8_ops",
    "xid_ops",
];

/// pgvector's, for `USING hnsw` and `USING ivfflat`. The `sq8_*` three are
/// SPG's own scalar-quantised spelling, accepted since v7.11.
const VECTOR: &[&str] = &[
    "vector_cosine_ops",
    "vector_l2_ops",
    "vector_ip_ops",
    "vector_l1_ops",
    "halfvec_cosine_ops",
    "halfvec_l2_ops",
    "halfvec_ip_ops",
    "halfvec_l1_ops",
    "sparsevec_cosine_ops",
    "sparsevec_l2_ops",
    "sparsevec_ip_ops",
    "sparsevec_l1_ops",
    "bit_hamming_ops",
    "bit_jaccard_ops",
    "sq8_cosine_ops",
    "sq8_l2_ops",
    "sq8_ip_ops",
];

/// 7.38.1 S5.1 (pg_dump wall: pg_opclass) — every (access method,
/// opclass name) pair SPG answers for, for the `pg_opclass` catalog
/// synthesis. The vector list rides under both pgvector AM names,
/// exactly as `exists_for_access_method` resolves them.
pub(crate) fn all_opclasses() -> impl Iterator<Item = (&'static str, &'static str)> {
    let families: &[(&str, &[&str])] = &[
        ("btree", BTREE),
        ("gin", GIN),
        ("brin", BRIN),
        ("gist", GIST),
        ("spgist", SPGIST),
        ("hash", HASH),
        ("hnsw", VECTOR),
        ("ivfflat", VECTOR),
    ];
    families
        .iter()
        .flat_map(|(am, list)| list.iter().map(move |n| (*am, *n)))
        .collect::<alloc::vec::Vec<_>>()
        .into_iter()
}
