//! Session-parameter handling split out of `lib.rs` (lib.rs split 16):
//! `set_session_param` records a `SET <name> = <value>` (folding the
//! MySQL/PG FK-check + string-dialect toggles into engine state),
//! `session_param` reads one back (the FTS dispatcher consults
//! `default_text_search_config`), and `ev_ctx` builds an `EvalContext`
//! pre-chained with that config. Whole `impl Engine` methods; the
//! execute dispatcher drives `set_session_param`, `select.rs` drives
//! `ev_ctx`, and `dml.rs` / `plpgsql.rs` read via `session_param`.

use alloc::string::String;

use spg_storage::ColumnSchema;

use crate::Engine;
use crate::eval::EvalContext;

/// v7.39 (RLS) — reserved `session_params` key holding the effective session
/// role set by `SET ROLE` (absent = the default Admin superuser login).
/// `current_user` / RLS enforcement read it via `EvalContext.session_gucs`.
/// The `__spg_` prefix keeps it out of the user-visible GUC namespace.
pub(crate) const CURRENT_ROLE_KEY: &str = "__spg_current_role";

/// v7.39 (RLS) — the login identity (superuser). SPG's embedded engine and
/// its default server session both authenticate as this.
pub(crate) const LOGIN_ROLE: &str = "admin";

impl Engine {
    /// v7.39 (RLS) — the effective session role: the `SET ROLE` override, or
    /// the Admin login identity. Drives `current_user` and RLS role matching.
    #[must_use]
    pub(crate) fn current_role(&self) -> &str {
        self.session_params
            .get(CURRENT_ROLE_KEY)
            .map_or(LOGIN_ROLE, String::as_str)
    }

    /// v7.39 (RLS) — whether the session bypasses RLS. PG: superusers always
    /// bypass. SPG maps this to "no non-Admin `SET ROLE` is in effect": the
    /// default login and an explicit `SET ROLE admin` are superuser; any other
    /// role is policy-subject.
    #[must_use]
    pub(crate) fn is_superuser(&self) -> bool {
        self.current_role().eq_ignore_ascii_case(LOGIN_ROLE)
    }

