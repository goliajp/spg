//! Expression evaluator. Given a parsed `Expr`, a `Row`, and the row's column
//! schema, produce a `Value`. v0.4 implements:
//!
//! - literals
//! - column lookups (bare and qualified `t.col`)
//! - unary minus / NOT
//! - binary arithmetic, comparison, AND, OR
//! - numeric widening (`Int → BigInt → Float`) at evaluation time
//! - SQL three-valued logic for NULL:
//!     * any arithmetic / comparison op with a NULL operand → NULL
//!     * `TRUE OR NULL` → TRUE, `FALSE OR NULL` → NULL,
//!     * `FALSE AND NULL` → FALSE, `TRUE AND NULL` → NULL,
//!     * `NOT NULL` → NULL
//!
//! v0.4 deliberately does *not* implement: function calls, string
//! concatenation, IS NULL / IS NOT NULL, BETWEEN, IN, etc. Those come later.

use alloc::borrow::Cow;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{BinOp, CastTarget, ColumnName, Expr, Literal};
use spg_storage::{ColumnSchema, Row, Value};

pub(crate) mod binop;
mod cast;
pub mod compiled;
mod datetime;
mod encoding;
mod encodings;
mod format;
pub(crate) mod functions;
mod inet;
pub(crate) mod math;
mod regexp;
mod resolve;
mod strings;
pub(crate) mod textsearch;
pub(crate) mod values;

pub use crate::conversions::format_money_array;
pub(crate) use binop::{
    add_interval_to_micros, and_3vl, apply_binary, apply_binary_by_ref, apply_binary_interval,
};
use binop::{apply_binary_in, apply_unary, compare, pow10_i128};
pub use cast::{cast_to_vector, cast_value, parse_vector_text};
pub(crate) use compiled::{
    CompiledExpr, compile_column_pos, compile_expr, eval_compiled, eval_compiled_ref,
    fully_compilable,
};
use datetime::{
    age, date_format_mysql, date_part, date_trunc, extract_field, from_unixtime, unix_timestamp_of,
};
use encoding::{decode_text, encode_text};
pub use format::{
    days_from_civil, format_bigint_array, format_bool_array, format_bytea_array, format_bytea_hex,
    format_date, format_date_array, format_float, format_float_array, format_int_array,
    format_interval, format_interval_array, format_money, format_numeric, format_numeric_array,
    format_numeric_kind, format_real, format_smallint_array, format_text_array, format_time,
    format_timestamp, format_timestamp_array, format_timestamptz, format_timestamptz_at,
    format_timetz, format_uuid_array, parse_date_literal, parse_timestamp_literal,
};
// v7.39 (GUC knife 3) — session render styles + styled formatters.
pub use format::{
    DateOrder, DateStyleKind, IntervalStyleKind, RenderStyle, format_date_array_styled,
    format_date_styled, format_float_array_styled, format_float_styled,
    format_interval_array_styled, format_interval_styled, format_real_styled,
    format_timestamp_array_styled, format_timestamp_styled, format_timestamptz_styled,
    format_timestamptz_tz, parse_date_literal_ordered, parse_timestamp_literal_ordered,
    parse_timestamp_literal_tz_ordered,
};
use functions::apply_function;
use inet::{inet_host, inet_masklen, inet_network, inet_op_bool_result};
pub(crate) use math::{f64_ceil, f64_floor, f64_sqrt};
use math::{
    f64_exp, f64_ln, f64_powi, f64_round_half_away, f64_trunc, prng_next_f64, prng_next_u64,
};
pub(crate) use regexp::{
    CompiledRe, compile_re, compiled_is_match, regex_is_match, regexp_matches_rows,
};
use regexp::{regexp_matches, regexp_replace, regexp_split_to_array};
use resolve::{
    collation_fold_for_compare, compare_is_case_insensitive, composite_eq, eval_expr_cow,
    is_owned_compare_value, resolve_column, resolve_column_borrowed, text_prefix_chars,
};
pub(crate) use resolve::{
    column_at, column_collation, find_column_pos, is_binary_coerced, locate_column,
};
use strings::{
    TrimSide, format_string, pg_quote_ident, pg_quote_literal, pg_typeof_name, string_left_right,
    string_pad, string_trim, to_char, value_to_format_text,
};
pub use textsearch::{
    decode_tsquery_external, decode_tsvector_external, format_tsquery, format_tsvector,
};
use textsearch::{
    fts_phraseto_tsquery, fts_plainto_tsquery, fts_setweight, fts_to_tsquery, fts_to_tsvector,
    fts_ts_headline, fts_ts_rank, fts_ts_rank_cd, fts_ts_rewrite, fts_tsquery_bool,
    fts_websearch_to_tsquery, ts_match, tsvector_concat,
};
pub use values::gen_random_uuid_bytes;
/// v7.39 (tz epic) — fixed-offset / abbreviation resolution, exposed
/// for `SET timezone` validation (named zones go through the host tzdb).
pub(crate) fn datetime_resolve_zone_offset(z: &str) -> Option<i64> {
    datetime::resolve_zone_offset(z)
}

pub use values::value_to_text;
pub use values::value_to_text_styled;
pub use values::value_to_text_typed;
pub use values::value_to_text_typed_styled;
pub use values::value_to_text_with_fsp;
use values::{
    array_2d_dims, array_element_at, array_len, array_rebuild, value_cmp_for_min_max, value_to_f64,
    values_equal_for_nullif,
};

/// Resolution context for evaluating a single row. `table_alias` is the alias
/// (or table name) callers should accept as the qualifier on a column ref —
/// e.g. `FROM users AS u` makes `u.name` valid and rejects `other.name`.
#[derive(Clone)]
#[allow(missing_debug_implementations)] // sequence_resolver is a dyn Fn — no Debug
pub struct EvalContext<'a> {
    pub columns: &'a [ColumnSchema],
    pub table_alias: Option<&'a str>,
    /// v6.1.1 — bound parameters for `$N` placeholders inside the
    /// expression tree. Empty for simple queries; populated by the
    /// prepared-statement Execute path with Bind values converted
    /// to `Value`. Index N (1-based per PG) hits `params[N-1]`.
    pub params: &'a [Value<'static>],
    /// v7.12.1 — session text-search config (from `SET
    /// default_text_search_config = '<name>'`). Resolved when the
    /// engine builds an `EvalContext` and consumed by the FTS
    /// function dispatcher when `to_tsvector(text)` /
    /// `plainto_tsquery(text)` etc are called without an explicit
    /// config arg. `None` falls through to `simple`.
    pub default_text_search_config: Option<&'a str>,
    /// v7.17.0 Phase 1.1 — `nextval` / `currval` / `setval`
    /// resolver. The engine builds this around a `&mut Catalog`
    /// so apply_function can mutate sequence state without
    /// eval owning a catalog reference. When `None`, sequence
    /// functions return an error (read-only contexts).
    pub sequence_resolver: Option<&'a SequenceResolver<'a>>,
    /// v7.37.16 (16.12) — read-only catalog reference for
    /// builtins that need catalog walks (e.g. `pg_partition_root`,
    /// `pg_partition_ancestors`). `None` falls through to the
    /// "no catalog available" branch which returns NULL — same
    /// shape PG returns for a non-existent OID. Most evaluation
    /// sites don't need catalog access (row scans, projections);
    /// they construct contexts with `catalog: None` and the
    /// engine populates `Some(&self.catalog)` only at the engine's
    /// top-level entry points where the borrow is unambiguous.
    pub catalog: Option<&'a spg_storage::Catalog>,
    /// v7.39 (round 346, M1) — is this a MySQL-dialect session? The two
    /// dialects disagree about what counts as a truth value: MariaDB
    /// takes any non-zero number (and a string's leading number) as
    /// true, PG refuses anything that is not boolean. Set from the
    /// engine by [`EvalContext::with_engine`]; a context built without
    /// one keeps PG's stricter reading.
    pub mysql_dialect: bool,
    /// Session GUCs set via `SET name = value` / `set_config`, keyed by
    /// lowercased name. `current_setting('app.foo')` reads custom
    /// (namespaced) settings from here — the mechanism apps use for
    /// request context / RLS. `None` in read-only contexts that have no
    /// session; unknown names then fall through to PG defaults.
    pub session_gucs: Option<&'a alloc::collections::BTreeMap<String, String>>,
    /// v7.39 (read01 round 58) — the engine's role store, so
    /// `has_table_privilege('bob', …)` can expand bob's role MEMBERSHIPS (a
    /// grant to a group role answers `true` for its inheriting members). `None`
    /// in a context with no engine behind it — the role then stands alone.
    pub users: Option<&'a crate::users::UserStore>,
    /// v7.39 (read01 round 61) — how deep we are inside USER-DEFINED function
    /// bodies. A function's body is evaluated with a child context, and a body
    /// may call another function, so this bounds the recursion (a function that
    /// calls itself would otherwise blow the stack, which an embed host cannot
    /// catch).
    pub fn_depth: u16,
    /// v7.39 (read01 round 63) — the ENGINE, for a user-function body that has
    /// its own FROM (`SELECT v FROM t WHERE id = k`). Such a body has to run
    /// through the real executor: reading `catalog`'s rows straight from eval
    /// would bypass the row-header visibility filter, so under in-place MVCC a
    /// function would happily read DEAD rows. `None` in a context with no
    /// engine behind it — a body with a FROM then errors, saying so.
    pub engine: Option<&'a crate::Engine>,
    /// v7.38 (read01 U15) — per-scan deterministic sampler state for
    /// `TABLESAMPLE … REPEATABLE(seed)`. A fresh cell is created before a
    /// scan whose predicate may draw `__tsm_fract(seed)`; the cell holds
    /// `None` until the first draw seeds it from that literal, then a
    /// scan-local xorshift sequence (isolated from the process-global
    /// `random()` PRNG, so it's deterministic and rescan-stable). `None`
    /// here means no sampler is attached.
    pub sample_rng: Option<&'a core::cell::Cell<Option<u64>>>,
    /// v7.38 (read01 P3.25) — native-stack-overflow guard. Lazily seeded
    /// with the stack pointer of the outermost `eval_expr` call; deeper
    /// calls compare their own pointer against it and bail with
    /// [`EvalError::StackDepthExceeded`] once usage crosses a safe margin,
    /// so a pathologically nested expression errors instead of aborting the
    /// process. Owned (not a borrowed cell) so it stays stack-local and
    /// never touches `Engine`'s `Sync` bound.
    pub recursion_base: core::cell::Cell<usize>,
    /// v7.39 (GUC knife 3) — session render style (DateStyle /
    /// IntervalStyle / extra_float_digits) for text output produced
    /// inside expression evaluation (`::text` casts). Contexts built
    /// away from the session (per-shard scan filters, index probes)
    /// keep the default — they don't render text output.
    pub render_style: crate::eval::format::RenderStyle,
    /// v7.39 (tz epic) — host IANA timezone lookups for named zones
    /// (session rendering, AT TIME ZONE, literal zone suffixes).
    pub tz_offset_fn: Option<crate::TzOffsetFn>,
    pub tz_localize_fn: Option<crate::TzLocalizeFn>,
    pub tz_abbrev_fn: Option<crate::TzAbbrevFn>,
    /// v7.38 (read01 P5.24) — host-provided CSPRNG (the server injects
    /// `/dev/urandom`). Cryptographic builtins (`gen_random_bytes`,
    /// `gen_salt`) draw from this instead of the process-static xorshift
    /// PRNG, so their output isn't predictable. `None` (no host CSPRNG)
    /// falls back to the PRNG — fine for the non-cryptographic `random()`.
    pub salt_fn: Option<crate::SaltFn>,
    /// v7.39 (read01 pgstatfuncs.c) — calling-connection identity for
    /// pg_backend_pid(); `None` (embedded / detached contexts) → 1.
    pub backend_pid_fn: Option<crate::BackendPidFn>,
    /// v7.39 (round 476) — the WAL byte position, for the LSN functions.
    pub wal_lsn_fn: Option<crate::WalLsnFn>,
    /// v7.39 (round 318, V51) — host connection-control hook for
    /// `pg_cancel_backend` / `pg_terminate_backend`. `None` (embedded /
    /// detached contexts) ⇒ there is nothing to signal, so they answer
    /// false rather than pretending the signal landed.
    pub backend_signal_fn: Option<crate::BackendSignalFn>,
    /// v7.38 (read01 P6.08) — host wall clock (µs since Unix epoch). `uuidv7`
    /// uses it for the real time-ordered 48-bit millisecond prefix; `None`
    /// (no host clock) falls back to the deterministic anchor.
    pub clock: Option<crate::ClockFn>,
    /// v7.38 (T24) — read-only view of the engine's transaction-version state,
    /// so the `txid_*` / `pg_*_xact_id` / `pg_xact_status` builtins report the
    /// real transaction ids instead of a constant stub. `None` on the scan /
    /// join / aggregate contexts that never evaluate them.
    pub xact: Option<XactView<'a>>,
    /// v7.38 (T24) — PG's `txid_current()` ASSIGNS an id to a transaction that
    /// has none. In autocommit a read-only statement has no writer version, so
    /// the first call allocates one here and later calls in the same statement
    /// reuse it — `SELECT txid_current(), txid_current()` must agree, as in PG.
    pub assigned_xid: core::cell::Cell<Option<u64>>,
}

/// v7.38 (T24) — the transaction-id surface PG's `txid_*` family exposes.
/// SPG's writer versions ARE its transaction ids (`row_header::next_version`),
/// so no separate xid counter is needed — this is the bridge U22 was waiting
/// on.
#[derive(Clone, Copy, Debug)]
pub struct XactView<'a> {
    /// The id assigned to the current transaction (allocated at BEGIN) or, in
    /// autocommit, to the current statement once it has written. `None` when
    /// nothing has been assigned — `*_if_assigned` returns NULL there, as PG does.
    pub current: Option<u64>,
    /// Ids allocated by transactions that have neither committed nor aborted.
    pub active: &'a alloc::collections::BTreeSet<u64>,
    /// Ids of rolled-back transactions.
    pub aborted: &'a alloc::collections::BTreeSet<u64>,
}

/// v7.17.0 — sequence-mutating callback used by `apply_function`
/// for `nextval` / `currval` / `setval`. Implemented by the
/// engine to thread `&mut Catalog` access through an immutable
/// `&EvalContext`.
pub type SequenceResolver<'a> = dyn Fn(SequenceOp) -> Result<i64, EvalError> + 'a;

/// v7.17.0 — sequence operation requested by an Expr eval.
#[derive(Debug, Clone)]
pub enum SequenceOp {
    Next(String),
    Curr(String),
    Set {
        name: String,
        value: i64,
        is_called: bool,
    },
}

impl<'a> EvalContext<'a> {
    pub const fn new(columns: &'a [ColumnSchema], table_alias: Option<&'a str>) -> Self {
        Self {
            columns,
            table_alias,
            params: &[],
            default_text_search_config: None,
            sequence_resolver: None,
            catalog: None,
            mysql_dialect: false,
            session_gucs: None,
            users: None,
            fn_depth: 0,
            engine: None,
            sample_rng: None,
            recursion_base: core::cell::Cell::new(0),
            render_style: crate::eval::format::RenderStyle {
                date_style: crate::eval::format::DateStyleKind::Iso,
                date_order: crate::eval::format::DateOrder::Mdy,
                interval_style: crate::eval::format::IntervalStyleKind::Postgres,
                extra_float_digits: 1,
                bytea_escape: false,
                mysql: false,
            },
            tz_offset_fn: None,
            tz_localize_fn: None,
            tz_abbrev_fn: None,
            salt_fn: None,
            backend_pid_fn: None,
            wal_lsn_fn: None,
            backend_signal_fn: None,
            clock: None,
            xact: None,
            assigned_xid: core::cell::Cell::new(None),
        }
    }

    /// v7.39 (GUC knife 3) — attach the session render style.
    #[must_use]
    pub const fn with_render_style(mut self, style: crate::eval::format::RenderStyle) -> Self {
        self.render_style = style;
        self
    }

    /// v7.39 (round 318, V51) — attach the host connection-control hook.
    #[must_use]
    pub const fn with_backend_signal_fn(mut self, f: Option<crate::BackendSignalFn>) -> Self {
        self.backend_signal_fn = f;
        self
    }

    /// v7.39 (round 476) — attach the WAL byte-position provider.
    #[must_use]
    pub const fn with_wal_lsn_fn(mut self, f: Option<crate::WalLsnFn>) -> Self {
        self.wal_lsn_fn = f;
        self
    }

    /// v7.39 (read01 pgstatfuncs.c) — attach the calling-connection id.
    #[must_use]
    pub const fn with_backend_pid_fn(mut self, f: Option<crate::BackendPidFn>) -> Self {
        self.backend_pid_fn = f;
        self
    }

    /// v7.39 (tz epic) — attach the host timezone lookups.
    #[must_use]
    pub const fn with_tz_fns(
        mut self,
        offset: Option<crate::TzOffsetFn>,
        localize: Option<crate::TzLocalizeFn>,
        abbrev: Option<crate::TzAbbrevFn>,
    ) -> Self {
        self.tz_offset_fn = offset;
        self.tz_localize_fn = localize;
        self.tz_abbrev_fn = abbrev;
        self
    }

    /// v7.39 (tz epic) — offset (µs east) of an arbitrary zone spec at
    /// a UTC instant: fixed forms resolve statically, named zones
    /// through the host tzdb. None = unknown zone.
    #[must_use]
    pub fn zone_offset_at(&self, zone: &str, utc_micros: i64) -> Option<i64> {
        if let Some(off) = datetime::resolve_zone_offset(zone) {
            return Some(off);
        }
        self.tz_offset_fn.and_then(|f| f(zone, utc_micros))
    }

    /// v7.39 (tz epic) — the SESSION zone's offset at a UTC instant
    /// (per-value: DST zones vary within one statement).
    #[must_use]
    pub fn session_tz_offset_at(&self, utc_micros: i64) -> i64 {
        let Some(zone) = self.session_gucs.and_then(|g| g.get("timezone")) else {
            return 0;
        };
        self.zone_offset_at(zone, utc_micros).unwrap_or(0)
    }

    /// v7.39 (tz epic) — the session zone's designation at an instant
    /// (named zones only; None lets renderers spell UTC / +HH).
    #[must_use]
    pub fn session_tz_abbrev_at(&self, utc_micros: i64) -> Option<alloc::string::String> {
        let zone = self.session_gucs.and_then(|g| g.get("timezone"))?;
        if datetime::resolve_zone_offset(zone).is_some()
            || zone.eq_ignore_ascii_case("utc")
            || zone.eq_ignore_ascii_case("gmt")
        {
            return None;
        }
        self.tz_abbrev_fn.and_then(|f| f(zone, utc_micros))
    }

    /// v7.39 (tz epic) — local wall micros in `zone` -> UTC micros
    /// (PG's DST disambiguation for named zones).
    #[must_use]
    pub fn zone_local_to_utc(&self, zone: &str, local_micros: i64) -> Option<i64> {
        zone_local_to_utc_with(zone, local_micros, self.tz_localize_fn)
    }

    /// v7.38 (read01 P5.24) — attach the host CSPRNG so cryptographic
    /// builtins don't fall back to the predictable PRNG.
    #[must_use]
    pub const fn with_salt_fn(mut self, f: Option<crate::SaltFn>) -> Self {
        self.salt_fn = f;
        self
    }

    /// v7.38 (read01 P6.08) — attach the host wall clock so `uuidv7` gets a
    /// real time-ordered prefix instead of the deterministic anchor.
    #[must_use]
    pub const fn with_clock(mut self, f: Option<crate::ClockFn>) -> Self {
        self.clock = f;
        self
    }

    /// v7.38 (read01 U15) — attach a per-scan `TABLESAMPLE REPEATABLE`
    /// sampler cell. The cell (seeded lazily on first `__tsm_fract` draw)
    /// must outlive the context and be created fresh per scan so a rescan
    /// re-seeds and reproduces the same sample.
    #[must_use]
    pub const fn with_sample_rng(mut self, cell: &'a core::cell::Cell<Option<u64>>) -> Self {
        self.sample_rng = Some(cell);
        self
    }

    /// Attach the session's GUC map so `current_setting` can resolve
    /// custom (namespaced) settings written with `SET` / `set_config`.
    #[must_use]
    /// v7.39 (read01 round 63) — thread the engine (see `engine`).
    pub const fn with_engine(mut self, engine: &'a crate::Engine) -> Self {
        self.mysql_dialect = engine.backslash_escapes;
        // v7.39 (round 368, M20 P3) — the dialect also decides how a binary
        // string renders in a string context (latin-1 bytes vs PG `\x…`).
        self.render_style.mysql = engine.backslash_escapes;
        self.engine = Some(engine);
        self
    }

    /// v7.39 (read01 round 58) — thread the role store (see `users`).
    pub const fn with_users(mut self, users: &'a crate::users::UserStore) -> Self {
        self.users = Some(users);
        self
    }

    /// v7.39 (round 524) — attach a whole session bag at once. Every
    /// write path needs the same four, and taking them one at a time is
    /// how three of them ended up with none.
    #[must_use]
    pub(crate) fn with_session<'b: 'a>(mut self, s: &'b DmlSession) -> Self {
        self.session_gucs = Some(&s.gucs);
        self.users = Some(&s.users);
        self.render_style = s.render_style;
        self.tz_offset_fn = s.tz_offset_fn;
        self.tz_localize_fn = s.tz_localize_fn;
        self.tz_abbrev_fn = s.tz_abbrev_fn;
        self
    }

    pub const fn with_session_gucs(
        mut self,
        gucs: &'a alloc::collections::BTreeMap<String, String>,
    ) -> Self {
        self.session_gucs = Some(gucs);
        self
    }

    /// v7.37.16 (16.12) — attach a read-only catalog reference
    /// so builtins like `pg_partition_root` can walk partition
    /// roles. Defaults to None (NULL semantics).
    #[must_use]
    pub const fn with_catalog(mut self, catalog: &'a spg_storage::Catalog) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// v7.38 (T-tstz Phase 2) — the micro-offset of the session `TimeZone` GUC
    /// (`SET TimeZone = '+09'` → +9h). A fixed offset / abbreviation resolves;
    /// UTC and an unset GUC give 0; a named IANA zone (no tzdata) also gives 0
    /// so a timestamptz still renders — as `+00` — rather than erroring on
    /// every display. Timestamptz rendering / cast is the only consumer.
    #[must_use]
    pub fn session_tz_offset(&self) -> i64 {
        self.session_gucs
            .and_then(|g| g.get("timezone"))
            .and_then(|z| datetime::resolve_zone_offset(z))
            .unwrap_or(0)
    }

    /// v7.38 (T24) — attach the transaction-version view the `txid_*` builtins
    /// read. Defaults to None, where they fall back to the process-wide cursor.
    #[must_use]
    pub const fn with_xact(mut self, xact: XactView<'a>) -> Self {
        self.xact = Some(xact);
        self
    }

    /// v7.17.0 — attach a sequence resolver. The engine wraps a
    /// `&mut Catalog` in a closure that performs the requested
    /// SequenceOp.
    #[must_use]
    pub const fn with_sequence_resolver(mut self, resolver: &'a SequenceResolver<'a>) -> Self {
        self.sequence_resolver = Some(resolver);
        self
    }

    /// v6.1.1 — attach a parameter buffer for `$N` placeholder
    /// resolution. The slice must outlive the context; callers
    /// construct it from the prepared statement's Bind values.
    #[must_use]
    pub const fn with_params(mut self, params: &'a [Value<'static>]) -> Self {
        self.params = params;
        self
    }

    /// v7.12.1 — attach the session's
    /// `default_text_search_config`. Used by the FTS function
    /// dispatcher when no explicit config arg is given.
    #[must_use]
    pub const fn with_default_text_search_config(mut self, cfg: Option<&'a str>) -> Self {
        self.default_text_search_config = cfg;
        self
    }
}

/// v7.39 (round 523) — read a timestamp literal, reporting whether it
/// carried an offset. Re-exported for the INSERT path, which decides
/// there whether a value already names an instant.
pub(crate) fn parse_timestamp_literal_tz_ordered_pub(
    s: &str,
    order: DateOrder,
) -> Option<(i64, bool)> {
    format::parse_timestamp_literal_tz_ordered(s, order)
}

/// v7.39 (round 523) — a FIXED zone's offset, when the name is one
/// (`+09`, `UTC-5`). Named zones go through the host's tzdb instead.
#[must_use]
pub(crate) fn resolve_zone_offset_pub(zone: &str) -> Option<i64> {
    datetime::resolve_zone_offset(zone)
}

/// v7.39 (round 523) — a wall-clock reading in `zone` as a UTC instant.
///
/// A free function because the INSERT path needs it too, and that path
/// carries no `EvalContext`: it evaluates VALUES through a context-free
/// literal walker. `EvalContext::zone_local_to_utc` delegates here so the
/// two cannot drift.
#[must_use]
pub(crate) fn zone_local_to_utc_with(
    zone: &str,
    local_micros: i64,
    localize: Option<crate::TzLocalizeFn>,
) -> Option<i64> {
    if let Some(off) = datetime::resolve_zone_offset(zone) {
        return Some(local_micros - off);
    }
    localize.and_then(|f| f(zone, local_micros))
}

/// v7.39 (round 523) — the session zone an assignment to a timestamptz
/// column is read in, or `None` when the session is on UTC and no shift
/// applies.
#[derive(Debug, Clone)]
pub(crate) struct SessionCoercion {
    /// The session zone, when it is not UTC. `None` leaves an instant
    /// where it was.
    pub zone: Option<alloc::string::String>,
    pub localize: Option<crate::TzLocalizeFn>,
    /// The session's date order. A written date is ambiguous
    /// (`01/02/2020`), and this is what resolves it.
    pub order: DateOrder,
}

impl SessionCoercion {
    /// The UTC instant a naive wall-clock reading names in the session
    /// zone, or `None` when the session is on UTC.
    #[must_use]
    pub(crate) fn wall_to_utc(&self, wall: i64) -> Option<i64> {
        let zone = self.zone.as_ref()?;
        zone_local_to_utc_with(zone, wall, self.localize)
    }

    /// v7.39 (round 524) — the session facts an ASSIGNMENT is read
    /// under, from an evaluation context. `None` when both are the
    /// defaults and nothing needs re-reading.
    #[must_use]
    pub(crate) fn from_ctx(ctx: &EvalContext<'_>) -> Option<Self> {
        let zone = ctx
            .session_gucs
            .and_then(|g| g.get("timezone"))
            .filter(|z| !z.eq_ignore_ascii_case("utc") && !z.eq_ignore_ascii_case("gmt"))
            .cloned();
        let order = ctx.render_style.date_order;
        if zone.is_none() && order == DateOrder::Mdy {
            return None;
        }
        Some(Self {
            zone,
            localize: ctx.tz_localize_fn,
            order,
        })
    }
}

/// v7.39 (round 524) — the session facts a DML evaluation context needs,
/// cloned so the row loop can still borrow the engine mutably.
///
/// Every write path built a BARE `EvalContext`, so an expression in an
/// UPDATE's SET or a DELETE's WHERE was evaluated by an engine that knew
/// nothing about the connection. One value, built once per statement,
/// and a grep for `dml_session` finds every path that has it.
pub(crate) struct DmlSession {
    pub gucs: alloc::collections::BTreeMap<String, String>,
    pub users: crate::users::UserStore,
    pub render_style: RenderStyle,
    pub tz_offset_fn: Option<crate::TzOffsetFn>,
    pub tz_localize_fn: Option<crate::TzLocalizeFn>,
    pub tz_abbrev_fn: Option<crate::TzAbbrevFn>,
}

/// v7.39 (round 524) — read a TEXT value bound for a temporal column
/// under the session's date order.
///
/// `01/02/2020` is February 1st in a DMY session and January 2nd in an
/// MDY one, and the write path was reading every one of them as MDY: a
/// `SELECT '01/02/2020'::date` answered PG's value while the same
/// literal INSERTed stored the day and month swapped. Nothing errors,
/// and once stored the two readings are indistinguishable.
#[must_use]
pub(crate) fn session_read_temporal_text(
    v: Value<'static>,
    target: spg_storage::DataType,
    coercion: Option<&SessionCoercion>,
) -> Value<'static> {
    use spg_storage::DataType as D;
    let Some(c) = coercion else { return v };
    if c.order == DateOrder::Mdy {
        return v;
    }
    let Value::Text(s) = &v else { return v };
    match target {
        D::Date => format::parse_date_literal_ordered(s, c.order).map_or(v, Value::Date),
        D::Timestamp | D::Timestamptz => format::parse_timestamp_literal_tz_ordered(s, c.order)
            .map_or(v, |(t, _)| Value::Timestamp(t)),
        _ => v,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EvalError {
    ColumnNotFound {
        name: String,
    },
    UnknownQualifier {
        qualifier: String,
    },
    DivisionByZero,
    TypeMismatch {
        detail: String,
    },
    /// v6.1.1 — `$N` reference past the number of bound parameters.
    /// Either the client sent too few in Bind, or the SQL has a
    /// placeholder the prepared statement didn't account for.
    PlaceholderOutOfRange {
        n: u16,
        bound: u16,
    },
    /// v7.38 (read01 P3.25) — the expression tree recursed deep enough to
    /// threaten a native stack overflow; we bail out with an error the way
    /// PG's `check_stack_depth()` does instead of aborting the process.
    StackDepthExceeded,
}

impl core::fmt::Display for EvalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            // v7.39 (read01 round 81) — PG's wording (and SQLSTATE trigger):
            // `column "x" does not exist`, 42703. The old "column not found: x"
            // matched none of the wire layer's `does not exist` patterns, so a
            // missing column reached the client as the generic error class.
            Self::ColumnNotFound { name } => write!(f, "column \"{name}\" does not exist"),
            // v7.39 (round 241) — PG's wording (and 42P01 trigger): a
            // qualifier that names no table in scope is "missing
            // FROM-clause entry for table \"x\"". The old "unknown table
            // qualifier" matched nothing a driver branches on.
            Self::UnknownQualifier { qualifier } => {
                write!(f, "missing FROM-clause entry for table \"{qualifier}\"")
            }
            Self::DivisionByZero => f.write_str("division by zero"),
            Self::TypeMismatch { detail } => write!(f, "type mismatch: {detail}"),
            Self::PlaceholderOutOfRange { n, bound } => write!(
                f,
                "parameter ${n} referenced but only {bound} bound by client"
            ),
            Self::StackDepthExceeded => {
                f.write_str("stack depth limit exceeded (expression nested too deeply)")
            }
        }
    }
}

/// v7.38 (read01 P3.25) — native-stack budget below the outermost
/// `eval_expr` frame. Native stacks are typically 2–8 MB; 768 KiB leaves
/// generous headroom while still permitting PG-class nesting depth (in a
/// release build ~hundreds-to-thousands of frames fit under this).
const MAX_EVAL_STACK_BYTES: usize = 768 * 1024;

/// Address of a local in the current frame — a portable stand-in for the
/// stack pointer (stacks grow downward on all supported targets).
#[inline(never)]
fn eval_stack_ptr() -> usize {
    let probe = 0u8;
    core::ptr::addr_of!(probe) as usize
}

/// v7.38 (read01 P6.40) — enforce a user DOMAIN's NOT NULL + CHECK constraints
/// on a value being cast to it (`x::domain`). NULL fails a NOT NULL domain;
/// otherwise every CHECK (which references the pseudo-column `VALUE`) must not
/// evaluate to false. Returns the value unchanged when all constraints pass.
fn apply_domain_constraints<'a>(
    v: Value<'a>,
    dom: &spg_storage::DomainDef,
    name: &str,
    cat: &spg_storage::Catalog,
) -> Result<Value<'a>, EvalError> {
    if matches!(v, Value::Null) {
        // A NOT NULL anywhere in the chain rejects a NULL.
        let mut cur = Some(dom);
        while let Some(d) = cur {
            if !d.nullable {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("domain {name} does not allow null values"),
                });
            }
            cur = d
                .base_domain
                .as_ref()
                .and_then(|p| cat.domain_types().get(p.as_str()));
        }
        return Ok(v);
    }
    // v7.39 (round 259) — walk the domain chain BASE-FIRST (probed: a
    // value violating both a parent's and the child's constraint reports
    // the PARENT's). The message names the domain being cast TO, but the
    // constraint that actually failed — `value for domain pchild violates
    // check constraint "pbase_check"`.
    let mut chain: alloc::vec::Vec<&spg_storage::DomainDef> = alloc::vec![dom];
    let mut cur = dom;
    while let Some(parent) = cur
        .base_domain
        .as_ref()
        .and_then(|p| cat.domain_types().get(p.as_str()))
    {
        // A cycle cannot be created through CREATE DOMAIN (the parent must
        // already exist), but stop defensively rather than loop forever.
        if chain.iter().any(|d| core::ptr::eq(*d, parent)) {
            break;
        }
        chain.push(parent);
        cur = parent;
    }
    chain.reverse();
    for owner in chain {
        apply_domain_checks_of(&v, owner, name)?;
    }
    Ok(v)
}

