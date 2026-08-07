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

/// v7.39 (read01 round 51) — reserved `session_params` key holding the LOGIN
/// identity: the `user` the client sent in the startup packet. `session_user`
/// reports it, and `current_user` falls back to it when no `SET ROLE` is in
/// effect. Absent (embedded engine, or a wire that never set it) = LOGIN_ROLE.
pub(crate) const SESSION_USER_KEY: &str = "__spg_session_user";

/// v7.37 (round 830) — reserved `session_params` key marking the login
/// identity as VERIFIED: the connection presented a credential the server
/// checked (SCRAM or cleartext), rather than merely naming itself in the
/// startup packet. Absent = unverified, which is the embedded engine and
/// the server's open mode.
///
/// The distinction is what lets a login name carry privilege. Open mode
/// accepts any startup as the admin role, so a name there is a label and
/// nothing more — keying privilege on it would let anyone pick their own.
/// A checked credential is a different thing, and it is the only
/// configuration where roles and their policies mean anything at all.
pub(crate) const SESSION_AUTHENTICATED_KEY: &str = "__spg_session_authenticated";

/// v7.39 (RLS) — the login identity (superuser). SPG's embedded engine and
/// its default server session both authenticate as this.
pub(crate) const LOGIN_ROLE: &str = "admin";

/// v7.39 (read01 round 58) — PG's bootstrap superuser. `synth_pg_roles` has
/// always reported a `postgres` row (admin tools probe for it), so the name has
/// to BE a role: refusing `SET ROLE postgres` while advertising it in pg_roles
/// would be the same self-contradiction the ACL work went and fixed. It is a
/// superuser, like the login role.
pub(crate) const BOOTSTRAP_ROLE: &str = "postgres";

impl Engine {
    /// v7.39 (read01 round 51) — the login identity: the startup packet's
    /// `user`, else the Admin default. Drives `session_user`.
    #[must_use]
    pub(crate) fn session_user(&self) -> &str {
        self.session_params
            .get(SESSION_USER_KEY)
            .map_or(LOGIN_ROLE, String::as_str)
    }

    /// v7.39 (read01 round 51) — record the connection's login identity.
    /// The server calls this once per connection from the startup packet.
    pub fn set_session_user(&mut self, user: &str) {
        self.session_params
            .insert(String::from(SESSION_USER_KEY), String::from(user));
    }

    /// v7.37 (round 830) — record that this connection's login identity was
    /// verified against a stored credential. The server calls it once per
    /// connection, right after `set_session_user`, when it demanded a
    /// password; open-mode connections never do.
    pub fn set_session_authenticated(&mut self) {
        self.session_params.insert(
            String::from(SESSION_AUTHENTICATED_KEY),
            String::from("1"),
        );
    }

    /// Was this session's login identity checked against a credential?
    #[must_use]
    pub(crate) fn session_is_authenticated(&self) -> bool {
        self.session_params.contains_key(SESSION_AUTHENTICATED_KEY)
    }

    /// v7.39 (RLS) — the effective session role: the `SET ROLE` override, or
    /// the login identity. Drives `current_user` and RLS role matching.
    #[must_use]
    pub(crate) fn current_role(&self) -> &str {
        self.session_params
            .get(CURRENT_ROLE_KEY)
            .map_or_else(|| self.session_user(), String::as_str)
    }