    /// v7.12.1 — record a `SET <name> = <value>` parameter. Names
    /// are case-folded to lowercase to match PG; values keep their
    /// caller-supplied form so observability paths see what was
    /// requested. Only `default_text_search_config` is consulted by
    /// the engine today.
    pub(crate) fn set_session_param(&mut self, name: String, value: spg_sql::ast::SetValue) {
        let normalised = match value {
            spg_sql::ast::SetValue::String(s) => s,
            spg_sql::ast::SetValue::Ident(s) => s,
            spg_sql::ast::SetValue::Number(s) => s,
            // v7.39 (GUC) — `SET name = DEFAULT` / `SET TIME ZONE
            // LOCAL` restore the default, i.e. drop the session
            // override (storing "" would make SHOW render an empty
            // string instead of the default).
            spg_sql::ast::SetValue::Default => {
                self.session_params.remove(&name.to_ascii_lowercase());
                self.refresh_render_style();
                return;
            }
        };
        let key = name.to_ascii_lowercase();
        // v7.14.0 — mysqldump preamble emits
        // `SET FOREIGN_KEY_CHECKS=0` so it can CREATE TABLE in any
        // order despite cross-table FK references; the closing
        // section emits `SET FOREIGN_KEY_CHECKS=1` (or
        // `=@OLD_FOREIGN_KEY_CHECKS` which resolves to "ON" in our
        // session-variable-aware path). Match both shapes.
        // Also accept PG's `session_replication_role = 'replica'`
        // which suppresses trigger + FK enforcement during a
        // logical replication apply (pg_dump preserves this for
        // schema-only mode but it shows up in some restores).
        let value_off = matches!(
            normalised.to_ascii_lowercase().as_str(),
            "0" | "off" | "false"
        );
        let value_on = matches!(
            normalised.to_ascii_lowercase().as_str(),
            "1" | "on" | "true"
        );
        if key == "foreign_key_checks"
            || key == "session_replication_role" && normalised.eq_ignore_ascii_case("replica")
        {
            if value_off || key == "session_replication_role" {
                self.foreign_key_checks = false;
            } else if value_on
                || (key == "session_replication_role" && normalised.eq_ignore_ascii_case("origin"))
            {
                self.foreign_key_checks = true;
                // Drain pending FK queue against the now-complete
                // catalog. Errors here surface as the SET reply —
                // caller knows enabling checks revealed orphans.
                let _ = self.drain_pending_foreign_keys();
            }
        }
        // v7.22 (round-13 T3) — string-literal dialect signals.
        // `SET sql_mode = …` is something only MySQL clients and
        // mysqldump preambles emit → MySQL escape semantics.
        // `SET standard_conforming_strings = on|off` is PG's own
        // switch for exactly this behaviour (every pg_dump preamble
        // sets it to on). The same SQL text lexes differently per
        // dialect, so a flip invalidates the plan cache.
        let new_escapes = if key == "sql_mode" {
            Some(true)
        } else if key == "standard_conforming_strings" {
            Some(value_off)
        } else {
            None
        };
        if let Some(flag) = new_escapes
            && flag != self.backslash_escapes
        {
            self.backslash_escapes = flag;
            self.plan_cache.clear();
        }
        // v7.39 (GUC) — PG stores ms-unit time GUCs as an integer and
        // renders SHOW/current_setting in the largest whole unit
        // ("250" → "250ms", "5000" → "5s"). Normalise at store time so
        // every read surface agrees.
        let normalised = if matches!(
            key.as_str(),
            "statement_timeout"
                | "lock_timeout"
                | "idle_in_transaction_session_timeout"
                | "idle_session_timeout"
        ) {
            match parse_pg_duration_ms(&normalised) {
                Some(ms) => render_pg_duration_ms(ms),
                None => normalised,
            }
        } else {
            normalised
        };
        // v7.39 (GUC knife 3) — datestyle is sticky per category (a bare
        // 'DMY' keeps the current style; 'German' forces DMY); PG stores
        // and SHOWs the RESOLVED canonical pair. intervalstyle /
        // extra_float_digits just refresh the cached RenderStyle.
        let normalised = if key == "datestyle" {
            match parse_datestyle_parts(&normalised, self.render_style) {
                Some((st, ord)) => String::from(datestyle_canonical(st, ord)),
                // Invalid values are rejected earlier (validate_known_guc);
                // an unvalidated caller keeps the raw text.
                None => normalised,
            }
        } else {
            normalised
        };
        let is_render_guc = matches!(
            key.as_str(),
            "datestyle" | "intervalstyle" | "extra_float_digits"
        );
        self.session_params.insert(key, normalised);
        if is_render_guc {
            self.refresh_render_style();
        }
    }

    /// v7.39 (GUC knife 3) — recompute the cached `RenderStyle` from the
    /// session store. Called after any write/removal of a render GUC.
    pub(crate) fn refresh_render_style(&mut self) {
        let mut style = crate::eval::RenderStyle::default();
        if let Some(ds) = self.session_param("datestyle")
            && let Some((st, ord)) = parse_datestyle_parts(ds, style)
        {
            style.date_style = st;
            style.date_order = ord;
        }
        if let Some(is) = self.session_param("intervalstyle")
            && let Some(k) = parse_intervalstyle(is)
        {
            style.interval_style = k;
        }
        if let Some(efd) = self.session_param("extra_float_digits")
            && let Ok(n) = efd.trim().parse::<i32>()
        {
            style.extra_float_digits = n;
        }
        self.render_style = style;
    }