/// v7.39 (round 259) — run ONE domain's own CHECK list against `v`. The
/// error names `target` (the domain the value is being cast to) and
/// `owner` (whose constraint failed); for a single-level domain they are
/// the same, which is the pre-259 wording.
fn apply_domain_checks_of(
    v: &Value<'_>,
    dom: &spg_storage::DomainDef,
    target: &str,
) -> Result<(), EvalError> {
    let name = target;
    for chk in &dom.checks {
        let src = &chk.expr;
        // v7.39 (round 260) — report the constraint that failed by NAME.
        let owner = chk.name.as_str();
        let expr = spg_sql::parser::parse_expression(src).map_err(|e| EvalError::TypeMismatch {
            detail: alloc::format!("domain {name} CHECK ({src:?}) failed to re-parse: {e:?}"),
        })?;
        let synth_cols = alloc::vec![spg_storage::ColumnSchema::new(
            "value",
            dom.base_type,
            dom.nullable,
        )];
        let synth_ctx = EvalContext::new(&synth_cols, None);
        // Owned copy so the temporary row doesn't borrow `v`'s lifetime.
        let synth_row = spg_storage::Row {
            values: alloc::vec![v.clone().into_owned()],
        };
        let r = eval_expr(&expr, &synth_row, &synth_ctx)?;
        if matches!(r, Value::Bool(false)) {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "value for domain {name} violates check constraint \"{owner}\""
                ),
            });
        }
    }
    Ok(())
}

/// v7.38 (read01 P6.67) — validate a value cast to a user ENUM: a text label
/// must be one of the enum's members (else error, as PG does); a NULL is a
/// valid typed null. The stored representation stays the text label.
/// v7.39 (read01 rowtypes.c) — cast into a user composite type: parse the
/// `(v1,"v 2",)` record text (double-quote wrapping with doubled quotes,
/// empty field = NULL) and coerce each field to the declared type; a ROW
/// value re-labels positionally.
/// v7.39 (round 350/351, M7 + M11) — how MySQL reads a TEXT operand of
/// an arithmetic or comparison operator. The identity in the PG dialect,
/// and out-of-line so it costs the recursive `eval_expr` frame nothing.
///
/// Measured on MariaDB 11: `'2024-01-15' + INTERVAL 1 DAY` shifts the
/// date; `'1abc'+0` is 1, `'abc'+0` is 0, `'2024-01-15'+0` is 2024; two
/// strings compare as STRINGS (`'10' > '9'` is 0) while a mixed pair
/// compares numerically (`'10' > 9` is 1).
#[inline(never)]
pub(crate) fn mysql_operand_reading_pair(
    op: BinOp,
    l: Value<'static>,
    r: Value<'static>,
) -> (Value<'static>, Value<'static>) {
    if !mysql_coerces(op) {
        return (l, r);
    }
    match (&l, &r) {
        (Value::Text(t), Value::Interval { .. }) => (text_as_temporal(t).unwrap_or(l.clone()), r),
        (Value::Interval { .. }, Value::Text(t)) => {
            let rr = text_as_temporal(t).unwrap_or(r.clone());
            (l, rr)
        }
        // v7.39 (round 353, M10) — a boolean IS an integer in MySQL, so
        // `!1 + 1` is 1 (measured). It was `operator does not exist:
        // boolean + integer`.
        (Value::Bool(b), other)
            if mysql_arith(op) && other.data_type().is_some_and(is_numeric_type) =>
        {
            (Value::BigInt(i64::from(*b)), r)
        }
        (other, Value::Bool(b))
            if mysql_arith(op) && other.data_type().is_some_and(is_numeric_type) =>
        {
            let rr = Value::BigInt(i64::from(*b));
            (l, rr)
        }
        (Value::Text(t), other) if other.data_type().is_some_and(is_numeric_type) => {
            (mysql_number_of(t), r)
        }
        (other, Value::Text(t)) if other.data_type().is_some_and(is_numeric_type) => {
            let rr = mysql_number_of(t);
            (l, rr)
        }
        // v7.39 (round 367, M20 P2) — a binary string beside a number
        // reads as its big-endian integer value (`0x10 + 0` = 16,
        // `0x10 = 16` is true). Beside a Text operand it stays bytes so
        // the byte-wise string compare (`0x61 = 'a'`) still fires.
        (Value::Bytes(b), other) if other.data_type().is_some_and(is_numeric_type) => {
            (mysql_bytes_as_number(b), r)
        }
        (other, Value::Bytes(b)) if other.data_type().is_some_and(is_numeric_type) => {
            let rr = mysql_bytes_as_number(b);
            (l, rr)
        }
        // Arithmetic between two strings is numeric; comparison is not.
        (Value::Text(a), Value::Text(b)) if mysql_arith(op) => {
            (mysql_number_of(a), mysql_number_of(b))
        }
        _ => (l, r),
    }
}

/// Is this a mixed string/number pair, which MySQL compares numerically?
fn mysql_mixed_pair(l: &Value<'_>, r: &Value<'_>) -> bool {
    matches!((l, r), (Value::Text(_), o) | (o, Value::Text(_))
        if o.data_type().is_some_and(is_numeric_type))
}

const fn mysql_arith(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
    )
}

/// Does this comparison need the owned path — because a value's type
/// wants it, because a CI collation folds it, or (MySQL) because a mixed
/// string/number pair compares NUMERICALLY there while two strings
/// compare as strings.
#[inline(never)]
fn needs_owned_compare(
    lc: &Value<'_>,
    rc: &Value<'_>,
    lhs: &Expr,
    rhs: &Expr,
    ctx: &EvalContext<'_>,
) -> bool {
    is_owned_compare_value(lc)
        || is_owned_compare_value(rc)
        || compare_is_case_insensitive(lhs, rhs, ctx)
        || (ctx.mysql_dialect && mysql_mixed_pair(lc, rc))
}

/// Which operators take MySQL's string→number reading.
const fn mysql_coerces(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Add
            | BinOp::Sub
            | BinOp::Mul
            | BinOp::Div
            | BinOp::Mod
            | BinOp::Eq
            | BinOp::NotEq
            | BinOp::Lt
            | BinOp::LtEq
            | BinOp::Gt
            | BinOp::GtEq
    )
}

/// Does this type take part in MySQL's numeric coercion?
fn is_numeric_type(t: spg_storage::DataType) -> bool {
    use spg_storage::DataType as D;
    matches!(
        t,
        D::SmallInt | D::Int | D::BigInt | D::Float | D::Real | D::Numeric { .. }
    )
}

/// v7.39 (round 364, M4 P2) — a value as it participates in a MySQL
/// session's default-collation comparison: text folds (accent- and
/// case-insensitive), everything else is itself. Used by IN and LIKE,
/// whose comparisons do not pass through `collation_fold_for_compare`.
fn mysql_collation_key(v: Value<'static>, mysql: bool, pads: bool) -> Value<'static> {
    match v {
        // v7.38.16 — BpChar too. `mysql_compare_fold` trims trailing
        // spaces before folding, which is the PAD SPACE half of the same
        // comparison, so a CHAR cell needs exactly this call and was not
        // getting it: `s IN ('ALPHA','BETA')` on CHAR(8) answered 1 where
        // MySQL 9.7.1 answers 1,2. `eval/values.rs` had the pair right
        // and these two sites did not.
        // v7.38.17 — CHAR's padding is not data, TEXT's trailing
        // spaces are. Two calls because they are two questions.
        // v7.38.18 — and the collation's padding rule. A CHAR's padding
        // is the type's and never counts; a TEXT's is the collation's.
        // `t IN ('ALPHA')` on a `utf8mb4_uca1400_ai_ci` column — what a
        // MariaDB dump declares — missed the row holding `'alpha  '`,
        // which MariaDB 12.3.2 matches.
        Value::BpChar(s) if mysql => Value::text(spg_storage::mysql_compare_fold_char(&s)),
        Value::Text(s) if mysql && pads => Value::text(spg_storage::mysql_compare_fold_char(&s)),
        Value::Text(s) if mysql => Value::text(spg_storage::mysql_compare_fold(&s)),
        other => other,
    }
}

/// A string as MySQL reads it in numeric position: an exact integer when
/// the leading number is one, otherwise a double.
#[inline(never)]
fn mysql_number_of(s: &str) -> Value<'static> {
    let n = mysql_leading_number(s);
    if n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 {
        #[allow(clippy::cast_possible_truncation)]
        Value::BigInt(n as i64)
    } else {
        Value::Float(n)
    }
}

/// v7.39 (round 367, M20 P2) — a MySQL binary string (a `0x…` / `X'…'` /
/// `b'…'` literal, backed by `Value::Bytes`) reads as its bytes'
/// BIG-ENDIAN unsigned integer in a numeric context: `0x4142 + 0` is
/// 16706, `0x10 = 16` is true (measured on MariaDB 11). Only the low 16
/// bytes participate — a hex literal used in arithmetic is at most an
/// 8-byte BIGINT in practice — and a value past `i64::MAX` becomes a
/// NUMERIC so nothing wraps negative.
fn mysql_bytes_as_number(b: &[u8]) -> Value<'static> {
    let start = b.len().saturating_sub(16);
    let acc = b[start..]
        .iter()
        .fold(0u128, |a, &x| (a << 8) | u128::from(x));
    if acc <= i64::MAX as u128 {
        #[allow(clippy::cast_possible_truncation)]
        Value::BigInt(acc as i64)
    } else {
        crate::conversions::big_literal_to_value(&alloc::format!("{acc}"))
    }
}

/// MySQL's `/`: a real division, and NULL on a zero divisor. `None`
/// when this pairing is not the integer/integer case PG and MySQL
/// disagree about.
#[inline(never)]
pub(crate) fn mysql_true_division(
    op: BinOp,
    l: &Value<'_>,
    r: &Value<'_>,
    text_operand: bool,
) -> Option<Value<'static>> {
    // v7.39 (round 372) — MySQL's `x % 0` / `x MOD 0` is NULL, not the PG
    // "division by zero" error (measured on MariaDB 11: `10%0`, `10 MOD
    // 0`, `10.5%0` are all NULL, matching `1/0`). A non-zero divisor takes
    // the normal modulo path.
    if matches!(op, BinOp::Mod) {
        return if value_is_zero(r) {
            Some(Value::Null)
        } else {
            None
        };
    }
    if !matches!(op, BinOp::Div) {
        return None;
    }
    // v7.39 (round 393) — MariaDB `/` on exact (int / decimal) operands is a
    // DECIMAL whose scale is the LEFT operand's scale + 4 (`7/2` is 3.5000,
    // `10.0/3` is 3.33333, `7.00/2` is 3.500000), NOT a float. A float /
    // double operand — or a STRING one, `'10'/'4'` is 2.5 (double) — makes
    // the result a float; a zero divisor is NULL.
    if text_operand
        || matches!(l, Value::Float(_) | Value::Real(_))
        || matches!(r, Value::Float(_) | Value::Real(_))
    {
        let f = |v: &Value<'_>| -> Option<f64> {
            match v {
                Value::Float(x) => Some(*x),
                Value::Real(x) => Some(f64::from(*x)),
                Value::SmallInt(n) => Some(f64::from(*n)),
                Value::Int(n) => Some(f64::from(*n)),
                #[allow(clippy::cast_precision_loss)]
                Value::BigInt(n) => Some(*n as f64),
                _ => None,
            }
        };
        let (a, b) = (f(l)?, f(r)?);
        return Some(if b == 0.0 {
            Value::Null
        } else {
            Value::Float(a / b)
        });
    }
    let (ls, lsc) = exact_decimal_parts(l)?;
    let (rs, rsc) = exact_decimal_parts(r)?;
    if rs == 0 {
        return Some(Value::Null);
    }
    let result_scale = u32::from(lsc) + 4;
    // result_scaled = round( ls * 10^(rsc + 4) / rs ), half away from zero.
    let pow = 10i128.checked_pow(u32::from(rsc) + 4)?;
    let num = ls.checked_mul(pow)?;
    let q = num / rs;
    let rem = num % rs;
    let bump = if rem.unsigned_abs() * 2 >= rs.unsigned_abs() {
        if (num < 0) == (rs < 0) { 1 } else { -1 }
    } else {
        0
    };
    Some(Value::numeric(q + bump, u16::try_from(result_scale).ok()?))
}

/// The `(scaled, scale)` of an exact integer / NUMERIC value: an integer
/// has scale 0. None for a float / non-numeric (they take the float path).
fn exact_decimal_parts(v: &Value<'_>) -> Option<(i128, u16)> {
    match v {
        Value::SmallInt(n) => Some((i128::from(*n), 0)),
        Value::Int(n) => Some((i128::from(*n), 0)),
        Value::BigInt(n) => Some((i128::from(*n), 0)),
        Value::Numeric {
            scaled,
            scale,
            kind: spg_storage::NumericKind::Finite,
        } => Some((*scaled, *scale)),
        _ => None,
    }
}

/// v7.39 (round 383) — the UNSIGNED 64-bit value a MySQL bitwise operand
/// reads as. MySQL's `& | ^ ~ << >>` all work on `BIGINT UNSIGNED`, so an
/// operand is its 64-bit two's-complement pattern (a negative integer:
/// `-5` is `0xFFFF…FB`), rounded to the nearest integer (a float / numeric:
/// `2.9` is 3), its big-endian value (a `0x…` binary string), or its
/// leading number (a string). Anything else (an inet, a range, a
/// bit-string) returns None so the operator keeps its own meaning.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn mysql_bit_u64(v: &Value<'_>) -> Option<u64> {
    match v {
        Value::SmallInt(n) => Some(i64::from(*n) as u64),
        Value::Int(n) => Some(i64::from(*n) as u64),
        Value::BigInt(n) => Some(*n as u64),
        Value::Bool(b) => Some(u64::from(*b)),
        Value::Float(x) => Some(x.round() as i64 as u64),
        Value::Real(x) => Some(f64::from(*x).round() as i64 as u64),
        Value::Numeric {
            scaled,
            scale,
            kind: spg_storage::NumericKind::Finite,
        } => {
            // Round half away from zero to an integer, then take the low
            // 64 bits (two's complement) — `~2.9` is `~3`.
            if *scale > 38 {
                return None;
            }
            let div = 10i128.pow(u32::from(*scale));
            let q = scaled / div;
            let rem = scaled % div;
            let rounded = if rem.unsigned_abs() * 2 >= div.unsigned_abs() {
                q + scaled.signum()
            } else {
                q
            };
            Some(rounded as u64)
        }
        Value::Bytes(b) => mysql_bit_u64(&mysql_bytes_as_number(b)),
        Value::Text(s) => mysql_bit_u64(&mysql_number_of(s)),
        _ => None,
    }
}

/// A MySQL bitwise result — a `BIGINT UNSIGNED`. It stays a signed
/// `BigInt` while it fits (so an integer-context consumer — MAKE_SET
/// bits, ELT / SUBSTRING / REPEAT counts — still takes it); a value past
/// `i64::MAX` (a set bit 63, e.g. `~5`) has no signed integer type, so it
/// becomes a scale-0 NUMERIC, which holds the whole `0..=2^64-1` range and
/// renders as the plain integer MySQL prints.
fn u64_as_value(n: u64) -> Value<'static> {
    match i64::try_from(n) {
        Ok(v) => Value::BigInt(v),
        Err(_) => Value::numeric(i128::from(n), 0),
    }
}

/// v7.39 (round 383) — the MySQL bitwise operators on UNSIGNED 64-bit
/// integers. Returns None when either operand is not number-like (so an
/// `inet << int` / `bit(n) & bit(n)` / geometric `#` keeps its own path)
/// or the operator is not bitwise. A shift of 64 or more is 0 (MySQL does
/// not mask the shift count).
pub(crate) fn mysql_bitwise(op: BinOp, l: &Value<'_>, r: &Value<'_>) -> Option<Value<'static>> {
    let out = match op {
        BinOp::BitAnd => mysql_bit_u64(l)? & mysql_bit_u64(r)?,
        BinOp::BitOr => mysql_bit_u64(l)? | mysql_bit_u64(r)?,
        BinOp::BitXor => mysql_bit_u64(l)? ^ mysql_bit_u64(r)?,
        // `<<` / `>>` share the inet-containment BinOps; a numeric pair is a
        // shift, anything else stays inet / range / bit-string.
        BinOp::InetContainedBy => {
            let (a, n) = (mysql_bit_u64(l)?, mysql_bit_u64(r)?);
            if n >= 64 { 0 } else { a << n }
        }
        BinOp::InetContains => {
            let (a, n) = (mysql_bit_u64(l)?, mysql_bit_u64(r)?);
            if n >= 64 { 0 } else { a >> n }
        }
        _ => return None,
    };
    Some(u64_as_value(out))
}

/// v7.39 (round 383) — MySQL unary `~x`: the UNSIGNED 64-bit complement
/// (`~5` is 18446744073709551610). None for a non-number operand (a
/// bit-string / inet / macaddr keeps PG's typed complement).
pub(crate) fn mysql_bit_not(v: &Value<'_>) -> Option<Value<'static>> {
    Some(u64_as_value(!mysql_bit_u64(v)?))
}

/// v7.39 (round 390, type-fidelity epic P5) — the inline `SET('a','b',…)`
/// variant list an expression's column is declared with, or None. Mirrors
/// `expr_enum_type_name` — a bare `Expr::Column` looked up by name.
pub(crate) fn expr_set_variants<'e>(
    e: &'e Expr,
    columns: &'e [ColumnSchema],
) -> Option<&'e [String]> {
    match e {
        Expr::Column(c) => columns
            .iter()
            .find(|col| col.name == c.name)
            .and_then(|col| col.inline_set_variants.as_deref()),
        _ => None,
    }
}

/// v7.39 (round 402) — the inline `ENUM('a','b',…)` variant list an
/// expression's column is declared with, or None. Like `expr_set_variants`.
pub(crate) fn expr_inline_enum_variants<'e>(
    e: &'e Expr,
    columns: &'e [ColumnSchema],
) -> Option<&'e [String]> {
    match e {
        Expr::Column(c) => columns
            .iter()
            .find(|col| col.name == c.name)
            .and_then(|col| col.inline_enum_variants.as_deref()),
        _ => None,
    }
}

/// The 1-based ordinal a stored inline-ENUM text carries in a numeric
/// context (`e + 0` is 1 for the first member); the empty string / an
/// unknown member is 0 (MySQL's implicit `''` enum error value).
pub(crate) fn enum_text_to_ordinal(text: &str, variants: &[String]) -> i64 {
    variants
        .iter()
        .position(|v| v == text)
        .map_or(0, |p| p as i64 + 1)
}

/// The bitmask a stored SET text carries in a numeric context: each
/// comma-separated member contributes `1 << its position` in the declared
/// variant list (`'a,c'` over `('a','b','c','d')` is 1 | 4 = 5). An empty
/// string is 0; an unknown member (should not occur — the write path
/// validates) contributes nothing.
pub(crate) fn set_text_to_bitmask(text: &str, variants: &[String]) -> i64 {
    if text.is_empty() {
        return 0;
    }
    let mut bits = 0i64;
    for member in text.split(',') {
        if let Some(pos) = variants.iter().position(|v| v == member) {
            bits |= 1i64 << pos;
        }
    }
    bits
}

/// Is this an arithmetic / bitwise operator MySQL evaluates a SET column
/// numerically under? (`s + 0`, `s & flag`, …). The comparison operators
/// are NOT here — `s = 'a,c'` stays a text compare.
pub(crate) const fn is_mysql_numeric_binop(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Add
            | BinOp::Sub
            | BinOp::Mul
            | BinOp::Div
            | BinOp::Mod
            | BinOp::BitAnd
            | BinOp::BitOr
            | BinOp::BitXor
            | BinOp::InetContainedBy
            | BinOp::InetContains
    )
}

/// v7.39 (round 372) — is `v` a numeric zero (any width / kind)? Used to
/// route `x % 0` / `MOD(x, 0)` to NULL under the MySQL dialect.
pub(crate) fn value_is_zero(v: &Value<'_>) -> bool {
    match v {
        Value::SmallInt(n) => *n == 0,
        Value::Int(n) => *n == 0,
        Value::BigInt(n) => *n == 0,
        Value::Float(x) => *x == 0.0,
        Value::Real(x) => *x == 0.0,
        Value::Numeric { scaled, .. } => *scaled == 0,
        _ => false,
    }
}

/// `-'5'` in the MySQL dialect. Out-of-line for the same frame reason.
#[inline(never)]
fn mysql_negate_text(
    op: spg_sql::ast::UnOp,
    v: &Value<'static>,
) -> Option<Result<Value<'static>, EvalError>> {
    match v {
        Value::Text(t) => Some(apply_unary(op, mysql_number_of(t))),
        _ => None,
    }
}

/// The MySQL reading of a unary operator, or None to let `apply_unary` (the
/// PG path) run. Kept out of `eval_expr`'s recursive frame — see the
/// round-383 frame cliff. Covers `NOT` on any truth value (round 346),
/// `-'str'` numeric negation (round 351), and the unsigned `~` complement
/// (round 383).
#[inline(never)]
fn mysql_unary_arm(
    op: spg_sql::ast::UnOp,
    v: &Value<'static>,
) -> Option<Result<Value<'static>, EvalError>> {
    use spg_sql::ast::UnOp;
    match op {
        // `NOT 5` is 0 — read any non-bool as a truth value; a bool / NULL
        // keeps the PG path (still refused there for non-bool).
        UnOp::Not if !matches!(v, Value::Bool(_) | Value::Null) => Some(mysql_not(v)),
        // `-'5'` is -5, `-'abc'` is 0.
        UnOp::Neg => mysql_negate_text(op, v),
        // `+ anything` is that thing: measured on MariaDB 11, `+'x'` is
        // 'x', `+TRUE` is 1, `+NULL` is NULL. No type check at all, unlike
        // PG, which refuses every non-numeric operand.
        UnOp::Plus => Some(Ok(v.clone())),
        // `~5` is the unsigned 64-bit complement; NULL stays NULL (PG path).
        UnOp::BitNot if !matches!(v, Value::Null) => mysql_bit_not(v).map(Ok),
        _ => None,
    }
}

/// A date / timestamp string as its temporal value, or `None` when it is
/// not one (in which case the operand is left exactly as it was).
#[inline(never)]
fn text_as_temporal(t: &str) -> Option<Value<'static>> {
    parse_timestamp_literal(t)
        .map(Value::Timestamp)
        .or_else(|| parse_date_literal(t).map(Value::Date))
}

/// v7.39 (round 620) — an unadorned string literal, which is what PG calls
/// `unknown`: a value whose type the context gets to choose. `''::TEXT` is
/// not one, and neither is a text column.
fn is_unknown_string_literal(e: &Expr) -> bool {
    matches!(e, Expr::Literal(spg_sql::ast::Literal::String(_)))
}

/// v7.39 (round 620) — resolve such a literal to boolean, which is what a
/// boolean connective asks of it. An unparseable one is PG's input-syntax
/// error (22P02), not a type complaint: `'a' AND true` says
/// `invalid input syntax for type boolean: "a"`, exactly as `'a'::BOOLEAN`
/// does — same failure, same words, because it is the same coercion.
#[inline(never)]
fn coerce_unknown_literal_to_bool(e: &Expr) -> Result<Value<'static>, EvalError> {
    let Expr::Literal(spg_sql::ast::Literal::String(s)) = e else {
        unreachable!("guarded by is_unknown_string_literal")
    };
    cast::cast_value_in(
        Value::Text(s.clone().into()),
        spg_sql::ast::CastTarget::Bool,
        false,
    )
}

/// v7.39 (round 621) — a literal that is plainly not a boolean, and the PG
/// type name for it. `NULL` and a bare string literal are deliberately absent:
/// neither carries a type of its own, and a boolean connective is a context
/// that gives them one.
fn non_boolean_literal_type(e: &Expr) -> Option<&'static str> {
    use spg_sql::ast::Literal as L;
    match e {
        Expr::Literal(L::Integer(_)) => Some("integer"),
        Expr::Literal(L::Float(_)) => Some("double precision"),
        Expr::Literal(L::Numeric { .. } | L::NumericBig(_)) => Some("numeric"),
        _ => None,
    }
}

/// v7.39 (round 621) — `AND` / `OR`, evaluated the way PG evaluates them.
///
/// Round 620 handled the unknown literal here; round 621 adds the part that
/// makes `WHERE x <> 0 AND 1/x > 0` work at all. SPG evaluated both sides
/// always, so the guard idiom — the whole reason that predicate is written
/// that way — raised on the very rows the guard exists to exclude. Measured
/// against PG: `false AND (1/0 = 0)` answers `f`, `true OR (1/0 = 0)` answers
/// `t`, and a filter guarded that way returns its rows.
///
/// PG affords that AND still refuses `false AND 1`, because the two happen at
/// different times: the operand types are checked during ANALYSIS, before any
/// evaluation, and the short circuit is a RUN-TIME decision. Both parts are
/// here — the right-hand operand's type is read statically (it is the side
/// that may go unevaluated), and only a type that is definitively known and
/// definitively not boolean is refused. An unknown type is left alone, so a
/// shape the describer cannot type keeps the old behaviour rather than
/// earning a spurious error.
///
/// Order is PG's too, and it is strictly left-first: `(1/0 = 0) AND false`
/// raises on both, because the left is evaluated before anything can decide
/// that it did not need to be.
///
/// Out-of-line so it costs `eval_expr` no frame (the round-305 frame cliff).
#[inline(never)]
fn eval_connective(
    lhs: &Expr,
    op: BinOp,
    rhs: &Expr,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let side = |e: &Expr| -> Result<Value<'static>, EvalError> {
        if is_unknown_string_literal(e) {
            coerce_unknown_literal_to_bool(e)
        } else {
            eval_expr(e, row, ctx)
        }
    };
    let l = side(lhs)?;
    // The analysis-time half: refuse a right-hand operand that is plainly not
    // boolean, whether or not the short circuit would reach it.
    //
    // Only a LITERAL is read this way. The first cut asked
    // `describe_expr_type` for any expression's type, and it answers
    // confidently and wrongly for shapes that matter here — `NULL` comes back
    // as text, so `true AND NULL` earned a type error; and a MATCH … AGAINST
    // folds internally into an OR over tsvector operands, so full-text search
    // stopped working. Three existing pins caught all of it. A literal cannot
    // be misread, and it is what PG's own refusals in this area are about.
    if let Some(ty) = non_boolean_literal_type(rhs) {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "argument of {} must be type boolean, not type {ty}",
                if matches!(op, BinOp::And) {
                    "AND"
                } else {
                    "OR"
                },
            ),
        });
    }
    // Resolving an unknown literal belongs to the same half — it is a
    // coercion PG performs while analysing, so `false AND 'a'` says
    // `invalid input syntax for type boolean: "a"` rather than answering `f`.
    let rhs_resolved = if is_unknown_string_literal(rhs) {
        Some(coerce_unknown_literal_to_bool(rhs)?)
    } else {
        None
    };
    // The run-time half.
    match (op, &l) {
        (BinOp::And, Value::Bool(false)) => return Ok(Value::Bool(false)),
        (BinOp::Or, Value::Bool(true)) => return Ok(Value::Bool(true)),
        _ => {}
    }
    let r = match rhs_resolved {
        Some(v) => v,
        None => side(rhs)?,
    };
    if matches!(op, BinOp::And) {
        and_3vl(l, r)
    } else {
        apply_binary(op, l, r)
    }
}

/// v7.39 (round 346, M1) — the MySQL reading of `AND` / `OR`, out-of-line
/// so it costs `eval_expr` no frame (see the round-305 frame cliff).
#[inline(never)]
fn eval_mysql_connective(
    lhs: &Expr,
    op: BinOp,
    rhs: &Expr,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let l = as_mysql_truth(eval_expr(lhs, row, ctx)?)?;
    let r = as_mysql_truth(eval_expr(rhs, row, ctx)?)?;
    apply_mysql_connective(op, l, r)
}

/// v7.39 (round 407) — apply a MySQL logical connective (`AND` / `OR` /
/// `XOR`) to two operands already reduced to truth values (`Bool` or
/// `Null`). AND / OR reuse the dialect-blind `apply_binary`; `XOR` is
/// MySQL-only (no `apply_binary` arm) and computed here: NULL on either
/// side yields NULL, otherwise the exclusive-or of the two truth values.
pub(crate) fn apply_mysql_connective(
    op: BinOp,
    l: Value<'static>,
    r: Value<'static>,
) -> Result<Value<'static>, EvalError> {
    if op == BinOp::LogicalXor {
        return Ok(match (&l, &r) {
            (Value::Bool(a), Value::Bool(b)) => Value::Bool(a != b),
            _ => Value::Null,
        });
    }
    apply_binary(op, l, r)
}

#[inline(never)]
pub(crate) fn as_mysql_truth(v: Value<'static>) -> Result<Value<'static>, EvalError> {
    Ok(match v {
        Value::Null => Value::Null,
        other => Value::Bool(predicate_is_true(&other, "AND", true)?),
    })
}

/// The MySQL reading of `NOT`, likewise out-of-line.
#[inline(never)]
fn mysql_not(v: &Value<'_>) -> Result<Value<'static>, EvalError> {
    Ok(Value::Bool(!predicate_is_true(v, "NOT", true)?))
}

/// v7.39 (round 346, M1) — is this value TRUE, in a position that wants a
/// truth value (WHERE / CASE WHEN / NOT / AND / OR / HAVING / ON)?
///
/// The engine used to write `matches!(v, Value::Bool(true))` at every such
/// position, so anything that was not already a boolean silently read as
/// FALSE. `SELECT CASE WHEN 1 THEN 'a' END` answered NULL and — far worse —
/// `SELECT … WHERE 1` returned **no rows at all**. Neither dialect does
/// that: MariaDB 11 takes any non-zero number as true, and PG 18.4 raises
/// `argument of WHERE must be type boolean, not type integer`.
///
/// NULL is not true (three-valued logic) and is not an error in either.
pub(crate) fn predicate_is_true(v: &Value<'_>, kw: &str, mysql: bool) -> Result<bool, EvalError> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Null => Ok(false),
        _ if mysql => Ok(mysql_truthy(v)),
        // PG resolves a bare literal in this position through boolean
        // INPUT, so `CASE WHEN 'true'` is legal and `'abc'` is not.
        Value::Text(t) => match crate::eval::cast::cast_value(
            Value::text(t.to_string()),
            spg_sql::ast::CastTarget::Bool,
        )? {
            Value::Bool(b) => Ok(b),
            _ => Ok(false),
        },
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "argument of {kw} must be type boolean, not type {}",
                crate::eval::strings::pg_typeof_name(other)
            ),
        }),
    }
}

/// MariaDB 11's reading, measured: a number is true when it is not zero
/// (`-1` and `0.5` are both true); a string contributes its LEADING
/// number, so `'1abc'` is true while `'abc'` and `''` are false.
fn mysql_truthy(v: &Value<'_>) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::SmallInt(n) => *n != 0,
        Value::Int(n) => *n != 0,
        Value::BigInt(n) => *n != 0,
        Value::Float(f) => *f != 0.0,
        Value::Real(f) => *f != 0.0,
        Value::Numeric { scaled, .. } => *scaled != 0,
        Value::Text(t) => mysql_leading_number(t) != 0.0,
        Value::BpChar(t) => mysql_leading_number(t) != 0.0,
        // Everything else converts to a non-zero number in MariaDB (a
        // DATE reads as its YYYYMMDD digits, for one).
        _ => true,
    }
}

/// The leading numeric prefix of a string, MySQL-style: `'1abc'` is 1,
/// `'abc'` and `''` are 0.
#[inline(never)]
pub(crate) fn mysql_leading_number(s: &str) -> f64 {
    let t = s.trim_start();
    let mut end = 0usize;
    let mut seen_dot = false;
    let mut seen_digit = false;
    // v7.39 (round 351, M11) — the exponent form counts: MariaDB reads
    // `'1e3'` as 1000 and `'1.5e2'` as 150 (measured). A trailing `e`
    // with no digits after it is not part of the number (`'1e'` is 1).
    let mut seen_exp = false;
    let mut exp_at = 0usize;
    for (i, c) in t.char_indices() {
        match c {
            '-' | '+' if i == 0 => {}
            '-' | '+' if seen_exp && i == exp_at + 1 => {}
            '0'..='9' => seen_digit = true,
            '.' if !seen_dot && !seen_exp => seen_dot = true,
            'e' | 'E' if seen_digit && !seen_exp => {
                seen_exp = true;
                exp_at = i;
            }
            _ => break,
        }
        end = i + c.len_utf8();
    }
    if !seen_digit {
        return 0.0;
    }
    // Trim an exponent that never got its digits.
    let mut text = &t[..end];
    while !text.is_empty() && text.parse::<f64>().is_err() {
        text = &text[..text.len() - 1];
    }
    text.parse::<f64>().unwrap_or(0.0)
}

/// v7.39 (read01 ruleutils.c) — resolve a relation name to its synthetic
/// oid: user tables in the 16384+ band (table_names order), views at
/// 32768+, and the synthesised system catalogs at their REAL PG oids.
/// `None` when the name is unknown (the caller keeps the legacy text
/// behaviour so `'anything'::regclass::text` still round-trips).
pub(crate) fn regclass_name_to_oid(cat: &spg_storage::Catalog, bare: &str) -> Option<i64> {
    // v7.39 (round 337, V62) — an INDEX and a SEQUENCE are relations too:
    // both have a `pg_class` row, so both answer to `::regclass` in PG.
    // v7.39 (round 338, V64) — and the bands live in ONE allocator now,
    // shared with the catalog synths, so `pg_class.oid = 'x'::regclass`
    // holds for every kind rather than only for tables.
    if let Some(oid) = crate::system_catalog::relation_oid(cat, bare) {
        return Some(oid);
    }
    Some(match bare {
        "pg_type" => 1247,
        "pg_attribute" => 1249,
        "pg_proc" => 1255,
        "pg_class" => 1259,
        "pg_database" => 1262,
        "pg_constraint" => 2606,
        "pg_index" => 2610,
        "pg_namespace" => 2615,
        // v7.39 (round 650) — the text-search catalogs. This list is a
        // hand-kept subset of `CATALOG_RELATIONS`, which is why adding a
        // catalog there was not enough for `'pg_ts_config'::regclass`.
        "pg_ts_config" => 3602,
        "pg_ts_config_map" => 3603,
        "pg_ts_dict" => 3600,
        "pg_ts_parser" => 3601,
        "pg_ts_template" => 3764,
        // 7.38.1 S5.1 — stop hand-copying: anything CATALOG_RELATIONS
        // publishes resolves here too (pg_dump's dependency pass casts
        // 'pg_extension' / 'pg_amop' / 'pg_opfamily'::regclass).
        other => {
            return crate::system_catalog::CATALOG_RELATIONS
                .iter()
                .find(|(n, _)| other.eq_ignore_ascii_case(n))
                .map(|(_, oid)| *oid);
        }
    })
}