    /// v7.39 (RLS) — whether the session bypasses RLS. PG: superusers always
    /// bypass. SPG maps this to "no non-Admin `SET ROLE` is in effect": the
    /// default login and an explicit `SET ROLE admin` are superuser; any other
    /// role is policy-subject.
    #[must_use]
    pub(crate) fn is_superuser(&self) -> bool {
        // v7.39 (read01 round 51) — keyed on whether an explicit `SET ROLE` to
        // a non-superuser role is in effect, NOT on the login NAME. The wire
        // reports the startup packet's `user` as current_user / session_user;
        // if superuser-ness followed that name, every connection as e.g.
        // "unmei" would silently become RLS-subject. Reported identity and
        // privilege semantics stay decoupled.
        //
        // v7.39 (read01 round 58) — the role's own SUPERUSER attribute decides
        // now. `SET ROLE admin` still is one (the built-in login role), and so
        // is any role created SUPERUSER. PG never inherits the attribute
        // through membership, so this reads the role itself, not its set.
        match self.session_params.get(CURRENT_ROLE_KEY) {
            Some(r) => self.role_is_superuser(r),
            // v7.37 (round 830) — with no SET ROLE in effect, the login
            // identity decides IF it was verified. Measured before this:
            // psql authenticated as a role with rolsuper = f, over a table
            // with row security enabled and a USING policy in place, read
            // every row including another owner's — for every projection
            // shape, because the predicate was never injected at all.
            //
            // The unconditional `true` this replaces was deliberate and is
            // still right for the case it was written for: in open mode the
            // startup `user` is unverified, so letting it carry privilege
            // would mean anyone could name themselves into a role. What
            // changed is that the name is no longer always unverified — once
            // a credentialed LOGIN role exists the server demands SCRAM, and
            // that is exactly the configuration where policies and grants
            // are supposed to bind.
            //
            // `role_is_superuser` still exempts the admin and bootstrap
            // logins and any role created SUPERUSER, so authenticating as an
            // administrator changes nothing.
            None if self.session_is_authenticated() => {
                self.role_is_superuser(self.session_user())
            }
            None => true,
        }
    }

    /// v7.39 (round 334, V55) — is THAT role a superuser? Split out of
    /// [`Self::is_superuser`] so a `SECURITY DEFINER` body can be
    /// authorised as the function's owner rather than the session's role.
    pub(crate) fn role_is_superuser(&self, role: &str) -> bool {
        role.eq_ignore_ascii_case(LOGIN_ROLE)
            || role.eq_ignore_ascii_case(BOOTSTRAP_ROLE)
            || self.users.get(role).is_some_and(|rec| rec.superuser)
    }