    /// v7.12.1 — read a session parameter set via `SET`. Used by
    /// the FTS function dispatcher to resolve the default config
    /// for `to_tsvector(text)` / `plainto_tsquery(text)` etc.
    /// v7.39 (tz epic) — validate + canonicalise a `SET timezone`
    /// value: 'utc' -> 'UTC'; fixed offsets / abbreviations keep their
    /// spelling; IANA names resolve through the host tzdb to their
    /// canonical case. Unknown -> PG's invalid-parameter error.
    pub(crate) fn canonicalize_timezone(
        &self,
        value: &str,
    ) -> Result<String, crate::EngineError> {
        let v = value.trim();
        if v.eq_ignore_ascii_case("utc") || v.eq_ignore_ascii_case("gmt") {
            return Ok(v.to_ascii_uppercase());
        }
        if crate::eval::datetime_resolve_zone_offset(v).is_some() {
            return Ok(String::from(v));
        }
        match self.tz_canon_fn {
            Some(f) => match f(v) {
                Some(canon) => Ok(canon),
                None => Err(crate::EngineError::Unsupported(alloc::format!(
                    "invalid value for parameter \"TimeZone\": \"{v}\""
                ))),
            },
            // No host tzdb (bare no_std embedding): keep the pre-epic
            // accept-and-store behaviour — rendering degrades to UTC
            // rather than rejecting a name we cannot verify.
            None => Ok(String::from(v)),
        }
    }

    /// v7.39 (GUC knife 3) — the parsed session render style (wire /
    /// COPY renderers snapshot it once per statement).
    #[must_use]
    pub fn render_style(&self) -> crate::eval::RenderStyle {
        self.render_style
    }

    /// v7.39 (tz epic) — per-statement session TimeZone snapshot for
    /// the timestamptz renderers. SET already validated the value, so
    /// an unresolvable name here (host lost its tzdb) degrades to UTC.
    #[must_use]
    pub fn session_tz(&self) -> crate::SessionTz {
        let Some(z) = self.session_param("timezone") else {
            return crate::SessionTz::Utc;
        };
        if z.eq_ignore_ascii_case("utc") || z.eq_ignore_ascii_case("gmt") {
            return crate::SessionTz::Utc;
        }
        if let Some(off) = crate::eval::datetime_resolve_zone_offset(z) {
            return if off == 0 {
                crate::SessionTz::Utc
            } else {
                crate::SessionTz::Fixed(off)
            };
        }
        match (self.tz_offset_fn, self.tz_abbrev_fn) {
            (Some(of), Some(af)) => crate::SessionTz::Named(String::from(z), of, af),
            _ => crate::SessionTz::Utc,
        }
    }