/// v7.39 (round 263) — crate-visible wrapper so the write path can
/// relabel + coerce a value into a composite column's declared type.
pub(crate) fn apply_composite_cast_pub(
    v: Value<'static>,
    comp: &spg_storage::CompositeDef,
    cat: Option<&spg_storage::Catalog>,
) -> Result<Value<'static>, EvalError> {
    apply_composite_cast_in(v, comp, cat)
}

/// v7.39 (round 264) — resolve one field's value, recursing when the
/// field is itself a COMPOSITE. Without this a nested field kept the
/// inner record's TEXT rendering, so `(x).inner.street` errored and
/// `row_to_json` nested a string rather than an object.
fn coerce_composite_field(
    val: Value<'static>,
    fname: &str,
    fty: spg_storage::DataType,
    user_ty: Option<&str>,
    cat: Option<&spg_storage::Catalog>,
) -> Result<Value<'static>, EvalError> {
    if matches!(val, Value::Null) {
        return Ok(val);
    }
    if let Some(tn) = user_ty
        && let Some(inner) = cat.and_then(|c| c.composite_types().get(tn))
    {
        return apply_composite_cast_in(val, inner, cat);
    }
    crate::conversions::coerce_value(val, fty, fname, 0).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("{e}"),
    })
}

fn apply_composite_cast(
    v: Value<'static>,
    comp: &spg_storage::CompositeDef,
) -> Result<Value<'static>, EvalError> {
    apply_composite_cast_in(v, comp, None)
}

fn apply_composite_cast_in(
    v: Value<'static>,
    comp: &spg_storage::CompositeDef,
    cat: Option<&spg_storage::Catalog>,
) -> Result<Value<'static>, EvalError> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Composite(fields) => {
            if fields.len() != comp.fields.len() {
                // PG reports the SHAPE mismatch as a plain cast refusal.
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("cannot cast type record to {}", comp.name),
                });
            }
            // v7.39 (round 263) — relabel AND coerce: this branch only
            // renamed the fields, so `ROW('x','notanint')::addr` kept the
            // text in an int field and PG's input error never fired.
            let mut out: alloc::vec::Vec<(alloc::string::String, Value<'static>)> =
                alloc::vec::Vec::with_capacity(comp.fields.len());
            for (i, ((name, fty), (_, val))) in comp.fields.iter().zip(fields).enumerate() {
                let ut = comp.field_user_types.get(i).and_then(Option::as_deref);
                let coerced = coerce_composite_field(val, name, *fty, ut, cat)?;
                out.push((name.clone(), coerced));
            }
            Ok(Value::Composite(out))
        }
        Value::Text(s) => {
            let raw = parse_record_text(s.as_ref()).ok_or_else(|| EvalError::TypeMismatch {
                detail: alloc::format!("malformed record literal: \"{s}\""),
            })?;
            if raw.len() != comp.fields.len() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("malformed record literal: \"{s}\""),
                });
            }
            let mut out: alloc::vec::Vec<(alloc::string::String, Value<'static>)> =
                alloc::vec::Vec::with_capacity(raw.len());
            for (i, ((fname, fty), field_text)) in comp.fields.iter().zip(raw).enumerate() {
                let ut = comp.field_user_types.get(i).and_then(Option::as_deref);
                let val = match field_text {
                    None => Value::Null,
                    Some(t) => coerce_composite_field(Value::text(t), fname, *fty, ut, cat)?,
                };
                out.push((fname.clone(), val));
            }
            Ok(Value::Composite(out))
        }
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "cannot cast {} to composite type \"{}\"",
                crate::conversions::pg_type_name_for_error_opt(other.data_type()),
                comp.name
            ),
        }),
    }
}

/// Split PG's record text `(f1,f2,...)` into per-field raw strings
/// (None = empty field = NULL). Double quotes wrap fields containing
/// metacharacters; `""` inside is a literal quote; a backslash escapes
/// the next character.
fn parse_record_text(s: &str) -> Option<alloc::vec::Vec<Option<alloc::string::String>>> {
    let t = s.trim();
    let inner = t.strip_prefix('(')?.strip_suffix(')')?;
    let mut out: alloc::vec::Vec<Option<alloc::string::String>> = alloc::vec::Vec::new();
    let chars: alloc::vec::Vec<char> = inner.chars().collect();
    let mut field = alloc::string::String::new();
    let mut quoted_seen = false;
    let mut i = 0usize;
    let mut in_quotes = false;
    loop {
        if i >= chars.len() {
            if in_quotes {
                return None;
            }
            out.push(if field.is_empty() && !quoted_seen {
                None
            } else {
                Some(field.clone())
            });
            break;
        }
        let c = chars[i];
        if in_quotes {
            match c {
                '"' if chars.get(i + 1) == Some(&'"') => {
                    field.push('"');
                    i += 2;
                }
                '"' => {
                    in_quotes = false;
                    i += 1;
                }
                '\\' => {
                    field.push(*chars.get(i + 1)?);
                    i += 2;
                }
                _ => {
                    field.push(c);
                    i += 1;
                }
            }
        } else {
            match c {
                '"' => {
                    in_quotes = true;
                    quoted_seen = true;
                    i += 1;
                }
                ',' => {
                    out.push(if field.is_empty() && !quoted_seen {
                        None
                    } else {
                        Some(core::mem::take(&mut field))
                    });
                    quoted_seen = false;
                    i += 1;
                }
                '\\' => {
                    field.push(*chars.get(i + 1)?);
                    i += 2;
                }
                _ => {
                    field.push(c);
                    i += 1;
                }
            }
        }
    }
    Some(out)
}

fn apply_enum_cast<'a>(
    v: Value<'a>,
    en: &spg_storage::EnumDef,
    name: &str,
) -> Result<Value<'a>, EvalError> {
    match &v {
        Value::Null => Ok(v),
        Value::Text(s) => {
            if en.labels.iter().any(|l| l.as_str() == s.as_ref()) {
                Ok(v)
            } else {
                Err(EvalError::TypeMismatch {
                    detail: alloc::format!("invalid input value for enum {name}: {s:?}"),
                })
            }
        }
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "cannot cast {} to enum {name}",
                crate::conversions::pg_type_name_for_error_opt(other.data_type())
            ),
        }),
    }
}

/// v7.39 (read01 utils/adt, enum.c) — enum_first / enum_last /
/// enum_range resolved from the argument's STATIC enum type (an explicit
/// `::enumtype` cast or a column's `ColumnSchema.user_enum_type`) over the
/// catalog's member order. Returns None when no argument names a known
/// enum, letting the generic function path produce its usual error.
/// Out-of-line (`inline(never)`) so the sizable locals don't land in
/// `eval_expr`'s recursion frame.
/// Enum-ness lives outside the DataType lattice: the witness for "this
/// expression is enum-typed" is an explicit `::enumtype` cast or a column
/// whose `ColumnSchema.user_enum_type` is set.
/// v7.39 (round 258) — crate-visible wrapper so the projection builder
/// can keep an expression's enum identity (see `select.rs`).
pub(crate) fn expr_enum_type_name_pub<'e>(
    e: &'e Expr,
    columns: &'e [ColumnSchema],
) -> Option<&'e str> {
    expr_enum_type_name(e, columns)
}

/// v7.39 (round 425) — the fractional-seconds precision a projected
/// expression should RENDER with: the widest declared precision among the
/// MySQL temporal columns it reads. `MAX(d3)` and `d3 + INTERVAL 1 SECOND`
/// both keep `d3`'s three digits, as MariaDB does. `None` when the
/// expression touches no such column, which leaves PG rendering untouched.
///
/// Residual (recorded, not modelled): MariaDB also WIDENS the precision from
/// some operands — `DATE_ADD(d3, INTERVAL 1 MICROSECOND)` prints six digits
/// there. Taking the max over referenced columns covers the common shapes
/// and never narrows below the source column.
pub(crate) fn expr_mysql_fsp(e: &Expr, columns: &[ColumnSchema]) -> Option<u8> {
    fn walk(e: &Expr, columns: &[ColumnSchema], best: &mut Option<u8>) {
        match e {
            Expr::Column(c) => {
                if let Some(f) = columns
                    .iter()
                    .find(|col| col.name == c.name)
                    .and_then(|col| col.mysql_fsp)
                {
                    *best = Some(best.map_or(f, |b: u8| b.max(f)));
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                walk(lhs, columns, best);
                walk(rhs, columns, best);
            }
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => walk(expr, columns, best),
            Expr::FunctionCall { args, .. } => {
                for a in args {
                    walk(a, columns, best);
                }
            }
            Expr::Case {
                operand,
                branches,
                else_branch,
            } => {
                if let Some(o) = operand.as_deref() {
                    walk(o, columns, best);
                }
                for (w, t) in branches {
                    walk(w, columns, best);
                    walk(t, columns, best);
                }
                if let Some(el) = else_branch.as_deref() {
                    walk(el, columns, best);
                }
            }
            _ => {}
        }
    }
    let mut best = None;
    walk(e, columns, &mut best);
    best
}

/// v7.39 (round 467) — is this expression MySQL-UNSIGNED?
///
/// MySQL decides unsignedness statically, from the expression's type, not
/// from the value it happens to produce. Measured on MariaDB 11: `SUM(a) -
/// 100` answers -99 even though `a` is `INT UNSIGNED`, because SUM's result
/// type is not unsigned; `a - 5` on the same column raises 1690. So this
/// walks the expression the way MySQL's type resolution does.
///
/// A cast names its target `unsigned` (the parser lowercases MySQL's
/// `CAST(x AS UNSIGNED)` into `CastTarget::Named`). Arithmetic is unsigned
/// when EITHER operand is — that is MySQL's rule, and it is why `1 - b`
/// raises while `5 - a` does not: both are unsigned expressions, but only
/// the first has a negative result.
///
/// Deliberately NOT unsigned: unary minus (MariaDB answers -1 for
/// `-CAST(1 AS UNSIGNED)`), and every function result including the
/// aggregates. Both measured.
pub(crate) fn expr_is_mysql_unsigned(e: &Expr, columns: &[ColumnSchema]) -> bool {
    match e {
        Expr::Column(c) => columns
            .iter()
            .find(|col| col.name == c.name)
            .is_some_and(|col| col.is_unsigned),
        Expr::Cast {
            target: CastTarget::Named(n),
            ..
        } => n.eq_ignore_ascii_case("unsigned"),
        Expr::Binary {
            lhs,
            op: BinOp::Add | BinOp::Sub | BinOp::Mul,
            rhs,
        } => expr_is_mysql_unsigned(lhs, columns) || expr_is_mysql_unsigned(rhs, columns),
        _ => false,
    }
}

/// v7.39 (round 467) — MySQL arithmetic over an UNSIGNED operand, with
/// MySQL's range check.
///
/// `INT UNSIGNED` columns holding 1 and 5 made `a - b` answer **-4** in a
/// MySQL session. MariaDB raises `ERROR 1690 (22003): BIGINT UNSIGNED value
/// is out of range`. A negative answer where the server promises a
/// non-negative one is the kind of thing an application stores back into
/// the same column, so it was silent and wrong in the worst direction.
///
/// The check runs in i128 so the subtraction that underflows is observed
/// rather than wrapped, and it only fires when the expression is unsigned
/// AND both operands are integers — a NUMERIC or float operand takes the
/// ordinary path, as it does in MySQL.
///
/// `#[inline(never)]`: this is called from the recursive evaluator's
/// hottest frame, which already sits against the stack guard.
#[inline(never)]
fn apply_binary_mysql_unsigned(
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    l: Value<'static>,
    r: Value<'static>,
    ctx: &EvalContext,
) -> Result<Value<'static>, EvalError> {
    if ctx.mysql_dialect
        && matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul)
        && let Some(a) = mysql_int_operand(&l)
        && let Some(b) = mysql_int_operand(&r)
        && (expr_is_mysql_unsigned(lhs, ctx.columns) || expr_is_mysql_unsigned(rhs, ctx.columns))
    {
        let out = match op {
            BinOp::Add => a.checked_add(b),
            BinOp::Sub => a.checked_sub(b),
            _ => a.checked_mul(b),
        };
        let in_range = out.is_some_and(|v| (0..=i128::from(u64::MAX)).contains(&v));
        if !in_range {
            // MariaDB names the offending expression in the message, with
            // minimal parentheses — `a * 0 - 1`, not `((a * 0) - 1)`.
            // `pretty_expr` is the deparser that already produces that
            // shape. Residual, recorded rather than faked: MariaDB writes
            // its columns fully qualified in backticks
            // (`db`.`tbl`.`col`), and the database name is not something
            // the evaluation context carries.
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "BIGINT UNSIGNED value is out of range in '{}'",
                    spg_sql::ast::pretty_expr_mysql(&Expr::Binary {
                        lhs: alloc::boxed::Box::new(lhs.clone()),
                        op,
                        rhs: alloc::boxed::Box::new(rhs.clone()),
                    })
                ),
            });
        }
    }
    let out = apply_binary_in(op, l, r, ctx.mysql_dialect);
    // v7.39 (round 503) — MariaDB answers NULL for division / modulo by
    // zero; SPG raised.
    //
    // Measured against MariaDB 11: `SELECT 1/0`, `SELECT 5 DIV 0` and
    // `SELECT 5 % 0` are all NULL — and they are NULL under the DEFAULT
    // sql_mode too, which contains `ERROR_FOR_DIVISION_BY_ZERO`. That flag
    // governs WRITES, not the expression: the division evaluates to NULL,
    // and a strict-mode INSERT of that result is what raises 1365.
    //
    // The rule is therefore the DIALECT's, not the mode's: in a MySQL
    // session the expression is NULL. It is deliberately not gated on
    // `mysql_strict` — an earlier cut of this was, and the gate fired only
    // because the probe's context happens to carry no engine, which a
    // later round attaching one would have silently reversed.
    //
    // RESIDUAL, recorded rather than faked: MariaDB's strict-mode INSERT
    // of a division by zero raises 1365 and its non-strict INSERT stores
    // NULL. SPG's INSERT path evaluates elsewhere and still raises, so it
    // matches strict and diverges from non-strict. Closing that needs the
    // expression to know it is in a write, which nothing here carries.
    if ctx.mysql_dialect && matches!(out, Err(EvalError::DivisionByZero)) {
        return Ok(Value::Null);
    }
    out
}

/// The integer an operand contributes to the unsigned range check, or
/// `None` when it is not an integer at all (NULL, text, NUMERIC, float).
fn mysql_int_operand(v: &Value<'_>) -> Option<i128> {
    match v {
        Value::SmallInt(n) => Some(i128::from(*n)),
        Value::Int(n) => Some(i128::from(*n)),
        Value::BigInt(n) => Some(i128::from(*n)),
        // v7.39 (round 471) — a BIGINT UNSIGNED cell is stored as Numeric
        // with scale 0, so the range check has to see it as the integer it
        // is. Without this arm the column's own type moved it out of reach
        // of round 467's guard and `c - 5` went back to answering -4.
        Value::Numeric { scaled, scale, .. } if *scale == 0 => Some(*scaled),
        _ => None,
    }
}

fn expr_enum_type_name<'e>(e: &'e Expr, columns: &'e [ColumnSchema]) -> Option<&'e str> {
    match e {
        Expr::Cast {
            target: CastTarget::Named(n),
            ..
        } => Some(n.as_str()),
        Expr::Column(c) => columns
            .iter()
            .find(|col| col.name == c.name)
            // v7.39 (round 259) — a DOMAIN column carries its name in its
            // own field; both are "the user type this column is declared
            // as", which is what the callers (enum-order comparison,
            // pg_typeof) want. Callers gate on the catalog, so a name that
            // is one kind never resolves as the other.
            .and_then(|col| {
                col.user_enum_type
                    .as_deref()
                    .or(col.user_domain_type.as_deref())
            }),
        _ => None,
    }
}

/// v7.39 (enum order knife) — the member-label list for an enum-typed
/// expression, or None when the expression carries no enum witness or the
/// name is not a known enum. The returned slice borrows the catalog.
pub(crate) fn expr_enum_labels<'c>(
    e: &Expr,
    columns: &[ColumnSchema],
    catalog: Option<&'c spg_storage::Catalog>,
) -> Option<&'c [String]> {
    let name = expr_enum_type_name(e, columns)?;
    catalog
        .and_then(|cat| cat.enum_types().get(name))
        .map(|en| en.labels.as_slice())
}

/// v7.39 (enum order knife) — compare two enum labels by member order.
/// None when either side is not Text or not a member (caller falls back to
/// the generic comparison, so a stray value never panics or misorders
/// silently differently from before).
pub(crate) fn enum_ord_cmp(
    labels: &[String],
    a: &Value<'_>,
    b: &Value<'_>,
) -> Option<core::cmp::Ordering> {
    let pos = |v: &Value<'_>| -> Option<usize> {
        match v {
            Value::Text(s) => labels.iter().position(|l| l.as_str() == s.as_ref()),
            _ => None,
        }
    };
    Some(pos(a)?.cmp(&pos(b)?))
}

/// v7.39 (enum order knife) — Binary-comparison hook: when either side's
/// static type witnesses an enum and both runtime values are member labels,
/// compare by member order (PG's enumsortorder semantics). Out-of-line to
/// keep `eval_expr`'s recursion frame small.
#[inline(never)]
fn enum_compare_hook(
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    l: &Value<'_>,
    r: &Value<'_>,
    ctx: &EvalContext<'_>,
) -> Option<Result<Value<'static>, EvalError>> {
    let cat = ctx.catalog?;
    if cat.enum_types().is_empty() {
        return None;
    }
    let labels = expr_enum_labels(lhs, ctx.columns, ctx.catalog)
        .or_else(|| expr_enum_labels(rhs, ctx.columns, ctx.catalog))?;
    let ord = enum_ord_cmp(labels, l, r)?;
    let b = match op {
        BinOp::Eq => ord == core::cmp::Ordering::Equal,
        BinOp::NotEq => ord != core::cmp::Ordering::Equal,
        BinOp::Lt => ord == core::cmp::Ordering::Less,
        BinOp::LtEq => ord != core::cmp::Ordering::Greater,
        BinOp::Gt => ord == core::cmp::Ordering::Greater,
        BinOp::GtEq => ord != core::cmp::Ordering::Less,
        _ => return None,
    };
    Some(Ok(Value::Bool(b)))
}

/// v7.39 (round 693) — Binary-comparison hook for a declared collation, the
/// last shape F36 left open: `loc BETWEEN 'a' AND 'd'` returns a different
/// ROW SET under `en_US.utf8` than under byte order, not merely a different
/// order.
///
/// It sits beside [`enum_compare_hook`] because it is the same kind of fact
/// — something about the operand COLUMNS that `compare` cannot look up from
/// two values — and takes the same two protections: `#[inline(never)]`, so
/// `eval_expr`'s recursion frame does not grow (the comment at the call site
/// records a fourth `||` there tipping the 768 KiB guard on its own), and
/// the caller's Text/Text gate, so no integer comparison reaches it.
///
/// EQUALITY is deliberately not handled. PG18's `en_US.utf8` is
/// deterministic, so `=`, `<>`, `LIKE`, `IN` and `count(DISTINCT …)` give
/// byte-equality's answer — measured, all five. Only the ordering operators
/// change, and `least`/`greatest` follow them through their own comparator.
#[inline(never)]
fn collate_compare_hook(
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    l: &Value<'_>,
    r: &Value<'_>,
    ctx: &EvalContext<'_>,
) -> Option<Result<Value<'static>, EvalError>> {
    if !matches!(op, BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq) {
        return None;
    }
    let (Value::Text(a), Value::Text(b)) = (l, r) else {
        return None;
    };
    let resolve = |c: &spg_sql::ast::ColumnName| -> Option<alloc::string::String> {
        let pos = find_column_pos(c, ctx)?;
        ctx.columns.get(pos)?.collation_name.clone()
    };
    let derived = crate::collate_derive::derive(lhs, &resolve)
        .combine_pub(crate::collate_derive::derive(rhs, &resolve));
    if let Some((x, y)) = derived.conflict() {
        return Some(Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "collation mismatch between implicit collations \"{x}\" and \"{y}\""
            ),
        }));
    }
    // v7.38.18 (S2) — and the DATABASE's collation when neither side
    // declares one, which is what an undeclared text column is compared
    // under. `C` is byte order and resolves to `None`, so a database
    // that never asked for a locale takes the same path it always did.
    //
    // Ordering only. Whether a comparison PADS is decided in
    // `text_compare_of` from a MySQL collation NAME, and a PostgreSQL
    // database collating as `en_US.utf8` does not pad — inheritance
    // reaching that would make `'a' = 'a  '` true everywhere.
    let db = ctx
        .catalog
        .map(spg_storage::Catalog::db_collation)
        .filter(|d| !crate::collate::is_byte_wise(d));
    let ord = crate::collate::compare(derived.name().or(db)?, a, b)?;
    let b = match op {
        BinOp::Lt => ord == core::cmp::Ordering::Less,
        BinOp::LtEq => ord != core::cmp::Ordering::Greater,
        BinOp::Gt => ord == core::cmp::Ordering::Greater,
        BinOp::GtEq => ord != core::cmp::Ordering::Less,
        _ => return None,
    };
    Some(Ok(Value::Bool(b)))
}

/// v7.39 (round 693) — the collation `least`/`greatest` should compare by,
/// derived across every argument the same way a comparison's two operands
/// are. `None` keeps byte order, which is right for arguments that declare
/// nothing.
#[inline(never)]
fn greatest_least_collation(args: &[Expr], ctx: &EvalContext<'_>) -> Option<alloc::string::String> {
    let resolve = |c: &spg_sql::ast::ColumnName| -> Option<alloc::string::String> {
        let pos = find_column_pos(c, ctx)?;
        ctx.columns.get(pos)?.collation_name.clone()
    };
    let derived = args
        .iter()
        .fold(crate::collate_derive::Derived::None, |acc, a| {
            acc.combine_pub(crate::collate_derive::derive(a, &resolve))
        });
    derived
        .name()
        .filter(|n| crate::collate::is_supported(n))
        .map(alloc::string::ToString::to_string)
}

/// v7.39 (round 704) — rewrite a comparison's operator-not-found error when
/// the operand at fault is an UNKNOWN string literal against a numeric-family
/// value. PG commits such a literal to the other side's type before comparing,
/// so its error is the input function's — `invalid input syntax for type
/// integer: "abc"` — not `operator does not exist: integer = text`. An
/// explicit `::text` operand keeps the operator error (`1 IS DISTINCT FROM
/// 'a'::text`, measured on PG18), which is precisely the distinction two
/// `Value`s cannot carry: the first cut of this round rewrote inside
/// `binop::compare` and the r238 pin plus corpus 19 caught it the same day.
///
/// Error-path only — a comparison that succeeds never calls this — so the
/// 35.6 %-of-self-time note on `compare` is untouched.
#[cold]
#[inline(never)]
fn unknown_literal_cmp_error(
    err: EvalError,
    lhs: &Expr,
    rhs: &Expr,
    lv: &Value<'_>,
    rv: &Value<'_>,
) -> EvalError {
    let EvalError::TypeMismatch { detail } = &err else {
        return err;
    };
    // Two spellings of the same fall-through: `compare`'s operator error,
    // and the owned numeric path's conversion error (`f = 'y'` reaches
    // "cannot convert text to FLOAT"). Both mean the literal failed to
    // lift; neither is what PG says about an unknown literal.
    if !detail.starts_with("operator does not exist")
        && !detail.starts_with("cannot convert text to")
    {
        return err;
    }
    let numeric = |v: &Value<'_>| {
        matches!(
            v.data_type(),
            Some(
                spg_storage::DataType::SmallInt
                    | spg_storage::DataType::Int
                    | spg_storage::DataType::BigInt
                    | spg_storage::DataType::Float
                    | spg_storage::DataType::Real
                    | spg_storage::DataType::Numeric { .. }
            )
        )
    };
    let rewrite = |s: &Value<'_>, other: &Value<'_>| -> Option<EvalError> {
        let Value::Text(text) = s else { return None };
        let dt = other.data_type()?;
        Some(EvalError::TypeMismatch {
            detail: alloc::format!(
                "invalid input syntax for type {}: \"{text}\"",
                crate::conversions::pg_type_name_for_error(dt)
            ),
        })
    };
    if is_unknown_string_literal(lhs)
        && numeric(rv)
        && let Some(e) = rewrite(lv, rv)
    {
        return e;
    }
    if is_unknown_string_literal(rhs)
        && numeric(lv)
        && let Some(e) = rewrite(rv, lv)
    {
        return e;
    }
    err
}

fn enum_arg_type_name<'e>(args: &'e [Expr], ctx: &EvalContext<'e>) -> Option<&'e str> {
    args.iter()
        .find_map(|a| expr_enum_type_name(a, ctx.columns))
        .filter(|n| {
            ctx.catalog
                .is_some_and(|cat| cat.enum_types().contains_key(*n))
        })
}

/// Cheap value-free precheck so `eval_expr`'s recursion frame carries no
/// binding for the enum path (stack-depth guard budget).
#[inline(never)]
fn enum_introspection_applies(args: &[Expr], ctx: &EvalContext<'_>) -> bool {
    enum_arg_type_name(args, ctx).is_some()
}

#[inline(never)]
fn eval_enum_introspection(
    name: &str,
    args: &[Expr],
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let Some(en) = enum_arg_type_name(args, ctx)
        .and_then(|n| ctx.catalog.and_then(|cat| cat.enum_types().get(n)))
    else {
        // The precheck guarantees this arm is unreachable; keep a typed
        // error rather than a panic if the two ever drift.
        return Err(EvalError::TypeMismatch {
            detail: "could not determine polymorphic type".into(),
        });
    };
    let labels = &en.labels;
    if labels.is_empty() {
        return Ok(Value::Null);
    }
    if name.eq_ignore_ascii_case("enum_first") {
        return Ok(Value::text(labels[0].clone()));
    }
    if name.eq_ignore_ascii_case("enum_last") {
        return Ok(Value::text(labels[labels.len() - 1].clone()));
    }
    // enum_range(NULL) = all; enum_range(lo, hi) slices inclusively,
    // NULL bound = open end (PG).
    let pos_of = |v: &Value<'_>| -> Option<usize> {
        match v {
            Value::Text(s) => labels.iter().position(|l| l == s.as_ref()),
            _ => None,
        }
    };
    let (lo, hi) = if args.len() == 2 {
        let a = eval_expr(&args[0], row, ctx)?;
        let b = eval_expr(&args[1], row, ctx)?;
        (
            pos_of(&a).unwrap_or(0),
            pos_of(&b).unwrap_or(labels.len() - 1),
        )
    } else {
        (0, labels.len() - 1)
    };
    let out: alloc::vec::Vec<Option<String>> = labels
        .get(lo..=hi)
        .unwrap_or(&[])
        .iter()
        .map(|l| Some(l.clone()))
        .collect();
    Ok(Value::TextArray(out))
}

/// Apply one `[index]` subscript to a value — the single-step semantics shared
/// by 1-D array elements and JSON path access (`j['a']`, `j[0]`). NULL target
/// or index → NULL; a 1-based integer indexes a 1-D array (out of range → NULL,
/// non-array → error); JSON delegates to `path_get`.
fn apply_one_subscript(
    target_v: Value<'static>,
    index: &Expr,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let idx_v = eval_expr(index, row, ctx)?;
    if matches!(target_v, Value::Null) || matches!(idx_v, Value::Null) {
        return Ok(Value::Null);
    }
    // v7.38 (read01) — JSON/JSONB subscripting (`j['a']`, `j[0]`, chained
    // `j['a']['b']`) is object/array access, identical to the `->` operator
    // (text key → object field, integer → 0-based array element). PG 14+.
    if matches!(target_v, Value::Json(_)) {
        return crate::json::path_get(&target_v, &idx_v, false);
    }
    let i: i64 = match idx_v {
        Value::Int(n) => i64::from(n),
        Value::BigInt(n) => n,
        Value::SmallInt(n) => i64::from(n),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "array subscript must be integer, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            });
        }
    };
    if i < 1 {
        return Ok(Value::Null);
    }
    let pos = (i - 1) as usize;
    match array_element_at(&target_v, pos) {
        Some(v) => Ok(v),
        None if array_len(&target_v).is_some() => Ok(Value::Null),
        None => Err(EvalError::TypeMismatch {
            detail: format!(
                "subscript target must be an array, got {}",
                crate::conversions::pg_type_name_for_error_opt(target_v.data_type())
            ),
        }),
    }
}

/// v7.38 (read01, 2D-subscript) — index a 2-D array (`arr[i][j]`). PG needs
/// exactly two subscripts to reach an element; a single subscript on a 2-D
/// array yields NULL (not the row), and any out-of-range index → NULL. Both
/// subscripts are 1-based.
fn eval_matrix_subscript(
    base: &Value<'static>,
    idx_exprs: &[&Expr],
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    if idx_exprs.len() != 2 {
        return Ok(Value::Null);
    }
    let mut idx = [0i64; 2];
    for (k, ix) in idx_exprs.iter().enumerate() {
        idx[k] = match eval_expr(ix, row, ctx)? {
            Value::Null => return Ok(Value::Null),
            Value::Int(n) => i64::from(n),
            Value::BigInt(n) => n,
            Value::SmallInt(n) => i64::from(n),
            other => {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array subscript must be integer, got {}",
                        crate::conversions::pg_type_name_for_error_opt(other.data_type())
                    ),
                });
            }
        };
    }
    let (r, c) = (idx[0], idx[1]);
    if r < 1 || c < 1 {
        return Ok(Value::Null);
    }
    let (ri, ci) = ((r - 1) as usize, (c - 1) as usize);
    macro_rules! elem {
        ($rows:expr, $map:expr) => {
            Ok($rows
                .get(ri)
                .and_then(|inner| inner.get(ci))
                .map_or(Value::Null, |cell| cell.as_ref().map_or(Value::Null, $map)))
        };
    }
    match base {
        Value::IntArray2D(rows) => elem!(rows, |n| Value::Int(*n)),
        Value::BigIntArray2D(rows) => elem!(rows, |n| Value::BigInt(*n)),
        Value::BoolArray2D(rows) => elem!(rows, |b| Value::Bool(*b)),
        Value::TextArray2D(rows) => {
            elem!(rows, |s| Value::Text(alloc::borrow::Cow::Owned(s.clone())))
        }
        _ => Ok(Value::Null),
    }
}