    /// v7.12.1 — record a `SET <name> = <value>` parameter. Names
    /// are case-folded to lowercase to match PG; values keep their
    /// caller-supplied form so observability paths see what was
    /// requested. Only `default_text_search_config` is consulted by
    /// the engine today.
    /// v7.39 (round 501) — is `name` a parameter this session may set?
    ///
    /// PG18 answers `ERROR: unrecognized configuration parameter "x"` for
    /// a name it does not know, and refuses the ones a session cannot
    /// change with a wording that says why. SPG accepted anything —
    /// round 500 measured `SET nonexistent_knob = 3` answering `SET` — so
    /// a typo'd parameter name was taken silently and the setting the
    /// caller believed they had made was never made.
    ///
    /// Three kinds of name are accepted beyond PG18's own list, because
    /// rejecting them would break callers that are not wrong:
    ///
    /// * anything containing a dot — PG treats `myapp.thing` as a
    ///   customised option and accepts it, and extensions rely on that;
    /// * the MySQL-dialect names SPG honours (`sql_mode`,
    ///   `foreign_key_checks`, …), which PG has no concept of and which
    ///   `mysqldump` preambles emit;
    /// * SPG's own internal keys.
    ///
    /// Returns the PG error text, or `None` when the SET may proceed.
    pub(crate) fn reject_unsettable_guc(&self, name: &str) -> Option<alloc::string::String> {
        let key = name.to_ascii_lowercase();
        if key.contains('.')
            || key.starts_with("__spg")
            || matches!(
                key.as_str(),
                // MySQL dialect — `mysqldump` preambles and MySQL clients.
                "sql_mode"
                    | "foreign_key_checks"
                    | "unique_checks"
                    | "autocommit"
                    | "names"
                    | "character_set_client"
                    | "character_set_connection"
                    | "character_set_results"
                    | "collation_connection"
                    | "sql_quote_show_create"
                    | "sql_notes"
                    | "time_zone"
                    | "sql_safe_updates"
                    | "innodb_strict_mode"
                    | "net_write_timeout"
                    | "net_read_timeout"
                    | "wait_timeout"
                    | "interactive_timeout"
                    | "max_allowed_packet"
                    | "group_concat_max_len"
                    | "old_alter_table"
                    | "sql_log_bin"
                    | "session_replication_role"
            )
        {
            return None;
        }
        match crate::guc_catalog::guc_context(&key) {
            None => Some(alloc::format!(
                "unrecognized configuration parameter \"{key}\""
            )),
            // PG's own wording per context, so a client that matches on
            // the message keeps working.
            Some("internal") => Some(alloc::format!("parameter \"{key}\" cannot be changed")),
            Some("postmaster") => Some(alloc::format!(
                "parameter \"{key}\" cannot be changed without restarting the server"
            )),
            Some("sighup") => Some(alloc::format!("parameter \"{key}\" cannot be changed now")),
            Some(_) => None,
        }
    }

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
            // MySQL/MariaDB turn backslash escapes OFF only when the
            // sql_mode list contains NO_BACKSLASH_ESCAPES; any other
            // value (including an empty list) leaves them ON. Verified
            // vs MariaDB: `SET sql_mode='STRICT_TRANS_TABLES'` → `'\n'`
            // is a newline, `='NO_BACKSLASH_ESCAPES,STRICT_TRANS_TABLES'`
            // → two bytes. sql_mode is a full replacement, so evaluate
            // the whole new value rather than tracking a delta.
            Some(!normalised.to_ascii_uppercase().contains("NO_BACKSLASH_ESCAPES"))
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
        // v7.39 (round 470) — the OTHER thing sql_mode carries: strictness.
        // MariaDB's default list has STRICT_TRANS_TABLES, and a list
        // without any STRICT_ flag makes a value that would raise get bent
        // to fit instead. Measured on MariaDB 11 with `SET sql_mode=''`:
        // INT <- 99999999999999 stores 2147483647, TINYINT <- 999 stores
        // 127, INT UNSIGNED <- -5 stores 0, VARCHAR(3) <- 'toolong' stores
        // 'too', INT <- 'abc' stores 0 and <- '12xy' stores 12.
        if key == "sql_mode" {
            let upper = normalised.to_ascii_uppercase();
            self.mysql_strict =
                upper.contains("STRICT_TRANS_TABLES") || upper.contains("STRICT_ALL_TABLES");
        }
        // v7.39 (GUC) — PG stores ms-unit time GUCs as an integer and
        // renders SHOW/current_setting in the largest whole unit
        // ("250" → "250ms", "5000" → "5s"). Normalise at store time so
        // every read surface agrees.
        // v7.39 (round 204) — memory GUCs canonicalize to the largest
        // binary unit at store time, so `SET work_mem = '65536'` and
        // `= '64MB'` both SHOW `64MB`, matching PG.
        // v7.39 (round 522) — which parameters those are now comes from
        // `guc_unit`, the same table `pg_settings` reads.
        let normalised = match guc_unit(key.as_str()) {
            Some("ms") => match parse_pg_duration_ms(&normalised) {
                Some(ms) => render_pg_duration_ms(ms),
                None => normalised,
            },
            Some(_) => match parse_pg_mem_kb(&normalised) {
                Some(kb) => render_pg_mem_kb(kb),
                None => normalised,
            },
            None => normalised,
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
            // v7.39 (round 524) — `bytea_output` joins them: it was
            // accepted and never read.
            "datestyle" | "intervalstyle" | "extra_float_digits" | "bytea_output"
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
        if let Some(bo) = self.session_param("bytea_output") {
            style.bytea_escape = bo.trim().eq_ignore_ascii_case("escape");
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
    pub(crate) fn canonicalize_timezone(&self, value: &str) -> Result<String, crate::EngineError> {
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

    /// v7.39 (round 547) — apply the GUC defaults `ALTER ROLE … SET` /
    /// `ALTER DATABASE … SET` recorded, in PG's order of specificity.
    ///
    /// Measured on PG18: with all four scopes set, a new session got the
    /// role-in-database value. So the least specific is applied first and
    /// the most specific last, each overwriting.
    pub fn apply_db_role_settings(&mut self, database: &str, role: &str) {
        let scopes: alloc::vec::Vec<(alloc::string::String, alloc::string::String)> = alloc::vec![
            (alloc::string::String::new(), alloc::string::String::new()),
            (alloc::string::String::from(database), alloc::string::String::new()),
            (alloc::string::String::new(), alloc::string::String::from(role)),
            (alloc::string::String::from(database), alloc::string::String::from(role)),
        ];
        let mut apply: alloc::vec::Vec<(alloc::string::String, alloc::string::String)> =
            alloc::vec::Vec::new();
        for key in &scopes {
            if let Some(params) = self.active_catalog().db_role_settings().get(key) {
                for (k, v) in params {
                    apply.push((k.clone(), v.clone()));
                }
            }
        }
        for (k, v) in apply {
            let _ = self.execute(&alloc::format!("SET {k} = '{v}'"));
        }
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
        let lower = name.to_ascii_lowercase();
        // v7.39 (read01 round 118, B3) — `transaction_isolation` is not a plain
        // session GUC in the params map; it tracks the live per-transaction
        // level (`BEGIN ISOLATION LEVEL …`, reset at COMMIT/ROLLBACK). The wire
        // `SHOW` handler reads this, so it must report the live value rather
        // than a seeded "read committed".
        if lower == "transaction_isolation" {
            return Some(self.current_isolation_level.as_pg_str());
        }
        self.session_params.get(&lower).map(String::as_str)
    }


    /// v7.39 (read01 round 46) — raise a PG-style NOTICE for the statement
    /// now executing. The text is PG's exact wording minus the "NOTICE:  "
    /// banner (the wire layer adds that); e.g. `table "t" does not exist,
    /// skipping`.
    pub(crate) fn notice(&mut self, text: alloc::string::String) {
        self.pending_notices.push(crate::Notice {
            severity: crate::NoticeSeverity::Notice,
            message: text,
        });
    }

    /// v7.39 (round 320, V53) — `RESET ALL` / the reset half of
    /// `DISCARD ALL`: drop every GUC override, keeping the internal keys
    /// that are not GUCs at all (the connection's login identity and its
    /// database). Clearing the whole map took those with it.
    pub(crate) fn reset_all_gucs(&mut self) {
        let keep: alloc::vec::Vec<(String, String)> = [SESSION_USER_KEY, "spg.database"]
            .iter()
            .filter_map(|k| {
                self.session_params
                    .get(*k)
                    .map(|v| (String::from(*k), v.clone()))
            })
            .collect();
        self.session_params.clear();
        for (k, v) in keep {
            self.session_params.insert(k, v);
        }
    }

    /// v7.39 (round 318, V41) — raise a PG-style WARNING. Same channel as
    /// [`Self::notice`], one level louder: PG uses it for "the command
    /// succeeded but did nothing useful" cases such as `SET CONSTRAINTS`
    /// outside a transaction block.
    pub(crate) fn warning(&mut self, text: alloc::string::String) {
        self.pending_notices.push(crate::Notice {
            severity: crate::NoticeSeverity::Warning,
            message: text,
        });
    }

    /// v7.39 (round 757, F31-B3) — deliver a plpgsql body's RAISE
    /// messages into the pending-notice queue, honouring
    /// `client_min_messages` (INFO passes unconditionally, as in PG).
    pub(crate) fn drain_raise_sink(&mut self, sink: crate::triggers::NoticeSink) {
        self.queue_raised(sink.into_inner());
    }

    /// The vec-shaped half: body walkers that cannot hold `&mut self`
    /// collect into a plain Vec and the owning method queues it here.
    pub(crate) fn queue_raised(
        &mut self,
        raised: alloc::vec::Vec<(crate::NoticeSeverity, alloc::string::String)>,
    ) {
        for (severity, message) in raised {
            if self.notice_severity_reaches_client(severity) {
                self.pending_notices.push(crate::Notice { severity, message });
            }
        }
    }

    /// v7.39 (read01 round 46) — drain the NOTICEs the last statement
    /// raised. pgwire emits one NoticeResponse per entry ahead of the
    /// statement's CommandComplete; embedded callers may ignore them.
    #[must_use]
    pub fn take_notices(&mut self) -> alloc::vec::Vec<crate::Notice> {
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

    /// v7.39 (round 621) — does a message of this severity reach the client?
    ///
    /// `client_min_messages` was validated on the way in and then never read,
    /// so `SET client_min_messages = warning` — and even `= error` — left the
    /// NOTICEs coming. Every `DROP … IF EXISTS` on a name that is not there
    /// said so, which is why the standing differential corpus could not use
    /// the GUC to quieten its own setup and carried the asymmetry in eighteen
    /// of its files.
    ///
    /// PG's order, ascending: debug5 < debug4 < debug3 < debug2 < debug1 <
    /// log < notice < warning < error < fatal < panic. A message is sent when
    /// its own severity is at least the setting. Anything above `warning`
    /// suppresses both of the severities SPG raises.
    #[must_use]
    pub fn notice_severity_reaches_client(&self, severity: crate::NoticeSeverity) -> bool {
        fn rank(s: &str) -> u8 {
            match s {
                "debug5" => 0,
                "debug4" => 1,
                "debug3" => 2,
                "debug2" => 3,
                "debug1" => 4,
                "log" => 5,
                "notice" => 6,
                "warning" => 7,
                "error" => 8,
                "fatal" => 9,
                "panic" => 10,
                // Not one of PG's levels — SET would have refused it, so this
                // is the default rather than a silent drop.
                _ => 6,
            }
        }
        let setting = self
            .session_param("client_min_messages")
            .map_or(6, |v| rank(&v.trim().to_ascii_lowercase()));
        let own = match severity {
            crate::NoticeSeverity::Notice => 6,
            crate::NoticeSeverity::Warning => 7,
            // PG sends INFO to the client unconditionally.
            crate::NoticeSeverity::Info => return true,
        };
        own >= setting
    }

    /// v7.12.1 — build an `EvalContext` chained with the session's
    /// `default_text_search_config`. Engine-internal callers use
    /// this instead of `EvalContext::new` so the FTS function
    /// dispatcher sees the SET configuration.
    /// v7.39 (round 523) — the session zone's offset at a UTC instant,
    /// or 0 when the session is on UTC.
    ///
    /// The clock rewrite needs it: `current_date` and the local-clock
    /// family read the session's wall clock, and SPG's unified clock
    /// reads UTC, so `SET TimeZone = 'Asia/Tokyo'` left `current_date`
    /// naming yesterday for nine hours of every day.
    pub(crate) fn session_tz_offset_at(&self, utc_micros: i64) -> i64 {
        let Some(zone) = self.session_params.get("timezone") else {
            return 0;
        };
        if zone.eq_ignore_ascii_case("utc") || zone.eq_ignore_ascii_case("gmt") {
            return 0;
        }
        if let Some(off) = crate::eval::resolve_zone_offset_pub(zone) {
            return off;
        }
        self.tz_offset_fn
            .and_then(|f| f(zone, utc_micros))
            .unwrap_or(0)
    }

    /// v7.39 (round 524) — the session, cloned for a write path's
    /// evaluation context. See [`crate::eval::DmlSession`].
    pub(crate) fn dml_session(&self) -> crate::eval::DmlSession {
        crate::eval::DmlSession {
            gucs: self.session_params.clone(),
            users: self.users.clone(),
            render_style: self.render_style,
            tz_offset_fn: self.tz_offset_fn,
            tz_localize_fn: self.tz_localize_fn,
            tz_abbrev_fn: self.tz_abbrev_fn,
        }
    }

    /// v7.39 (round 523) — the session facts an assignment into a
    /// column is read under: the zone a naive timestamp names a
    /// wall-clock reading in, and the order an ambiguous date is read
    /// with. `None` when both are the defaults.
    ///
    /// The INSERT path evaluates VALUES through a context-free literal
    /// walker with no `EvalContext`, so it takes these as an argument
    /// the way it already takes the dialect.
    /// v7.39 (round 524) — the date order joined it: the same
    /// context-free walker read every written date as MDY.
    pub(crate) fn session_coercion(&self) -> Option<crate::eval::SessionCoercion> {
        let zone = self
            .session_params
            .get("timezone")
            .filter(|z| !z.eq_ignore_ascii_case("utc") && !z.eq_ignore_ascii_case("gmt"))
            .cloned();
        let order = self.render_style.date_order;
        if zone.is_none() && order == crate::eval::DateOrder::Mdy {
            return None;
        }
        Some(crate::eval::SessionCoercion {
            zone,
            localize: self.tz_localize_fn,
            order,
        })
    }

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
            // v7.39 (read01 round 58) — and the role store, so the privilege
            // builtins can expand role membership.
            .with_users(&self.users)
            // v7.39 (read01 round 63) — and the engine itself, so a user
            // function whose body has its own FROM can run that body through
            // the real executor (visibility filter and all).
            .with_engine(self)
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
            .with_wal_lsn_fn(self.wal_lsn_fn)
            // v7.39 (round 318, V51) — and the connection-control hook, so
            // pg_cancel_backend / pg_terminate_backend really signal.
            .with_backend_signal_fn(self.backend_signal_fn)
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

/// v7.39 (round 522) — the unit PG counts a GUC in.
///
/// PG keeps a parameter's value in TWO forms and they are not the same
/// string: `pg_settings.setting` is a bare number counting `unit`s
/// (`work_mem` → `4096`, unit `kB`), while SHOW / `current_setting`
/// render the human form (`4MB`). Measured on PG18: a value with no
/// suffix is already in the GUC's unit, so `SET work_mem = 8192` and
/// `= '8MB'` are the same setting.
///
/// One table so the SET-time normaliser and `pg_settings` cannot drift
/// apart on which parameters carry a unit — round 515 spent a round
/// re-syncing two copies of a list like this one.
pub(crate) fn guc_unit(name: &str) -> Option<&'static str> {
    match name {
        "statement_timeout"
        | "lock_timeout"
        | "idle_in_transaction_session_timeout"
        | "idle_session_timeout"
        | "transaction_timeout" => Some("ms"),
        "work_mem" | "maintenance_work_mem" => Some("kB"),
        // Counted in BLOCKS, and PG names the block size as the unit.
        "shared_buffers" | "temp_buffers" | "effective_cache_size" | "wal_buffers" => Some("8kB"),
        _ => None,
    }
}

/// The bare count `pg_settings.setting` reports for a stored value —
/// the inverse of the human form SHOW renders. `None` when the
/// parameter has no unit or the value does not parse, and the caller
/// keeps the string it already had.
pub(crate) fn guc_raw_setting(name: &str, stored: &str) -> Option<String> {
    match guc_unit(name)? {
        "ms" => parse_pg_duration_ms(stored).map(|ms| alloc::format!("{ms}")),
        "kB" => parse_pg_mem_kb(stored).map(|kb| alloc::format!("{kb}")),
        // A block count, so the kB reading divides by the block size.
        "8kB" => parse_pg_mem_kb(stored).map(|kb| alloc::format!("{}", kb / 8)),
        _ => None,
    }
}

/// v7.39 (round 204) — parse a PG memory-size GUC value to a count of
/// KILOBYTES (work_mem's base unit). Accepts a bare integer (already
/// kB) or a `<n><unit>` with unit B/kB/MB/GB/TB. `None` on malformed
/// input so the caller keeps the raw string.
pub(crate) fn parse_pg_mem_kb(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let lowered = s.to_ascii_lowercase();
    let (num_part, mult_kb): (&str, u64) = if let Some(p) = lowered.strip_suffix("tb") {
        (p, 1024 * 1024 * 1024)
    } else if let Some(p) = lowered.strip_suffix("gb") {
        (p, 1024 * 1024)
    } else if let Some(p) = lowered.strip_suffix("mb") {
        (p, 1024)
    } else if let Some(p) = lowered.strip_suffix("kb") {
        (p, 1)
    } else if let Some(p) = lowered.strip_suffix('b') {
        // bytes → kB only when a whole multiple of 1024.
        let n: u64 = p.trim().parse().ok()?;
        return if n % 1024 == 0 { Some(n / 1024) } else { None };
    } else {
        (lowered.as_str(), 1)
    };
    let n: u64 = num_part.trim().parse().ok()?;
    n.checked_mul(mult_kb)
}

/// v7.39 (round 204) — render a kB count the way PG's SHOW does: the
/// largest binary unit that divides it evenly.
fn render_pg_mem_kb(kb: u64) -> String {
    use alloc::format;
    if kb == 0 {
        return String::from("0");
    }
    if kb % (1024 * 1024) == 0 {
        format!("{}GB", kb / (1024 * 1024))
    } else if kb % 1024 == 0 {
        format!("{}MB", kb / 1024)
    } else {
        format!("{kb}kB")
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

        /// v7.39 (round 621) — `client_min_messages` decided nothing: the GUC
        /// was validated on the way in (round 204) and then never read, so
        /// `SET client_min_messages = warning` — and even `= error` — left
        /// every `DROP … IF EXISTS` notice coming.
        ///
        /// The wire half of this was checked against live PG18 over seven
        /// shapes and matches byte for byte; what is pinned here is the
        /// decision itself, which is the part that can drift.
        #[test]
        fn client_min_messages_gates_by_pg_severity_order() {
            use crate::NoticeSeverity::{Notice, Warning};
            let mut e = Engine::new();
            // The default is `notice`: both severities reach the client.
            assert!(e.notice_severity_reaches_client(Notice));
            assert!(e.notice_severity_reaches_client(Warning));

            e.execute("SET client_min_messages = warning").unwrap();
            assert!(!e.notice_severity_reaches_client(Notice));
            assert!(e.notice_severity_reaches_client(Warning));

            for above in ["error", "fatal", "panic"] {
                e.execute(&alloc::format!("SET client_min_messages = {above}"))
                    .unwrap();
                assert!(!e.notice_severity_reaches_client(Notice), "{above}");
                assert!(!e.notice_severity_reaches_client(Warning), "{above}");
            }

            // Everything at or below `notice` lets both through — PG's order
            // is debug5 < … < log < notice < warning < error < fatal < panic.
            for below in ["notice", "log", "debug1", "debug5"] {
                e.execute(&alloc::format!("SET client_min_messages = {below}"))
                    .unwrap();
                assert!(e.notice_severity_reaches_client(Notice), "{below}");
                assert!(e.notice_severity_reaches_client(Warning), "{below}");
            }

            // Case is not the caller's problem, and RESET is the road back.
            e.execute("SET client_min_messages = WARNING").unwrap();
            assert!(!e.notice_severity_reaches_client(Notice));
            e.execute("RESET client_min_messages").unwrap();
            assert!(e.notice_severity_reaches_client(Notice));

            // And an out-of-domain value is still refused rather than
            // silently taken as some default.
            assert!(e.execute("SET client_min_messages = bogus_zz").is_err());
        }
    }
}