    #[must_use]
    pub fn session_param(&self, name: &str) -> Option<&str> {
        self.session_params
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// v7.39 (read01 round 46) — raise a PG-style NOTICE for the statement
    /// now executing. The text is PG's exact wording minus the "NOTICE:  "
    /// banner (the wire layer adds that); e.g. `table "t" does not exist,
    /// skipping`.
    pub(crate) fn notice(&mut self, text: alloc::string::String) {
        self.pending_notices.push(text);
    }

    /// v7.39 (read01 round 46) — drain the NOTICEs the last statement
    /// raised. pgwire emits one NoticeResponse per entry ahead of the
    /// statement's CommandComplete; embedded callers may ignore them.
    #[must_use]
    pub fn take_notices(&mut self) -> alloc::vec::Vec<alloc::string::String> {
        core::mem::take(&mut self.pending_notices)
    }

    /// v7.37.7 — PG `statement_timeout` GUC read accessor. Returns the
    /// session-set value in **milliseconds**, parsed from the raw
    /// `SET statement_timeout = N` string. Returns `None` when:
    /// - the GUC is unset,
    /// - the value is `0` (PG semantics: 0 = no timeout),
    /// - the value fails to parse.
    ///
    /// Accepted input shapes mirror PG's `GUC_UNIT_MS` parser:
    /// - bare digits: `100` → 100 ms (PG default unit when GUC is in ms)
    /// - explicit ms: `100ms`, `100 ms`
    /// - seconds:     `1s`, `30s` → 1000 / 30000 ms
    /// - minutes:     `5min` → 300000 ms
    ///
    /// The host (`spg-server` per-query watchdog) consults this when
    /// constructing the `CancelToken` deadline so a SQL-set
    /// `SET statement_timeout = 1000` is honoured per-session — the
    /// effective deadline becomes `min(SPG_QUERY_TIMEOUT_MS, session)`.
    /// Returning `None` from this fn means "no session override, use
    /// the host-level timeout only".
    #[must_use]
    pub fn session_statement_timeout_ms(&self) -> Option<u64> {
        let raw = self.session_param("statement_timeout")?;
        parse_pg_duration_ms(raw).filter(|ms| *ms > 0)
    }

    /// v7.12.1 — build an `EvalContext` chained with the session's
    /// `default_text_search_config`. Engine-internal callers use
    /// this instead of `EvalContext::new` so the FTS function
    /// dispatcher sees the SET configuration.
    pub(crate) fn ev_ctx<'a>(
        &'a self,
        columns: &'a [ColumnSchema],
        alias: Option<&'a str>,
    ) -> EvalContext<'a> {
        EvalContext::new(columns, alias)
            .with_render_style(self.render_style)
            .with_tz_fns(self.tz_offset_fn, self.tz_localize_fn, self.tz_abbrev_fn)
            .with_default_text_search_config(self.session_param("default_text_search_config"))
            // Thread the session GUC map so current_setting resolves
            // custom `SET app.foo = …` settings (request-context / RLS).
            .with_session_gucs(&self.session_params)
            // v7.37.16 (16.12) — thread the read-only catalog so
            // builtins like pg_partition_root can walk partition
            // roles. Other EvalContext call sites (scan paths,
            // joinfold, aggregate) continue to construct without
            // catalog access; catalog-aware builtins return NULL
            // there per documented contract.
            .with_catalog(&self.catalog)
            // v7.38 (read01 P5.24) — thread the host CSPRNG so gen_random_bytes
            // / gen_salt use real entropy instead of the predictable PRNG.
            .with_salt_fn(self.salt_fn)
            // v7.39 (read01 pgstatfuncs.c) — calling-connection identity.
            .with_backend_pid_fn(self.backend_pid_fn)
            // v7.38 (read01 P6.08) — thread the host wall clock so uuidv7 gets
            // a real time-ordered prefix.
            .with_clock(self.clock)
            // v7.38 (T24) — thread the transaction-version state so the txid_*
            // builtins report real ids instead of a constant stub.
            .with_xact(self.xact_view())
    }

    /// v7.38 (T24) — read-only snapshot of the transaction-version state the
    /// `txid_*` / `pg_xact_status` builtins read. A transaction's id is
    /// allocated at BEGIN (`transaction.rs`), so it is stable across the
    /// statements of that transaction, as in PG. In autocommit the id exists
    /// only once the statement has written.
    pub(crate) fn xact_view(&self) -> crate::eval::XactView<'_> {
        crate::eval::XactView {
            current: self
                .current_tx
                .and_then(|t| self.tx_writer_versions.get(&t).copied())
                .or(self.stmt_writer_version),
            active: &self.active_writer_versions,
            aborted: &self.aborted_versions,
        }
    }
}