/// Out-of-lined `eval_expr` arm — keeps the recursive frame small
/// (stack-depth guard budget); body unchanged.
#[inline(never)]
fn eval_cast_arm(
    expr: &Expr,
    target: &CastTarget,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let v = eval_expr(expr, row, ctx)?;
    // v7.39 (round 473) — `<oid>::regclass` names the relation.
    //
    // The cast itself has no catalog, so it answered the bare number:
    // `indexrelid::regclass` printed `100001` where PG prints `ix1`, and
    // pg_class / pg_index rows are read by joining on oids and rendering
    // them — a tool cannot match the two up. `relation_name_for_oid`
    // mirrors `relation_oid`'s walks so the two directions agree; an oid
    // that names nothing keeps rendering as the number, which is what PG
    // does for a dropped relation's oid too.
    if matches!(target, CastTarget::RegClass)
        && let Some(cat) = ctx.catalog
    {
        let oid = match &v {
            Value::Int(n) => Some(i64::from(*n)),
            Value::BigInt(n) => Some(*n),
            _ => None,
        };
        if let Some(oid) = oid
            && let Some(name) = crate::system_catalog::relation_name_for_oid(cat, oid)
        {
            return Ok(Value::text(name));
        }
    }
    // v7.38 (read01 P6.40) — a cast to a user DOMAIN (`x::posint`)
    // enforces the domain's NOT NULL + CHECK constraints, matching PG.
    // The base-type coercion already happened when `v` was produced
    // (the domain is a constrained alias of its base type); here we
    // only run the constraints.
    if let CastTarget::Named(name) = target
        && let Some(cat) = ctx.catalog
    {
        if let Some(dom) = cat.domain_types().get(name.as_str()) {
            return apply_domain_constraints(v, dom, name, cat);
        }
        // v7.38 (read01 P6.67) — `'label'::<user enum>` validates the
        // label against the enum's members (a non-member errors like
        // PG). A typed NULL passes through carrying the enum type.
        if let Some(en) = cat.enum_types().get(name.as_str()) {
            return apply_enum_cast(v, en, name);
        }
        // v7.39 (read01 rowtypes.c) — `'(1,x)'::<composite>` parses PG's
        // record text form against the type's field list; a ROW value
        // re-labels its fields.
        if let Some(comp) = cat.composite_types().get(name.as_str()) {
            // v7.39 (round 264) — pass the catalog so a NESTED composite
            // field resolves into a record rather than staying text.
            return apply_composite_cast_in(v, comp, ctx.catalog);
        }
        // v7.39 (round 509) — every TABLE also names a row type, and
        // `jsonb_populate_record(NULL::mytable, …)` is PG's canonical
        // spelling for "shaped like this table". PG accepts `NULL::mytable`
        // and refuses `1::mytable` with "cannot cast type integer to
        // mytable" — the type exists, the conversion does not. This only
        // ever worked here because a NULL skipped cast resolution entirely;
        // now that it does not, the row type has to be named explicitly.
        if cat.get(name.as_str()).is_some() {
            return if matches!(v, Value::Null) {
                Ok(Value::Null)
            } else {
                Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "cannot cast type {} to {name}",
                        crate::eval::strings::pg_typeof_name(&v),
                    ),
                })
            };
        }
        // v7.39 (round 513) — `regnamespace` and `regrole` resolve against
        // things that live outside the type table: schemas on the catalog,
        // roles on the engine. They belong here for the same reason the
        // relation check above does — this is the arm that can see them.
        // v7.39 (round 526) — the NUMERIC direction, which is the one a
        // catalog join uses: `relnamespace::regnamespace` names the
        // schema a relation lives in, and it errored with "unsupported
        // cast target" while `'public'::regnamespace` worked. Round 513
        // added the name direction only, so the half that reads a
        // catalog was the half missing.
        if (name.eq_ignore_ascii_case("regnamespace") || name.eq_ignore_ascii_case("regrole"))
            && let Some(oid) = match &v {
                Value::Int(n) => Some(i64::from(*n)),
                Value::BigInt(n) => Some(*n),
                _ => None,
            }
        {
            let named = if name.eq_ignore_ascii_case("regnamespace") {
                crate::system_catalog::schema_name_for_oid(oid)
            } else {
                ctx.engine.and_then(|e| e.role_name_for_oid(oid))
            };
            // PG prints the bare number for an oid that names nothing,
            // exactly as `regclass` does.
            return Ok(Value::text(
                named.unwrap_or_else(|| alloc::format!("{oid}")),
            ));
        }
        if name.eq_ignore_ascii_case("regnamespace")
            && let Value::Text(t) = &v
        {
            let want = t.trim().trim_matches('"');
            // 7.38.1 S5.1 — the name direction answers the DUAL
            // (oid, name) value for the schemas with a published oid:
            // regnamespace IS an oid in PG, and pg_dump compares it
            // against numeric namespace columns (`opcnamespace =
            // 'pg_catalog'::regnamespace`) — while the wire render
            // stays the NAME, as PG's does (the round-513 contract).
            // The RegClass dual carries exactly that pair. A user
            // schema without a published oid keeps plain text.
            return if spg_storage::is_builtin_schema(want) || cat.schema_exists(want) {
                Ok(match want {
                    "pg_catalog" => Value::RegClass(11, "pg_catalog".into()),
                    "public" => Value::RegClass(2200, "public".into()),
                    "information_schema" => Value::RegClass(13000, "information_schema".into()),
                    _ => Value::text(want.to_string()),
                })
            } else {
                Err(EvalError::TypeMismatch {
                    detail: alloc::format!("schema \"{want}\" does not exist"),
                })
            };
        }
        if name.eq_ignore_ascii_case("regrole")
            && let Value::Text(t) = &v
        {
            let want = t.trim().trim_matches('"').to_string();
            // PG ships predefined roles that exist whether or not anybody
            // created them; SPG carries the rest on the engine.
            const PREDEFINED: &[&str] = &[
                "pg_read_all_data",
                "pg_write_all_data",
                "pg_monitor",
                "pg_read_all_settings",
                "pg_read_all_stats",
                "pg_stat_scan_tables",
                "pg_signal_backend",
                "pg_checkpoint",
                "pg_maintain",
                "pg_use_reserved_connections",
                "pg_create_subscription",
            ];
            let known = PREDEFINED.iter().any(|r| r.eq_ignore_ascii_case(&want))
                || ctx.engine.is_some_and(|e| e.role_exists(&want));
            return if known {
                Ok(Value::text(want))
            } else {
                Err(EvalError::TypeMismatch {
                    detail: alloc::format!("role \"{want}\" does not exist"),
                })
            };
        }
        // v7.39 (round 509) — the cast target is checked even when the
        // operand is NULL. `cast_value_in` short-circuits a NULL before it
        // looks at the target, so `NULL::nosuchtype` silently answered NULL
        // and `pg_typeof(NULL::nosuchtype)` answered `unknown`, while
        // `1::nosuchtype` errored — the gap was exactly the NULL case, in
        // both spellings. Everything a catalog can name has been tried by
        // now; what is left is the builtin table.
        if matches!(v, Value::Null)
            && !crate::eval::cast::builtin_target_resolves(name, ctx.mysql_dialect)
        {
            return Err(EvalError::TypeMismatch {
                detail: cast::unknown_type_error_text(name),
            });
        }
    }
    // v7.39 (round 285) — `::record`, the anonymous composite type. PG
    // treats it as an IDENTITY cast on anything already composite: the
    // value keeps its fields and their names, so `(ROW(1,2)::record).f1`
    // and `(r).x` still resolve. Only a non-composite is refused, with
    // PG's wording. `record` is not a catalog type, so this cannot live
    // in the lookups above.
    if let CastTarget::Named(name) = target
        && name.eq_ignore_ascii_case("record")
    {
        return match v {
            Value::Composite(_) | Value::Null => Ok(v),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "cannot cast type {} to record",
                    crate::eval::strings::pg_typeof_name(&other),
                ),
            }),
        };
    }
    // v7.38 (read01, T22) — a numeric OID cast to regclass reverse-looks
    // up the user relation name (PG's 16384+ band, assigned in
    // table_names() order). System OIDs / non-matches fall through to
    // the integer-rendering path in cast_value.
    if matches!(target, CastTarget::RegClass) {
        // v7.39 (read01 ruleutils.c) — regclass is DUAL-shape: oid for
        // catalog joins (conrelid = 't'::regclass), name for display.
        let oid_in = match &v {
            Value::SmallInt(n) => Some(i64::from(*n)),
            Value::Int(n) => Some(i64::from(*n)),
            Value::BigInt(n) => Some(*n),
            _ => None,
        };
        if let (Some(oid), Some(cat)) = (oid_in, ctx.catalog) {
            if oid >= 16384 {
                if let Some(name) = cat.table_names().into_iter().nth((oid - 16384) as usize) {
                    return Ok(Value::RegClass(oid, name.into()));
                }
            }
        }
        if let (Value::Text(s), Some(cat)) = (&v, ctx.catalog) {
            let bare = s
                .rsplit('.')
                .next()
                .unwrap_or(s)
                .trim_matches('"')
                .to_string();
            if let Some(oid) = regclass_name_to_oid(cat, &bare) {
                return Ok(Value::RegClass(oid, bare.into()));
            }
            // v7.39 (round 337, V62) — a name that is no relation at all is
            // PG's error, not a silent pass-through. `'nope'::regclass`
            // used to answer the TEXT `nope`, so a downstream
            // `pg_get_viewdef('nope'::regclass)` reported "no such view"
            // when the truth is there is no such relation — and a
            // catalog join on it quietly matched nothing. PG 18.4:
            // `ERROR: relation "nope" does not exist`. (`to_regclass` is
            // the spelling that answers NULL instead, and still does.)
            //
            // The system views SPG synthesises have no oid space, so they
            // keep the textual form rather than erroring.
            const SYSTEM_RELS: &[&str] = &[
                "pg_roles",
                "pg_user",
                "pg_tables",
                "pg_views",
                "pg_settings",
                "pg_stat_activity",
                "pg_stat_database",
                "pg_stat_user_tables",
                "pg_class",
                "pg_attribute",
                "pg_type",
                "pg_proc",
                "pg_namespace",
                "pg_constraint",
                "pg_index",
                "pg_rewrite",
            ];
            if !SYSTEM_RELS.contains(&bare.as_str()) {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("relation \"{bare}\" does not exist"),
                });
            }
        }
    }
    // v7.39 (round 339, V63) — `::regproc` / `::regprocedure` resolve
    // against the USER function catalog too. Name resolution ran against
    // the static pg_proc table alone, so `'my_fn'::regproc` — the form
    // every catalog query and pg_dump uses to name a function — raised
    // `function "my_fn" does not exist` for a function that plainly did.
    // The cast layer has no catalog handle; this is the same interception
    // point the `::regclass` block above uses.
    if let (CastTarget::Named(tname), Some(cat), Value::Text(s)) = (target, ctx.catalog, &v) {
        let lower = tname.to_ascii_lowercase();
        if matches!(lower.as_str(), "regproc" | "regprocedure") {
            let raw = s.trim();
            // regprocedure carries the argument list: `f(int,text)`.
            let (name_part, args_part) = match raw.split_once('(') {
                Some((n, rest)) => (n.trim(), Some(rest.trim_end_matches(')'))),
                None => (raw, None),
            };
            let bare = name_part
                .strip_prefix("public.")
                .unwrap_or(name_part)
                .trim_matches('"');
            let cands = cat.functions_named(bare);
            if let Some(args_txt) = args_part {
                // An overload IS distinguishable here — the argument list
                // is what regprocedure exists to carry.
                let want =
                    crate::system_catalog::canonical_arg_types(&alloc::format!("({args_txt})"));
                if let Some(f) = cands
                    .iter()
                    .find(|f| crate::system_catalog::canonical_arg_types(&f.args_repr) == want)
                {
                    let rendered = alloc::format!(
                        "{bare}({})",
                        crate::system_catalog::canonical_arg_types(&f.args_repr)
                    );
                    // v7.39 (round 342, V65) — dual shape: the oid for
                    // catalog joins, the rendering for display.
                    let oid = crate::system_catalog::function_oid_by_signature(cat, bare, &want)
                        .unwrap_or(0);
                    return Ok(Value::RegProc(oid, rendered.into()));
                }
            } else {
                match cands.len() {
                    0 => {}
                    1 => {
                        let oid = crate::system_catalog::function_oid(cat, bare).unwrap_or(0);
                        return Ok(Value::RegProc(oid, bare.into()));
                    }
                    _ => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!("more than one function named \"{bare}\""),
                        });
                    }
                }
            }
        }
    }
    // v7.38 (T-tstz Phase 1) — `<timestamptz>::text` renders the offset
    // (`2024-01-15 10:30:00+00`); plain timestamp does not. The runtime
    // value is the same tz-less `Value::Timestamp`, so consult the
    // inner expression's static type. Falls through to the ordinary
    // cast on any shape the static typer can't resolve — worst case is
    // today's no-offset rendering, never a wrong instant.
    if matches!(target, CastTarget::Text)
        && let Value::Timestamp(t) = &v
        && crate::describe::describe_expr(expr, ctx.columns)
            .is_some_and(|s| matches!(s.ty, spg_storage::DataType::Timestamptz))
    {
        // v7.39 (tz epic) — per-VALUE session offset (DST zones
        // vary within a statement); non-ISO DateStyles carry the
        // zone designation instead of the numeric offset.
        let off = ctx.session_tz_offset_at(*t);
        let abbr = ctx.session_tz_abbrev_at(*t);
        return Ok(Value::text(format::format_timestamptz_tz(
            *t,
            &ctx.render_style,
            off,
            abbr.as_deref(),
        )));
    }
    // v7.39 (read01 utils/adt, datetime.c) — the relative
    // reserved words resolve against the transaction clock:
    // 'today'/'tomorrow'/'yesterday' are midnight dates, 'now'
    // is the current instant ('now'::date = today). Clockless
    // engines fall through to the parser (which rejects them).
    if matches!(
        target,
        CastTarget::Date | CastTarget::Timestamp | CastTarget::Timestamptz
    ) && let Value::Text(word) = &v
        && let Some(clock) = ctx.clock
    {
        let w = word.trim().to_ascii_lowercase();
        if matches!(w.as_str(), "today" | "tomorrow" | "yesterday" | "now") {
            let now_us = clock();
            let today = i32::try_from(now_us.div_euclid(86_400_000_000)).ok();
            if let Some(today) = today {
                let day = match w.as_str() {
                    "tomorrow" => today + 1,
                    "yesterday" => today - 1,
                    _ => today,
                };
                return Ok(match (&target, w.as_str()) {
                    (CastTarget::Date, _) => Value::Date(day),
                    (_, "now") => Value::Timestamp(now_us),
                    _ => Value::Timestamp(crate::conversions::date_days_to_micros(day)),
                });
            }
        }
    }
    // v7.39 (GUC knife 5) — text INPUT to date/timestamp under a
    // non-MDY DateOrder disambiguates by the session order
    // (`'01/02/2024'::date` is Feb 1 under DMY). The default MDY
    // order flows through cast_value's parse_date_literal.
    if ctx.render_style.date_order != format::DateOrder::Mdy {
        match (&target, &v) {
            (CastTarget::Date, Value::Text(s)) => {
                if let Some(d) = format::parse_date_literal_ordered(s, ctx.render_style.date_order)
                {
                    return Ok(Value::Date(d));
                }
            }
            (CastTarget::Timestamp, Value::Text(s)) => {
                if let Some(t) =
                    format::parse_timestamp_literal_ordered(s, ctx.render_style.date_order)
                {
                    return Ok(Value::Timestamp(t));
                }
            }
            _ => {}
        }
    }
    // v7.39 (round 309, V30) — the mirror of the timestamptz arm
    // below. A literal carrying a zone NAME is legal input to the
    // zone-less types, and PG throws the zone away rather than
    // converting: `'2020-01-01 10:00:00 America/New_York'::timestamp`
    // is 10:00, not 15:00. Round 289 did this for a numeric `+02`
    // offset; a named zone still failed to parse at all.
    //
    // The name is not simply stripped — PG validates it, and says so
    // (`time zone "bogus/zone" not recognized`, lowercased) rather than
    // reporting a malformed literal. That check is why this belongs
    // here and not in `cast_value`: resolving a zone needs the host
    // functions, which only the context carries.
    let zoneless_target = match &target {
        CastTarget::Timestamp => Some("timestamp"),
        CastTarget::Date => Some("date"),
        // `::time` has no CastTarget of its own; it arrives named.
        CastTarget::Named(n) if n.eq_ignore_ascii_case("time") => Some("time"),
        _ => None,
    };
    if let Some(kind) = zoneless_target
        && let Value::Text(txt) = &v
        && let Some((wall, zone)) = split_trailing_zone_name(txt, ctx.render_style.date_order)
    {
        if ctx.zone_local_to_utc(zone, wall).is_none() {
            // Measured boundary: PG calls the token a ZONE NAME — and
            // so reports a misspelling as such — only when it is
            // path-shaped. A bare word it does not know (`ABCD`, `QQQ`,
            // `UTC_X`) makes the whole literal invalid syntax instead,
            // because nothing marks it as having meant a zone at all.
            if zone.contains('/') {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "time zone \"{}\" not recognized",
                        zone.to_ascii_lowercase()
                    ),
                });
            }
        } else {
            return Ok(match kind {
                "date" => {
                    Value::Date(i32::try_from(wall.div_euclid(86_400_000_000)).map_err(|_| {
                        EvalError::TypeMismatch {
                            detail: "timestamp out of DATE range".into(),
                        }
                    })?)
                }
                "time" => Value::Time(wall.rem_euclid(86_400_000_000)),
                _ => Value::Timestamp(wall),
            });
        }
    }
    // v7.39 (tz epic) — timestamptz INPUT: an offset-less
    // literal is a wall-clock reading in the session zone
    // (PG); a trailing IANA zone name localises there. Both
    // fall through to cast_value when nothing matches (its
    // parse treats naive input as UTC — correct for a UTC
    // session).
    if matches!(target, CastTarget::Timestamptz)
        && let Value::Text(txt) = &v
    {
        let order = ctx.render_style.date_order;
        let sess_zone = ctx
            .session_gucs
            .and_then(|g| g.get("timezone"))
            .map(String::as_str);
        // Trailing zone name: the last space-separated token,
        // when it names a resolvable zone (contains a letter
        // and isn't consumed by the plain parse).
        if let Some(idx) = txt.trim_end().rfind(' ') {
            let (head, tail) = (txt[..idx].trim(), txt[idx + 1..].trim());
            let tail_is_zoneish = tail.len() > 1
                && tail.bytes().any(|b| b.is_ascii_alphabetic())
                && !tail.eq_ignore_ascii_case("bc")
                && !tail.eq_ignore_ascii_case("ad");
            if tail_is_zoneish
                && format::parse_timestamp_literal_tz_ordered(txt, order).is_none()
                && let Some((wall, false)) = format::parse_timestamp_literal_tz_ordered(head, order)
                && let Some(utc) = ctx.zone_local_to_utc(tail, wall)
            {
                return Ok(Value::Timestamp(utc));
            }
        }
        if let Some((wall, had_tz)) = format::parse_timestamp_literal_tz_ordered(txt, order) {
            if had_tz {
                return Ok(Value::Timestamp(wall));
            }
            if let Some(zone) = sess_zone
                && !zone.eq_ignore_ascii_case("utc")
                && !zone.eq_ignore_ascii_case("gmt")
                && let Some(utc) = ctx.zone_local_to_utc(zone, wall)
            {
                return Ok(Value::Timestamp(utc));
            }
            return Ok(Value::Timestamp(wall));
        }
    }
    // v7.39 (round 523) — a NAIVE timestamp cast to timestamptz is a
    // wall-clock reading in the session zone, exactly as the text form
    // above already is. This was a no-op, so under `SET TimeZone =
    // 'Asia/Tokyo'` a `TIMESTAMP '2020-01-01 00:00:00'::timestamptz`
    // named 09:00 JST — a different INSTANT, nine hours from the one PG
    // stores, not a different rendering of the same one.
    //
    // The source's static type is the witness: SPG keeps timestamptz in
    // the same `Value::Timestamp`, so only an expression that is not
    // ALREADY timestamptz may be shifted, or a tstz-to-tstz cast would
    // move the instant twice.
    if matches!(target, CastTarget::Timestamptz)
        && let Value::Timestamp(wall) = &v
        && !matches!(
            crate::describe::describe_expr(expr, ctx.columns).map(|s| s.ty),
            Some(spg_storage::DataType::Timestamptz)
        )
        && let Some(zone) = ctx.session_gucs.and_then(|g| g.get("timezone"))
        && !zone.eq_ignore_ascii_case("utc")
        && !zone.eq_ignore_ascii_case("gmt")
        && let Some(utc) = ctx.zone_local_to_utc(zone, *wall)
    {
        return Ok(Value::Timestamp(utc));
    }
    // v7.39 (round 523) — and the other direction: a timestamptz cast
    // DOWN to a zone-free type reads the local clock in the session
    // zone. `(TIMESTAMPTZ '2020-01-01 15:00:00Z')::date` answered
    // 2020-01-01 in Tokyo where PG answers 2020-01-02 — a whole day out
    // for every instant in the last nine hours of a UTC day, which is
    // exactly the shape a daily report groups on. `now()::timestamp`
    // likewise disagreed with `now() AT TIME ZONE <session zone>`, which
    // PG defines to be the same value.
    if matches!(target, CastTarget::Timestamp | CastTarget::Date)
        && let Value::Timestamp(t) = &v
        && crate::describe::describe_expr(expr, ctx.columns)
            .is_some_and(|s| matches!(s.ty, spg_storage::DataType::Timestamptz))
    {
        let local = t.saturating_add(ctx.session_tz_offset_at(*t));
        return Ok(match target {
            CastTarget::Date => i32::try_from(local.div_euclid(86_400_000_000))
                .map_or(Value::Timestamp(local), Value::Date),
            _ => Value::Timestamp(local),
        });
    }
    // v7.39 (GUC knife 3) — the out-function casts honour the
    // session render style, like PG's date_out/interval_out/
    // float8out under DateStyle/IntervalStyle/extra_float_digits.
    if matches!(target, CastTarget::Text) {
        match &v {
            // v7.39 (round 524) — `bytea_out` is a render GUC too.
            Value::Bytes(b) if ctx.render_style.bytea_escape => {
                return Ok(Value::text(format::format_bytea_escape(b)));
            }
            Value::Date(d) => {
                return Ok(Value::text(format::format_date_styled(
                    *d,
                    &ctx.render_style,
                )));
            }
            Value::Timestamp(t) => {
                return Ok(Value::text(format::format_timestamp_styled(
                    *t,
                    &ctx.render_style,
                )));
            }
            Value::Interval {
                months,
                days,
                micros,
            } => {
                return Ok(Value::text(format::format_interval_styled(
                    *months,
                    *days,
                    *micros,
                    &ctx.render_style,
                )));
            }
            Value::Float(x) => {
                return Ok(Value::text(format::format_float_styled(
                    *x,
                    &ctx.render_style,
                )));
            }
            Value::Real(x) => {
                return Ok(Value::text(format::format_real_styled(
                    *x,
                    &ctx.render_style,
                )));
            }
            _ => {}
        }
    }
    crate::eval::cast::cast_value_ref_in(v, target, ctx.mysql_dialect)
}

/// Out-of-lined `eval_expr` arm — keeps the recursive frame small
/// (stack-depth guard budget); body unchanged.
#[inline(never)]
fn eval_array_arm(
    items: &[Expr],
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let mut materialised: Vec<Value<'static>> = Vec::with_capacity(items.len());
    for elem in items {
        materialised.push(eval_expr(elem, row, ctx)?);
    }
    // v7.38 (read01, T10) — a constructor whose elements are all 1-D
    // arrays builds a 2-D array (`ARRAY[[1,2],[3,4]]`). All rows must
    // share a length (PG: "multidimensional arrays must have array
    // expressions with matching dimensions"). Int rows promote to
    // bigint if any row is bigint; a text row makes the whole thing text.
    // v7.39 (read01 round 73) — a row of ANY array kind counts. Round 72 gave
    // `ARRAY[true,false]` its real `bool[]` type, and this detector only knew
    // Int / BigInt / Text rows — so `ARRAY[ARRAY[true,false]]` stopped being a
    // 2-D array at all and collapsed into a 1-D text[] of rendered rows, with
    // `[1][2]` failing outright. A regression THIS campaign introduced, caught by
    // the very sweep that was chasing its own residual. Rows that are not
    // int/bigint arrays render into the text 2-D form below (SPG has no bool 2-D
    // storage variant — a recorded residual), but they stay 2-D.
    let all_arrays = !materialised.is_empty()
        && materialised.iter().all(|v| {
            values::array_len(v).is_some()
                && !matches!(
                    v,
                    Value::TextArray2D(_)
                        | Value::IntArray2D(_)
                        | Value::BigIntArray2D(_)
                        | Value::BoolArray2D(_)
                )
        });
    if all_arrays {
        let row_len = values::array_len(&materialised[0]).unwrap_or(0);
        let same_len = materialised
            .iter()
            .all(|v| values::array_len(v) == Some(row_len));
        if !same_len {
            return Err(EvalError::TypeMismatch {
                detail: "multidimensional arrays must have array expressions \
                         with matching dimensions"
                    .into(),
            });
        }
        // v7.39 (read01 round 75) — all-BOOL rows build a real `bool[][]`. BOOL is
        // the one element type whose ARRAY rendering (`t`) differs from its
        // scalar one (`true`), so the text-backed 2-D could not be right for it:
        // `ARRAY[ARRAY[true,false]]::text` wants `{{t,f}}` while `[1][2]::text`
        // wants `false`. Every other type renders the same either way — which is
        // why this is the only typed 2-D SPG needs.
        if materialised
            .iter()
            .all(|v| matches!(v, Value::BoolArray(_)))
        {
            let rows: Vec<Vec<Option<bool>>> = materialised
                .into_iter()
                .map(|v| match v {
                    Value::BoolArray(r) => r,
                    _ => unreachable!("checked above"),
                })
                .collect();
            return Ok(Value::BoolArray2D(rows));
        }
        let any_text = materialised
            .iter()
            .any(|v| !matches!(v, Value::IntArray(_) | Value::BigIntArray(_)));
        let any_big = materialised
            .iter()
            .any(|v| matches!(v, Value::BigIntArray(_)));
        if any_text {
            let rows: Vec<Vec<Option<String>>> = materialised
                .into_iter()
                .map(|v| match v {
                    Value::TextArray(r) => r,
                    // Any other element type renders into the text 2-D form,
                    // element by element. SPG has no typed 2-D storage beyond
                    // int / bigint / text, so a bool 2-D array IS text — and the
                    // SCALAR rendering is the one to use: `(arr)[1][2]::text`
                    // must read `false`, as in PG. (`pg_typeof` reporting
                    // `text[]` rather than `boolean[]` is the recorded residual;
                    // a typed 2-D needs new storage variants.)
                    other => {
                        let n = values::array_len(&other).unwrap_or(0);
                        (0..n)
                            .map(|i| match values::array_element_at(&other, i) {
                                None | Some(Value::Null) => None,
                                Some(v) => Some(value_to_text(&v)),
                            })
                            .collect()
                    }
                })
                .collect();
            return Ok(Value::TextArray2D(rows));
        }
        if any_big {
            let rows: Vec<Vec<Option<i64>>> = materialised
                .into_iter()
                .map(|v| match v {
                    Value::BigIntArray(r) => r,
                    Value::IntArray(r) => r.into_iter().map(|c| c.map(i64::from)).collect(),
                    _ => unreachable!(),
                })
                .collect();
            return Ok(Value::BigIntArray2D(rows));
        }
        let rows: Vec<Vec<Option<i32>>> = materialised
            .into_iter()
            .map(|v| match v {
                Value::IntArray(r) => r,
                _ => unreachable!(),
            })
            .collect();
        return Ok(Value::IntArray2D(rows));
    }
    // v7.39 (read01 round 72) — a HOMOGENEOUS array of a non-numeric, non-text
    // type keeps that type, and is unambiguous, so it is decided BEFORE the
    // numeric/text unification below. Everything outside the numeric ladder and
    // text used to fall into that loop's `_ => has_text = true` — a silent
    // degradation, not a decision: `ARRAY[true, false]` came back as `text[]`.
    // It usually LOOKED right (array_to_string renders `t` either way), which is
    // exactly what let it sit; the array FUNCTIONS are what tripped over it.
    if let Some(v) = values::homogeneous_typed_array(&materialised) {
        return Ok(crate::describe::upgrade_timestamptz_array(
            v,
            items,
            ctx.columns,
        ));
    }
    // v7.39 (round 236) — PG resolves an ARRAY constructor's elements to ONE
    // element type and refuses the constructor when they have no common one:
    // `ARRAY[1, 'a'::text]` is "ARRAY types integer and text cannot be
    // matched". SPG degraded to `text[]` instead, so `ARRAY[1, true]` came
    // back as `{1,t}` — a column of rendered strings that then behaved like
    // text everywhere downstream. Same rule (and the same untyped-literal
    // subtlety) as the set-operation resolution in round 233: a bare string
    // literal is PG's `unknown` and takes the other elements' type, so it is
    // identified from the SYNTAX, not from the value's runtime type.
    unify_array_elements(items, &mut materialised)?;
    // Coercing the untyped elements can make the array homogeneous
    // (`ARRAY[true,'t']` becomes two booleans), so re-try the typed-array
    // path before falling into the numeric/text ladder below — otherwise
    // the now-uniform boolean array would still degrade to text[].
    if let Some(v) = values::homogeneous_typed_array(&materialised) {
        return Ok(crate::describe::upgrade_timestamptz_array(
            v,
            items,
            ctx.columns,
        ));
    }
    let mut has_text = false;
    let mut has_float = false;
    let mut has_numeric = false;
    let mut has_bigint = false;
    let mut has_int = false;
    // A NumericBig or non-finite (NaN/Inf) numeric can't be held in
    // NumericArray's `(i128, scale)` cells, so it forces the text[]
    // fallback rather than a lossy/panicking conversion.
    let mut numeric_representable = true;
    for v in &materialised {
        match v {
            Value::Null => {}
            Value::Int(_) | Value::SmallInt(_) => has_int = true,
            Value::BigInt(_) => has_bigint = true,
            Value::Numeric {
                kind: spg_storage::NumericKind::Finite,
                ..
            } => {
                has_numeric = true;
            }
            Value::Numeric { .. } => {
                has_numeric = true;
                numeric_representable = false;
            }
            Value::NumericBig(_) => {
                has_numeric = true;
                numeric_representable = false;
            }
            Value::Float(_) => has_float = true,
            Value::Text(_) | Value::Json(_) => has_text = true,
            // v7.39 (round 652) — a reg value belongs to the array by
            // its OID half. Falling into the catch-all made
            // `ARRAY['pg_class'::regclass]` a text array, so
            // `oid = ANY(…)` compared bigint against text and was
            // refused — while the identical `oid = 'pg_class'::regclass`
            // worked. Same defect the IN-list gate had, one layer down.
            Value::RegClass(..) | Value::RegProc(..) | Value::RegType(..) => has_bigint = true,
            _ => has_text = true,
        }
    }
    let any_numlike = has_int || has_bigint || has_numeric || has_float;
    if has_text || !any_numlike || (has_numeric && !numeric_representable) {
        let out: Vec<Option<String>> = materialised
            .into_iter()
            .map(|v| match v {
                Value::Null => None,
                Value::Text(s) | Value::Json(s) => Some(s.into_owned()),
                other => Some(value_to_text_for_array(&other, &ctx.render_style)),
            })
            .collect();
        return Ok(Value::TextArray(out));
    }
    // v7.38 (read01) — PG array-element unification across the numeric
    // ladder: any float → double precision[]; else any numeric →
    // numeric[] (each element keeps its own scale, PG's behaviour);
    // else the integer widths. Matches `pg_typeof(ARRAY[1, 2.5])` =
    // numeric[] and keeps downstream `[i]` arithmetic numeric.
    if has_float {
        let out: Vec<Option<f64>> = materialised
            .into_iter()
            .map(|v| match v {
                Value::Null => None,
                Value::Float(f) => Some(f),
                Value::Int(n) => Some(f64::from(n)),
                Value::SmallInt(n) => Some(f64::from(n)),
                #[allow(clippy::cast_precision_loss)]
                Value::BigInt(n) => Some(n as f64),
                #[allow(clippy::cast_precision_loss)]
                Value::Numeric { scaled, scale, .. } => {
                    Some(scaled as f64 / libm::pow(10.0, f64::from(scale)))
                }
                _ => None,
            })
            .collect();
        return Ok(Value::FloatArray(out));
    }
    if has_numeric {
        let out: Vec<Option<(i128, u16)>> = materialised
            .into_iter()
            .map(|v| match v {
                Value::Null => None,
                Value::SmallInt(n) => Some((i128::from(n), 0)),
                Value::Int(n) => Some((i128::from(n), 0)),
                Value::BigInt(n) => Some((i128::from(n), 0)),
                Value::Numeric { scaled, scale, .. } => Some((scaled, scale)),
                _ => None,
            })
            .collect();
        return Ok(Value::NumericArray(out));
    }
    if has_bigint {
        let out: Vec<Option<i64>> = materialised
            .into_iter()
            .map(|v| match v {
                Value::Null => None,
                Value::Int(n) => Some(i64::from(n)),
                Value::SmallInt(n) => Some(i64::from(n)),
                Value::BigInt(n) => Some(n),
                // Keep in step with the `has_bigint` classification above:
                // whatever is counted there has to be convertible here, and
                // the arm below panics rather than errors. Round 652 added
                // the reg family to the classifier and this materialiser
                // took a wire-visible panic until it learned them too.
                Value::RegClass(oid, _) | Value::RegProc(oid, _) | Value::RegType(oid, _) => {
                    Some(oid)
                }
                _ => unreachable!(),
            })
            .collect();
        return Ok(Value::BigIntArray(out));
    }
    let out: Vec<Option<i32>> = materialised
        .into_iter()
        .map(|v| match v {
            Value::Null => None,
            Value::Int(n) => Some(n),
            Value::SmallInt(n) => Some(i32::from(n)),
            _ => unreachable!(),
        })
        .collect();
    Ok(Value::IntArray(out))
}

/// Out-of-lined `eval_expr` arm — keeps the recursive frame small
/// (stack-depth guard budget); body unchanged.
#[inline(never)]
fn eval_function_call_arm(
    name: &str,
    args: &[Expr],
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    // v7.39 (read01 round 77) — named arguments (`f(x := 1)` / `f(x => 1)`).
    // The parser leaves them in the tree because only the catalog knows a user
    // function's parameter names; here they become positional, once, for
    // builtins and user functions alike.
    // v7.39 (read01 round 100) — `VARIADIC <array>` splices the array's
    // elements in as individual trailing arguments before dispatch, so a
    // variadic builtin (concat / concat_ws / format / …) sees them exactly as
    // if they had been written out. Done before the named-arg pass and the
    // positional dispatch.
    if args.iter().any(|a| matches!(a, Expr::Variadic(_))) {
        let expanded = expand_variadic_args(args, row, ctx)?;
        return eval_function_call_arm(name, &expanded, row, ctx);
    }
    // v7.39 (round 276) — `date_part('timezone'|…, <timestamp>)` must be
    // REJECTED the way EXTRACT already rejects it, and the judgement
    // needs the argument's STATIC declared type: SPG stores timestamptz
    // in the same `Value::Timestamp`, so a timestamptz legitimately
    // answers 0 while a plain timestamp must error. The dispatch below
    // receives values, not expressions, so the check belongs here —
    // the same place, and the same r237 trust rule (only a cast or a
    // column is believed), that the EXTRACT arm uses.
    if name.eq_ignore_ascii_case("date_part")
        && args.len() == 2
        && let Expr::Literal(spg_sql::ast::Literal::String(unit)) = &args[0]
        && matches!(
            unit.to_ascii_lowercase().as_str(),
            "timezone" | "timezone_hour" | "timezone_minute"
        )
        && matches!(&args[1], Expr::Cast { .. } | Expr::Column(_))
        && let Some(sch) = crate::describe::describe_expr(&args[1], ctx.columns)
        && matches!(sch.ty, spg_storage::DataType::Timestamp)
    {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "unit \"{}\" not supported for type timestamp without time zone",
                unit.to_ascii_lowercase()
            ),
        });
    }
    // v7.39 (round 258) — `pg_typeof` over an ENUM. An enum value travels
    // as `Value::Text` (its label), so the value-driven namer answered
    // `text`; the type lives in the EXPRESSION, which this arm still has.
    // `expr_enum_type_name` resolves a column or a cast statically — the
    // same static-only discipline round 253 used for EXTRACT's type name.
    if name.eq_ignore_ascii_case("pg_typeof")
        && let [arg] = args
    {
        // The name must be a REAL enum in the catalog. `expr_enum_type_name`
        // returns any named cast's target verbatim — it is only a
        // pre-filter for `expr_enum_labels`, which does the catalog
        // lookup — so using it alone hijacked every `x::float8` /
        // `x::int2` and reported SPG's internal spelling instead of PG's.
        // v7.39 (round 259) — domains report their own name too; like an
        // enum, a domain value travels as its BASE type's value, so the
        // name has to come from the expression. Both lookups are gated on
        // the catalog: `expr_enum_type_name` returns ANY named cast's
        // target verbatim, so an ungated use hijacks `x::float8`.
        let is_user_type = |e: &Expr| {
            expr_enum_type_name(e, ctx.columns)
                .filter(|n| {
                    // v7.39 (round 330, V48) — the information_schema
                    // domains are built into the server rather than
                    // catalog objects (a catalog domain is user data and
                    // would be dumped), so they are recognised here too.
                    crate::system_catalog::is_information_schema_domain(n)
                        || ctx.catalog.is_some_and(|cat| {
                            cat.enum_types().contains_key(*n)
                                || cat.domain_types().contains_key(*n)
                                // v7.39 (round 263) — composites too: a cast
                                // to one reported the generic `record`.
                                || cat.composite_types().contains_key(*n)
                        })
                })
                .map(alloc::string::String::from)
        };
        // v7.38.19 — a cast to a PSEUDO-type reports what PostgreSQL
        // reports, which is not always the name written.
        //
        // Measured on 18.4 rather than reasoned about: `cstring` and
        // `void` report themselves, while `anyelement`, `anynonarray`
        // and `unknown` all report `unknown` -- a polymorphic
        // placeholder resolves against the argument, and a bare literal
        // gives it nothing to resolve to. The value travels as text
        // either way, which is why the name has to come from the
        // expression: `'x'::cstring` renders `x` on both engines and
        // answered `text` here.
        //
        // The list here is SHORTER than `pseudo_type`'s on purpose: it
        // is the names measured on 18.4 for this call, no more. `record`
        // is the reason it has to be. `pg_typeof(ROW(1,'x')::r285::record)`
        // answers `r285` -- the composite's own name, not `record` --
        // and a first draft that reported every pseudo-type here turned
        // that into `unknown`, which `e2e_record_type_round285` caught.
        if let Some(named) = expr_enum_type_name(arg, ctx.columns)
            && let Some(pseudo) = crate::conversions::pseudo_type(named)
            && matches!(
                pseudo,
                "cstring" | "void" | "anyelement" | "anynonarray" | "unknown"
            )
        {
            return Ok(Value::text(match pseudo {
                "cstring" | "void" => pseudo,
                _ => "unknown",
            }));
        }
        let is_enum = is_user_type;
        if let Some(en) = is_enum(arg) {
            return Ok(Value::text(en));
        }
        // `ARRAY[<enum>, …]` reports the array form.
        if let Expr::Array(items) = arg
            && let Some(first) = items.first()
            && let Some(en) = is_enum(first)
        {
            return Ok(Value::text(alloc::format!("{en}[]")));
        }
    }
    if args.iter().any(|a| matches!(a, Expr::NamedArg { .. })) {
        let positional = resolve_named_args(name, args, ctx)?;
        return eval_function_call_arm(name, &positional, row, ctx);
    }
    eval_function_call_positional(name, args, row, ctx)
}

/// v7.39 (read01 round 100) — rewrite a call's argument list, replacing each
/// `VARIADIC <array>` with the array's elements as literal arguments. A NULL
/// array contributes no elements (PG treats `VARIADIC NULL` as empty). Regular
/// arguments are carried through untouched so they still evaluate against the
/// row in the recursive call.
fn expand_variadic_args(
    args: &[Expr],
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<alloc::vec::Vec<Expr>, EvalError> {
    let mut out = alloc::vec::Vec::with_capacity(args.len());
    for a in args {
        if let Expr::Variadic(inner) = a {
            let v = eval_expr(inner, row, ctx)?;
            let elems = crate::select::array_value_to_elements(&v).map_err(|_| {
                EvalError::TypeMismatch {
                    detail: "VARIADIC argument must be an array".into(),
                }
            })?;
            for e in elems {
                out.push(Expr::Literal(crate::value_to_literal(e)));
            }
        } else {
            out.push(a.clone());
        }
    }
    Ok(out)
}

/// The declared parameter names of `fname`, or `None` when it takes none.
/// Builtins whose parameters PG names live in the table; everything else asks
/// the catalog, where a user function's `args_repr` has carried its parameter
/// names since the day CREATE FUNCTION stored them.
fn declared_param_names(fname: &str, ctx: &EvalContext<'_>) -> Option<alloc::vec::Vec<String>> {
    let lower = fname.to_ascii_lowercase();
    let builtin: &[&str] = match lower.as_str() {
        "make_date" => &["year", "month", "day"],
        "make_time" => &["hour", "min", "sec"],
        "make_timestamp" | "make_timestamptz" => &["year", "month", "mday", "hour", "min", "sec"],
        "make_interval" => &["years", "months", "weeks", "days", "hours", "mins", "secs"],
        _ => &[],
    };
    if !builtin.is_empty() {
        return Some(builtin.iter().map(|s| (*s).to_string()).collect());
    }
    let cat = ctx.catalog?;
    let def = cat
        .functions()
        .values()
        .find(|f| f.name.eq_ignore_ascii_case(&lower))?;
    let names = spg_storage::function_arg_names(&def.args_repr);
    if names.iter().all(alloc::string::String::is_empty) {
        return None;
    }
    Some(names)
}

/// Rewrite a call's arguments into positional order. Positional arguments fill
/// slots left to right; a named one goes to its declared slot. Slots nobody
/// filled stay absent for a user function (arity is checked at the call) and
/// become integer 0 for the `make_*` builtins, whose trailing fields PG
/// defaults that way.
fn resolve_named_args(
    fname: &str,
    args: &[Expr],
    ctx: &EvalContext<'_>,
) -> Result<alloc::vec::Vec<Expr>, EvalError> {
    let Some(params) = declared_param_names(fname, ctx) else {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("function {fname}(...) does not support named arguments"),
        });
    };
    let mut slots: alloc::vec::Vec<Option<Expr>> = (0..params.len()).map(|_| None).collect();
    let mut next_positional = 0usize;
    for a in args {
        let (idx, val) = match a {
            Expr::NamedArg { name, expr } => {
                let i = params
                    .iter()
                    .position(|p| p.eq_ignore_ascii_case(name))
                    .ok_or_else(|| EvalError::TypeMismatch {
                        detail: alloc::format!("{fname}(...) has no argument named \"{name}\""),
                    })?;
                (i, (**expr).clone())
            }
            other => {
                let i = next_positional;
                next_positional += 1;
                (i, other.clone())
            }
        };
        if idx >= slots.len() {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("{fname}(...) got too many arguments"),
            });
        }
        if slots[idx].is_some() {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("{fname}(...) got multiple values for one argument"),
            });
        }
        slots[idx] = Some(val);
    }
    let make_family = fname.to_ascii_lowercase().starts_with("make_");
    let mut out = alloc::vec::Vec::with_capacity(slots.len());
    for slot in slots {
        match slot {
            Some(e) => out.push(e),
            None if make_family => {
                out.push(Expr::Literal(spg_sql::ast::Literal::Integer(0)));
            }
            // A user function's unfilled slot is simply not passed; the call's
            // own arity check phrases the error.
            None => {}
        }
    }
    Ok(out)
}

fn eval_function_call_positional(
    name: &str,
    args: &[Expr],
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    // v7.39 (round 237) — COALESCE / GREATEST / LEAST resolve their
    // arguments to one type the way CASE and ARRAY do. Checked statically:
    // an argument may have side effects (`COALESCE(nextval('s'), 1)`), so
    // its declared type is read rather than its value.
    if matches!(args.len(), 2..) {
        let construct = if name.eq_ignore_ascii_case("coalesce") {
            Some("COALESCE")
        } else if name.eq_ignore_ascii_case("greatest") {
            Some("GREATEST")
        } else if name.eq_ignore_ascii_case("least") {
            Some("LEAST")
        } else {
            None
        };
        if let Some(construct) = construct {
            unify_branch_types_static(construct, args.iter(), ctx)?;
        }
    }
    // v7.39 (read01 utils/adt, enum.c) — the enum introspection
    // v7.39 (read01 utils/adt, enum.c) — the enum introspection
    // family needs the ARGUMENT'S STATIC TYPE (the value is
    // usually NULL::enumtype): first/last/range over the
    // catalog's member order. Out-of-line so eval_expr's
    // recursion frame stays small (stack-depth guard budget).
    if (name.eq_ignore_ascii_case("enum_first")
        || name.eq_ignore_ascii_case("enum_last")
        || name.eq_ignore_ascii_case("enum_range"))
        && enum_introspection_applies(args, ctx)
    {
        return eval_enum_introspection(name, args, row, ctx);
    }
    // v7.39 (tz epic) — AT TIME ZONE (fn form: timezone(zone, ts))
    // with a NAMED zone needs the host tzdb and the argument's
    // static type for its two directions:
    //   naive AT ZONE  -> that zone's wall clock -> UTC instant
    //   tstz  AT ZONE  -> UTC instant -> that zone's wall clock
    // Fixed offsets / abbreviations keep the legacy path below.
    // v7.39 (round 523) — `to_char(tstz, fmt)` renders the LOCAL clock
    // in the session zone, and its zone tokens name that zone. It was
    // rendering the UTC reading and spelling it `UTC`, so a formatted
    // stamp disagreed with the same value's own `::text`.
    if args.len() == 2
        && name.eq_ignore_ascii_case("to_char")
        && let Some(zone) = ctx.session_gucs.and_then(|g| g.get("timezone"))
        && !zone.eq_ignore_ascii_case("utc")
        && !zone.eq_ignore_ascii_case("gmt")
        && crate::describe::describe_expr(&args[0], ctx.columns)
            .is_some_and(|s| matches!(s.ty, spg_storage::DataType::Timestamptz))
        && let Value::Timestamp(t) = eval_expr(&args[0], row, ctx)?
    {
        let off = ctx.session_tz_offset_at(t);
        let abbrev = ctx
            .session_tz_abbrev_at(t)
            .unwrap_or_else(|| zone.to_uppercase());
        let vals = [
            Value::Timestamp(t.saturating_add(off)),
            eval_expr(&args[1], row, ctx)?,
        ];
        return crate::eval::strings::to_char_in_zone(&vals, Some((&abbrev, off)));
    }
    // v7.39 (round 523) — `date_trunc(unit, tstz)` truncates on the
    // LOCAL calendar in the session zone. It was truncating in UTC and
    // rendering the result in the session zone, so under `SET TimeZone =
    // 'Asia/Tokyo'` a day truncation answered `2020-01-01 09:00:00+09` —
    // not a day boundary at all, and the wrong day for anything before
    // 09:00. Every report grouped by day was cut nine hours late.
    //
    // The three-argument form already does exactly this, DST reverse
    // lookup and all, so the session zone is passed to THAT rather than
    // written a second time. Only a statically-known timestamptz shifts:
    // a naive timestamp has no zone to be read in.
    if args.len() == 2
        && (name.eq_ignore_ascii_case("date_trunc") || name.eq_ignore_ascii_case("date_bin"))
        && let Some(zone) = ctx.session_gucs.and_then(|g| g.get("timezone"))
        && !zone.eq_ignore_ascii_case("utc")
        && !zone.eq_ignore_ascii_case("gmt")
        && crate::describe::describe_expr(&args[1], ctx.columns)
            .is_some_and(|s| matches!(s.ty, spg_storage::DataType::Timestamptz))
    {
        let vals = [
            eval_expr(&args[0], row, ctx)?,
            eval_expr(&args[1], row, ctx)?,
            Value::text(zone.clone()),
        ];
        return datetime::date_trunc(&vals, ctx);
    }
    if args.len() == 2
        && name.eq_ignore_ascii_case("timezone")
        && let zone_v = eval_expr(&args[0], row, ctx)?
        && let Value::Text(zone) = &zone_v
        && datetime::resolve_zone_offset(zone.as_ref()).is_none()
        && !zone.trim().eq_ignore_ascii_case("utc")
        && !zone.trim().eq_ignore_ascii_case("gmt")
        && zone.parse::<i64>().is_err()
        && ctx.tz_offset_fn.is_some()
    {
        let zone = zone.trim();
        let src_is_tstz = crate::describe::describe_expr(&args[1], ctx.columns)
            .is_some_and(|s| matches!(s.ty, spg_storage::DataType::Timestamptz));
        let inner = eval_expr(&args[1], row, ctx)?;
        if let Value::Timestamp(t) = inner {
            if src_is_tstz {
                if let Some(off) = ctx.zone_offset_at(zone, t) {
                    return Ok(Value::Timestamp(t + off));
                }
            } else if let Some(utc) = ctx.zone_local_to_utc(zone, t) {
                return Ok(Value::Timestamp(utc));
            }
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("time zone \"{zone}\" not recognized"),
            });
        }
    }
    // v7.29 (round-22 phase 3) - prefix fast path: LEFT(col, n)
    // on a TEXT column borrows the cell and clones only the
    // prefix. The generic path clones the WHOLE cell first -
    // a LEFT(body, 120) over 24k x 30 KB rows spent 383 ms
    // copying bytes it then threw away (7 ms without LEFT).
    if args.len() == 2
        && name.eq_ignore_ascii_case("left")
        && let Expr::Column(c) = &args[0]
        && let Some(cell) = resolve_column_borrowed(c, row, ctx)?
    {
        {
            match cell {
                Value::Null => return Ok(Value::Null),
                Value::Text(t) => {
                    let n_v = eval_expr(&args[1], row, ctx)?;
                    if let Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) = n_v {
                        let n = match n_v {
                            Value::SmallInt(x) => i64::from(x),
                            Value::Int(x) => i64::from(x),
                            Value::BigInt(x) => x,
                            _ => 0,
                        };
                        return Ok(Value::text(text_prefix_chars(t, n)));
                    }
                }
                _ => {}
            }
        }
    }
    // v7.38 (T-tstz Phase 1) — the ONE case where pg_typeof needs the
    // static type: timestamptz. The runtime value is a tz-less
    // Value::Timestamp, so the value-driven answer below can only ever
    // say "without time zone". For every other type the value-driven
    // path is strictly better (it distinguishes json vs jsonb, keeps
    // NULL as "unknown", and is not fooled by describe_expr's lossy
    // heuristics), so we consult the static typer ONLY when it says
    // Timestamptz and otherwise fall through untouched.
    if args.len() == 1
        && name.eq_ignore_ascii_case("pg_typeof")
        && crate::describe::describe_expr(&args[0], ctx.columns)
            .is_some_and(|s| matches!(s.ty, spg_storage::DataType::Timestamptz))
    {
        return Ok(Value::text::<alloc::string::String>(
            "timestamp with time zone".into(),
        ));
    }
    // v7.39 (round 694) — `oid[]`. Its VALUE is a BigIntArray, so the
    // value-driven namer answers `bigint[]`; the declared type lives in the
    // expression, exactly as it does for the Timestamptz arm above and for
    // the enum / domain / composite arms further down. The scalar `oid`
    // needed the same treatment in round 667.
    if args.len() == 1
        && name.eq_ignore_ascii_case("pg_typeof")
        && crate::describe::describe_expr(&args[0], ctx.columns)
            .is_some_and(|s| matches!(s.ty, spg_storage::DataType::OidArray))
    {
        return Ok(Value::text::<alloc::string::String>("oid[]".into()));
    }
    // v7.39 (read01 round 56) — a COMPOSITE column reports its type NAME, not
    // the generic `record` the runtime value would give. Composite-ness lives
    // outside the DataType lattice (the stored form is JSON), so the witness is
    // the column's `user_composite_type` — the same shape as the enum witness.
    if args.len() == 1
        && name.eq_ignore_ascii_case("pg_typeof")
        && let Expr::Column(c) = &args[0]
        && let Some(cname) = ctx
            .columns
            .iter()
            .find(|sc| sc.name.eq_ignore_ascii_case(&c.name))
            .and_then(|sc| sc.user_composite_type.as_deref())
    {
        return Ok(Value::text::<alloc::string::String>(cname.into()));
    }
    // v7.39 (round 291) — `name` is a DECLARED type over a Value::Text,
    // so the value can never witness it; the schema is the only witness,
    // exactly as for a composite column above.
    if args.len() == 1
        && name.eq_ignore_ascii_case("pg_typeof")
        && let Expr::Column(c) = &args[0]
        && ctx
            .columns
            .iter()
            .any(|sc| sc.name.eq_ignore_ascii_case(&c.name) && sc.ty == spg_storage::DataType::Name)
    {
        return Ok(Value::text::<alloc::string::String>("name".into()));
    }
    // …and the same for a bare `'abc'::name`, where the cast TARGET is
    // the witness. PG computes pg_typeof statically; SPG reads the
    // value, which by then is an ordinary text.
    if args.len() == 1
        && name.eq_ignore_ascii_case("pg_typeof")
        && let Expr::Cast {
            target: spg_sql::ast::CastTarget::Named(n),
            ..
        } = &args[0]
        && n.eq_ignore_ascii_case("name")
    {
        return Ok(Value::text::<alloc::string::String>("name".into()));
    }
    // v7.39 (read01 round 116) — a bare, uncoerced string literal is PG's
    // `unknown` type, not text: `pg_typeof('x')` / `pg_typeof('123')` /
    // `pg_typeof('2024-01-01')` all report `unknown`. The literal only becomes
    // text once context coerces it — a cast (`'x'::text`), a concatenation, or
    // a function argument — each of which is a different Expr node that falls
    // through to the value-driven path below (which correctly says text).
    if args.len() == 1
        && name.eq_ignore_ascii_case("pg_typeof")
        && matches!(&args[0], Expr::Literal(spg_sql::ast::Literal::String(_)))
    {
        return Ok(Value::text::<alloc::string::String>("unknown".into()));
    }
    // v7.37.16 — pg_typeof of a NULL cell reports the COLUMN's
    // static type when it has one (PG: `VALUES (NULL),(1.5)`
    // types the column numeric and its NULL row's pg_typeof says
    // numeric, not unknown).
    //
    // A BARE `NULL` stays "unknown", as PG has it. That used to fall
    // out of TEXT being absent from the name table — a NULL literal
    // describes as TEXT, so the lookup returned None and the caller
    // reported unknown. Round 871 filled that table in, which silently
    // took the bare-NULL behaviour with it and broke
    // `pg_typeof_null_returns_unknown`. The rule is now stated rather
    // than emergent: an untyped NULL literal is unknown, a NULL that
    // was cast reports what it was cast to.
    if args.len() == 1 && name.eq_ignore_ascii_case("pg_typeof") {
        if matches!(&args[0], Expr::Literal(spg_sql::ast::Literal::Null)) {
            return Ok(Value::text::<alloc::string::String>("unknown".into()));
        }
        let v = eval_expr(&args[0], row, ctx)?;
        if matches!(v, Value::Null)
            && let Some(shape) = crate::describe::describe_expr(&args[0], ctx.columns)
            && let Some(n) = pg_typeof_name_for_datatype(shape.ty)
        {
            return Ok(Value::text(n));
        }
        // v7.39 (round 640) — a NON-null cell normally answers from the
        // value, which is right for every type whose identity the value
        // carries. `xid8` has no value of its own — a cell is a
        // `Value::BigInt` — so it can only ever say "bigint" unless the
        // schema is asked. `xid` is listed with it because a cell that
        // reached here as a plain integer (a synthesised catalog row
        // that has not been converted) should still name its column's
        // type rather than the storage it arrived in.
        //
        // v7.39 (round 667) — `oid` joins them for the same reason and no
        // other: its cell is a `Value::BigInt` too, so `pg_typeof(1::oid)`
        // answered `bigint`. Still not a general switch — where the value
        // knows its own identity it is the better witness, because an
        // expression's static shape is an approximation and its result is
        // the fact.
        if !matches!(v, Value::Null)
            && let Some(shape) = crate::describe::describe_expr(&args[0], ctx.columns)
            && matches!(
                shape.ty,
                spg_storage::DataType::Xid
                    | spg_storage::DataType::Xid8
                    | spg_storage::DataType::Oid
            )
            && let Some(n) = pg_typeof_name_for_datatype(shape.ty)
        {
            return Ok(Value::text(n));
        }
        return apply_function(name, &[v], ctx);
    }
    // v7.37 D.1 — COALESCE result-type coercion. PG gives COALESCE the
    // common type of its branches, so a typed sibling (`NULL::time`,
    // `col::time`) makes the whole expression that type and an untyped
    // string-literal branch is coerced to it. Without this,
    // `COALESCE(NULL::time, '12:00')::text` rendered the raw `12:00`
    // instead of `12:00:00`. Only kicks in when the picked value is a
    // bare Text and a non-text cast-target sibling exists.
    if name.eq_ignore_ascii_case("coalesce") && !args.is_empty() {
        // v7.39 (round 609) — PG's COALESCE does not evaluate a branch past
        // the first non-NULL one. This evaluated every branch into a `Vec`
        // and so RAISED errors PG never raises: `coalesce(1, 1/0)`,
        // `coalesce(NULL, 2, 1/0)` and `coalesce(1, NULL, 1/0)` all failed
        // with "division by zero" where PG answers 1, 2 and 1.
        //
        // A branch after the pick is still READ for its type — that is what
        // decides the result's, and `COALESCE(1, 2.5)` is numeric in both
        // engines — but its error is discarded, because PG never runs it and
        // so never reports it. A branch that fails contributes no type,
        // which is the same as SPG having no declared type to widen to.
        //
        // The two `Vec`s this replaces cost two allocations a row even for
        // `coalesce(id, 0)` over a plain INTEGER column, where the answer
        // needs none.
        let mut result: Option<Value<'static>> = None;
        let mut tbuf = [spg_storage::DataType::Int; 8];
        let mut ntypes = 0usize;
        let mut spill: Vec<spg_storage::DataType> = Vec::new();
        for a in args {
            let v = if result.is_none() {
                eval_expr(a, row, ctx)?
            } else {
                match eval_expr(a, row, ctx) {
                    Ok(v) => v,
                    Err(_) => continue,
                }
            };
            // v7.39 (round 649) — a NULL branch still has a TYPE, and PG
            // resolves COALESCE's result from the branches' declared
            // types, not from the values that survive. `Value::Null` has
            // no `data_type()`, so `coalesce(1::int, NULL::float8)`
            // collected only `integer` and answered integer where PG
            // answers double precision. Ask the expression when the
            // value cannot say — inside the arm that already runs, and
            // only on the NULL that would otherwise contribute nothing.
            let branch_ty = match v.data_type() {
                Some(t) => Some(t),
                None => crate::describe::describe_expr(a, ctx.columns).map(|sh| sh.ty),
            };
            if let Some(t) = branch_ty {
                if ntypes < tbuf.len() {
                    tbuf[ntypes] = t;
                    ntypes += 1;
                } else {
                    spill.push(t);
                }
            }
            if result.is_none() && !matches!(v, Value::Null) {
                result = Some(v);
            }
        }
        let result = result.unwrap_or(Value::Null);
        if matches!(result, Value::Text(_)) {
            if let Some(target) = args.iter().find_map(coalesce_type_hint) {
                return crate::eval::cast::cast_value(result, target);
            }
        }
        // v7.38 (read01) — otherwise widen the picked value to the PG
        // common type of all branches (COALESCE(1, 2.5) → numeric).
        if spill.is_empty() {
            return Ok(widen_to_common(result, &tbuf[..ntypes]));
        }
        let mut types: Vec<spg_storage::DataType> = tbuf[..ntypes].to_vec();
        types.append(&mut spill);
        return Ok(widen_to_common(result, &types));
    }
    let evaluated: Result<Vec<Value<'static>>, _> =
        args.iter().map(|a| eval_expr(a, row, ctx)).collect();
    let evaluated = evaluated?;
    // v7.39 (read01 json.c) — to_json(timestamptz) spells the instant in
    // ISO 8601 WITH the session-zone offset ("2024-03-09T14:05:06+00:00"),
    // unlike plain timestamp. The runtime value carries no tz tag, so the
    // argument's static type is the witness.
    if (name.eq_ignore_ascii_case("to_json") || name.eq_ignore_ascii_case("to_jsonb"))
        && evaluated.len() == 1
        && let Some(Value::Timestamp(t)) = evaluated.first()
        && args.first().is_some_and(|a| {
            crate::describe::describe_expr(a, ctx.columns)
                .is_some_and(|sh| matches!(sh.ty, spg_storage::DataType::Timestamptz))
        })
    {
        let off = ctx.session_tz_offset_at(*t);
        let local = t + off;
        let days = local.div_euclid(86_400_000_000);
        let day_us = local.rem_euclid(86_400_000_000);
        let (y, mo, d) = civil_from_days(i32::try_from(days).unwrap_or(0));
        let secs = day_us / 1_000_000;
        let frac = day_us % 1_000_000;
        let (hh, mi, ss) = (secs / 3600, (secs / 60) % 60, secs % 60);
        let mut txt = alloc::format!("{y:04}-{mo:02}-{d:02}T{hh:02}:{mi:02}:{ss:02}");
        if frac != 0 {
            let f = alloc::format!("{frac:06}");
            txt.push('.');
            txt.push_str(f.trim_end_matches('0'));
        }
        let (sign, omag) = if off < 0 { ('-', -off) } else { ('+', off) };
        let (oh, om) = (omag / 3_600_000_000, (omag / 60_000_000) % 60);
        let _ = core::fmt::Write::write_fmt(&mut txt, format_args!("{sign}{oh:02}:{om:02}"));
        return Ok(Value::json(alloc::format!("\"{txt}\"")));
    }
    // v7.39 (enum order knife) — greatest/least over enum-typed arguments
    // pick by member order, not label text (PG). The witness needs the arg
    // ASTs, so this can't live in the value-level function dispatch.
    if (name.eq_ignore_ascii_case("greatest") || name.eq_ignore_ascii_case("least"))
        && let Some(labels) = args
            .iter()
            .find_map(|a| expr_enum_labels(a, ctx.columns, ctx.catalog))
        && evaluated
            .iter()
            .all(|v| matches!(v, Value::Text(_) | Value::Null))
    {
        let is_greatest = name.eq_ignore_ascii_case("greatest");
        let mut best: Option<&Value<'static>> = None;
        for v in evaluated.iter().filter(|v| !matches!(v, Value::Null)) {
            best = Some(match best {
                None => v,
                Some(b) => match enum_ord_cmp(labels, v, b) {
                    Some(core::cmp::Ordering::Greater) if is_greatest => v,
                    Some(core::cmp::Ordering::Less) if !is_greatest => v,
                    Some(_) => b,
                    // A non-member snuck in — fall out to the generic path.
                    None => return apply_function(name, &evaluated, ctx),
                },
            });
        }
        return Ok(best.cloned().unwrap_or(Value::Null));
    }
    // v7.39 (round 693) — and the same for a declared COLLATION, which
    // `least`/`greatest` need for the same structural reason: the witness
    // is the argument's column, so it cannot live in the value-level
    // dispatch either. Measured on PG18 over a column declaring
    // en_US.utf8: `least(a,'d')` is `d` and `greatest(a,'d')` is `Zebra`,
    // where byte order gives the pair reversed.
    if (name.eq_ignore_ascii_case("greatest") || name.eq_ignore_ascii_case("least"))
        && evaluated
            .iter()
            .all(|v| matches!(v, Value::Text(_) | Value::Null))
        && let Some(coll) = greatest_least_collation(args, ctx)
    {
        let is_greatest = name.eq_ignore_ascii_case("greatest");
        let mut best: Option<&Value<'static>> = None;
        for v in evaluated.iter().filter(|v| !matches!(v, Value::Null)) {
            best = Some(match (best, v) {
                (None, _) => v,
                (Some(Value::Text(y)), Value::Text(x)) => {
                    match crate::collate::compare(&coll, x, y) {
                        Some(core::cmp::Ordering::Greater) if is_greatest => v,
                        Some(core::cmp::Ordering::Less) if !is_greatest => v,
                        Some(_) => best.unwrap_or(v),
                        // Not a collation this build performs after all —
                        // one answer, from the generic path.
                        None => return apply_function(name, &evaluated, ctx),
                    }
                }
                (Some(b), _) => b,
            });
        }
        return Ok(best.cloned().unwrap_or(Value::Null));
    }
    // v7.39 (round 621) — an unadorned string literal takes the type the
    // function's parameter asks for. `justify_interval('36 hours')` is answered
    // by PG and was refused here, and so were `justify_days('35 days')` and
    // `justify_hours('27 hours')` — the spelling everyone writes, since typing
    // `INTERVAL` in front of the literal is exactly what PG saves you from.
    //
    // Only a LITERAL is resolved, which is the same boundary round 620 drew for
    // the boolean connectives: `justify_interval(t)` over a TEXT column stays
    // refused, because PG refuses it too (no such overload). The arg ASTs are
    // needed to tell those apart, so this cannot live in the value-level
    // dispatch — the same reason the enum witness above sits here.
    if let Some(want) = unknown_literal_param_type(name) {
        let mut coerced = evaluated;
        for (i, a) in args.iter().enumerate() {
            if is_unknown_string_literal(a)
                && let Some(slot) = coerced.get_mut(i)
            {
                *slot = cast::cast_value_in(
                    core::mem::replace(slot, Value::Null),
                    want.clone(),
                    false,
                )?;
            }
        }
        return apply_function(name, &coerced, ctx);
    }
    apply_function(name, &evaluated, ctx)
}

/// v7.39 (round 621) — the parameter type a bare string literal resolves to.
///
/// PG resolves an `unknown` literal to whatever the chosen overload declares.
/// SPG has no overload resolution to hang that on, so the functions whose only
/// parameter is unambiguous are listed. `None` leaves the argument alone.
fn unknown_literal_param_type(name: &str) -> Option<spg_sql::ast::CastTarget> {
    match name.to_ascii_lowercase().as_str() {
        "justify_days" | "justify_hours" | "justify_interval" => {
            Some(spg_sql::ast::CastTarget::Interval)
        }
        _ => None,
    }
}

/// Out-of-lined `eval_expr` arm — keeps the recursive frame small
/// (stack-depth guard budget); body unchanged.
#[inline(never)]
fn eval_any_all_arm(
    expr: &Expr,
    op: &BinOp,
    array: &Expr,
    is_any: bool,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let lhs = eval_expr(expr, row, ctx)?;
    let arr = eval_expr(array, row, ctx)?;
    any_all_over(lhs, arr, op, is_any)
}