/// v7.37.7 — parse a PG-style `GUC_UNIT_MS` duration string into
/// milliseconds. Accepts the same shapes PG itself accepts for
/// `statement_timeout` and related ms-based GUCs.
///
/// Returns `None` on parse failure (callers treat None as "GUC not
/// set / default applies").
/// v7.39 (GUC knife 3) — parse a DateStyle value ('ISO, MDY' / 'German'
/// / 'DMY' / …) against the current style: keywords apply in order,
/// each updating its own category (PG semantics; German implies DMY).
/// Returns None on any unrecognised keyword.
pub(crate) fn parse_datestyle_parts(
    value: &str,
    current: crate::eval::RenderStyle,
) -> Option<(crate::eval::DateStyleKind, crate::eval::DateOrder)> {
    use crate::eval::{DateOrder, DateStyleKind};
    let mut st = current.date_style;
    let mut ord = current.date_order;
    let mut any = false;
    for part in value.split(',') {
        let p = part.trim().to_ascii_lowercase();
        match p.as_str() {
            "iso" => st = DateStyleKind::Iso,
            "german" => {
                st = DateStyleKind::German;
                ord = DateOrder::Dmy;
            }
            "sql" => st = DateStyleKind::Sql,
            "postgres" => st = DateStyleKind::Postgres,
            "mdy" | "us" | "noneuro" | "noneuropean" => ord = DateOrder::Mdy,
            "dmy" | "euro" | "european" => ord = DateOrder::Dmy,
            "ymd" => ord = DateOrder::Ymd,
            _ => return None,
        }
        any = true;
    }
    if any { Some((st, ord)) } else { None }
}

/// The canonical `SHOW datestyle` text for a resolved pair.
pub(crate) fn datestyle_canonical(
    st: crate::eval::DateStyleKind,
    ord: crate::eval::DateOrder,
) -> &'static str {
    use crate::eval::{DateOrder, DateStyleKind};
    match (st, ord) {
        (DateStyleKind::Iso, DateOrder::Mdy) => "ISO, MDY",
        (DateStyleKind::Iso, DateOrder::Dmy) => "ISO, DMY",
        (DateStyleKind::Iso, DateOrder::Ymd) => "ISO, YMD",
        (DateStyleKind::German, DateOrder::Mdy) => "German, MDY",
        (DateStyleKind::German, DateOrder::Dmy) => "German, DMY",
        (DateStyleKind::German, DateOrder::Ymd) => "German, YMD",
        (DateStyleKind::Sql, DateOrder::Mdy) => "SQL, MDY",
        (DateStyleKind::Sql, DateOrder::Dmy) => "SQL, DMY",
        (DateStyleKind::Sql, DateOrder::Ymd) => "SQL, YMD",
        (DateStyleKind::Postgres, DateOrder::Mdy) => "Postgres, MDY",
        (DateStyleKind::Postgres, DateOrder::Dmy) => "Postgres, DMY",
        (DateStyleKind::Postgres, DateOrder::Ymd) => "Postgres, YMD",
    }
}

/// v7.39 (GUC knife 3) — IntervalStyle keyword → kind.
pub(crate) fn parse_intervalstyle(value: &str) -> Option<crate::eval::IntervalStyleKind> {
    use crate::eval::IntervalStyleKind as K;
    match value.trim().to_ascii_lowercase().as_str() {
        "postgres" => Some(K::Postgres),
        "sql_standard" => Some(K::SqlStandard),
        "iso_8601" => Some(K::Iso8601),
        "postgres_verbose" => Some(K::PostgresVerbose),
        _ => None,
    }
}

pub(crate) fn parse_pg_duration_ms(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // PG accepts trailing unit suffix: ms / s / min / h / d. Strip in
    // priority order (longer first so `min` doesn't match as `m`).
    let lowered = s.to_ascii_lowercase();
    let (num_part, multiplier_ms): (&str, u64) = if let Some(p) = lowered.strip_suffix("ms") {
        (p, 1)
    } else if let Some(p) = lowered.strip_suffix("min") {
        (p, 60_000)
    } else if let Some(p) = lowered.strip_suffix('s') {
        (p, 1_000)
    } else if let Some(p) = lowered.strip_suffix('h') {
        (p, 3_600_000)
    } else if let Some(p) = lowered.strip_suffix('d') {
        (p, 86_400_000)
    } else {
        // No unit suffix — bare digits in the GUC's native unit (ms
        // for `statement_timeout`).
        (lowered.as_str(), 1)
    };
    let n: u64 = num_part.trim().parse().ok()?;
    n.checked_mul(multiplier_ms)
}