/// v7.39 (round 597) — the ANY/ALL comparison with both sides already in
/// hand. Split out so a CONSTANT right-hand array can be built once at
/// compile time instead of once per row: `WHERE id = ANY (ARRAY[1..10])`
/// rebuilt the array for all 500k rows and cost 268 ms against PG18's 8.3,
/// rising to 494 ms at twenty elements, while the equivalent
/// `id IN (1..10)` took 2.3. The body below is unchanged; it never touched
/// `row`.
pub(crate) fn any_all_over(
    lhs: Value<'static>,
    arr: Value<'static>,
    op: &BinOp,
    is_any: bool,
) -> Result<Value<'static>, EvalError> {
    if matches!(arr, Value::Null) {
        return Ok(Value::Null);
    }
    // v7.38 (read01) — an unknown-string RHS (`x = ANY('{1,2,3}')`)
    // takes the LHS's type: coerce the external array text to the array
    // type matching the LHS's element type, like PG.
    let arr = match &arr {
        Value::Text(_) => {
            // The LHS's element type, or TEXT when the LHS is an
            // untyped NULL (PG's unknown → text default).
            let arr_ty = match lhs.data_type() {
                Some(spg_storage::DataType::SmallInt) => spg_storage::DataType::SmallIntArray,
                Some(spg_storage::DataType::Int) => spg_storage::DataType::IntArray,
                Some(spg_storage::DataType::BigInt) => spg_storage::DataType::BigIntArray,
                Some(spg_storage::DataType::Numeric { .. }) => spg_storage::DataType::NumericArray,
                Some(spg_storage::DataType::Float) => spg_storage::DataType::FloatArray,
                Some(spg_storage::DataType::Bool) => spg_storage::DataType::BoolArray,
                Some(spg_storage::DataType::Date) => spg_storage::DataType::DateArray,
                _ => spg_storage::DataType::TextArray,
            };
            crate::conversions::coerce_value(arr.clone(), arr_ty, "", 0).unwrap_or(arr)
        }
        _ => arr,
    };
    // Build the element list generically so every scalar array type
    // (numeric[], float8[], bool[], date[], …) is accepted, not just
    // int/bigint/text.
    let Some(len) = array_len(&arr) else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "ANY/ALL right-hand side must be an array, got {}",
                crate::conversions::pg_type_name_for_error_opt(arr.data_type())
            ),
        });
    };
    let elems: Vec<Option<Value>> = (0..len)
        .map(|i| match array_element_at(&arr, i) {
            Some(Value::Null) | None => None,
            Some(v) => Some(v),
        })
        .collect();
    // PG: `x op ANY (empty)` → false and `x op ALL (empty)` →
    // true, decided purely by emptiness — the comparison is
    // never evaluated, so a NULL LHS is irrelevant. This must
    // short-circuit before `saw_null` is seeded from the LHS,
    // otherwise `NULL op ANY/ALL (empty)` wrongly yields NULL.
    if elems.is_empty() {
        return Ok(Value::Bool(!is_any));
    }
    let mut saw_null = matches!(lhs, Value::Null);
    let mut saw_match = false;
    let mut saw_mismatch = false;
    for elem in elems {
        let elem_v = match elem {
            Some(v) => v,
            None => {
                saw_null = true;
                continue;
            }
        };
        if matches!(lhs, Value::Null) {
            saw_null = true;
            continue;
        }
        match apply_binary(*op, lhs.clone(), elem_v) {
            Ok(Value::Bool(true)) => saw_match = true,
            Ok(Value::Bool(false)) => saw_mismatch = true,
            Ok(Value::Null) => saw_null = true,
            Ok(other) => {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "ANY/ALL comparison didn't return Bool: {}",
                        crate::conversions::pg_type_name_for_error_opt(other.data_type())
                    ),
                });
            }
            Err(e) => return Err(e),
        }
    }
    let result = if is_any {
        if saw_match {
            Value::Bool(true)
        } else if saw_null {
            Value::Null
        } else {
            Value::Bool(false)
        }
    } else if saw_mismatch {
        Value::Bool(false)
    } else if saw_null {
        Value::Null
    } else {
        Value::Bool(true)
    };
    Ok(result)
}

/// Out-of-lined `eval_expr` arm — keeps the recursive frame small
/// (stack-depth guard budget); body unchanged.
#[inline(never)]
fn eval_case_arm(
    operand: &Option<alloc::boxed::Box<Expr>>,
    branches: &[(Expr, Expr)],
    else_branch: &Option<alloc::boxed::Box<Expr>>,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    // v7.39 (round 237) — PG resolves the RESULT branches to one type and
    // refuses the CASE when they have no common one, before running
    // anything. SPG returned whichever branch fired, so
    // `CASE WHEN true THEN 1 ELSE 'a'::text END` answered `1` and the same
    // expression answered text on another row.
    {
        // PG resolves the ELSE branch FIRST and then the WHEN results, and
        // its message names the running type before the conflicting one —
        // which is why `THEN 1 ELSE 'a'::text` reports "text and integer"
        // while a two-WHEN `THEN 1 ... THEN true` reports "integer and
        // boolean". Probed against 18.4; the order is observable.
        let mut results: Vec<&Expr> = Vec::with_capacity(branches.len() + 1);
        if let Some(e) = else_branch {
            results.push(e);
        }
        results.extend(branches.iter().map(|(_, r)| r));
        unify_branch_types_static("CASE", results, ctx)?;
    }
    let operand_value = match operand {
        Some(o) => Some(eval_expr(o, row, ctx)?),
        None => None,
    };
    // v7.37 D.1 — CASE result-type coercion (same rule as COALESCE): a
    // typed result branch (`... THEN '10:00'::time`) makes the whole
    // CASE that type, so an untyped string-literal branch is coerced to
    // it. Compute the hint once from every THEN/ELSE branch.
    let case_hint = branches
        .iter()
        .map(|(_, t)| t)
        .chain(else_branch.iter().map(|b| b.as_ref()))
        .find_map(coalesce_type_hint);
    // v7.38 (read01) — the CASE result is PG's common type of every
    // THEN/ELSE branch, so a taken integer branch is widened to
    // numeric when a sibling branch is numeric (and `pg_typeof` /
    // downstream division match PG). Only one branch is evaluated, so
    // the type must come from the branch expressions statically.
    let branch_types: Vec<spg_storage::DataType> = branches
        .iter()
        .map(|(_, t)| t)
        .chain(else_branch.iter().map(|b| b.as_ref()))
        .filter_map(|e| crate::describe::describe_expr(e, ctx.columns).map(|s| s.ty))
        .collect();
    let coerce = |v: Value<'static>| -> Result<Value<'static>, EvalError> {
        let v = match (&v, &case_hint) {
            (Value::Text(_), Some(target)) => cast::cast_value(v, target.clone())?,
            _ => v,
        };
        Ok(widen_to_common(v, &branch_types))
    };
    for (when_expr, then_expr) in branches {
        let when_value = eval_expr(when_expr, row, ctx)?;
        let matched = match &operand_value {
            // v7.39 (round 346, M1) — the WHEN condition is a truth value,
            // not a boolean-shaped one: `CASE WHEN 1 THEN 'a' END` used to
            // answer NULL in BOTH dialects, where MariaDB answers `a` and
            // PG raises `argument of CASE/WHEN must be type boolean`.
            None => predicate_is_true(&when_value, "CASE/WHEN", ctx.mysql_dialect)?,
            // v7.39 (round 412) — under the MySQL default collation the
            // `CASE op WHEN v` equality folds Text/BpChar operands (CI +
            // accent + PAD SPACE), matching `op = v` outside CASE.
            Some(op_v) => {
                let (l, r) = if ctx.mysql_dialect {
                    match (op_v, &when_value) {
                        // v7.38.18 — fold each side on its OWN type. This
                        // pair match missed `CASE <char col> WHEN
                        // '<literal>'`: a BpChar against a Text is neither
                        // arm, so it compared bytes with the CHAR still
                        // padded and answered ELSE.
                        (x, y)
                            if spg_storage::mysql_fold_value(x).is_some()
                                && spg_storage::mysql_fold_value(y).is_some() =>
                        {
                            (
                                Value::text(spg_storage::mysql_fold_value(x).unwrap()),
                                Value::text(spg_storage::mysql_fold_value(y).unwrap()),
                            )
                        }
                        _ => (op_v.clone(), when_value),
                    }
                } else {
                    (op_v.clone(), when_value)
                };
                matches!(
                    apply_binary(spg_sql::ast::BinOp::Eq, l, r)?,
                    Value::Bool(true)
                )
            }
        };
        if matched {
            return coerce(eval_expr(then_expr, row, ctx)?);
        }
    }
    match else_branch {
        Some(e) => coerce(eval_expr(e, row, ctx)?),
        None => Ok(Value::Null),
    }
}

/// Out-of-lined `eval_expr` arm — keeps the recursive frame small
/// (stack-depth guard budget); body unchanged.
#[inline(never)]
fn eval_array_slice_arm(
    target: &Expr,
    lo: &Option<alloc::boxed::Box<Expr>>,
    hi: &Option<alloc::boxed::Box<Expr>>,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let target_v = eval_expr(target, row, ctx)?;
    if matches!(target_v, Value::Null) {
        return Ok(Value::Null);
    }
    let bound = |e: Option<&Expr>| -> Result<Option<i64>, EvalError> {
        match e {
            None => Ok(None),
            Some(b) => match eval_expr(b, row, ctx)? {
                Value::Null => Ok(None),
                Value::Int(n) => Ok(Some(i64::from(n))),
                Value::BigInt(n) => Ok(Some(n)),
                Value::SmallInt(n) => Ok(Some(i64::from(n))),
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array slice bound must be integer, got {}",
                        crate::conversions::pg_type_name_for_error_opt(other.data_type())
                    ),
                }),
            },
        }
    };
    let lo_b = bound(lo.as_deref())?;
    let hi_b = bound(hi.as_deref())?;
    fn window(len: usize, lo: Option<i64>, hi: Option<i64>) -> (usize, usize) {
        let start = lo.map_or(0, |l| (l.max(1) - 1) as usize).min(len);
        let end = hi.map_or(len, |h| h.max(0) as usize).min(len);
        (start, end.max(start))
    }
    match target_v {
        Value::TextArray(items) => {
            let (s, e) = window(items.len(), lo_b, hi_b);
            Ok(Value::TextArray(items[s..e].to_vec()))
        }
        Value::IntArray(items) => {
            let (s, e) = window(items.len(), lo_b, hi_b);
            Ok(Value::IntArray(items[s..e].to_vec()))
        }
        Value::BigIntArray(items) => {
            let (s, e) = window(items.len(), lo_b, hi_b);
            Ok(Value::BigIntArray(items[s..e].to_vec()))
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!(
                "slice target must be an array, got {}",
                crate::conversions::pg_type_name_for_error_opt(other.data_type())
            ),
        }),
    }
}

/// Out-of-lined `eval_expr` arm — keeps the recursive frame small
/// (stack-depth guard budget); body unchanged.
#[inline(never)]
fn eval_in_list_arm(
    expr: &Expr,
    list: &[Expr],
    negated: bool,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    // v7.39 (round 238) — PG resolves the whole list's type BEFORE comparing
    // anything, so `1 IN (1, 'a'::text)` is refused. SPG compared item by
    // item and broke on the first match, so the offending element was never
    // reached and the predicate quietly answered true. Checked statically,
    // like round 237: evaluating the rest of the list to inspect it would
    // change when side effects fire.
    require_in_list_comparable(expr, list, ctx)?;
    // v7.39 (round 364, M4 P2) — a MySQL session folds text before the
    // membership test, so `t IN ('FOO')` matches 'Foo'. `BINARY` is not
    // reachable through a bare column needle here; the fold is text-only.
    // v7.39 (round 370, M4 P4a) — an explicit `COLLATE utf8mb4_bin` needle
    // column is byte-wise, so it does not fold. v7.39 (round 371, M4 P4b) —
    // a per-expression `… COLLATE utf8mb4_bin` / `BINARY …` on the needle
    // OR any list item forces the whole membership test byte-wise.
    let in_fold = ctx.mysql_dialect
        && !resolve::operand_is_binary_column(expr, ctx)
        && !resolve::is_binary_coerced(expr)
        && !list.iter().any(|i| resolve::is_binary_coerced(i));
    // v7.38.18 — the membership test has ONE collation, the needle's,
    // and its NAME decides whether trailing spaces count.
    let in_pads = crate::collate::pads_space(resolve::column_collation_name(expr, ctx).as_deref());
    let needle = mysql_collation_key(eval_expr(expr, row, ctx)?, in_fold, in_pads);
    let needle_null = matches!(needle, Value::Null);
    let mut saw_null = needle_null && !list.is_empty();
    let mut matched = false;
    if !needle_null {
        for item in list {
            let v = mysql_collation_key(eval_expr(item, row, ctx)?, in_fold, in_pads);
            if matches!(v, Value::Null) {
                saw_null = true;
                continue;
            }
            match apply_binary(BinOp::Eq, needle.clone(), v)? {
                Value::Bool(true) => {
                    matched = true;
                    break;
                }
                Value::Bool(false) => {}
                Value::Null => saw_null = true,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "IN comparison didn't return Bool: {}",
                            crate::conversions::pg_type_name_for_error_opt(other.data_type())
                        ),
                    });
                }
            }
        }
    }
    let inner = if matched {
        Value::Bool(true)
    } else if saw_null {
        Value::Null
    } else {
        Value::Bool(false)
    };
    Ok(match (negated, inner) {
        (true, Value::Bool(b)) => Value::Bool(!b),
        (_, v) => v,
    })
}

/// Out-of-lined `eval_expr` arm — keeps the recursive frame small
/// (stack-depth guard budget); body unchanged.
#[inline(never)]
fn eval_like_arm(
    expr: &Expr,
    pattern: &Expr,
    negated: bool,
    case_insensitive: bool,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let v = eval_expr(expr, row, ctx)?;
    let p = eval_expr(pattern, row, ctx)?;
    // NULL on either side propagates to NULL — same as PG.
    // v7.39 (bpchar epic) — LIKE matches bpchar on its PADDED
    // stored form, per PG's bpchar pattern operators.
    let (text, pat) = match (v, p) {
        (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
        (Value::Text(a) | Value::BpChar(a), Value::Text(b) | Value::BpChar(b)) => (a, b),
        (Value::Text(_) | Value::BpChar(_), other) | (other, _) => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "LIKE requires text operands, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            });
        }
    };
    // v7.25 (round-17) — ILIKE folds both operands (PG
    // lowercases per the default collation).
    // v7.39 (round 364, M4 P2) — a MySQL session's default collation is
    // accent- and case-insensitive, so `LIKE` folds both sides the way
    // `ILIKE` does; the wildcards `%` / `_` are not Latin letters so the
    // fold leaves them alone.
    // v7.39 (round 370, M4 P4a) — an explicit `COLLATE utf8mb4_bin` column
    // matches byte-wise, so it does not fold. v7.39 (round 371, M4 P4b) —
    // a per-expression `… COLLATE utf8mb4_bin` / `BINARY …` on either the
    // value or the pattern forces byte-wise too.
    let mysql = ctx.mysql_dialect
        && !resolve::operand_is_binary_column(expr, ctx)
        && !resolve::operand_is_binary_column(pattern, ctx)
        && !resolve::is_binary_coerced(expr)
        && !resolve::is_binary_coerced(pattern);
    let m = if case_insensitive {
        like_match(&text.to_lowercase(), &pat.to_lowercase())?
    } else if mysql {
        like_match(
            &spg_storage::mysql_ci_fold(&text),
            &spg_storage::mysql_ci_fold(&pat),
        )?
    } else {
        like_match(&text, &pat)?
    };
    Ok(Value::Bool(if negated { !m } else { m }))
}

/// Out-of-lined `eval_expr` arm — keeps the recursive frame small
/// (stack-depth guard budget); body unchanged.
#[inline(never)]
fn eval_extract_arm(
    field: &spg_sql::ast::ExtractField,
    source: &Expr,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let v = eval_expr(source, row, ctx)?;
    extract_from_value(field, v, source, ctx)
}

/// v7.39 (round 595) — the field extraction, with the source value already
/// in hand. Split out so the compiled-predicate program can pop the source
/// off its stack instead of handing the whole node back to the interpreter:
/// one non-compilable node used to disqualify the entire WHERE, and
/// `WHERE extract(year FROM t) = 2020` was interpreting the column read and
/// the comparison too. The body below is unchanged; it never touched `row`.
pub(crate) fn extract_from_value(
    field: &spg_sql::ast::ExtractField,
    v: Value<'static>,
    source: &Expr,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    // v7.39 (round 382) — MySQL coerces a date/time STRING to its temporal
    // value for EXTRACT (`EXTRACT(YEAR FROM '2020-05-15')` is 2020, and the
    // time fields read a `'... HH:MM:SS'` string); PG needs a typed source.
    let v = match &v {
        Value::Text(s) if ctx.mysql_dialect => text_as_temporal(s).unwrap_or(v),
        _ => v,
    };
    // v7.39 (tz epic) — timezone[_hour|_minute] of a timestamptz
    // reports the SESSION offset at that instant (PG: 32400 for
    // Tokyo; -14400 for New York in July).
    if matches!(
        field,
        spg_sql::ast::ExtractField::Timezone
            | spg_sql::ast::ExtractField::TimezoneHour
            | spg_sql::ast::ExtractField::TimezoneMinute
    ) && let Value::Timestamp(t) = &v
        && crate::describe::describe_expr(source, ctx.columns)
            .is_some_and(|s| matches!(s.ty, spg_storage::DataType::Timestamptz))
    {
        let off_secs = ctx.session_tz_offset_at(*t) / 1_000_000;
        let n = match field {
            spg_sql::ast::ExtractField::Timezone => off_secs,
            spg_sql::ast::ExtractField::TimezoneHour => off_secs / 3600,
            _ => (off_secs / 60) % 60,
        };
        // v7.39 (round 253) — numeric, like every other EXTRACT result.
        return Ok(Value::Numeric {
            scaled: i128::from(n),
            scale: 0,
            kind: spg_storage::NumericKind::Finite,
        });
    }
    // v7.39 (round 523) — and every OTHER field of a timestamptz reads
    // the local clock in the session zone, which is the whole reason PG
    // has the type. `extract(hour from …)` answered the UTC hour under
    // `SET TimeZone = 'Asia/Tokyo'` — 0 where PG says 9 — and
    // `extract(dow …)` therefore named the wrong DAY, so a report
    // grouped by weekday put nine hours of every Sunday under Saturday.
    // Only fields of the local clock shift; epoch and julian are
    // absolute, and the timezone fields answered above.
    let v = match &v {
        Value::Timestamp(t)
            if !matches!(
                field,
                spg_sql::ast::ExtractField::Epoch
                    | spg_sql::ast::ExtractField::Julian
                    | spg_sql::ast::ExtractField::Timezone
                    | spg_sql::ast::ExtractField::TimezoneHour
                    | spg_sql::ast::ExtractField::TimezoneMinute
            ) && crate::describe::describe_expr(source, ctx.columns)
                .is_some_and(|s| matches!(s.ty, spg_storage::DataType::Timestamptz)) =>
        {
            Value::Timestamp(t.saturating_add(ctx.session_tz_offset_at(*t)))
        }
        _ => v,
    };
    // v7.39 (round 253) — the source's PG type name for error wording,
    // upgraded from the declared type when statically known (a tstz
    // VALUE is indistinguishable from a timestamp). Only a cast /
    // column is trusted (the r237 lesson: describe_expr reports a
    // binary operator as its left operand's type).
    let static_declared = matches!(source, Expr::Cast { .. } | Expr::Column(_))
        .then(|| crate::describe::describe_expr(source, ctx.columns))
        .flatten()
        .map(|sch| sch.ty);
    let src_name = match static_declared {
        Some(spg_storage::DataType::Timestamptz) => "timestamp with time zone",
        _ => datetime::value_src_type_name(&v),
    };
    // PG rejects the timezone family on a plain timestamp (0A000);
    // only reject when the declared type is STATICALLY timestamp — a
    // dynamic value stays lenient (the pre-r253 zero answer).
    if matches!(
        field,
        spg_sql::ast::ExtractField::Timezone
            | spg_sql::ast::ExtractField::TimezoneHour
            | spg_sql::ast::ExtractField::TimezoneMinute
    ) && matches!(static_declared, Some(spg_storage::DataType::Timestamp))
    {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "unit \"{}\" not supported for type timestamp without time zone",
                alloc::format!("{field}").to_lowercase()
            ),
        });
    }
    // v7.39 (round 418) — MySQL's compound units (`DAY_SECOND`, `YEAR_MONTH`,
    // …) reach here as `ExtractField::Other`, which PG rejects. Under the
    // MySQL dialect they pack several components into one integer instead.
    if ctx.mysql_dialect
        && let spg_sql::ast::ExtractField::Other(name) = field
        && let Some(packed) = crate::eval::datetime::mysql_compound_extract(name, &v)
    {
        return Ok(packed);
    }
    extract_field(field, &v, src_name)
}

/// Out-of-lined `eval_expr` arm — keeps the recursive frame small
/// (stack-depth guard budget); body unchanged.
#[inline(never)]
fn eval_array_subscript_arm(
    expr: &Expr,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    // Collect the whole subscript chain so PG's multi-dimensional
    // access (`arr[i][j]` is ONE N-subscript op) is distinguishable
    // from chained 1-D indexing. `arr[1][2]` parses as
    // `(arr[1])[2]`; PG indexes the matrix directly and returns NULL
    // for a partial subscript (`arr[1]` on a 2-D array is NULL).
    let mut idx_exprs: Vec<&Expr> = Vec::new();
    let mut base = expr;
    while let Expr::ArraySubscript { target, index } = base {
        idx_exprs.push(index);
        base = target;
    }
    idx_exprs.reverse();
    let base_v = eval_expr(base, row, ctx)?;
    if matches!(
        base_v,
        Value::IntArray2D(_)
            | Value::BigIntArray2D(_)
            | Value::TextArray2D(_)
            | Value::BoolArray2D(_)
    ) {
        return eval_matrix_subscript(&base_v, &idx_exprs, row, ctx);
    }
    // 1-D array / JSON: apply each subscript left-to-right. This
    // reproduces the prior single-subscript semantics exactly, and
    // chained JSON (`j['a']['b']`) still resolves step by step.
    let mut cur = base_v;
    for ix in idx_exprs {
        cur = apply_one_subscript(cur, ix, row, ctx)?;
    }
    Ok(cur)
}

/// Out-of-lined `eval_expr` arm — keeps the recursive frame small
/// (stack-depth guard budget); body unchanged.
#[inline(never)]
fn eval_field_access_arm(
    base: &Expr,
    field: &str,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    // v7.38 (read01, T9) — composite field access `(expr).field`.
    // The base evaluates to a record; look the member up by name
    // (`f1`..`fN` for an anonymous ROW, base column names for a
    // whole-row). A NULL record yields NULL (PG semantics).
    let v = eval_expr(base, row, ctx)?;
    match v {
        Value::Null => Ok(Value::Null),
        Value::Composite(fields) => fields
            .into_iter()
            .find(|(name, _)| name == field)
            .map(|(_, val)| val)
            .ok_or_else(|| missing_field_error(base, field, ctx)),
        _ => Err(not_a_composite_error(base, field, ctx)),
    }
}

/// v7.39 (round 285) — PG words a missing composite field three ways, and
/// which one you get depends on the base expression's STATIC type, not on
/// the value:
///
///   * a named composite — `column "nosuch" not found in data type rc9`
///   * a whole-row table reference — `column rt8.nosuch does not exist`
///     (unquoted, and qualified — the odd one out)
///   * an anonymous ROW or `::record` — `could not identify column
///     "nosuch" in record data type`
///
/// All three read off live PG 18.4. A `Value::Composite` carries its field
/// names but not its type name, so the base expression is what decides.
fn missing_field_error(base: &Expr, field: &str, ctx: &EvalContext<'_>) -> EvalError {
    // v7.39 (round 307, V25) — the named type may arrive by cast OR from
    // the schema of a column that a projection produced, so ask once and
    // let the catalog say whether it is a composite. Before this only
    // the cast spelling was recognised, which is why a composite that
    // came through a derived table or a CTE — where the base is a plain
    // column — fell through to the anonymous-record wording.
    if let Some(name) = base_named_type(base, ctx)
        && ctx
            .catalog
            .is_some_and(|c| c.composite_types().contains_key(name))
    {
        return EvalError::TypeMismatch {
            detail: alloc::format!("column \"{field}\" not found in data type {name}"),
        };
    }
    if let Expr::Column(c) = base
        && c.qualifier.is_none()
    {
        // A column DECLARED as a named composite reports that type — the
        // schema records it in `user_composite_type`, which is the only
        // place the name survives (a `Value::Composite` does not carry it).
        if let Some(name) = ctx
            .columns
            .iter()
            .find(|col| col.name.eq_ignore_ascii_case(&c.name))
            .and_then(|col| col.user_composite_type.as_ref())
        {
            return EvalError::TypeMismatch {
                detail: alloc::format!("column \"{field}\" not found in data type {name}"),
            };
        }
        // A whole-row reference to a real table is the odd wording out:
        // qualified, and unquoted.
        if ctx.catalog.is_some_and(|cat| cat.get(&c.name).is_some()) {
            return EvalError::TypeMismatch {
                detail: alloc::format!("column {}.{field} does not exist", c.name),
            };
        }
    }
    EvalError::TypeMismatch {
        detail: alloc::format!("could not identify column \"{field}\" in record data type"),
    }
}

/// v7.39 (round 307, V25) — the user-declared type name behind a field
/// access, if any: either written as a cast (`ROW(…)::rc9`) or carried on
/// the column's schema.
///
/// All three schema slots are consulted rather than just the composite
/// one. A projection currently files the name of a `::rc9` cast under
/// `user_enum_type` — `expr_enum_type_name` answers for ANY named cast,
/// without asking whether the name is an enum — so keying off one slot
/// would answer for some shapes and not others. The caller decides what
/// the name MEANS by asking the catalog, which is the only thing that
/// actually knows; this function just finds it.
fn base_named_type<'c>(base: &'c Expr, ctx: &'c EvalContext<'_>) -> Option<&'c str> {
    match base {
        Expr::Cast {
            target: CastTarget::Named(name),
            ..
        } => Some(name.as_str()),
        Expr::Column(c) => ctx
            .columns
            .iter()
            .find(|sc| sc.name.eq_ignore_ascii_case(&c.name))
            .and_then(|sc| {
                sc.user_composite_type
                    .as_deref()
                    .or(sc.user_domain_type.as_deref())
                    .or(sc.user_enum_type.as_deref())
            }),
        _ => None,
    }
}

/// v7.39 (round 307, V25) — PG's wording when field notation is applied
/// to something that is not a composite at all. It names the type:
/// `column notation .f applied to type pos9, which is not a composite
/// type` — for a domain and an enum alike. Only when the base has no
/// user-declared type at all does the generic message stand.
fn not_a_composite_error(base: &Expr, field: &str, ctx: &EvalContext<'_>) -> EvalError {
    if let Some(name) = base_named_type(base, ctx)
        && ctx.catalog.is_some_and(|c| {
            c.enum_types().contains_key(name) || c.domain_types().contains_key(name)
        })
    {
        return EvalError::TypeMismatch {
            detail: alloc::format!(
                "column notation .{field} applied to type {name}, which is not a composite type"
            ),
        };
    }
    EvalError::TypeMismatch {
        detail: alloc::format!("field access `.{field}` requires a composite (record) value"),
    }
}

/// Out-of-lined `eval_expr` arm — keeps the recursive frame small
/// (stack-depth guard budget); body unchanged.
#[inline(never)]
/// v7.39 (round 328, V45) — the three-valued boolean tests. None of them
/// ever answers NULL: a NULL input is "not true" and "not false", and IS
/// UNKNOWN is precisely the NULL case. Verified against PG 18.4 —
/// `NULL::bool IS TRUE` is false, `IS NOT TRUE` true, `IS UNKNOWN` true,
/// and `false IS NOT FALSE` false.
fn eval_bool_test_arm(
    expr: &Expr,
    value: Option<bool>,
    negated: bool,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let v = eval_expr(expr, row, ctx)?;
    let hit = match (value, &v) {
        // IS UNKNOWN — the input is NULL.
        (None, Value::Null) => true,
        // v7.39 (round 625) — and PG rejects a non-boolean here too:
        // `argument of IS UNKNOWN must be type boolean`. MySQL has no
        // IS UNKNOWN, so there is no dialect branch.
        (None, Value::Bool(_)) => false,
        (None, other) => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "argument of IS {}UNKNOWN must be type boolean, not type {}",
                    if negated { "NOT " } else { "" },
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            });
        }
        (Some(_), Value::Null) => false,
        (Some(want), Value::Bool(b)) => *b == want,
        // v7.39 (round 397) — MySQL reads a non-boolean as a truth value
        // for `IS TRUE` / `IS FALSE` (`5 IS TRUE` is 1, `0 IS FALSE` is 1,
        // `'abc' IS TRUE` is 0). PG rejects a non-boolean at parse time, so
        // this only fires under the dialect; a NULL is already handled.
        (Some(want), other) if ctx.mysql_dialect => mysql_truthy(other) == want,
        // v7.39 (round 625, S05b/F29) — on PG a non-boolean is REJECTED, and
        // the comment above said so while the arm below answered `false`
        // anyway. `1 IS TRUE` came back false, which reads as "the test was
        // run and did not hold" rather than "you cannot ask this of an
        // integer" — the wrong answer for every non-boolean type, eight of
        // them measured. PG's own sentence, which names the operator and the
        // type it got.
        (Some(_), other) => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "argument of IS {}{} must be type boolean, not type {}",
                    if negated { "NOT " } else { "" },
                    if value == Some(true) { "TRUE" } else { "FALSE" },
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            });
        }
    };
    Ok(Value::Bool(hit != negated))
}

fn eval_is_null_arm(
    expr: &Expr,
    negated: bool,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    // v7.38 (read01 P4.11) — `ROW(...) IS [NOT] NULL` is evaluated
    // field-wise, not as a whole-value null test: a row IS NULL when
    // every field is null, and IS NOT NULL when every field is
    // non-null — so the two are NOT simple negations (ROW(1,NULL) is
    // neither). A field that is itself a row is a non-null value, so
    // the check does not recurse. The `(a, b) IS NULL` tuple spelling
    // is already desugared to `a IS NULL AND b IS NULL` in the parser;
    // this covers the explicit `ROW(...)` constructor.
    if let Expr::FunctionCall { name, args } = expr
        && name.eq_ignore_ascii_case("row")
    {
        let mut all_null = true;
        let mut all_non_null = true;
        for a in args {
            if matches!(eval_expr(a, row, ctx)?, Value::Null) {
                all_non_null = false;
            } else {
                all_null = false;
            }
        }
        return Ok(Value::Bool(if negated { all_non_null } else { all_null }));
    }
    // v7.39 (round 962) — the same field-wise rule for a row-valued
    // EXPRESSION, not just the `ROW(...)` spelling. P4.11 keyed on the
    // syntax, so every other way to hold a row got the whole-value test:
    // measured against PG18.4, `SELECT an IS NULL FROM an` on a row whose
    // every column is NULL answered `t` there and `f` here, and a column
    // declared with a composite type behaved the same way. Round 961 made
    // whole-row references reachable through a projection, which is what
    // surfaced it.
    //
    // Fields are tested exactly as the `ROW(...)` arm tests its
    // arguments, without recursing — a field that is itself a row is a
    // non-null value.
    let v = eval_expr(expr, row, ctx)?;
    if let Value::Composite(fields) = &v {
        let mut all_null = true;
        let mut all_non_null = true;
        for (_, f) in fields {
            if matches!(f, Value::Null) {
                all_non_null = false;
            } else {
                all_null = false;
            }
        }
        return Ok(Value::Bool(if negated { all_non_null } else { all_null }));
    }
    let is_null = matches!(v, Value::Null);
    Ok(Value::Bool(if negated { !is_null } else { is_null }))
}