/// v7.39 (GUC) — render a millisecond count the way PG's SHOW does:
/// the largest unit that divides it evenly; zero is unit-less.
fn render_pg_duration_ms(ms: u64) -> String {
    use alloc::format;
    if ms == 0 {
        return String::from("0");
    }
    if ms % 86_400_000 == 0 {
        format!("{}d", ms / 86_400_000)
    } else if ms % 3_600_000 == 0 {
        format!("{}h", ms / 3_600_000)
    } else if ms % 60_000 == 0 {
        format!("{}min", ms / 60_000)
    } else if ms % 1_000 == 0 {
        format!("{}s", ms / 1_000)
    } else {
        format!("{ms}ms")
    }
}

#[cfg(test)]
mod tests {
    use super::parse_pg_duration_ms;
    use alloc::format;

    #[test]
    fn parse_bare_digits_treats_as_ms() {
        assert_eq!(parse_pg_duration_ms("100"), Some(100));
        assert_eq!(parse_pg_duration_ms("0"), Some(0));
        assert_eq!(parse_pg_duration_ms("60000"), Some(60_000));
    }

    #[test]
    fn parse_ms_suffix() {
        assert_eq!(parse_pg_duration_ms("100ms"), Some(100));
        assert_eq!(parse_pg_duration_ms("100 ms"), Some(100));
    }

    #[test]
    fn parse_seconds() {
        assert_eq!(parse_pg_duration_ms("1s"), Some(1_000));
        assert_eq!(parse_pg_duration_ms("30s"), Some(30_000));
    }

    #[test]
    fn parse_minutes_uses_three_letter_suffix() {
        assert_eq!(parse_pg_duration_ms("5min"), Some(300_000));
        // `5m` is NOT valid PG (PG requires `min`); confirm we mirror.
        assert_eq!(parse_pg_duration_ms("5m"), None);
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert_eq!(parse_pg_duration_ms(""), None);
        assert_eq!(parse_pg_duration_ms("abc"), None);
        assert_eq!(parse_pg_duration_ms("100x"), None);
    }

    #[test]
    fn parse_handles_whitespace() {
        assert_eq!(parse_pg_duration_ms("  100  "), Some(100));
    }

    #[test]
    fn parse_overflow_returns_none() {
        // u64::MAX seconds overflows when multiplied by 1000 ms/s.
        assert_eq!(parse_pg_duration_ms(&format!("{}s", u64::MAX)), None);
    }

    #[cfg(test)]
    mod session_integration {
        use crate::Engine;
        use spg_sql::ast::SetValue;

        #[test]
        fn set_statement_timeout_round_trips_ms() {
            let mut e = Engine::new();
            e.set_session_param("statement_timeout".into(), SetValue::Number("250".into()));
            assert_eq!(e.session_statement_timeout_ms(), Some(250));
        }

        #[test]
        fn set_statement_timeout_zero_is_none() {
            // PG semantics: 0 means "no timeout".
            let mut e = Engine::new();
            e.set_session_param("statement_timeout".into(), SetValue::Number("0".into()));
            assert_eq!(e.session_statement_timeout_ms(), None);
        }

        #[test]
        fn statement_timeout_unset_is_none() {
            let e = Engine::new();
            assert_eq!(e.session_statement_timeout_ms(), None);
        }

        #[test]
        fn statement_timeout_accepts_ms_suffix_via_string_set() {
            let mut e = Engine::new();
            e.set_session_param(
                "statement_timeout".into(),
                SetValue::String("1500ms".into()),
            );
            assert_eq!(e.session_statement_timeout_ms(), Some(1500));
        }
    }
}