pub fn eval_expr(
    expr: &Expr,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    // v7.38 (read01 P3.25) — guard against a native stack overflow on a
    // pathologically nested expression (`a AND a AND … ` × thousands): the
    // recursion base is seeded on the outermost call, and once a deeper
    // call has consumed more than the budget we return an error the way
    // PG's check_stack_depth() does, rather than aborting the process.
    let sp = eval_stack_ptr();
    let base = ctx.recursion_base.get();
    if base == 0 {
        ctx.recursion_base.set(sp);
    } else if base.saturating_sub(sp) > MAX_EVAL_STACK_BYTES {
        return Err(EvalError::StackDepthExceeded);
    }
    match expr {
        Expr::AggregateOrdered { .. } => Err(EvalError::TypeMismatch {
            detail: "aggregate ORDER BY is only valid inside an aggregating SELECT".into(),
        }),
        // A named argument is only meaningful inside a call, where the callee's
        // parameter names give it a slot. Anywhere else it is a syntax error,
        // and saying so beats silently evaluating it as if the name were absent.
        Expr::NamedArg { name, .. } => Err(EvalError::TypeMismatch {
            detail: alloc::format!("named argument \"{name}\" is only valid in a function call"),
        }),
        Expr::Literal(l) => Ok(literal_to_value(l)),
        Expr::Column(c) => resolve_column(c, row, ctx),
        Expr::Placeholder(n) => {
            let idx = usize::from(*n).saturating_sub(1);
            ctx.params
                .get(idx)
                .cloned()
                .ok_or_else(|| EvalError::PlaceholderOutOfRange {
                    n: *n,
                    bound: u16::try_from(ctx.params.len()).unwrap_or(u16::MAX),
                })
        }
        // v7.39 (round 620) — an unadorned string literal carries PG's
        // `unknown` type, and a boolean connective is a context that resolves
        // it TO boolean. `'true' AND true`, `'f' OR false` and `NOT 'a'` are
        // answered by PG (`t`, `f`, and the input-syntax error respectively)
        // and were all refused here with `argument of … must be type boolean,
        // not type text` — the message PG reserves for an operand that really
        // IS text (`''::TEXT AND true`), which stays refused. Out-of-line and
        // behind a literal-shaped guard: this is the recursive frame the
        // 768 KiB stack budget is tuned against.
        Expr::Unary {
            op: spg_sql::ast::UnOp::Not,
            expr,
        } if !ctx.mysql_dialect && is_unknown_string_literal(expr) => apply_unary(
            spg_sql::ast::UnOp::Not,
            coerce_unknown_literal_to_bool(expr)?,
        ),
        Expr::Unary { op, expr } => {
            let v = eval_expr(expr, row, ctx)?;
            // The MySQL-specific unary readings (NOT any truth value, `-`/`~`
            // on a string, `~` unsigned) live out-of-line: `eval_expr` is the
            // recursive frame the 768 KiB stack-depth budget is tuned
            // against, and locals added here cost one nesting level each (the
            // round-305 / round-383 frame cliff).
            if ctx.mysql_dialect {
                if let Some(r) = mysql_unary_arm(*op, &v) {
                    return r;
                }
            }
            apply_unary(*op, v)
        }
        // v7.39 (round 346, M1) — MariaDB reads both sides of AND / OR as
        // truth values (`1 AND 2` is 1, measured). apply_binary has no
        // dialect, so the coercion happens here, where it does. The body
        // is out-of-line: `eval_expr` is the recursive frame the 768 KiB
        // stack-depth budget is tuned against, and locals added here cost
        // one nesting level each (the round-305 frame cliff).
        Expr::Binary { lhs, op, rhs }
            if ctx.mysql_dialect && matches!(op, BinOp::And | BinOp::Or | BinOp::LogicalXor) =>
        {
            eval_mysql_connective(lhs, *op, rhs, row, ctx)
        }
        // v7.39 (round 620/621) — the unknown-literal resolution and the
        // short circuit, both out of line. Placed AFTER the MySQL arm so the
        // dialect keeps its own reading of these connectives.
        Expr::Binary { lhs, op, rhs } if matches!(op, BinOp::And | BinOp::Or) => {
            eval_connective(lhs, *op, rhs, row, ctx)
        }
        Expr::Binary { lhs, op, rhs } => {
            // v7.32 (P4 borrow channel) — comparison fast path. A pure
            // comparison op only reads its operands and returns Bool,
            // and for non-NUMERIC / non-INTERVAL / non-CI-collation
            // operands `apply_binary` IS just the NULL-3VL check plus
            // the ref-based `compare` (NUMERIC routes through fixed-
            // point `apply_binary_numeric`; INTERVAL through
            // `apply_binary_interval`; CI columns fold). So read the
            // operands borrowed — a column cell is no longer cloned
            // just to compare it (`WHERE thread_id != ''` alone cloned
            // one Text cell per scanned row). Anything that needs the
            // owned path falls through unchanged.
            if matches!(
                op,
                BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
            ) {
                let lc = eval_expr_cow(lhs, row, ctx)?;
                let rc = eval_expr_cow(rhs, row, ctx)?;
                // v7.39 (enum order knife) — enum-typed operands compare by
                // member order, not label text. Cold unless both sides are
                // Text and the catalog has enum types at all.
                if matches!(lc.as_ref(), Value::Text(_)) && matches!(rc.as_ref(), Value::Text(_)) {
                    if let Some(r) = enum_compare_hook(*op, lhs, rhs, lc.as_ref(), rc.as_ref(), ctx)
                    {
                        return r;
                    }
                    // v7.39 (round 693) — and the collation hook, under the
                    // same Text/Text gate for the same reason.
                    if let Some(r) =
                        collate_compare_hook(*op, lhs, rhs, lc.as_ref(), rc.as_ref(), ctx)
                    {
                        return r;
                    }
                }
                // v7.39 (round 351, M11) — the three conditions fold into
                // ONE call. Adding a fourth as another `||` here tipped the
                // 768 KiB stack guard on its own (measured — this is the
                // hottest recursive frame there is); one call is cheaper
                // than the three it replaces.
                let owned_path = needs_owned_compare(lc.as_ref(), rc.as_ref(), lhs, rhs, ctx);
                if !owned_path {
                    if lc.as_ref().is_null() || rc.as_ref().is_null() {
                        return Ok(Value::Null);
                    }
                    return compare(*op, lc.as_ref(), rc.as_ref()).map_err(|e| {
                        unknown_literal_cmp_error(e, lhs, rhs, lc.as_ref(), rc.as_ref())
                    });
                }
                let (l, r) = collation_fold_for_compare(
                    *op,
                    lhs,
                    rhs,
                    lc.into_owned(),
                    rc.into_owned(),
                    ctx,
                );
                // The owned call consumes the values; the rewrite needs the
                // literal's text and the other side's type only on the ERROR
                // path, so capture those two up front — the capture itself is
                // gated on the cheap expr test, so a comparison with no
                // unknown literal pays one branch.
                let probe = (is_unknown_string_literal(lhs) || is_unknown_string_literal(rhs))
                    .then(|| (l.clone(), r.clone()));
                return apply_binary_in(*op, l, r, ctx.mysql_dialect).map_err(|e| match &probe {
                    Some((pl, pr)) => unknown_literal_cmp_error(e, lhs, rhs, pl, pr),
                    None => e,
                });
            }
            let l = eval_expr(lhs, row, ctx)?;
            let r = eval_expr(rhs, row, ctx)?;
            // v7.17.0 Phase 2.5 — collation-aware text comparison.
            // When either operand of a comparison op references a
            // column declared `COLLATE "case_insensitive"` (or any
            // MySQL `_ci` collation), case-fold both sides before
            // the byte-wise compare so `WHERE name = 'foo'` matches
            // stored `'Foo'`. Non-Text values fall straight through
            // — the helper is a no-op outside Text-Text equality
            // and inequality.
            let (l, r) = collation_fold_for_compare(*op, lhs, rhs, l, r, ctx);
            // v7.39 (GUC knife 4) — `date/interval/float || text` textifies
            // through the out-functions, which honour the session render
            // style. Pre-render the style-sensitive operand here (the
            // orthodox home is an implicit-cast node at type resolution;
            // until then this keeps apply_binary style-free). Default
            // style short-circuits — text_concat's own value_to_text
            // produces the identical bytes.
            if matches!(op, spg_sql::ast::BinOp::Concat)
                && ctx.render_style != format::RenderStyle::default()
            {
                let styled = |v: Value<'static>| -> Value<'static> {
                    match &v {
                        Value::Date(_)
                        | Value::Timestamp(_)
                        | Value::Interval { .. }
                        | Value::Float(_)
                        | Value::Real(_) => {
                            Value::text(values::value_to_text_styled(&v, &ctx.render_style))
                        }
                        _ => v,
                    }
                };
                let (sl, sr) = (styled(l), styled(r));
                return apply_binary(*op, sl, sr);
            }
            // v7.38.13 — in PG mode `apply_binary_mysql_unsigned` checks a
            // dialect flag and forwards, and `apply_binary_in` does the
            // same; both take two 48-byte `Value`s by value. Skip them.
            if ctx.mysql_dialect {
                apply_binary_mysql_unsigned(*op, lhs, rhs, l, r, ctx)
            } else {
                binop::apply_binary(*op, l, r)
            }
        }
        Expr::Cast { expr, target } => eval_cast_arm(expr, target, row, ctx),
        Expr::FieldAccess { base, field } => eval_field_access_arm(base, field, row, ctx),
        Expr::IsNull { expr, negated } => eval_is_null_arm(expr, *negated, row, ctx),
        // v7.39 (round 328, V45) — `x IS [NOT] TRUE | FALSE | UNKNOWN`.
        // Out-of-line like its IS NULL neighbour: an inline body here
        // grows every frame of the recursive evaluator, which is what
        // tipped the 512KB depth guard in round 305.
        Expr::BoolTest {
            expr,
            value,
            negated,
        } => eval_bool_test_arm(expr, *value, *negated, row, ctx),
        Expr::FunctionCall { name, args } => eval_function_call_arm(name, args, row, ctx),
        // v7.39 (read01 round 100) — VARIADIC is spliced into its enclosing
        // call before the args are evaluated (see eval_function_call_arm); a
        // bare one reaching here was written outside a function call.
        Expr::Variadic(_) => Err(EvalError::TypeMismatch {
            detail: "VARIADIC is only valid as a function-call argument".into(),
        }),
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => eval_like_arm(expr, pattern, *negated, *case_insensitive, row, ctx),
        Expr::Extract { field, source } => eval_extract_arm(field, source, row, ctx),
        // v4.10: subquery nodes should have been resolved into
        // Literal / InList nodes by Engine::resolve_select_subqueries
        // before the row loop. Anything reaching here is a bug.
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. } => Err(EvalError::TypeMismatch {
            detail: "subquery reached row eval — engine resolver bug".into(),
        }),
        // v7.30.2 (mailrs round-25) — flat `expr [NOT] IN (a, b, …)`.
        // Iterative scan with PG three-valued logic: TRUE on the first
        // Eq match; if nothing matched, NULL when the needle is NULL or
        // any comparison was NULL; FALSE otherwise. Empty list (only
        // reachable via an empty subquery result) is FALSE / TRUE even
        // for a NULL needle — no comparison ever happens.
        Expr::InList {
            expr,
            list,
            negated,
        } => eval_in_list_arm(expr, list, *negated, row, ctx),
        // v4.12: window functions should have been rewritten into
        // synthetic __win_N column references by
        // exec_select_with_window before row eval. Anything
        // reaching here is similarly a bug.
        Expr::WindowFunction { .. } => Err(EvalError::TypeMismatch {
            detail: "window function reached row eval — engine rewrite bug".into(),
        }),
        // v7.10.10 — `ARRAY[expr, expr, …]` constructor.
        // v7.11.13 — element-type detection: all integers →
        // IntArray (or BigIntArray when widening), any Text →
        // TextArray. Non-TEXT non-integer elements (Bool, Float)
        // stringify into TextArray as the safe default.
        Expr::Array(items) => eval_array_arm(items, row, ctx),
        // v7.10.12 — `arr[i]` PG-style 1-based indexing.
        // Out-of-range indices (including i ≤ 0) return NULL.
        Expr::ArraySubscript { .. } => eval_array_subscript_arm(expr, row, ctx),
        // Array slice `arr[lo:hi]` — PG 1-based, both ends
        // inclusive, out-of-range bounds clamp, missing bounds
        // extend to the array's ends. Result keeps the element
        // type; an empty window yields an empty array.
        Expr::ArraySlice { target, lo, hi } => eval_array_slice_arm(target, lo, hi, row, ctx),
        // v7.10.12 — `x op ANY(arr)` / `x op ALL(arr)`. PG
        // 3VL: ANY → true if any element compares-true; NULL if
        // no true but some NULL; false otherwise. ALL: false if
        // any compares-false; NULL if no false but some NULL;
        // true otherwise.
        Expr::AnyAll {
            expr,
            op,
            array,
            is_any,
        } => eval_any_all_arm(expr, op, array, *is_any, row, ctx),
        // v7.13.0 — CASE WHEN … END (mailrs round-5 G9).
        // Short-circuit on the first matching branch. Searched form
        // (operand=None) treats each branch's WHEN as a Bool
        // predicate. Simple form (operand=Some) compares with =.
        // ELSE on no match; NULL if no ELSE.
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => eval_case_arm(operand, branches, else_branch, row, ctx),
    }
}

/// v7.10.10 — best-effort text rendering for non-TEXT array
/// elements (numbers, bools, etc.). The PG rule is that
/// `ARRAY[1, 2]` is `int[]`, but SPG's v7.10 only models TEXT[],
/// so we widen by stringifying. NUMERIC formatting goes through
/// the existing canonical helpers to stay consistent with
/// `format_numeric` / `format_date` etc.
/// v7.37 D.1 — the COALESCE result-type hint: a sibling branch's explicit
/// cast target (`NULL::time`, `col::time`), unless it is Text (Text carries no
/// coercion). Returns the first non-Text `CastTarget` found, mirroring PG's
/// left-to-right common-type resolution for the common single-typed-branch case.
fn coalesce_type_hint(e: &Expr) -> Option<CastTarget> {
    match e {
        Expr::Cast { target, .. } if !matches!(target, CastTarget::Text) => Some(target.clone()),
        _ => None,
    }
}

/// v7.38 (read01) — widen `v` to the already-resolved common type `common`
/// of a `CASE`/`COALESCE`/`GREATEST`/`LEAST`/`NULLIF` result, so the value's
/// type matches the one PG reports and downstream operators (e.g. `/`) see
/// the widened type (integer division vs numeric division). Only widens;
/// anything already at the common type, or that fails to coerce, is returned
/// untouched (this must never turn a working expression into an error).
/// NUMERIC is scale-preserving: an existing exact-numeric keeps its own scale
/// (PG renders `COALESCE(1.50, 2)` as `1.50`); only integers promote, to
/// scale 0.
pub(crate) fn widen_value_to(v: Value<'static>, common: spg_storage::DataType) -> Value<'static> {
    use spg_storage::DataType as DT;
    // Only widen numeric- and temporal-category results: these are the ones
    // whose type actually changes a downstream value (integer vs numeric
    // division, date vs timestamp). Widening a string result (varchar ∪ text)
    // would only relabel the type while risking a spurious length-limit error
    // when coercing into a modelled-length varchar/char, so leave it as-is.
    if !matches!(
        common,
        DT::SmallInt
            | DT::Int
            | DT::BigInt
            | DT::Numeric { .. }
            // v7.39 (round 649) — `real` was absent here too, so even once
            // `common_type` learned to rank it, the value was handed back
            // unwidened: `coalesce(1::int, 1::real)` stayed integer where
            // PG says real. Two lists, one ladder — the gap had to be
            // closed in both.
            | DT::Real
            | DT::Float
            | DT::Date
            | DT::Time
            | DT::Timestamp
            | DT::Timestamptz
    ) {
        return v;
    }
    if matches!(v, Value::Null) {
        return v;
    }
    if v.data_type() == Some(common) {
        return v;
    }
    if matches!(common, spg_storage::DataType::Numeric { .. })
        && matches!(v, Value::Numeric { .. } | Value::NumericBig(_))
    {
        return v;
    }
    let target = if matches!(common, spg_storage::DataType::Numeric { .. }) {
        spg_storage::DataType::Numeric {
            precision: 0,
            scale: 0,
        }
    } else {
        common
    };
    match crate::conversions::coerce_value(v.clone(), target, "", 0) {
        Ok(cv) => cv,
        Err(_) => v,
    }
}

/// Widen `v` to the PG common type of the sibling `types`, or leave it as-is
/// when the types don't resolve to a single widening type. See
/// [`widen_value_to`] and [`crate::describe::common_type`].
pub(crate) fn widen_to_common(
    v: Value<'static>,
    types: &[spg_storage::DataType],
) -> Value<'static> {
    match crate::describe::common_type(types) {
        Some(common) => widen_value_to(v, common),
        None => v,
    }
}

pub(crate) fn value_to_text_for_array(v: &Value, style: &format::RenderStyle) -> String {
    match v {
        Value::Text(s) | Value::Json(s) => s.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::SmallInt(n) => n.to_string(),
        // PG renders booleans in array external form as `t` / `f`
        // (the bool type's output function), not `true` / `false`.
        Value::Bool(b) => {
            if *b {
                "t".into()
            } else {
                "f".into()
            }
        }
        Value::Float(x) => format::format_float_styled(*x, style),
        Value::Real(x) => format::format_real_styled(*x, style),
        Value::Date(d) => format::format_date_styled(*d, style),
        Value::Timestamp(t) => format::format_timestamp_styled(*t, style),
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => format_numeric_kind(*kind, *scaled, *scale),
        // v7.39 — everything else renders its canonical PG text (this
        // Debug fallback is how `ARRAY['\xff'::bytea]` printed
        // `{Bytes([255])}` on the wire).
        _ => values::value_to_text_styled(v, style),
    }
}

/// SQL `LIKE` matcher. Wildcards are `%` (any run, possibly empty) and `_`
/// (exactly one char). `\` escapes the next pattern char so `\%` matches a
/// literal `%`. Matches the whole input — no implicit anchoring needed
/// since SQL `LIKE` is always full-string. Errs on a trailing unpaired
/// escape the matcher actually reaches with text left (PG's lazy 22025).
fn like_match(text: &str, pattern: &str) -> Result<bool, EvalError> {
    let pat: Vec<char> = pattern.chars().collect();
    like_match_str(text, &pat, 0)
}

/// v7.37.16 — pg_typeof spelling for a STATIC column type (the
/// NULL-cell fallback; the value-level table is `pg_typeof_name`).
/// TEXT maps to None because a NULL literal describes as TEXT — the
/// unknown stand-in — and pg_typeof(NULL) must stay "unknown"; every
/// type not listed also returns None (caller keeps the value answer).
pub(crate) fn pg_typeof_name_for_datatype(t: spg_storage::DataType) -> Option<&'static str> {
    use spg_storage::DataType as D;
    Some(match t {
        D::SmallInt => "smallint",
        D::Int => "integer",
        D::BigInt => "bigint",
        D::Float => "double precision",
        D::Real => "real",
        D::Numeric { .. } => "numeric",
        D::Bool => "boolean",
        D::Date => "date",
        D::Time => "time without time zone",
        D::Timestamp => "timestamp without time zone",
        D::Timestamptz => "timestamp with time zone",
        D::Name => "name",
        D::Xid => "xid",
        D::Xid8 => "xid8",
        D::Oid => "oid",
        // v7.39 (round 694) — and its array, for the same reason.
        D::OidArray => "oid[]",
        D::Uuid => "uuid",
        D::Interval => "interval",
        // v7.39 (round 871) — the rest of what a NULL cast can be
        // annotated with. This table decided which types survived
        // `pg_typeof(NULL::t)`: the twenty above answered, everything
        // else fell to `_ => None` and reported `unknown`. That reads
        // as a NULL problem and is not one — `NULL::uuid` was right all
        // along while `NULL::text` was wrong, because one was listed
        // and the other was not.
        //
        // Names are PG18's own, taken from running `pg_typeof` there
        // rather than from memory: `bit varying` not `varbit`, `bit`
        // for any width, `character varying`, `"char"` quoted.
        D::Text => "text",
        D::Multirange(k) => match k {
            spg_storage::RangeKind::Int4 => "int4multirange",
            spg_storage::RangeKind::Int8 => "int8multirange",
            spg_storage::RangeKind::Num => "nummultirange",
            spg_storage::RangeKind::Ts => "tsmultirange",
            spg_storage::RangeKind::TsTz => "tstzmultirange",
            spg_storage::RangeKind::Date => "datemultirange",
        },
        D::Varchar(_) => "character varying",
        // PG names `char(n)` "character"; the one-byte internal type
        // spelled `"char"` is a DIFFERENT type there, and SPG maps both
        // onto `Char(u32)` — so this arm must answer for the declared
        // one. Round 871's first attempt said `"char"` here and would
        // have reported `char(5)` as the internal type.
        D::Char(_) => "character",
        D::Json => "json",
        D::Jsonb => "jsonb",
        D::Bytes => "bytea",
        D::Inet => "inet",
        D::Cidr => "cidr",
        D::Macaddr => "macaddr",
        D::Macaddr8 => "macaddr8",
        D::Bit(_) => "bit",
        D::BitVarying(_) => "bit varying",
        D::Xml => "xml",
        D::Money => "money",
        D::Point => "point",
        D::Lseg => "lseg",
        D::Path => "path",
        D::PgBox => "box",
        D::Polygon => "polygon",
        D::Line => "line",
        D::Circle => "circle",
        D::TextArray => "text[]",
        D::IntArray => "integer[]",
        D::BigIntArray => "bigint[]",
        D::SmallIntArray => "smallint[]",
        D::FloatArray => "double precision[]",
        D::NumericArray => "numeric[]",
        D::BoolArray => "boolean[]",
        D::DateArray => "date[]",
        D::TimestampArray => "timestamp without time zone[]",
        D::TimestamptzArray => "timestamp with time zone[]",
        D::UuidArray => "uuid[]",
        D::JsonArray => "json[]",
        D::JsonbArray => "jsonb[]",
        D::BytesArray => "bytea[]",
        D::VarcharArray => "character varying[]",
        D::CharArray => "\"char\"[]",
        D::IntervalArray => "interval[]",
        _ => return None,
    })
}

/// v7.37.16 — zero-allocation LIKE core: the text side walks a `&str`
/// cursor (char-semantic — `_` consumes one CHARACTER, `%` backtracks
/// only at char boundaries) instead of collecting a `Vec<char>` per
/// call. The old per-row collect was ~50 ns/row of pure allocator
/// traffic on a 50 k-row `WHERE s LIKE '%…%'` scan (the heavy.rs
/// like_filter 2.8× loss); the pattern side stays a compile-once
/// `&[char]` (see `Step::Like`).
pub(crate) fn like_match_str(text: &str, pat: &[char], mut pi: usize) -> Result<bool, EvalError> {
    let mut t = text;
    while pi < pat.len() {
        match pat[pi] {
            '%' => {
                // Collapse consecutive `%` and try every possible split.
                while pi < pat.len() && pat[pi] == '%' {
                    pi += 1;
                }
                if pi == pat.len() {
                    return Ok(true);
                }
                let mut rest = t;
                loop {
                    if like_match_str(rest, pat, pi)? {
                        return Ok(true);
                    }
                    match rest.chars().next() {
                        Some(c) => rest = &rest[c.len_utf8()..],
                        None => return Ok(false),
                    }
                }
            }
            '_' => match t.chars().next() {
                Some(c) => {
                    t = &t[c.len_utf8()..];
                    pi += 1;
                }
                None => return Ok(false),
            },
            // v7.39 (round 144, like_match.c) — a trailing unpaired escape is
            // PG's 22025 error, but LAZILY: only when the matcher reaches it
            // with text left. A branch where the text is already exhausted
            // returns false without ever "seeing" the trailing escape
            // ('x' LIKE 'x\' is false; 'xy' LIKE 'x\' errors).
            '\\' if pi + 1 >= pat.len() => {
                if t.is_empty() {
                    return Ok(false);
                }
                return Err(EvalError::TypeMismatch {
                    detail: "LIKE pattern must not end with escape character".into(),
                });
            }
            '\\' => {
                let want = pat[pi + 1];
                match t.chars().next() {
                    Some(c) if c == want => {
                        t = &t[c.len_utf8()..];
                        pi += 2;
                    }
                    _ => return Ok(false),
                }
            }
            c => match t.chars().next() {
                Some(tc) if tc == c => {
                    t = &t[c.len_utf8()..];
                    pi += 1;
                }
                _ => return Ok(false),
            },
        }
    }
    Ok(t.is_empty())
}

/// v7.24 (round-15) — `string_to_array(text, delimiter)`.
fn fn_string_to_array(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    // v7.37.17 (17.6 siblings) — the 3-arg PG form adds
    // `null_string`: elements equal to it become SQL NULL.
    let (text_arg, delim_arg, null_arg) = match args {
        [t, d] => (t, d, None),
        [t, d, n] => (t, d, Some(n)),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "string_to_array expects 2 or 3 arguments, got {}",
                    args.len()
                ),
            });
        }
    };
    let null_string: Option<&str> = match null_arg {
        None | Some(Value::Null) => None,
        Some(Value::Text(s)) => Some(s.as_ref()),
        Some(other) => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "string_to_array null_string must be text, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            });
        }
    };
    let text = match text_arg {
        Value::Null => return Ok(Value::Null),
        Value::Text(t) => t,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "string_to_array expects text, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            });
        }
    };
    // PG (9.1+): empty input → empty array, regardless of delimiter.
    if text.is_empty() {
        return Ok(Value::TextArray(Vec::new()));
    }
    let nullify = |p: String| -> Option<String> {
        if null_string == Some(p.as_str()) {
            None
        } else {
            Some(p)
        }
    };
    let parts: Vec<Option<String>> = match delim_arg {
        // NULL delimiter → one element per character.
        Value::Null => text.chars().map(|c| nullify(c.to_string())).collect(),
        Value::Text(d) if d.is_empty() => alloc::vec![nullify(text.to_string())],
        Value::Text(d) => text
            .split(d.as_ref())
            .map(|p| nullify(p.to_string()))
            .collect(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "string_to_array delimiter must be text, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            });
        }
    };
    Ok(Value::TextArray(parts))
}

/// v6.4.3 — `error_on_null(v)`. Returns `v` unchanged if non-NULL;
/// errors otherwise. Convenience to assert NOT NULL inside an
/// expression without wrapping it in COALESCE + raise hacks.
fn error_on_null(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 1 {
        return Err(EvalError::TypeMismatch {
            detail: format!("error_on_null() takes 1 arg, got {}", args.len()),
        });
    }
    if matches!(args[0], Value::Null) {
        return Err(EvalError::TypeMismatch {
            detail: "error_on_null(): argument is NULL".into(),
        });
    }
    Ok(args[0].clone().into_owned())
}

/// Helper: coerce a Value to an Option<String> for regex args. NULL
/// propagates as None (caller short-circuits to Value::Null).
fn text_arg(v: &Value) -> Result<Option<String>, EvalError> {
    match v {
        Value::Text(s) => Ok(Some(s.to_string())),
        Value::Null => Ok(None),
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "regex function expects TEXT arg, got {}",
                crate::conversions::pg_type_name_for_error_opt(other.data_type())
            ),
        }),
    }
}

// Month-name tables shared by the date formatters in `eval::strings`
// (`date_format_mysql`) and `eval::datetime` via `use super::`. Kept in
// `eval.rs` alongside `civil_from_days` so the calendar primitives live
// in one place.
const MONTH_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Howard Hinnant's `civil_from_days` — converts days since the Unix
/// epoch back to a proleptic-Gregorian (year, month, day) triple. Stays
/// in `eval.rs` (shared with the date SQL functions here and with
/// `eval::strings`); the inverse `days_from_civil` lives in
/// `eval::format`. Both keep the engine off `std` time facilities.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn civil_from_days(days: i32) -> (i32, u32, u32) {
    let z = i64::from(days) + 719_468;
    let era = z.div_euclid(146_097);
    // doe ∈ [0, 146_097); fits in u32 with room to spare. Same for
    // every other quantity below — `as u32` truncations are safe by
    // construction.
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe.saturating_sub(doe / 1460) + doe / 36524 - doe / 146_096) / 365;
    let y_base = i64::from(yoe) + era * 400;
    let doy = doe.saturating_sub(365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy.saturating_sub((153 * mp + 2) / 5) + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y_base + 1 } else { y_base };
    (y as i32, m, d)
}

/// Add `months` (signed) to a `(year, month, day)` triple using PG's
/// clamp-to-last-day rule (so `'2024-01-31' + 1 month` → `'2024-02-29'`).
fn add_months_to_civil(y: i32, m: u32, d: u32, months: i32) -> (i32, u32, u32) {
    let total_months = i64::from(y) * 12 + i64::from(m) - 1 + i64::from(months);
    let new_year = i32::try_from(total_months.div_euclid(12)).unwrap_or(i32::MAX);
    let new_month_zero = total_months.rem_euclid(12);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let new_month = (new_month_zero as u32) + 1;
    let max_day = days_in_month(new_year, new_month);
    (new_year, new_month, d.min(max_day))
}

const fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        2 => {
            // Proleptic Gregorian leap rule.
            if y.rem_euclid(4) == 0 && (y.rem_euclid(100) != 0 || y.rem_euclid(400) == 0) {
                29
            } else {
                28
            }
        }
        // 4 / 6 / 9 / 11 plus any out-of-range month (callers normalise
        // first, but be defensive) get the 30-day fallback.
        _ => 30,
    }
}

pub(crate) fn literal_to_value(l: &Literal) -> Value<'static> {
    match l {
        Literal::Integer(n) => {
            if let Ok(small) = i32::try_from(*n) {
                Value::Int(small)
            } else {
                Value::BigInt(*n)
            }
        }
        Literal::Float(x) => Value::Float(*x),
        Literal::Numeric { unscaled, scale } => Value::Numeric {
            scaled: *unscaled,
            scale: *scale,
            kind: spg_storage::NumericKind::Finite,
        },
        Literal::NumericBig(s) => crate::conversions::big_literal_to_value(s),
        // v7.38.8 — already decoded, so the row loop neither clones a
        // string nor coerces one back into a timestamp.
        Literal::Timestamp { micros, .. } => Value::Timestamp(*micros),
        Literal::Date { days, .. } => Value::Date(*days),
        Literal::String(s) => Value::text(s.clone()),
        Literal::Vector(v) => Value::vector(v.clone()),
        Literal::TextArray(items) => Value::TextArray(items.clone()),
        Literal::IntArray(items) => Value::IntArray(items.clone()),
        Literal::BigIntArray(items) => Value::BigIntArray(items.clone()),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
        Literal::Interval {
            months,
            days,
            micros,
            ..
        } => Value::Interval {
            months: *months,
            days: *days,
            micros: *micros,
        },
    }
}

impl crate::Engine {
    /// v7.39 (read01 round 63) — run a user function whose body has its own
    /// FROM. The arguments are substituted into the body as literals and the
    /// SELECT goes through the REAL executor, so it sees exactly the rows a
    /// hand-written query would: the row-header visibility filter applies, and
    /// under in-place MVCC a dead row stays dead.
    ///
    /// PG returns the FIRST row of a scalar SQL function's body (and NULL when
    /// it returns none).
    pub(crate) fn run_user_fn_query(
        &self,
        def: &spg_storage::FunctionDef,
        stmt: &spg_sql::ast::SelectStatement,
        arg_names: &[alloc::string::String],
        args: &spg_storage::Row<'static>,
        fn_depth: u16,
    ) -> Result<Value<'static>, EvalError> {
        const MAX_QUERY_FN_DEPTH: u16 = 8;
        if fn_depth >= MAX_QUERY_FN_DEPTH {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "function {:?}: a body with its own FROM may nest at most {MAX_QUERY_FN_DEPTH} deep",
                    def.name
                ),
            });
        }
        // Bind the arguments — the same helper the set-returning path uses, so
        // both resolve an argument identically (a COLUMN of the body's own FROM
        // shadows a same-named argument, as in PG).
        let owned: alloc::vec::Vec<Value<'static>> =
            args.values.iter().map(|v| v.clone().into_owned()).collect();
        let bound =
            bind_user_fn_args(self.active_catalog(), stmt, arg_names, &owned).map_err(|e| {
                EvalError::TypeMismatch {
                    detail: alloc::format!("function {:?}: {e}", def.name),
                }
            })?;

        // v7.39 (round 334, V55) — a SECURITY DEFINER body is authorised as
        // the function's OWNER. Measured on PG 18.4: a definer function
        // owned by `owner55` counts rows of a table `caller55` cannot read,
        // while the SECURITY INVOKER sibling is refused.
        let as_role = def.security_definer.then(|| def.owner.as_deref()).flatten();
        let out = self
            .exec_select_cancel_as(&bound, crate::CancelToken::none(), as_role)
            .map_err(|e| EvalError::TypeMismatch {
                detail: alloc::format!("function {:?}: {e}", def.name),
            })?;
        let crate::QueryResult::Rows { rows, .. } = out else {
            return Ok(Value::Null);
        };
        let Some(first) = rows.first() else {
            // No row: PG's scalar SQL function returns NULL.
            return Ok(Value::Null);
        };
        let v = first.values.first().cloned().unwrap_or(Value::Null);
        let declared = def.returns.trim();
        if declared.eq_ignore_ascii_case("VOID") {
            return Ok(Value::Null);
        }
        crate::eval::cast::cast_value(v.into_owned(), declared_return_cast_target(declared))
            .or_else(|_| Ok(Value::Null))
    }
}

/// v7.39 (read01 round 65) — bind a call's arguments into a function body's
/// SELECT, as literals. Shared by the scalar path (round 63) and the
/// set-returning one, so both resolve an argument the same way — including the
/// rule that a COLUMN of the body's own FROM shadows a same-named argument.
pub(crate) fn bind_user_fn_args(
    cat: &spg_storage::Catalog,
    stmt: &spg_sql::ast::SelectStatement,
    arg_names: &[alloc::string::String],
    args: &[Value<'static>],
) -> Result<spg_sql::ast::SelectStatement, EvalError> {
    let mut bound = stmt.clone();
    let mut binds: alloc::collections::BTreeMap<alloc::string::String, spg_sql::ast::Expr> =
        alloc::collections::BTreeMap::new();
    for (i, name) in arg_names.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        let shadowed = body_from_tables(stmt).iter().any(|t| {
            cat.get(t).is_some_and(|tb| {
                tb.schema()
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(name))
            })
        });
        if shadowed {
            continue;
        }
        let v = args.get(i).cloned().unwrap_or(Value::Null);
        let lit =
            crate::substitute::value_to_literal_expr(v).map_err(|e| EvalError::TypeMismatch {
                detail: alloc::format!("argument {name} cannot be bound into the body: {e}"),
            })?;
        binds.insert(name.to_ascii_lowercase(), lit);
    }
    substitute_arg_refs_in_select(&mut bound, &binds);
    Ok(bound)
}

/// The base tables a function body's FROM names — used to decide whether an
/// argument name is shadowed by a column of the same name.
fn body_from_tables(
    stmt: &spg_sql::ast::SelectStatement,
) -> alloc::vec::Vec<alloc::string::String> {
    let mut out = alloc::vec::Vec::new();
    if let Some(from) = &stmt.from {
        out.push(from.primary.name.clone());
        for j in &from.joins {
            out.push(j.table.name.clone());
        }
    }
    out
}

/// Replace every bare reference to an argument name with its literal value.
fn substitute_arg_refs_in_select(
    stmt: &mut spg_sql::ast::SelectStatement,
    binds: &alloc::collections::BTreeMap<alloc::string::String, spg_sql::ast::Expr>,
) {
    use spg_sql::ast::{Expr, SelectItem};
    fn walk(e: &mut Expr, binds: &alloc::collections::BTreeMap<alloc::string::String, Expr>) {
        match e {
            Expr::Column(c) => {
                if c.qualifier.is_none()
                    && let Some(lit) = binds.get(&c.name.to_ascii_lowercase())
                {
                    *e = lit.clone();
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                walk(lhs, binds);
                walk(rhs, binds);
            }
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => walk(expr, binds),
            Expr::FunctionCall { args, .. } => args.iter_mut().for_each(|a| walk(a, binds)),
            Expr::Case {
                operand,
                branches,
                else_branch,
            } => {
                if let Some(o) = operand {
                    walk(o, binds);
                }
                for (c, v) in branches.iter_mut() {
                    walk(c, binds);
                    walk(v, binds);
                }
                if let Some(x) = else_branch {
                    walk(x, binds);
                }
            }
            Expr::InList { expr, list, .. } => {
                walk(expr, binds);
                list.iter_mut().for_each(|it| walk(it, binds));
            }
            Expr::AnyAll { expr, array, .. } => {
                walk(expr, binds);
                walk(array, binds);
            }
            Expr::Array(items) => items.iter_mut().for_each(|it| walk(it, binds)),
            Expr::ArraySubscript { target, index } => {
                walk(target, binds);
                walk(index, binds);
            }
            _ => {}
        }
    }
    for item in &mut stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            walk(expr, binds);
        }
    }
    if let Some(w) = &mut stmt.where_ {
        walk(w, binds);
    }
    if let Some(h) = &mut stmt.having {
        walk(h, binds);
    }
    if let Some(gs) = &mut stmt.group_by {
        gs.iter_mut().for_each(|g| walk(g, binds));
    }
    for o in &mut stmt.order_by {
        walk(&mut o.expr, binds);
    }
    if let Some(from) = &mut stmt.from {
        for j in &mut from.joins {
            if let Some(on) = &mut j.on {
                walk(on, binds);
            }
        }
    }
    // v7.39 (read01 round 69) — the UNION peers. A body like
    // `SELECT k UNION ALL SELECT k * 10` has its second half in `unions`, and
    // leaving it unsubstituted made the argument look like a missing column.
    for (_, peer) in &mut stmt.unions {
        substitute_arg_refs_in_select(peer, binds);
    }
    for cte in &mut stmt.ctes {
        if let Some(s) = cte.body.as_select_mut() {
            substitute_arg_refs_in_select(s, binds);
        }
    }
}

/// v7.38.4 (sentori step 54) — the cast target for a function's DECLARED
/// return type.
///
/// `def.returns` holds the type as the user wrote it, so an array is
/// `bigint[]`; a `CastTarget::Named` spells the same type `bigint_array`.
/// The coercion therefore could not resolve `RETURNS bigint[]`, and the
/// `or_else(NULL)` under every call site turned "I could not coerce this"
/// into a NULL answer: the body computed `{1,2}` and the caller got
/// nothing, with no error anywhere. Their version keys compare through one
/// of these, so every version-targeted push reached zero devices while
/// reporting success.
///
/// Shared by both coercion sites — the pure-expression body and the one
/// with its own FROM — because they had the same line written twice and
/// fixing one would have left the other.
pub(crate) fn declared_return_cast_target(declared: &str) -> spg_sql::ast::CastTarget {
    let name = declared.trim().strip_suffix("[]").map_or_else(
        || alloc::string::String::from(declared.trim()),
        |base| alloc::format!("{}_array", base.trim()),
    );
    spg_sql::ast::CastTarget::Named(name)
}

impl crate::Engine {
    /// v7.39 (read01 round 64) — call a plpgsql function as a scalar. The body
    /// runs on the interpreter the DO block and triggers already use, with the
    /// arguments bound as locals; its `SELECT … INTO` and `FOR … IN SELECT`
    /// resolvers go through the READ path, so what the body sees is what a
    /// hand-written query would see (visibility filter and all).
    ///
    /// A body that writes is refused — the call arrives through expression
    /// evaluation, which holds the engine immutably. Refusing is the honest
    /// answer; silently dropping the write would be the worst one.
    pub(crate) fn call_plpgsql_scalar_fn(
        &self,
        def: &spg_storage::FunctionDef,
        arg_names: &[alloc::string::String],
        args: &spg_storage::Row<'static>,
    ) -> Result<Value<'static>, EvalError> {
        let block =
            spg_sql::parse_function_body(def.body.trim()).map_err(|e| EvalError::TypeMismatch {
                detail: alloc::format!("function {:?} body does not parse: {e}", def.name),
            })?;
        let mut locals: alloc::collections::BTreeMap<alloc::string::String, Value<'static>> =
            alloc::collections::BTreeMap::new();
        for (i, n) in arg_names.iter().enumerate() {
            if n.is_empty() {
                continue;
            }
            locals.insert(
                n.to_ascii_lowercase(),
                args.values.get(i).cloned().unwrap_or(Value::Null),
            );
        }
        let dts = self
            .session_param("default_text_search_config")
            .map(alloc::string::String::from);

        let select_into = |stmt: &spg_sql::ast::Statement| -> Result<
            Value<'static>,
            crate::triggers::TriggerError,
        > {
            let spg_sql::ast::Statement::Select(s) = stmt else {
                return Err(crate::triggers::TriggerError::EvalFailed {
                    function: def.name.clone(),
                    cause: EvalError::TypeMismatch {
                        detail: "SELECT … INTO body must be a SELECT".into(),
                    },
                });
            };
            let r = self
                .exec_select_cancel(s, crate::CancelToken::none())
                .map_err(|e| crate::triggers::TriggerError::EvalFailed {
                    function: def.name.clone(),
                    cause: EvalError::TypeMismatch {
                        detail: alloc::format!("SELECT … INTO failed: {e}"),
                    },
                })?;
            match r {
                crate::QueryResult::Rows { rows, .. } => Ok(rows
                    .into_iter()
                    .next()
                    .and_then(|row| row.values.into_iter().next())
                    .unwrap_or(Value::Null)),
                _ => Ok(Value::Null),
            }
        };
        let for_query = |stmt: &spg_sql::ast::Statement| -> Result<
            (
                alloc::vec::Vec<alloc::string::String>,
                alloc::vec::Vec<alloc::vec::Vec<Value<'static>>>,
            ),
            crate::triggers::TriggerError,
        > {
            let spg_sql::ast::Statement::Select(s) = stmt else {
                return Err(crate::triggers::TriggerError::EvalFailed {
                    function: def.name.clone(),
                    cause: EvalError::TypeMismatch {
                        detail: "FOR … IN body must be a SELECT".into(),
                    },
                });
            };
            let r = self
                .exec_select_cancel(s, crate::CancelToken::none())
                .map_err(|e| crate::triggers::TriggerError::EvalFailed {
                    function: def.name.clone(),
                    cause: EvalError::TypeMismatch {
                        detail: alloc::format!("FOR … IN SELECT failed: {e}"),
                    },
                })?;
            match r {
                crate::QueryResult::Rows { columns, rows } => Ok((
                    columns.iter().map(|c| c.name.clone()).collect(),
                    rows.into_iter().map(|row| row.values).collect(),
                )),
                _ => Ok((alloc::vec::Vec::new(), alloc::vec::Vec::new())),
            }
        };

        let out = crate::triggers::call_plpgsql_scalar(
            &def.name,
            &block,
            locals,
            dts.as_deref(),
            Some(&select_into),
            Some(&for_query),
            // A scalar call has no set to build; RETURN NEXT / RETURN QUERY are
            // errors here, as in PG.
            // (second None below: the read path holds the engine immutably, so
            // RAISE messages have nowhere session-bound to go — B3 residual.)
            None,
            None,
        )
        .map_err(|e| EvalError::TypeMismatch {
            detail: alloc::format!("{e}"),
        })?;
        let declared = def.returns.trim();
        let Some(v) = out else {
            if declared.eq_ignore_ascii_case("VOID") {
                return Ok(Value::Null);
            }
            // PG: a non-void function that falls out of the bottom.
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "control reached end of function {:?} without RETURN",
                    def.name
                ),
            });
        };
        if declared.eq_ignore_ascii_case("VOID") {
            return Ok(Value::Null);
        }
        crate::eval::cast::cast_value(
            v.into_owned(),
            spg_sql::ast::CastTarget::Named(alloc::string::String::from(declared)),
        )
        .or_else(|_| Ok(Value::Null))
    }
}

impl crate::Engine {
    /// v7.39 (read01 round 66) — run a `RETURNS SETOF` plpgsql function and
    /// collect the rows `RETURN NEXT` / `RETURN QUERY` appended. Same
    /// interpreter, same read-path resolvers as the scalar call — the only
    /// difference is that a SINK is provided, which is what makes those two
    /// statements legal.
    pub(crate) fn call_plpgsql_setof_fn(
        &self,
        def: &spg_storage::FunctionDef,
        arg_names: &[alloc::string::String],
        args: &[Value<'static>],
    ) -> Result<alloc::vec::Vec<alloc::vec::Vec<Value<'static>>>, EvalError> {
        let block =
            spg_sql::parse_function_body(def.body.trim()).map_err(|e| EvalError::TypeMismatch {
                detail: alloc::format!("function {:?} body does not parse: {e}", def.name),
            })?;
        let mut locals: alloc::collections::BTreeMap<alloc::string::String, Value<'static>> =
            alloc::collections::BTreeMap::new();
        for (i, n) in arg_names.iter().enumerate() {
            if n.is_empty() {
                continue;
            }
            locals.insert(
                n.to_ascii_lowercase(),
                args.get(i).cloned().unwrap_or(Value::Null),
            );
        }
        let dts = self
            .session_param("default_text_search_config")
            .map(alloc::string::String::from);
        let run_select = |stmt: &spg_sql::ast::Statement,
                          what: &str|
         -> Result<crate::QueryResult, crate::triggers::TriggerError> {
            let spg_sql::ast::Statement::Select(s) = stmt else {
                return Err(crate::triggers::TriggerError::EvalFailed {
                    function: def.name.clone(),
                    cause: EvalError::TypeMismatch {
                        detail: alloc::format!("{what} body must be a SELECT"),
                    },
                });
            };
            self.exec_select_cancel(s, crate::CancelToken::none())
                .map_err(|e| crate::triggers::TriggerError::EvalFailed {
                    function: def.name.clone(),
                    cause: EvalError::TypeMismatch {
                        detail: alloc::format!("{what} failed: {e}"),
                    },
                })
        };
        let select_into = |stmt: &spg_sql::ast::Statement| -> Result<
            Value<'static>,
            crate::triggers::TriggerError,
        > {
            match run_select(stmt, "SELECT … INTO")? {
                crate::QueryResult::Rows { rows, .. } => Ok(rows
                    .into_iter()
                    .next()
                    .and_then(|r| r.values.into_iter().next())
                    .unwrap_or(Value::Null)),
                _ => Ok(Value::Null),
            }
        };
        let for_query = |stmt: &spg_sql::ast::Statement| -> Result<
            (
                alloc::vec::Vec<alloc::string::String>,
                alloc::vec::Vec<alloc::vec::Vec<Value<'static>>>,
            ),
            crate::triggers::TriggerError,
        > {
            match run_select(stmt, "FOR … IN / RETURN QUERY")? {
                crate::QueryResult::Rows { columns, rows } => Ok((
                    columns.iter().map(|c| c.name.clone()).collect(),
                    rows.into_iter().map(|r| r.values).collect(),
                )),
                _ => Ok((alloc::vec::Vec::new(), alloc::vec::Vec::new())),
            }
        };
        let sink: core::cell::RefCell<alloc::vec::Vec<alloc::vec::Vec<Value<'static>>>> =
            core::cell::RefCell::new(alloc::vec::Vec::new());
        crate::triggers::call_plpgsql_scalar(
            &def.name,
            &block,
            locals,
            dts.as_deref(),
            Some(&select_into),
            Some(&for_query),
            Some(&sink),
            // Read path — immutable engine borrow; B3 residual.
            None,
        )
        .map_err(|e| EvalError::TypeMismatch {
            detail: alloc::format!("{e}"),
        })?;
        Ok(sink.into_inner())
    }
}

/// v7.39 (round 236) — resolve an ARRAY constructor's element types the way
/// PG does. Typed elements must share a type category; a bare string or NULL
/// literal is untyped and converts to whatever the typed elements resolved
/// to, reporting the value (not a type mismatch) when it will not convert.
fn unify_array_elements(
    items: &[Expr],
    materialised: &mut [Value<'static>],
) -> Result<(), EvalError> {
    unify_construct_values("ARRAY", items, materialised)
}

/// v7.39 (round 238) — an `IN (...)` list must be comparable with its
/// needle. Reports the operator the way PG does
/// ("operator does not exist: integer = text"), and — like round 237 —
/// judges only the operands whose type is genuinely known.
fn require_in_list_comparable(
    needle: &Expr,
    list: &[Expr],
    ctx: &EvalContext<'_>,
) -> Result<(), EvalError> {
    let known_ty = |e: &Expr| {
        matches!(e, Expr::Cast { .. } | Expr::Literal(_) | Expr::Column(_))
            .then(|| crate::describe::describe_expr(e, ctx.columns).map(|s| s.ty))
            .flatten()
    };
    // An untyped literal adopts the needle's type, so it never conflicts.
    //
    // v7.39 (round 652) — and so does a cast to one of the reg types.
    // `'pg_class'::regclass` carries an oid AND a name, which is why
    // `compare` has arms for both, but it DESCRIBES as text — SPG has no
    // `DataType` for it. This check ran before any comparison and
    // refused `WHERE oid IN ('pg_class'::regclass, …)` as `bigint =
    // text`, while the identical `oid = 'pg_class'::regclass` worked.
    // That shape is how pg_dump, ORMs and monitoring queries name a
    // handful of relations at once, so it is not a corner.
    let reg_cast = |e: &Expr| {
        match e {
            Expr::Cast { target, .. } => match target {
                spg_sql::ast::CastTarget::RegClass | spg_sql::ast::CastTarget::RegType => true,
                // `regproc` and `regnamespace` have no variant of their
                // own; they arrive through the generic named path.
                spg_sql::ast::CastTarget::Named(n) => {
                    n.eq_ignore_ascii_case("regproc")
                        || n.eq_ignore_ascii_case("regnamespace")
                        || n.eq_ignore_ascii_case("regtype")
                        || n.eq_ignore_ascii_case("regclass")
                }
                _ => false,
            },
            _ => false,
        }
    };
    let untyped = |e: &Expr| {
        reg_cast(e)
            || matches!(
                e,
                Expr::Literal(spg_sql::ast::Literal::String(_))
                    | Expr::Literal(spg_sql::ast::Literal::Null)
            )
    };
    // The needle can be untyped too (`NULL IN (1,2)`): SPG has no `Unknown`
    // DataType, so a bare NULL describes as TEXT and would look like a text
    // needle conflicting with integer list items.
    if untyped(needle) {
        return Ok(());
    }
    let Some(nt) = known_ty(needle) else {
        return Ok(());
    };
    for item in list {
        if untyped(item) {
            continue;
        }
        let Some(it) = known_ty(item) else { continue };
        if !crate::conversions::types_unify(nt, it) {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "operator does not exist: {} = {}",
                    crate::conversions::pg_type_name_for_error(nt),
                    crate::conversions::pg_type_name_for_error(it),
                ),
            });
        }
    }
    Ok(())
}

/// v7.39 (round 237) — STATIC branch-type resolution for the constructs
/// whose branches must not all be evaluated: CASE runs only the branch it
/// takes, and a COALESCE / GREATEST argument may have side effects
/// (`COALESCE(nextval('s'), 1)`), so the check reads each branch's declared
/// type instead of its value. Same rule and wording as the value-driven
/// ARRAY path below; an untyped literal is converted here (a literal has no
/// side effects) so a value that will not convert is reported as PG does.
/// v7.39 (round 609) — takes anything that yields the branches, so the
/// COALESCE caller no longer builds a `Vec<&Expr>` of them for every row.
pub(crate) fn unify_branch_types_static<'e>(
    construct: &str,
    branches: impl IntoIterator<Item = &'e Expr> + Clone,
    ctx: &EvalContext<'_>,
) -> Result<(), EvalError> {
    use spg_storage::DataType;
    let untyped = |e: &Expr| {
        matches!(
            e,
            Expr::Literal(spg_sql::ast::Literal::String(_))
                | Expr::Literal(spg_sql::ast::Literal::Null)
        )
    };
    let mut resolved: Option<DataType> = None;
    for e in branches.clone() {
        if untyped(e) {
            continue;
        }
        // Only branches whose type is GENUINELY known take part. A general
        // `describe_expr` is a best-effort hint for wire type tags, not a
        // type checker: it reports a binary operator as its left operand's
        // type, so `payload->'a'` (jsonb in PG) came back as text and this
        // check refused a working `COALESCE(payload->'a', '{}'::jsonb)`.
        // Refusing a valid query is worse than missing an invalid one, so
        // the check confines itself to an explicit cast, a typed literal and
        // a plain column reference.
        let known = matches!(e, Expr::Cast { .. } | Expr::Literal(_) | Expr::Column(_));
        if !known {
            continue;
        }
        // 7.38.1 S5.1 — a reg* cast is an OID wearing a name: describe
        // says Text (the wire render), but it compares and unions with
        // numeric catalog columns (pg_dump: `SELECT classid … UNION
        // ALL SELECT 'pg_opfamily'::regclass …`). Its static claim is
        // not genuinely known here, so it sits the check out — the
        // dual RegClass value reconciles at runtime.
        if matches!(
            e,
            Expr::Cast {
                target: spg_sql::ast::CastTarget::RegType | spg_sql::ast::CastTarget::RegClass,
                ..
            }
        ) {
            continue;
        }
        let Some(ty) = crate::describe::describe_expr_type(e, ctx.columns) else {
            continue;
        };
        match resolved {
            None => resolved = Some(ty),
            Some(prev) if crate::conversions::types_unify(prev, ty) => {
                if matches!(prev, DataType::Int | DataType::SmallInt) {
                    resolved = Some(ty);
                }
            }
            Some(prev) => {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "{construct} types {} and {} cannot be matched",
                        crate::conversions::pg_type_name_for_error(prev),
                        crate::conversions::pg_type_name_for_error(ty),
                    ),
                });
            }
        }
    }
    let Some(target) = resolved else {
        return Ok(());
    };
    if matches!(target, DataType::Text) {
        return Ok(());
    }
    // v7.39 (round 398) — MySQL aggregates a mixed int/string CASE /
    // COALESCE to a string (`CASE WHEN 1 THEN 1 ELSE 'x' END` is '1', not an
    // error); PG requires the untyped string literals to coerce to the
    // resolved numeric type, so it refuses. Under the dialect, skip that
    // coercion check — the value is returned as-is / widened by the caller.
    if ctx.mysql_dialect {
        return Ok(());
    }
    for e in branches {
        if !untyped(e) {
            continue;
        }
        if let Expr::Literal(spg_sql::ast::Literal::String(lit)) = e {
            crate::conversions::coerce_value(Value::text(lit.clone()), target, "", 0).map_err(
                |err| match err {
                    crate::EngineError::Eval(ev) => ev,
                    other => EvalError::TypeMismatch {
                        detail: alloc::format!("{other}"),
                    },
                },
            )?;
        }
    }
    Ok(())
}

/// v7.39 (round 237) — the same resolution for every construct that builds
/// one value out of several branches: ARRAY, CASE, COALESCE, GREATEST and
/// LEAST. PG names the construct in the message ("CASE types text and
/// integer cannot be matched"), which is why the caller passes it in.
pub(crate) fn unify_construct_values(
    construct: &str,
    items: &[Expr],
    materialised: &mut [Value<'static>],
) -> Result<(), EvalError> {
    use spg_storage::DataType;
    let untyped = |e: &Expr| {
        matches!(
            e,
            Expr::Literal(spg_sql::ast::Literal::String(_))
                | Expr::Literal(spg_sql::ast::Literal::Null)
        )
    };
    // The type the typed elements agree on, if any.
    let mut resolved: Option<DataType> = None;
    for (i, v) in materialised.iter().enumerate() {
        if items.get(i).is_some_and(untyped) {
            continue;
        }
        let Some(ty) = v.data_type() else { continue };
        match resolved {
            None => resolved = Some(ty),
            Some(prev) if crate::conversions::types_unify(prev, ty) => {
                // Keep the wider of the two so the coercion below targets it.
                if matches!(prev, DataType::Int | DataType::SmallInt) {
                    resolved = Some(ty);
                }
            }
            Some(prev) => {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "{construct} types {} and {} cannot be matched",
                        crate::conversions::pg_type_name_for_error(prev),
                        crate::conversions::pg_type_name_for_error(ty),
                    ),
                });
            }
        }
    }
    // Untyped literals adopt that type; a failure names the value, as PG does.
    let Some(target) = resolved else {
        return Ok(());
    };
    if matches!(target, DataType::Text) {
        return Ok(());
    }
    for (i, v) in materialised.iter_mut().enumerate() {
        if !items.get(i).is_some_and(untyped) || matches!(v, Value::Null) {
            continue;
        }
        *v = crate::conversions::coerce_value(v.clone(), target, "", i).map_err(|e| match e {
            crate::EngineError::Eval(ev) => ev,
            other => EvalError::TypeMismatch {
                detail: alloc::format!("{other}"),
            },
        })?;
    }
    Ok(())
}

/// v7.39 (round 309, V30) — split `'<timestamp> <zone name>'` into the
/// wall-clock reading and the zone token, for the zone-less target types.
///
/// Returns `None` when the literal parses on its own (nothing to strip)
/// or when the trailing token is not zone-SHAPED — those keep the
/// ordinary "invalid input syntax" path, which is what PG answers for
/// `'2020-01-01 10:00:00 xyz'`. Whether a zone-shaped token is a REAL
/// zone is the caller's question; getting that wrong is a different
/// error in PG, and conflating the two would report a malformed literal
/// for a merely-misspelled zone.
///
/// Deliberately does not accept a bare time (`'10:00:00 America/New_York'`):
/// PG refuses a named zone there, and only reaches this spelling through
/// a full timestamp literal.
fn split_trailing_zone_name(txt: &str, order: format::DateOrder) -> Option<(i64, &str)> {
    // Already valid without help — leave it alone.
    if format::parse_timestamp_literal_wall_ordered(txt, order).is_some() {
        return None;
    }
    let trimmed = txt.trim_end();
    let idx = trimmed.rfind(' ')?;
    let (head, tail) = (trimmed[..idx].trim(), trimmed[idx + 1..].trim());
    // An era marker is part of the timestamp, not a zone.
    let zone_shaped = tail.len() > 1
        && tail.bytes().any(|b| b.is_ascii_alphabetic())
        && !tail.eq_ignore_ascii_case("bc")
        && !tail.eq_ignore_ascii_case("ad");
    if !zone_shaped {
        return None;
    }
    let wall = format::parse_timestamp_literal_wall_ordered(head, order)?;
    Some((wall, tail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use spg_sql::ast::UnOp;
    use spg_storage::{ColumnSchema, DataType, Row};

    fn col(name: &str, ty: DataType) -> ColumnSchema {
        ColumnSchema::new(name, ty, true)
    }

    fn ctx<'a>(cols: &'a [ColumnSchema], alias: Option<&'a str>) -> EvalContext<'a> {
        EvalContext::new(cols, alias)
    }

    /// v7.32 (P4 borrow channel) differential: the borrowed comparison
    /// fast path in `eval_expr`'s Binary arm must be byte-for-byte the
    /// pre-P4 owned path (`apply_binary` on cloned operands) across a
    /// cross-type value matrix and every comparison operator — covering
    /// the fast-path types (Text/Int/Float/Date/Timestamp/Bool/Null) and
    /// the owned-fallback types (Numeric/Interval).
    #[test]
    fn borrowed_compare_equals_owned_apply_binary() {
        let vals = vec![
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::SmallInt(3),
            Value::Int(3),
            Value::Int(-1),
            Value::BigInt(3),
            Value::BigInt(100),
            Value::Float(3.0),
            Value::Float(2.5),
            Value::text(String::new()),
            Value::text("a"),
            Value::text("b"),
            Value::Date(10),
            Value::Timestamp(1000),
            Value::Numeric {
                scaled: 30,
                scale: 1,
                kind: spg_storage::NumericKind::Finite,
            },
            Value::Interval {
                months: 0,
                days: 0,
                micros: 5,
            },
        ];
        let ops = [
            BinOp::Eq,
            BinOp::NotEq,
            BinOp::Lt,
            BinOp::LtEq,
            BinOp::Gt,
            BinOp::GtEq,
        ];
        let cs = vec![col("x", DataType::Int), col("y", DataType::Int)];
        let c = ctx(&cs, None);
        let lhs = Expr::Column(ColumnName {
            qualifier: None,
            name: "x".into(),
        });
        let rhs = Expr::Column(ColumnName {
            qualifier: None,
            name: "y".into(),
        });
        for l in &vals {
            for r in &vals {
                let row = Row::new(vec![l.clone(), r.clone()]);
                for op in ops {
                    let got = eval_expr(
                        &Expr::Binary {
                            lhs: alloc::boxed::Box::new(lhs.clone()),
                            op,
                            rhs: alloc::boxed::Box::new(rhs.clone()),
                        },
                        &row,
                        &c,
                    );
                    // Pre-P4 reference: owned operands through apply_binary
                    // (collation fold is a no-op for non-CI columns).
                    let want = apply_binary(op, l.clone(), r.clone());
                    assert_eq!(
                        format!("{got:?}"),
                        format!("{want:?}"),
                        "op={op:?} l={l:?} r={r:?}"
                    );
                }
            }
        }
    }

    fn lit(n: i64) -> Expr {
        Expr::Literal(Literal::Integer(n))
    }

    fn null() -> Expr {
        Expr::Literal(Literal::Null)
    }

    fn col_ref(name: &str) -> Expr {
        Expr::Column(ColumnName {
            qualifier: None,
            name: name.into(),
        })
    }

    #[test]
    fn literal_evaluates_to_value() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        assert_eq!(eval_expr(&lit(42), &r, &c).unwrap(), Value::Int(42));
        assert_eq!(
            eval_expr(&Expr::Literal(Literal::Float(1.5)), &r, &c).unwrap(),
            Value::Float(1.5)
        );
        assert_eq!(eval_expr(&null(), &r, &c).unwrap(), Value::Null);
    }

    #[test]
    fn column_lookup_unqualified() {
        let cs = vec![col("a", DataType::Int), col("b", DataType::Text)];
        let r = Row::new(vec![Value::Int(7), Value::text("hi")]);
        let c = ctx(&cs, None);
        assert_eq!(eval_expr(&col_ref("a"), &r, &c).unwrap(), Value::Int(7));
        assert_eq!(eval_expr(&col_ref("b"), &r, &c).unwrap(), Value::text("hi"));
    }

    #[test]
    fn column_not_found_errors() {
        let cs = vec![col("a", DataType::Int)];
        let r = Row::new(vec![Value::Int(0)]);
        let c = ctx(&cs, None);
        let err = eval_expr(&col_ref("ghost"), &r, &c).unwrap_err();
        assert!(matches!(err, EvalError::ColumnNotFound { ref name } if name == "ghost"));
    }

    #[test]
    fn qualified_column_matches_alias() {
        let cs = vec![col("a", DataType::Int)];
        let r = Row::new(vec![Value::Int(5)]);
        let c = ctx(&cs, Some("u"));
        let qualified = Expr::Column(ColumnName {
            qualifier: Some("u".into()),
            name: "a".into(),
        });
        assert_eq!(eval_expr(&qualified, &r, &c).unwrap(), Value::Int(5));
    }

    #[test]
    fn qualified_column_unknown_alias_errors() {
        let cs = vec![col("a", DataType::Int)];
        let r = Row::new(vec![Value::Int(5)]);
        let c = ctx(&cs, Some("u"));
        let wrong = Expr::Column(ColumnName {
            qualifier: Some("x".into()),
            name: "a".into(),
        });
        assert!(matches!(
            eval_expr(&wrong, &r, &c).unwrap_err(),
            EvalError::UnknownQualifier { .. }
        ));
    }

    #[test]
    fn arithmetic_with_widening() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let e = Expr::Binary {
            lhs: alloc::boxed::Box::new(lit(2)),
            op: BinOp::Add,
            rhs: alloc::boxed::Box::new(Expr::Literal(Literal::Float(0.5))),
        };
        assert_eq!(eval_expr(&e, &r, &c).unwrap(), Value::Float(2.5));
    }

    #[test]
    fn division_by_zero_errors() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let e = Expr::Binary {
            lhs: alloc::boxed::Box::new(lit(1)),
            op: BinOp::Div,
            rhs: alloc::boxed::Box::new(lit(0)),
        };
        assert_eq!(
            eval_expr(&e, &r, &c).unwrap_err(),
            EvalError::DivisionByZero
        );
    }

    #[test]
    fn comparison_returns_bool() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let e = Expr::Binary {
            lhs: alloc::boxed::Box::new(lit(1)),
            op: BinOp::Lt,
            rhs: alloc::boxed::Box::new(lit(2)),
        };
        assert_eq!(eval_expr(&e, &r, &c).unwrap(), Value::Bool(true));
    }

    #[test]
    fn null_propagates_through_arithmetic() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let e = Expr::Binary {
            lhs: alloc::boxed::Box::new(lit(1)),
            op: BinOp::Add,
            rhs: alloc::boxed::Box::new(null()),
        };
        assert_eq!(eval_expr(&e, &r, &c).unwrap(), Value::Null);
    }

    #[test]
    fn stack_depth_guard_trips_on_pathological_nesting() {
        // Built directly as an AST — the parser's own budgets (256
        // chained binary operators, 64 nesting levels) reject such SQL
        // long before eval sees it, so this exercises the eval-side
        // guard on its own. 30 000 frames overshoot the 768 KiB budget
        // at any conceivable frame size; the guard errors at the byte
        // budget, far below the worker stack, so deeper is safer.
        let mut e = Expr::Literal(Literal::Bool(true));
        for _ in 0..30_000 {
            e = Expr::Binary {
                lhs: alloc::boxed::Box::new(e),
                op: BinOp::And,
                rhs: alloc::boxed::Box::new(Expr::Literal(Literal::Bool(true))),
            };
        }
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let err = eval_expr(&e, &r, &c).unwrap_err();
        assert!(matches!(err, EvalError::StackDepthExceeded), "{err:?}");
        // Dropping a 30 000-deep Box chain recurses in the drop glue —
        // deeper than the eval guard allows the EVAL side to go — so
        // leak it rather than gamble on the test thread's stack.
        core::mem::forget(e);
    }

    #[test]
    fn and_three_valued_logic() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let tt = |a: bool, b_null: bool| Expr::Binary {
            lhs: alloc::boxed::Box::new(Expr::Literal(Literal::Bool(a))),
            op: BinOp::And,
            rhs: alloc::boxed::Box::new(if b_null {
                null()
            } else {
                Expr::Literal(Literal::Bool(true))
            }),
        };
        // FALSE AND NULL → FALSE
        assert_eq!(
            eval_expr(&tt(false, true), &r, &c).unwrap(),
            Value::Bool(false)
        );
        // TRUE AND NULL → NULL
        assert_eq!(eval_expr(&tt(true, true), &r, &c).unwrap(), Value::Null);
        // TRUE AND TRUE → TRUE
        assert_eq!(
            eval_expr(&tt(true, false), &r, &c).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn or_three_valued_logic() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let or_with_null = |a: bool| Expr::Binary {
            lhs: alloc::boxed::Box::new(Expr::Literal(Literal::Bool(a))),
            op: BinOp::Or,
            rhs: alloc::boxed::Box::new(null()),
        };
        // TRUE OR NULL → TRUE
        assert_eq!(
            eval_expr(&or_with_null(true), &r, &c).unwrap(),
            Value::Bool(true)
        );
        // FALSE OR NULL → NULL
        assert_eq!(
            eval_expr(&or_with_null(false), &r, &c).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn not_on_null_is_null() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let e = Expr::Unary {
            op: UnOp::Not,
            expr: alloc::boxed::Box::new(null()),
        };
        assert_eq!(eval_expr(&e, &r, &c).unwrap(), Value::Null);
    }

    #[test]
    fn text_comparison_lexicographic() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let e = Expr::Binary {
            lhs: alloc::boxed::Box::new(Expr::Literal(Literal::String("apple".into()))),
            op: BinOp::Lt,
            rhs: alloc::boxed::Box::new(Expr::Literal(Literal::String("banana".into()))),
        };
        assert_eq!(eval_expr(&e, &r, &c).unwrap(), Value::Bool(true));
    }

    #[test]
    fn interval_format_basics() {
        // v7.37.5 β — three-arg signature. PG byte-equal:
        // `'1 day'` ≠ `'24 hours'` now, the format reflects it.
        assert_eq!(format_interval(0, 0, 0), "00:00:00");
        assert_eq!(format_interval(0, 1, 0), "1 day");
        assert_eq!(format_interval(0, -1, 0), "-1 days");
        assert_eq!(format_interval(0, 0, 86_400_000_000), "24:00:00");
        assert_eq!(format_interval(0, 0, 3_600_000_000), "01:00:00");
        assert_eq!(format_interval(0, 1, 9_000_000), "1 day 00:00:09");
        assert_eq!(format_interval(14, 0, 0), "1 year 2 mons");
        assert_eq!(format_interval(-1, 0, 0), "-1 mons");
    }

    #[test]
    fn interval_format_pg_byte_equal_day_vs_24h() {
        // v7.37.5 β — the PG-canonical distinction `'1 day'` ≠ `'24 hours'`
        // is preserved in the formatter, not just the parser.
        assert_eq!(format_interval(0, 1, 0), "1 day");
        assert_eq!(format_interval(0, 0, 86_400_000_000), "24:00:00");
        assert_ne!(
            format_interval(0, 1, 0),
            format_interval(0, 0, 86_400_000_000),
        );
    }
}
