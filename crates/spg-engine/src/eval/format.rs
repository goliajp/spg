//! Canonical PG text representation of typed values + date/time literal
//! parsing, split out of `eval.rs` (cut 31). The value→text formatters
//! (`format_date` / `format_timestamp` / `format_timestamptz` /
//! `format_time` / `format_timetz` / `format_money` / `format_interval`
//! / `format_numeric` and the array formatters) plus the inverse
//! `parse_date_literal` / `parse_timestamp_literal` text→value parsers
//! with their TZ-suffix helpers. `civil_from_days` stays in `eval.rs`
//! (shared with the date SQL functions there); the calendar-arithmetic
//! helpers (`add_months_to_civil` / `days_in_month`) stay alongside
//! `shift_date_by_months` in `eval.rs`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{MONTH_ABBR, MONTH_FULL, civil_from_days};

// ---- v7.39 (GUC knife 3) — render styles ----
//
// PG's DateStyle / IntervalStyle / extra_float_digits GUCs change the
// TEXT of date/timestamp/interval/float output everywhere the out-
// functions run (wire cells, COPY, ::text casts). The engine caches a
// parsed `RenderStyle` per session; formatters take it by reference.
// Every shape below is verified against live PG18 (knife-3 probe).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DateOrder {
    Mdy,
    Dmy,
    Ymd,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DateStyleKind {
    Iso,
    German,
    Sql,
    Postgres,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntervalStyleKind {
    Postgres,
    SqlStandard,
    Iso8601,
    PostgresVerbose,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RenderStyle {
    pub date_style: DateStyleKind,
    pub date_order: DateOrder,
    pub interval_style: IntervalStyleKind,
    /// PG default 1: >= 1 means shortest-round-trip output; 0 and
    /// negative trim to 15+n (float8) / 6+n (float4) significant digits.
    pub extra_float_digits: i32,
    /// v7.39 (round 524) — the session asked for `bytea_output =
    /// 'escape'`. PG's other bytea form: printable bytes as themselves, a
    /// backslash doubled, everything else three-digit octal. SPG accepted
    /// the SET and rendered hex regardless, so a client that asked for
    /// escape got the form it had just said it did not want.
    pub bytea_escape: bool,
    /// v7.39 (round 368, M20 P3) — the session is on the MySQL dialect, so
    /// a binary string (`Value::Bytes`, e.g. a `0x…` literal) renders as
    /// its raw bytes read latin-1 (`0x41` → 'A', `CONCAT(0x41,'B')` →
    /// 'AB'), not as PG's `\x…` hex form.
    pub mysql: bool,
}

impl Default for RenderStyle {
    fn default() -> Self {
        Self {
            date_style: DateStyleKind::Iso,
            date_order: DateOrder::Mdy,
            interval_style: IntervalStyleKind::Postgres,
            extra_float_digits: 1,
            bytea_escape: false,
            mysql: false,
        }
    }
}

/// Day-of-week abbreviation. 1970-01-01 (day 0) was a Thursday.
fn dow_abbr(days: i32) -> &'static str {
    const DOW: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    DOW[((days.rem_euclid(7)) as usize + 3) % 7]
}

/// `HH:MM:SS[.frac]` with the 6-digit fraction's trailing zeros
/// stripped — the intra-day part every timestamp style shares.
fn hms_from_day_micros(day_micros: i64) -> String {
    let secs = day_micros / 1_000_000;
    let frac = day_micros % 1_000_000;
    let hh = secs / 3600;
    let mm = (secs / 60) % 60;
    let ss = secs % 60;
    if frac == 0 {
        format!("{hh:02}:{mm:02}:{ss:02}")
    } else {
        let raw = format!("{frac:06}");
        let trimmed = raw.trim_end_matches('0');
        format!("{hh:02}:{mm:02}:{ss:02}.{trimmed}")
    }
}

/// `format_date` under a DateStyle. PG18 shapes:
/// ISO `2024-03-15` · German `15.03.2024` · SQL `03/15/2024` (DMY
/// `15/03/2024`) · Postgres `03-15-2024` (DMY `15-03-2024`). YMD field
/// order affects only ISO-irrelevant styles' INPUT; output falls back
/// to the MDY arrangement (as PG does).
pub fn format_date_styled(days: i32, style: &RenderStyle) -> String {
    if days == i32::MAX {
        return "infinity".into();
    }
    if days == i32::MIN {
        return "-infinity".into();
    }
    let (y, m, d) = civil_from_days(days);
    let (y, bc) = if y <= 0 { (1 - y, " BC") } else { (y, "") };
    let dmy = style.date_order == DateOrder::Dmy;
    match style.date_style {
        DateStyleKind::Iso => format!("{y:04}-{m:02}-{d:02}{bc}"),
        DateStyleKind::German => format!("{d:02}.{m:02}.{y:04}{bc}"),
        DateStyleKind::Sql => {
            if dmy {
                format!("{d:02}/{m:02}/{y:04}{bc}")
            } else {
                format!("{m:02}/{d:02}/{y:04}{bc}")
            }
        }
        DateStyleKind::Postgres => {
            if dmy {
                format!("{d:02}-{m:02}-{y:04}{bc}")
            } else {
                format!("{m:02}-{d:02}-{y:04}{bc}")
            }
        }
    }
}

/// `format_timestamp` under a DateStyle. German/SQL prepend their date
/// form; Postgres style is the asctime-like
/// `Dow Mon DD HH:MM:SS[.f] YYYY` (DMY: `Dow DD Mon …`).
pub fn format_timestamp_styled(micros: i64, style: &RenderStyle) -> String {
    if micros == i64::MAX {
        return "infinity".into();
    }
    if micros == i64::MIN {
        return "-infinity".into();
    }
    if style.date_style == DateStyleKind::Iso {
        return format_timestamp(micros);
    }
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    let days = micros.div_euclid(MICROS_PER_DAY);
    let day_micros = micros.rem_euclid(MICROS_PER_DAY);
    let day_i32 = i32::try_from(days).unwrap_or(i32::MAX);
    let hms = hms_from_day_micros(day_micros);
    match style.date_style {
        DateStyleKind::Iso => unreachable!("handled above"),
        DateStyleKind::German | DateStyleKind::Sql => {
            // format_date_styled carries the BC suffix on the date part;
            // move it after the time to match PG.
            let d = format_date_styled(day_i32, style);
            match d.strip_suffix(" BC") {
                Some(base) => format!("{base} {hms} BC"),
                None => format!("{d} {hms}"),
            }
        }
        DateStyleKind::Postgres => {
            let (y, m, d) = civil_from_days(day_i32);
            let (y, bc) = if y <= 0 { (1 - y, " BC") } else { (y, "") };
            let mon = MONTH_ABBR[(m as usize).saturating_sub(1).min(11)];
            let dow = dow_abbr(day_i32);
            if style.date_order == DateOrder::Dmy {
                format!("{dow} {d} {mon} {hms} {y:04}{bc}")
            } else {
                format!("{dow} {mon} {d} {hms} {y:04}{bc}")
            }
        }
    }
}

/// `format_timestamptz` under a DateStyle for the UTC session: ISO
/// keeps the `+00` offset suffix; the other styles append the zone
/// NAME (` UTC`), as PG does.
pub fn format_timestamptz_styled(micros: i64, style: &RenderStyle) -> String {
    format_timestamptz_tz(micros, style, 0, None)
}

/// v7.39 (tz epic) — the session-timezone-aware renderer: shift by the
/// per-value offset; ISO appends the numeric offset (`+09`, `+05:45`),
/// the other DateStyles append the zone designation — a named zone's
/// abbreviation (`JST`, `EDT`), a fixed offset's numeric form, or
/// `UTC` (all PG18-differential).
pub fn format_timestamptz_tz(
    micros: i64,
    style: &RenderStyle,
    offset_micros: i64,
    abbr: Option<&str>,
) -> String {
    if style.date_style == DateStyleKind::Iso {
        return format_timestamptz_at(micros, offset_micros);
    }
    if micros == i64::MAX || micros == i64::MIN {
        return format_timestamp(micros);
    }
    let body = format_timestamp_styled(micros + offset_micros, style);
    match abbr {
        Some(a) => format!("{body} {a}"),
        None if offset_micros == 0 => format!("{body} UTC"),
        None => {
            let total_min = (offset_micros / 60_000_000).abs();
            let (h, m) = (total_min / 60, total_min % 60);
            let sign = if offset_micros < 0 { '-' } else { '+' };
            if m == 0 {
                format!("{body} {sign}{h:02}")
            } else {
                format!("{body} {sign}{h:02}:{m:02}")
            }
        }
    }
}

/// `H:MM:SS[.frac]` — the sql_standard time body (hour unpadded),
/// over non-negative micros.
fn sql_std_time(abs_us: i64) -> String {
    let secs = abs_us / 1_000_000;
    let frac = abs_us % 1_000_000;
    let h = secs / 3600;
    let mm = (secs / 60) % 60;
    let ss = secs % 60;
    if frac == 0 {
        format!("{h}:{mm:02}:{ss:02}")
    } else {
        let raw = format!("{frac:06}");
        let trimmed = raw.trim_end_matches('0');
        format!("{h}:{mm:02}:{ss:02}.{trimmed}")
    }
}

/// `SS[.frac]` seconds body for iso_8601 / verbose fields.
fn secs_body(abs_us: i64) -> String {
    let ss = abs_us / 1_000_000;
    let frac = abs_us % 1_000_000;
    if frac == 0 {
        format!("{ss}")
    } else {
        let raw = format!("{frac:06}");
        let trimmed = raw.trim_end_matches('0');
        format!("{ss}.{trimmed}")
    }
}

/// `format_interval` under an IntervalStyle (PG18 differential truth):
///
/// - sql_standard — pure year-month `1-2` / `-1-2`; pure day-time
///   `1 0:00:00` (sign on the leading field, time fields absolute;
///   time-only `2:00:00` / `-0:00:01`); a mix of year-month and
///   day-time classes (or mixed signs) is non-conforming and prints
///   every part explicitly signed: `+1-2 +3 +4:05:06`, `+0-1 -1
///   +0:00:00`; zero → `0`.
/// - iso_8601 — `P[nY][nM][nD][T[nH][nM][nS]]`, each field
///   individually signed, zero → `PT0S`.
/// - postgres_verbose — `@ ` + `years/mons/days/hours/mins/secs`
///   fields; when the interval compares below zero every field is
///   negated and ` ago` is appended; zero → `@ 0`.
/// Render an interval that may be infinite.
///
/// v7.38.19 — PostgreSQL 18.4 renders the two infinities as the words
/// `infinity` and `-infinity`, in every interval style, and reads them
/// back (`'infinity'::text::interval` round-trips). `'inf'` is NOT
/// accepted, which is the one place interval differs from float here.
#[must_use]
pub fn format_interval_kinded(
    months: i32,
    days: i32,
    micros: i64,
    kind: spg_storage::IntervalKind,
) -> String {
    match kind {
        spg_storage::IntervalKind::PosInf => alloc::string::String::from("infinity"),
        spg_storage::IntervalKind::NegInf => alloc::string::String::from("-infinity"),
        spg_storage::IntervalKind::Finite => format_interval(months, days, micros),
    }
}

pub fn format_interval_styled(months: i32, days: i32, micros: i64, style: &RenderStyle) -> String {
    match style.interval_style {
        IntervalStyleKind::Postgres => format_interval(months, days, micros),
        IntervalStyleKind::SqlStandard => {
            let has_ym = months != 0;
            let has_dt = days != 0 || micros != 0;
            if !has_ym && !has_dt {
                return "0".into();
            }
            let y = months / 12;
            let mo = (months % 12).abs();
            // Sign coherence: every nonzero class must agree for the
            // conforming shapes; a year-month + day-time mix is always
            // the signed non-conforming shape.
            let signs: Vec<i8> = [i64::from(months), i64::from(days), micros]
                .iter()
                .filter(|v| **v != 0)
                .map(|v| if *v < 0 { -1i8 } else { 1 })
                .collect();
            let coherent = signs.windows(2).all(|w| w[0] == w[1]);
            if has_ym && !has_dt && coherent {
                return format!("{y}-{mo}");
            }
            if !has_ym && coherent {
                let neg = days < 0 || micros < 0;
                let time = sql_std_time(micros.abs());
                if days == 0 {
                    return format!("{}{time}", if neg { "-" } else { "" });
                }
                return format!("{days} {time}");
            }
            // Non-conforming: every part explicitly signed, absolute
            // field bodies.
            let sgn = |neg: bool| if neg { '-' } else { '+' };
            format!(
                "{}{}-{} {}{} {}{}",
                sgn(months < 0),
                y.abs(),
                mo,
                sgn(days < 0),
                days.abs(),
                sgn(micros < 0),
                sql_std_time(micros.abs())
            )
        }
        IntervalStyleKind::Iso8601 => {
            if months == 0 && days == 0 && micros == 0 {
                return "PT0S".into();
            }
            let y = months / 12;
            let mo = months % 12;
            let mut out = String::from("P");
            if y != 0 {
                out.push_str(&format!("{y}Y"));
            }
            if mo != 0 {
                out.push_str(&format!("{mo}M"));
            }
            if days != 0 {
                out.push_str(&format!("{days}D"));
            }
            if micros != 0 {
                out.push('T');
                let neg = micros < 0;
                let abs = micros.abs();
                let h = abs / 3_600_000_000;
                let m = (abs / 60_000_000) % 60;
                let s_us = abs % 60_000_000;
                let sgn = if neg { "-" } else { "" };
                if h != 0 {
                    out.push_str(&format!("{sgn}{h}H"));
                }
                if m != 0 {
                    out.push_str(&format!("{sgn}{m}M"));
                }
                if s_us != 0 {
                    out.push_str(&format!("{sgn}{}S", secs_body(s_us)));
                }
            }
            out
        }
        IntervalStyleKind::PostgresVerbose => {
            if months == 0 && days == 0 && micros == 0 {
                return "@ 0".into();
            }
            // PG's comparison convention (months at 30 days) decides
            // the overall sign; a negative interval prints its fields
            // negated with a trailing ` ago`.
            let total = i128::from(months) * 30 * 86_400_000_000
                + i128::from(days) * 86_400_000_000
                + i128::from(micros);
            let ago = total < 0;
            let (months, days, micros) = if ago {
                (-months, -days, -micros)
            } else {
                (months, days, micros)
            };
            let y = months / 12;
            let mo = months % 12;
            let neg_t = micros < 0;
            let abs = micros.abs();
            let h = abs / 3_600_000_000;
            let m = (abs / 60_000_000) % 60;
            let s_us = abs % 60_000_000;
            let mut parts: Vec<String> = Vec::new();
            let unit = |n: i64, singular: &'static str| -> String {
                if n == 1 {
                    singular.into()
                } else {
                    format!("{singular}s")
                }
            };
            if y != 0 {
                parts.push(format!("{y} {}", unit(i64::from(y), "year")));
            }
            if mo != 0 {
                parts.push(format!("{mo} {}", unit(i64::from(mo), "mon")));
            }
            if days != 0 {
                parts.push(format!("{days} {}", unit(i64::from(days), "day")));
            }
            let tsgn = if neg_t { "-" } else { "" };
            if h != 0 {
                parts.push(format!("{tsgn}{h} {}", unit(h, "hour")));
            }
            if m != 0 {
                parts.push(format!("{tsgn}{m} {}", unit(m, "min")));
            }
            if s_us != 0 {
                let body = secs_body(s_us);
                let plural = body != "1";
                parts.push(format!(
                    "{tsgn}{body} {}",
                    if plural { "secs" } else { "sec" }
                ));
            }
            let mut out = String::from("@ ");
            out.push_str(&parts.join(" "));
            if ago {
                out.push_str(" ago");
            }
            out
        }
    }
}

/// Styled array renderers — `{elem,…}` with per-element styled text.
pub fn format_date_array_styled(items: &[Option<i32>], style: &RenderStyle) -> String {
    array_styled(items, |d| format_date_styled(*d, style))
}

pub fn format_timestamp_array_styled(
    items: &[Option<i64>],
    with_tz: bool,
    style: &RenderStyle,
) -> String {
    if with_tz {
        array_styled(items, |t| format_timestamptz_styled(*t, style))
    } else {
        array_styled(items, |t| format_timestamp_styled(*t, style))
    }
}

pub fn format_interval_array_styled(
    items: &[Option<spg_storage::IntervalSpan>],
    style: &RenderStyle,
) -> String {
    array_styled(items, |iv| {
        if iv.kind.is_finite() {
            format_interval_styled(iv.months, iv.days, iv.micros, style)
        } else {
            format_interval_kinded(0, 0, 0, iv.kind)
        }
    })
}

pub fn format_float_array_styled(items: &[Option<f64>], style: &RenderStyle) -> String {
    array_styled(items, |f| format_float_styled(*f, style))
}

fn array_styled<T>(items: &[Option<T>], mut f: impl FnMut(&T) -> String) -> String {
    let mut out = String::with_capacity(2 + items.len() * 12);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(v) => push_array_element(&mut out, &f(v)),
        }
    }
    out.push('}');
    out
}

/// v7.39 (read01 round 73) — PG's array output QUOTES an element whose text
/// contains a delimiter, a brace, a quote, a backslash or whitespace — so an
/// interval array reads `{"1 day",02:00:00}`, not `{1 day,02:00:00}`. This lived
/// only in the text-array renderer; every typed array shared `array_styled`,
/// which never quoted, because none of the types it rendered had ever produced a
/// space. `array_agg(interval)` does — and the differential caught it the moment
/// round 73 gave that aggregate its real element type.
fn push_array_element(out: &mut String, s: &str) {
    let needs_quote = s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars()
            .any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\') || c.is_whitespace());
    if !needs_quote {
        out.push_str(s);
        return;
    }
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
}

/// C `%.{prec}g` over an f64 — what PG's float8out/float4out emit when
/// `extra_float_digits <= 0`: `prec` significant digits, fixed-point
/// when the decimal exponent is in `-4..prec`, else scientific;
/// trailing zeros trimmed in both shapes; exponent `e±NN`.
fn format_g(x: f64, prec: usize) -> String {
    let prec = prec.max(1);
    // Round to `prec` significant digits via {:.*e} (exact decimal
    // mantissa; handles the 9.99→10.0 exponent carry).
    let sci = format!("{:.*e}", prec - 1, x);
    let epos = sci.find('e').expect("{:e} always has an 'e'");
    let exp_val: i32 = sci[epos + 1..].parse().unwrap_or(0);
    let mant = &sci[..epos];
    if exp_val >= -4 && (exp_val as i64) < prec as i64 {
        // Fixed-point with prec-1-exp decimals, from the rounded value.
        let decimals =
            usize::try_from(i64::try_from(prec).unwrap_or(1) - 1 - i64::from(exp_val)).unwrap_or(0);
        let rounded: f64 = sci.parse().unwrap_or(x);
        let fixed = format!("{rounded:.decimals$}");
        if fixed.contains('.') {
            let t = fixed.trim_end_matches('0').trim_end_matches('.');
            t.into()
        } else {
            fixed
        }
    } else {
        let mant = if mant.contains('.') {
            mant.trim_end_matches('0').trim_end_matches('.')
        } else {
            mant
        };
        let (sign, digits) = if exp_val < 0 {
            ('-', format!("{}", -exp_val))
        } else {
            ('+', format!("{exp_val}"))
        };
        format!("{mant}e{sign}{digits:0>2}")
    }
}

/// PG `float8out` under `extra_float_digits`: >= 1 → shortest
/// round-trip (`format_float`); <= 0 → `%.{15+n}g`.
pub fn format_float_styled(x: f64, style: &RenderStyle) -> String {
    if style.mysql {
        return format_float_mysql(x);
    }
    if style.extra_float_digits >= 1 {
        return format_float(x);
    }
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let prec = (15 + style.extra_float_digits).clamp(1, 17) as usize;
    format_g(x, prec)
}

/// v7.39.2 — MySQL's float rendering, measured on 9.7.2.
///
/// Two rules differ from PostgreSQL's, and both were visible on every
/// non-trivial value: MySQL prints a FLOAT to SIX significant digits
/// (`3.14159265358979` reads back `3.14159`, where SPG printed the f32
/// shortest round-trip `3.1415927`), and it stays in FIXED notation for
/// a much wider band of exponents — `[-15, 14]` for both widths, so
/// `123456789` reads `123457000` rather than `1.2345679e+08` and `1e15`
/// reads `1e15` rather than `1000000000000000`.
///
/// A DOUBLE's digits already agreed (MySQL is shortest-round-trip there
/// too: `0.1+0.2` prints all seventeen), so only the window moves.
///
/// The exponent form carries no `+` and no zero padding: `1e15`,
/// `1.23457e15`, `1e-16`, `1.7976931348623157e308`.
fn format_mysql_float(sci: &str, mant: &str, exp_val: i32) -> String {
    if (-15..=14).contains(&exp_val) {
        // Trailing zeros go, in both shapes: the six-digit rounding
        // leaves them (`0.1` came out `0.100000`, `1` came out
        // `1.00000`) and MySQL prints neither.
        let fixed = fixed_from_sci(sci, exp_val);
        if fixed.contains('.') {
            return alloc::string::String::from(fixed.trim_end_matches('0').trim_end_matches('.'));
        }
        return fixed;
    }
    let mant = if mant.contains('.') {
        mant.trim_end_matches('0').trim_end_matches('.')
    } else {
        mant
    };
    alloc::format!("{mant}e{exp_val}")
}

/// Split a `{:e}`-shaped string into its mantissa and exponent.
fn split_sci(sci: &str) -> (&str, i32) {
    let epos = sci.find('e').expect("{:e} always has an 'e'");
    (&sci[..epos], sci[epos + 1..].parse().unwrap_or(0))
}

/// PG `float4out` under `extra_float_digits`: >= 1 → shortest
/// round-trip (`format_real`); <= 0 → `%.{6+n}g`.
///
/// On a MySQL session neither applies — see `format_mysql_float`.
pub fn format_real_mysql(x: f32) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.into();
    }
    // Six significant digits, then MySQL's window.
    let sci = alloc::format!("{:.*e}", 5, f64::from(x));
    let (mant, exp_val) = split_sci(&sci);
    format_mysql_float(&sci, mant, exp_val)
}

/// The f64 half of the same rule — see `format_real_mysql`.
pub fn format_float_mysql(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.into();
    }
    // The digits are the shortest round-trip, as MySQL's are; only the
    // fixed/scientific window differs from PostgreSQL's.
    let sci = shortest_float_sci(x);
    let (mant, exp_val) = split_sci(&sci);
    format_mysql_float(&sci, mant, exp_val)
}

pub fn format_real_styled(x: f32, style: &RenderStyle) -> String {
    if style.mysql {
        return format_real_mysql(x);
    }
    if style.extra_float_digits >= 1 {
        return format_real(x);
    }
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let prec = (6 + style.extra_float_digits).clamp(1, 9) as usize;
    format_g(f64::from(x), prec)
}

/// Render a `Date` (days since epoch) as `YYYY-MM-DD`. Negative values
/// for pre-1970 dates render with a leading `-` on the year.
pub fn format_date(days: i32) -> String {
    if days == i32::MAX {
        return "infinity".into();
    }
    if days == i32::MIN {
        return "-infinity".into();
    }
    let (y, m, d) = civil_from_days(days);
    // v7.39 (GUC knife 6, BC) — PG renders astronomical year <= 0 as
    // the positive year + " BC" (1 BC is astronomical year 0).
    if y <= 0 {
        return format!("{:04}-{m:02}-{d:02} BC", 1 - y);
    }
    format!("{y:04}-{m:02}-{d:02}")
}

/// Render a `Timestamp` (microseconds since epoch) as
/// `YYYY-MM-DD HH:MM:SS[.fff...]`. Trailing-zero fractional digits are
/// dropped; a whole-second value has no fractional part.
/// v7.15.0 — PG-canonical TIMESTAMPTZ wire format. Storage is
/// the same i64 microseconds UTC as TIMESTAMP, but the canonical
/// PG text output appends the session's UTC-offset suffix (`+00`
/// for the default UTC session, the form pg_dump emits). Mailrs
/// round-8 acceptance criterion: `SELECT col FROM tstz` should
/// round-trip to a literal that re-INSERTs without semantic
/// drift.
pub fn format_timestamptz(micros: i64) -> String {
    format_timestamptz_at(micros, 0)
}

/// v7.38 (T-tstz Phase 2) — render a UTC instant in a fixed-offset zone: shift
/// the wall clock by `offset_micros`, then append PG's offset suffix (`+09`,
/// `-05`, `+05:30`, `+00`). Minutes are shown only when non-zero, matching PG.
/// UTC (`offset_micros == 0`) reproduces the old `+00` output byte-for-byte.
pub fn format_timestamptz_at(micros: i64, offset_micros: i64) -> String {
    if micros == i64::MAX || micros == i64::MIN {
        return format_timestamp(micros);
    }
    let base = format_timestamp(micros + offset_micros);
    // v7.39 (GUC knife 6, BC) — the offset suffix goes before " BC"
    // (PG: `0044-03-15 10:20:30+00 BC`).
    let (base, bc) = match base.strip_suffix(" BC") {
        Some(b) => (String::from(b), " BC"),
        None => (base, ""),
    };
    let mut s = String::with_capacity(base.len() + 9);
    s.push_str(&base);
    let total_min = (offset_micros / 60_000_000).abs();
    let (h, m) = (total_min / 60, total_min % 60);
    s.push(if offset_micros < 0 { '-' } else { '+' });
    s.push_str(&alloc::format!("{h:02}"));
    if m != 0 {
        s.push(':');
        s.push_str(&alloc::format!("{m:02}"));
    }
    s.push_str(bc);
    s
}

/// v7.17.0 Phase 3.P0-35 — PG `money` canonical text form, en_US
/// PG `float8out` — the shortest round-trip decimal, rendered in
/// scientific notation when the base-10 exponent is `< -4` or `> 14`
/// (matching float.c's choice), otherwise fixed. Learned from read01
/// float.c study: PG switches to `1e+15` / `1e-05` where SPG used to
/// spell every digit (`1000000000000000000000000000000`). The exponent
/// is read from Rust's `{:e}` (exact — avoids log10 rounding at powers
/// of ten) and reformatted to PG's `e±NN` (sign always shown, ≥ 2
/// digits). Infinities / NaN / signed zero match `float8out` too.
pub fn format_float(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let sci = shortest_float_sci(x); // e.g. "1.234e15", "1e-5", "-2.5e-10"
    let epos = sci.find('e').expect("{:e} always has an 'e'");
    let exp_val: i32 = sci[epos + 1..].parse().unwrap_or(0);
    if (-4..=14).contains(&exp_val) {
        // Fixed-point rendering of the SAME digits the strict test
        // chose — `{x}` would re-derive them under Rust's rule.
        return fixed_from_sci(&sci, exp_val);
    }
    let mant = &sci[..epos];
    let exp = &sci[epos + 1..];
    let (sign, digits) = match exp.strip_prefix('-') {
        Some(d) => ('-', d),
        None => ('+', exp),
    };
    alloc::format!("{mant}e{sign}{digits:0>2}")
}

/// v7.39 (round 270) — the shortest decimal for an f32 in PG's sense,
/// as `{:e}` style ("1.5000001e10").
///
/// Rust's `{:e}` gives the shortest decimal that ROUND-TRIPS. PG wants
/// the shortest that lies STRICTLY INSIDE the value's rounding
/// interval. The two differ exactly on the values whose short form sits
/// on a half-ulp boundary: ties-to-even parses that boundary back to
/// the same float, so Rust accepts it, while PG does not. Measured on
/// PG 18.4: `15000000512::real` prints 1.5000001e+10, not 1.5e+10 —
/// 1.5e10 is exactly half an ulp below the value. The same rule shows
/// up in float8 (`1e23` prints 9.999999999999999e+22).
///
/// The boundary test is exact here because an f32 and its neighbour
/// both widen losslessly into f64 and their midpoint needs only 25
/// bits, so the midpoint is an f64 value that a <= 9-digit decimal
/// parses to exactly when — and only when — it IS that midpoint.
fn shortest_real_sci(x: f32) -> String {
    let wide = f64::from(x);
    let below = f64::from(next_f32(x, false));
    let above = f64::from(next_f32(x, true));
    // Midpoints to either neighbour; these are exact in f64.
    let lo = (wide + below) / 2.0;
    let hi = (wide + above) / 2.0;
    for p in 1..=9u32 {
        let cand = alloc::format!("{x:.*e}", (p - 1) as usize);
        let Ok(v) = cand.parse::<f64>() else { continue };
        // Round-trips as an f32 AND is not sitting on either boundary.
        #[allow(clippy::cast_possible_truncation)]
        if v as f32 == x && v != lo && v != hi {
            return cand;
        }
    }
    alloc::format!("{x:e}")
}

/// The adjacent f32 toward +inf (`up`) or -inf. Only ever called on a
/// finite non-zero value.
fn next_f32(x: f32, up: bool) -> f32 {
    let bits = x.to_bits();
    let stepped = if (x > 0.0) == up { bits + 1 } else { bits - 1 };
    f32::from_bits(stepped)
}

/// Re-render a `{:e}`-style string in fixed-point notation.
fn fixed_from_sci(sci: &str, exp: i32) -> String {
    let epos = sci.find('e').expect("{:e} always has an 'e'");
    let (mant, _) = sci.split_at(epos);
    let (sign, mant) = match mant.strip_prefix('-') {
        Some(m) => ("-", m),
        None => ("", mant),
    };
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    let point = exp + 1; // digits before the decimal point
    let mut out = String::from(sign);
    if point <= 0 {
        out.push_str("0.");
        for _ in 0..-point {
            out.push('0');
        }
        out.push_str(&digits);
    } else if (point as usize) >= digits.len() {
        out.push_str(&digits);
        for _ in 0..(point as usize - digits.len()) {
            out.push('0');
        }
    } else {
        out.push_str(&digits[..point as usize]);
        out.push('.');
        out.push_str(&digits[point as usize..]);
    }
    out
}

/// v7.38 (read01, T-float4) — PG `float4out`: the f32 shortest round-trip, in
/// fixed-point for decimal exponents in `-4..=5` and scientific otherwise
/// (a tighter window than float8's `-4..=14`, so `12345678::real` =
/// `1.2345678e+07` while `12345678::float8` stays `12345678`).
pub fn format_real(x: f32) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let sci = shortest_real_sci(x);
    let epos = sci.find('e').expect("{:e} always has an 'e'");
    let exp_val: i32 = sci[epos + 1..].parse().unwrap_or(0);
    if (-4..=5).contains(&exp_val) {
        // Re-expand the (possibly longer than Rust's) mantissa in
        // fixed-point rather than falling back to `{x}`, which would
        // reintroduce Rust's choice of digits.
        return fixed_from_sci(&sci, exp_val);
    }
    let mant = &sci[..epos];
    let exp = &sci[epos + 1..];
    let (sign, digits) = match exp.strip_prefix('-') {
        Some(d) => ('-', d),
        None => ('+', exp),
    };
    alloc::format!("{mant}e{sign}{digits:0>2}")
}

/// locale: `$N,NNN.CC`, negative → `-$1.23`. Mirrors PG's
/// `cash_out` for `lc_monetary = 'en_US.UTF-8'`.
pub fn format_money(cents: i64) -> String {
    let neg = cents < 0;
    let abs = cents.unsigned_abs();
    let dollars = abs / 100;
    let cc = abs % 100;
    // Insert comma thousands separators in the integer portion.
    let dollar_str = dollars.to_string();
    let bytes = dollar_str.as_bytes();
    let mut int_part = String::with_capacity(dollar_str.len() + dollar_str.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        // Position from the right: insert ',' before every 3rd
        // digit (except the first).
        let from_right = bytes.len() - i;
        if i > 0 && from_right % 3 == 0 {
            int_part.push(',');
        }
        int_part.push(*b as char);
    }
    let sign = if neg { "-" } else { "" };
    format!("{sign}${int_part}.{cc:02}")
}

/// v7.17.0 Phase 3.P0-34 — PG `TIMETZ` canonical text form
/// `HH:MM:SS[.ffffff]±HH[:MM]`. Mirrors PG `timetz_out`. The
/// offset uses `±HH` for whole-hour offsets and `±HH:MM` for
/// sub-hour offsets (matching PG's "minimal display" rule).
pub fn format_timetz(us: i64, offset_secs: i32) -> String {
    let time = format_time(us);
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let abs = offset_secs.unsigned_abs();
    let oh = abs / 3600;
    let om = (abs % 3600) / 60;
    if om == 0 {
        format!("{time}{sign}{oh:02}")
    } else {
        format!("{time}{sign}{oh:02}:{om:02}")
    }
}

/// v7.17.0 Phase 3.P0-32 — PG `TIME` canonical text form
/// `HH:MM:SS[.ffffff]`. Mirrors PG `time_out`. Trailing zeros in
/// the fractional component are stripped — `12:00:00.500000`
/// renders as `12:00:00.5` to match PG's text output.
pub fn format_time(us: i64) -> String {
    let total_secs = us.div_euclid(1_000_000);
    let frac = us.rem_euclid(1_000_000);
    let hh = total_secs / 3600;
    let mm = (total_secs / 60) % 60;
    let ss = total_secs % 60;
    if frac == 0 {
        format!("{hh:02}:{mm:02}:{ss:02}")
    } else {
        let raw = format!("{frac:06}");
        let trimmed = raw.trim_end_matches('0');
        format!("{hh:02}:{mm:02}:{ss:02}.{trimmed}")
    }
}

pub fn format_timestamp(micros: i64) -> String {
    // PG infinity sentinels.
    if micros == i64::MAX {
        return "infinity".into();
    }
    if micros == i64::MIN {
        return "-infinity".into();
    }
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    // Split into day + intra-day part with proper floor division so
    // negative timestamps render right too.
    let days = micros.div_euclid(MICROS_PER_DAY);
    let day_micros = micros.rem_euclid(MICROS_PER_DAY);
    let day_i32 = i32::try_from(days).unwrap_or(i32::MAX);
    let (y, m, d) = civil_from_days(day_i32);
    // v7.39 (GUC knife 6, BC) — " BC" trails the TIME part in PG.
    let (y, bc) = if y <= 0 { (1 - y, " BC") } else { (y, "") };
    let secs = day_micros / 1_000_000;
    let frac = day_micros % 1_000_000;
    let hh = secs / 3600;
    let mm = (secs / 60) % 60;
    let ss = secs % 60;
    if frac == 0 {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}{bc}")
    } else {
        // Strip trailing zeros from the 6-digit fractional component.
        let raw = format!("{frac:06}");
        let trimmed = raw.trim_end_matches('0');
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{trimmed}{bc}")
    }
}

/// Inverse of `civil_from_days` — converts (year, month, day) to days
/// since 1970-01-01. Out-of-range months / days saturate.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i32 {
    let y_adj = if m <= 2 {
        i64::from(y) - 1
    } else {
        i64::from(y)
    };
    let era = y_adj.div_euclid(400);
    let yoe = (y_adj - era * 400) as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d.saturating_sub(1);
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let total = era * 146_097 + i64::from(doe) - 719_468;
    i32::try_from(total).unwrap_or(i32::MAX)
}

/// Parse `YYYY-MM-DD` into a `Date` (days since Unix epoch). Returns
/// `None` on shape / numeric failure; the engine surfaces that as a
/// `TypeMismatch` with the original text included.
pub fn parse_date_literal(s: &str) -> Option<i32> {
    parse_date_literal_ordered(s, DateOrder::Mdy)
}

/// v7.39 (GUC knife 5) — DateOrder-aware date input. PG disambiguates
/// a three-field numeric date (`01/02/2024`, `02.01.2024`, `1/2/24`)
/// by the DateStyle field order: MDY reads month first (the default —
/// so `'01/02/2024'` is Jan 2 even with no SET), DMY day first, YMD
/// year first (`'24/01/02'`, and `'1/2/24'` is 2001-02-24!). A
/// two-digit year is < 70 → 20xx, >= 70 → 19xx. Field values that
/// don't fit the order error (MDY `'13/02/2024'` is out of range —
/// PG does NOT auto-swap). ISO year-first four-digit forms parse the
/// same under every order.
pub fn parse_date_literal_ordered(s: &str, order: DateOrder) -> Option<i32> {
    let s = s.trim();
    // v7.39 (GUC knife 6, BC) — a trailing era marker: `NNNN-MM-DD BC`
    // maps year N to astronomical year 1-N (there is no year zero);
    // an explicit AD is accepted and is the default.
    if let Some(base) = s
        .strip_suffix(" BC")
        .or_else(|| s.strip_suffix(" bc"))
        .or_else(|| s.strip_suffix(" Bc"))
    {
        let days = parse_date_literal_ordered(base, order)?;
        let (y, m, d) = civil_from_days(days);
        if y < 1 {
            return None;
        }
        return Some(days_from_civil(1 - y, m, d));
    }
    if let Some(base) = s.strip_suffix(" AD").or_else(|| s.strip_suffix(" ad")) {
        return parse_date_literal_ordered(base, order);
    }
    // PG special date values.
    if s.eq_ignore_ascii_case("epoch") {
        return Some(days_from_civil(1970, 1, 1));
    }
    if s.eq_ignore_ascii_case("infinity") || s.eq_ignore_ascii_case("+infinity") {
        return Some(i32::MAX);
    }
    if s.eq_ignore_ascii_case("-infinity") {
        return Some(i32::MIN);
    }
    let bytes = s.as_bytes();
    // ISO 8601 basic (compact) form `YYYYMMDD` — no separators.
    if bytes.len() == 8 && bytes.iter().all(u8::is_ascii_digit) {
        let y: i32 = s[0..4].parse().ok()?;
        let m: u32 = s[4..6].parse().ok()?;
        let d: u32 = s[6..8].parse().ok()?;
        if !(1..=12).contains(&m) || d < 1 || d > super::days_in_month(y, m) {
            return None;
        }
        return Some(days_from_civil(y, m, d));
    }
    // v7.38 (read01) — month-name forms in any of PG's field orders
    // (`Jan 5, 2020`, `5 Jan 2020`, `2020-Jan-05`, `5-Jan-2020`), case
    // insensitive. Try this before the numeric split so a `Mon-D-Y` dashed
    // form isn't mistaken for a numeric field.
    if s.bytes().any(|b| b.is_ascii_alphabetic()) {
        // v7.39 (read01 utils/adt, datetime.c) — 'J2451545' is a Julian
        // day number (JD of the Unix epoch is 2440588).
        if let Some(jd) = s.strip_prefix(['J', 'j'])
            && !jd.is_empty()
            && jd.bytes().all(|b| b.is_ascii_digit())
        {
            let jd: i64 = jd.parse().ok()?;
            return i32::try_from(jd - 2_440_588).ok();
        }
        return parse_month_name_date(s, order);
    }
    // v7.38 (read01) — year-first numeric form with `-`, `/` or `.` separators
    // and non-zero-padded month/day (`2020-1-5`, `2020/01/5`, `2020.1.05`), all
    // of which PG accepts. Requires exactly three all-digit fields, the first
    // being the 4-digit year, so it stays unambiguous (no MDY/DMY guessing).
    // v7.39 (read01 utils/adt, datetime.c) — the day-of-year form:
    // YEAR + exactly-three-digit ordinal ('2024-060' = Feb 29 2024).
    {
        let mut two = s.splitn(2, ['-', '/', '.']);
        if let (Some(ya), Some(dd)) = (two.next(), two.next())
            && ya.len() >= 3
            && dd.len() == 3
            && !dd.contains(['-', '/', '.', ' '])
            && ya.bytes().all(|b| b.is_ascii_digit())
            && dd.bytes().all(|b| b.is_ascii_digit())
        {
            let y: i32 = ya.parse().ok()?;
            let doy: i64 = dd.parse().ok()?;
            if y != 0 && (1..=366).contains(&doy) {
                let jan1 = days_from_civil(y, 1, 1);
                let days = jan1 + i32::try_from(doy).ok()? - 1;
                let (yy, _, _) = civil_from_days(days);
                if yy == y {
                    return Some(days);
                }
                return None; // 366 in a non-leap year
            }
        }
    }
    let mut parts = s.splitn(3, ['-', '/', '.']);
    let (fa, fb, fc) = (parts.next()?, parts.next()?, parts.next()?);
    if fc.contains(['-', '/', '.', ' ']) {
        return None; // trailing separator / extra field / garbage
    }
    if [fa, fb, fc]
        .iter()
        .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    // Year-first: a 3-or-more-digit first field is unambiguously the
    // year regardless of DateOrder (PG DecodeNumber's flen >= 3 rule:
    // `2020-1-5`, `123-4-5` = 0123-04-05).
    if fa.len() >= 3 && fb.len() <= 2 && fc.len() <= 2 {
        let y: i32 = fa.parse().ok()?;
        // No year zero in the Gregorian era notation (PG: out of range).
        if y == 0 {
            return None;
        }
        // DOY form: exactly-three-digit second field after a year with
        // no third field is handled below (two-field split); with three
        // fields this is plain Y-M-D.
        let m: u32 = fb.parse().ok()?;
        let d: u32 = fc.parse().ok()?;
        // PG validates the day against the actual (leap-aware) month
        // length: `'2024-02-30'` raises "date/time field value out of
        // range" rather than rolling forward into March.
        if !(1..=12).contains(&m) || d < 1 || d > super::days_in_month(y, m) {
            return None;
        }
        return Some(days_from_civil(y, m, d));
    }
    // Order-disambiguated short forms.
    let expand_year = |t: &str| -> Option<i32> {
        match t.len() {
            4 => t.parse().ok(),
            // PG's two-digit-year window.
            1 | 2 => {
                let n: i32 = t.parse().ok()?;
                Some(if n < 70 { 2000 + n } else { 1900 + n })
            }
            _ => None,
        }
    };
    let (ys, ms, ds) = match order {
        DateOrder::Mdy => (fc, fa, fb),
        DateOrder::Dmy => (fc, fb, fa),
        DateOrder::Ymd => (fa, fb, fc),
    };
    if ms.len() > 2 || ds.len() > 2 {
        return None;
    }
    let y = expand_year(ys)?;
    let m: u32 = ms.parse().ok()?;
    let d: u32 = ds.parse().ok()?;
    if !(1..=12).contains(&m) || d < 1 || d > super::days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// v7.38 (read01) — resolve a month-name date in any of PG's field orders.
/// Tokenises on space / comma / dash, then classifies exactly one month name,
/// one 4-digit year, and one 1–2 digit day, in any order. Case insensitive;
/// both `Jan` and `January` spellings. Leap-aware day validation, like the
/// numeric path. Returns `None` for anything ambiguous or malformed so the
/// caller raises the same "invalid input" / "out of range" errors as PG.
fn parse_month_name_date(s: &str, order: DateOrder) -> Option<i32> {
    let tokens: alloc::vec::Vec<&str> =
        s.split([' ', ',', '-']).filter(|t| !t.is_empty()).collect();
    if tokens.len() != 3 {
        return None;
    }
    let month_of = |t: &str| -> Option<u32> {
        let up = t.to_ascii_uppercase();
        MONTH_ABBR
            .iter()
            .position(|a| a.eq_ignore_ascii_case(&up))
            .or_else(|| MONTH_FULL.iter().position(|f| f.eq_ignore_ascii_case(&up)))
            .map(|i| i as u32 + 1)
    };
    let mut month: Option<u32> = None;
    let mut nums: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
    for t in tokens {
        if let Some(m) = month_of(t) {
            if month.replace(m).is_some() {
                return None; // two month names
            }
        } else if t.bytes().all(|b| b.is_ascii_digit()) {
            nums.push(t);
        } else {
            return None; // a non-month alphabetic token (`Foo`)
        }
    }
    let m = month?;
    if nums.len() != 2 {
        return None;
    }
    // v7.39 (read01 utils/adt, datetime.c DecodeNumber) — with a text
    // month, a 3+-digit numeric field is the YEAR; two short fields
    // disambiguate by DateOrder ('Jan-23-24' is day 23 / year 2024
    // under MDY/DMY, year 2023 / day 24 under YMD). Two-digit years
    // take PG's 1970-2069 window.
    let (ys, ds) = match (nums[0].len() >= 3, nums[1].len() >= 3) {
        (true, true) => return None,
        (true, false) => (nums[0], nums[1]),
        (false, true) => (nums[1], nums[0]),
        (false, false) => {
            if order == DateOrder::Ymd {
                (nums[0], nums[1])
            } else {
                (nums[1], nums[0])
            }
        }
    };
    let mut y: i32 = ys.parse().ok()?;
    if ys.len() <= 2 {
        y += if y < 70 { 2000 } else { 1900 };
    }
    let d: u32 = ds.parse().ok()?;
    if !(1..=12).contains(&m) || d < 1 || d > super::days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Parse `YYYY-MM-DD[ HH:MM:SS[.ffffff]]` into a `Timestamp`
/// (microseconds since Unix epoch). The time portion is optional;
/// missing → midnight. The fractional portion accepts 1–6 digits and
/// pads with zeros to microseconds.
pub fn parse_timestamp_literal(s: &str) -> Option<i64> {
    parse_timestamp_literal_ordered(s, DateOrder::Mdy)
}

/// v7.39 (GUC knife 5) — true when `s` tokenises as a numeric date shape
/// (three digit fields, or the 8-digit compact form) whose FIELDS parse but
/// whose values fail the calendar range checks — PG reports these as
/// `date/time field value out of range` (with a DateStyle hint), while
/// non-date-shaped text gets `invalid input syntax for type date`.
pub fn date_text_is_field_shaped(s: &str) -> bool {
    let s = s.trim();
    let date_part = match s.find([' ', 'T']) {
        Some(i) => &s[..i],
        None => s,
    };
    let b = date_part.as_bytes();
    if b.len() == 8 && b.iter().all(u8::is_ascii_digit) {
        return true;
    }
    let fields: alloc::vec::Vec<&str> = date_part.split(['-', '/', '.']).collect();
    fields.len() == 3
        && fields
            .iter()
            .all(|f| !f.is_empty() && f.len() <= 4 && f.bytes().all(|c| c.is_ascii_digit()))
}

/// v7.39 (GUC knife 5) — DateOrder-aware timestamp input; the date
/// part follows `parse_date_literal_ordered`'s disambiguation.
pub fn parse_timestamp_literal_ordered(s: &str, order: DateOrder) -> Option<i64> {
    parse_timestamp_literal_tz_ordered(s, order).map(|(us, _)| us)
}

/// v7.39 (tz epic) — like `parse_timestamp_literal_ordered` but also
/// reports whether the literal carried an explicit offset. A naive
/// literal's micros are the WALL clock (caller localises against the
/// session zone for a timestamptz); an offset-bearing literal's are
/// UTC already.
pub fn parse_timestamp_literal_tz_ordered(s: &str, order: DateOrder) -> Option<(i64, bool)> {
    if let Some(v) = timestamp_sentinel(s) {
        return Some((v, true));
    }
    let (days, day_micros, tz) = parse_timestamp_parts(s, order)?;
    let t = i64::from(days)
        .checked_mul(86_400_000_000)?
        .checked_add(day_micros)?
        .checked_sub(tz.unwrap_or(0))?;
    Some((t, tz.is_some()))
}

/// v7.39 (round 289) — the pieces every timestamp-literal reader needs:
/// the day number, the LOCAL clock inside that day, and the zone offset
/// the literal carried (if any). Split out so the wall-clock reader and
/// the UTC reader cannot drift apart — the first attempt duplicated the
/// body and promptly lost the `BC` era handling.
fn parse_timestamp_parts(s: &str, order: DateOrder) -> Option<(i32, i64, Option<i64>)> {
    let trimmed = s.trim();
    // PG special timestamp values. `infinity` / `-infinity` use the i64
    // sentinels (they compare greater/less than every finite timestamp).
    if trimmed.eq_ignore_ascii_case("epoch") {
        return Some((0, 0, Some(0)));
    }
    // The infinity sentinels are whole i64 instants, not a day+offset
    // pair, so they cannot ride the parts shape — the two public
    // readers below special-case them before calling here.
    if trimmed.eq_ignore_ascii_case("infinity")
        || trimmed.eq_ignore_ascii_case("+infinity")
        || trimmed.eq_ignore_ascii_case("-infinity")
    {
        return None;
    }
    // v7.39 (GUC knife 6, BC) — the era marker trails the TIME part
    // (`0044-03-15 10:20:30 BC`); strip it and re-map the parsed date.
    let (trimmed, era_bc) = match trimmed
        .strip_suffix(" BC")
        .or_else(|| trimmed.strip_suffix(" bc"))
    {
        Some(b) => (b.trim_end(), true),
        None => (
            trimmed
                .strip_suffix(" AD")
                .or_else(|| trimmed.strip_suffix(" ad"))
                .map_or(trimmed, str::trim_end),
            false,
        ),
    };
    let (date_part, time_part) = match trimmed.find([' ', 'T']) {
        Some(i) => (&trimmed[..i], Some(&trimmed[i + 1..])),
        None => (trimmed, None),
    };
    // v7.39 (round 662) — a date may carry the zone directly, with no time
    // between them: PG reads `'2020-01-01+00'::timestamptz` as midnight in
    // that zone. SPG had no split point there — the scan above looks for a
    // space or a `T` — so the whole string went to the date parser and came
    // back `invalid input syntax`. The zone cannot simply be scanned for,
    // because an ISO date is full of hyphens; the split is accepted only
    // when the prefix parses as a date AND the suffix as a zone.
    if time_part.is_none() && parse_date_literal_ordered(date_part, order).is_none() {
        if let Some(rest) = date_part.strip_suffix(['Z', 'z']) {
            if let Some(d) = parse_date_literal_ordered(rest, order) {
                return Some((d, 0, Some(0)));
            }
        }
        // Only `+`. PG REFUSES `'2020-01-01-05'` — measured — because a
        // trailing `-05` cannot be told apart from the date's own hyphens,
        // and it would rather reject than guess. The first version here
        // accepted it and answered `2020-01-01 05:00:00+00`, i.e. invented
        // an instant PG declines to name.
        for (i, c) in date_part.char_indices().rev() {
            if c != '+' {
                continue;
            }
            let (head, tail) = date_part.split_at(i);
            let (Some(d), Some(off)) = (
                parse_date_literal_ordered(head, order),
                parse_tz_offset_suffix(tail, c == '+'),
            ) else {
                continue;
            };
            return Some((d, 0, Some(off)));
        }
    }
    let mut days = parse_date_literal_ordered(date_part, order)?;
    if era_bc {
        let (y, m, d) = civil_from_days(days);
        if y < 1 {
            return None;
        }
        days = days_from_civil(1 - y, m, d);
    }
    let (day_micros, tz_offset) = match time_part {
        None => (0, None),
        Some(t) => parse_time_of_day_micros_tz(t)?,
    };
    Some((days, day_micros, tz_offset))
}

/// v7.39 (round 289) — the WALL-CLOCK value a literal spells, with any
/// zone designation ignored.
///
/// PG drops the zone when the target type has none: `'2020-01-01
/// 10:00:00+02'::timestamp` is `10:00:00`, not the `08:00:00` you get by
/// converting to UTC. SPG converted, so the cast returned a DIFFERENT
/// INSTANT with no error — the silent-wrong shape. It also refused a
/// named zone outright, where PG accepts and ignores it.
///
/// This is the same parse; it just does not apply the offset (the time
/// parser already reports the local clock separately) and strips a
/// trailing named zone the numeric-offset scanner cannot see.
/// A NAMED zone (`… America/New_York`) is not stripped here: PG
/// validates it — `'… Bogus/Zone'::timestamp` is `time zone
/// "bogus/zone" not recognized`, not a silent drop — and the zone
/// database lives on the host, not in this no_std crate. That case
/// still errors, which is wrong but LOUD; the numeric-offset case was
/// wrong and silent, which is why it is the one fixed here.
pub fn parse_timestamp_literal_wall_ordered(s: &str, order: DateOrder) -> Option<i64> {
    if let Some(v) = timestamp_sentinel(s) {
        return Some(v);
    }
    // Ignoring the offset is simply not applying it: the parts reader
    // already reports the LOCAL clock separately.
    let (days, day_micros, _tz) = parse_timestamp_parts(s, order)?;
    i64::from(days)
        .checked_mul(86_400_000_000)?
        .checked_add(day_micros)
}

/// `epoch` / `±infinity`, which are whole instants rather than a
/// date-and-time to assemble.
fn timestamp_sentinel(s: &str) -> Option<i64> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("epoch") {
        return Some(0);
    }
    if t.eq_ignore_ascii_case("infinity") || t.eq_ignore_ascii_case("+infinity") {
        return Some(i64::MAX);
    }
    if t.eq_ignore_ascii_case("-infinity") {
        return Some(i64::MIN);
    }
    None
}

/// v7.15.0 — Parse `HH:MM:SS[.frac][<tz>]` and return
/// `(day_micros, tz_offset_micros)` where `day_micros` is the
/// local-clock seconds-of-day in microseconds and
/// `tz_offset_micros` is the UTC offset (positive = east of
/// UTC, negative = west). Caller subtracts the offset to
/// normalise to UTC. PG's recognised TZ shapes after the
/// seconds (or frac) part:
///   * `+OO[:MM]` / `-OO[:MM]` — numeric offset
///   * `+OOMM` / `-OOMM` (no colon, less common but legal)
///   * ` UTC` / `UTC` / `Z` — explicit zero offset
/// Anything else after the seconds = parse failure (the caller
/// v7.39 (round 324, V42) — PG's message for a text literal that will not
/// become a date/time value. Two shapes, measured on PG 18.4:
///
///   * `invalid input syntax for type <t>: "<text>"` — the text is not
///     date/time shaped at all (`not-a-date`, `2020`, `2020-01-01 abc`);
///   * `date/time field value out of range: "<text>"` — it IS shaped like
///     one but a field's value is impossible (`2020-13-01`, `2020-02-30`,
///     `2020-01-01 25:00:00`).
///
/// The second form carries `HINT: Perhaps you need a different "DateStyle"
/// setting.` only when the offending field is a month or day outside its
/// UNIVERSAL range — the case a different field order could have
/// explained. `2020-02-30` (a day that is fine for the field but not for
/// that month), a time-of-day overflow and an oversized year get no hint.
/// The wire splits the `\nHINT:  ` tail into the ErrorResponse `H` field.
#[must_use]
pub(crate) fn datetime_input_error_text(text: &str, type_name: &str) -> alloc::string::String {
    let (kind, hint) = classify_datetime_input(text);
    match kind {
        DatetimeInputProblem::Syntax => {
            alloc::format!("invalid input syntax for type {type_name}: \"{text}\"")
        }
        DatetimeInputProblem::OutOfRange => {
            let mut m = alloc::format!("date/time field value out of range: \"{text}\"");
            if hint {
                m.push_str("\nHINT:  Perhaps you need a different \"DateStyle\" setting.");
            }
            m
        }
    }
}

enum DatetimeInputProblem {
    Syntax,
    OutOfRange,
}

/// `(problem, wants_datestyle_hint)` for a literal that failed to parse.
fn classify_datetime_input(text: &str) -> (DatetimeInputProblem, bool) {
    let t = text.trim();
    // Any character outside the date/time alphabet means PG never got as
    // far as reading a field value.
    if t.is_empty()
        || !t
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | '.' | ' ' | 'T' | '+' | 'Z'))
    {
        return (DatetimeInputProblem::Syntax, false);
    }
    // The date part is everything before the first space or `T`.
    let date_part = t.split([' ', 'T']).next().unwrap_or("");
    let fields: alloc::vec::Vec<&str> = date_part.split('-').collect();
    if fields.len() != 3
        || fields
            .iter()
            .any(|f| f.is_empty() || !f.chars().all(|c| c.is_ascii_digit()))
    {
        return (DatetimeInputProblem::Syntax, false);
    }
    // Shaped like a date; a month or day outside its universal range is
    // the case a different DateStyle could have explained.
    let month = fields[1].parse::<u32>().unwrap_or(0);
    let day = fields[2].parse::<u32>().unwrap_or(0);
    let field_out_of_range = !(1..=12).contains(&month) || !(1..=31).contains(&day);
    (DatetimeInputProblem::OutOfRange, field_out_of_range)
}

/// surfaces as "cannot parse … as TIMESTAMP").
fn parse_time_of_day_micros(t: &str) -> Option<(i64, i64)> {
    parse_time_of_day_micros_tz(t).map(|(us, tz)| (us, tz.unwrap_or(0)))
}

/// v7.39 (tz epic) — like `parse_time_of_day_micros` but reports
/// whether the literal carried an explicit offset (`None` = naive; a
/// timestamptz cast then interprets the wall clock in the session
/// zone, like PG).
fn parse_time_of_day_micros_tz(t: &str) -> Option<(i64, Option<i64>)> {
    let t = t.trim();
    // Detect & strip optional TZ suffix. Anchor on the first
    // `+` / `-` AFTER position 8 (so the leading sign on a
    // negative offset can't be mistaken for an `HH:MM:SS-OO`
    // boundary if the time itself is somehow malformed).
    // ` UTC` and trailing `Z` also count as zero-offset TZ tags.
    let (core, tz_micros) = if let Some(rest) = t.strip_suffix('Z') {
        (rest, Some(0i64))
    } else if let Some(rest) = t.strip_suffix(" UTC").or_else(|| t.strip_suffix("UTC")) {
        (rest, Some(0i64))
    } else if let Some((idx, sign_byte)) = find_offset_sign(t) {
        let suffix = &t[idx..];
        let micros = parse_tz_offset_suffix(suffix, sign_byte == b'+')?;
        (&t[..idx], Some(micros))
    } else {
        (t, None)
    };
    let (time, frac_str) = match core.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (core, None),
    };
    let bytes = time.as_bytes();
    // PG accepts both `HH:MM:SS` and the seconds-optional `HH:MM`
    // form in a TIMESTAMP literal (`'2024-01-15 10:30'::timestamp`
    // → `10:30:00`); hour-only (`'... 10'`) stays a parse error.
    let (hh, mm, ss): (i64, i64, i64) = if bytes.len() == 8 && bytes[2] == b':' && bytes[5] == b':'
    {
        (
            time[0..2].parse().ok()?,
            time[3..5].parse().ok()?,
            time[6..8].parse().ok()?,
        )
    } else if bytes.len() == 5 && bytes[2] == b':' {
        (time[0..2].parse().ok()?, time[3..5].parse().ok()?, 0)
    } else {
        return None;
    };
    if !(0..24).contains(&hh) || !(0..60).contains(&mm) || !(0..60).contains(&ss) {
        return None;
    }
    let frac_micros: i64 = match frac_str {
        None => 0,
        Some(f) => {
            // Pad right with zeros to 6 digits, then truncate extras.
            if f.is_empty() || f.len() > 9 {
                return None;
            }
            let mut padded = String::with_capacity(6);
            padded.push_str(&f[..f.len().min(6)]);
            while padded.len() < 6 {
                padded.push('0');
            }
            padded.parse().ok()?
        }
    };
    Some((
        ((hh * 3600 + mm * 60 + ss) * 1_000_000) + frac_micros,
        tz_micros,
    ))
}

/// Find the index of the TZ-offset sign byte (`+` or `-`) that
/// terminates an `HH:MM:SS[.fff]` time string, or `None` when
/// the time carries no numeric TZ suffix. Anchors past the first
/// 8 bytes (`HH:MM:SS`) so the seconds/minutes colons don't
/// confuse the scan.
fn find_offset_sign(t: &str) -> Option<(usize, u8)> {
    let bytes = t.as_bytes();
    // Start past `HH:MM` (5 bytes) — the seconds-optional literal
    // (`'2024-07-15 12:00+00'`) carries its offset at index 5; a
    // time body itself never contains `+`/`-`.
    if bytes.len() < 6 {
        return None;
    }
    for i in 5..bytes.len() {
        match bytes[i] {
            b'+' | b'-' => return Some((i, bytes[i])),
            _ => {}
        }
    }
    None
}

/// Parse `+OO`, `+OO:MM`, `+OOMM`, `-OO`, `-OO:MM`, `-OOMM` into
/// a UTC-offset microsecond delta. `is_positive` reflects the
/// already-stripped sign.
fn parse_tz_offset_suffix(suffix: &str, is_positive: bool) -> Option<i64> {
    // suffix starts with `+` or `-`; strip it.
    let body = &suffix[1..];
    let (hh, mm): (i64, i64) = if let Some((h, m)) = body.split_once(':') {
        (h.parse().ok()?, m.parse().ok()?)
    } else {
        match body.len() {
            2 => (body.parse().ok()?, 0),
            3 => {
                // PG's "+0530" form lacks the colon; but a 3-char
                // body is `OOM` which is ambiguous (`+053` ?). PG
                // doesn't emit that; reject.
                return None;
            }
            4 => {
                let h: i64 = body[0..2].parse().ok()?;
                let m: i64 = body[2..4].parse().ok()?;
                (h, m)
            }
            _ => return None,
        }
    };
    if !(0..=18).contains(&hh) || !(0..60).contains(&mm) {
        return None;
    }
    let abs = (hh * 3600 + mm * 60) * 1_000_000;
    Some(if is_positive { abs } else { -abs })
}

/// Render an `Interval { months, days, micros }` in a PG-ish shape.
/// The output mirrors `psql`'s text format: years/months from the
/// months part, days from its own dimension (no carry from micros —
/// this is the PG-canonical separation so `'1 day'` ≠ `'24 hours'`),
/// HH:MM:SS[.frac] from micros. v7.37.5 β added the `days` parameter
/// for PG byte-equal; `micros` may still carry hours ≥ 24 (PG keeps
/// the unnormalised form on the wire).
pub fn format_interval(months: i32, days: i32, micros: i64) -> String {
    let mut parts: Vec<String> = Vec::new();
    let years = months / 12;
    let mons = months % 12;
    // PG renders the unit in the singular only for `+1`; `-1` and any
    // other value pluralise. Helper closes over that rule.
    let unit = |n: i64, singular: &'static str, plural: &'static str| -> &'static str {
        if n == 1 { singular } else { plural }
    };
    // PG shows an explicit `+` on a positive field that follows a
    // negative one (`-2 mons +3 days`), so mixed signs stay readable.
    let mut prev_negative = false;
    if years != 0 {
        parts.push(format!(
            "{years} {}",
            unit(i64::from(years), "year", "years")
        ));
        prev_negative = years < 0;
    }
    if mons != 0 {
        let plus = if prev_negative && mons > 0 { "+" } else { "" };
        parts.push(format!(
            "{plus}{mons} {}",
            unit(i64::from(mons), "mon", "mons")
        ));
        prev_negative = mons < 0;
    }
    if days != 0 {
        let plus = if prev_negative && days > 0 { "+" } else { "" };
        parts.push(format!(
            "{plus}{days} {}",
            unit(i64::from(days), "day", "days")
        ));
    }
    let mut rem = micros;
    if rem != 0 {
        let neg = rem < 0;
        if neg {
            rem = -rem;
        }
        let secs = rem / 1_000_000;
        let frac = rem % 1_000_000;
        let hh = secs / 3600;
        let mm = (secs / 60) % 60;
        let ss = secs % 60;
        // PG shows an explicit `+` on the time part when a preceding date
        // field was negative but the time itself is positive, e.g.
        // `-1 days +02:00:00`. `is_before` = the last-printed date field's
        // sign.
        let is_before = if days != 0 {
            days < 0
        } else if mons != 0 {
            mons < 0
        } else {
            years < 0
        };
        let sign = if neg {
            "-"
        } else if is_before {
            "+"
        } else {
            ""
        };
        if frac == 0 {
            parts.push(format!("{sign}{hh:02}:{mm:02}:{ss:02}"));
        } else {
            let raw = format!("{frac:06}");
            let trimmed = raw.trim_end_matches('0');
            parts.push(format!("{sign}{hh:02}:{mm:02}:{ss:02}.{trimmed}"));
        }
    }
    if parts.is_empty() {
        // PG renders a zero interval as `00:00:00`, not `0`.
        "00:00:00".into()
    } else {
        parts.join(" ")
    }
}

/// v7.10.9 — render a TEXT[] in PG's external array form
/// (`{a,b,NULL}`). Elements containing whitespace, commas,
/// quotes, or braces get double-quoted with `\\` / `\"` escapes.
/// NULL elements use the literal token `NULL`. Public so the
/// wire layer can produce the canonical text-mode encoding.
pub fn format_text_array(items: &[Option<String>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 8);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(s) => {
                // PG array_out quotes an element containing any structural
                // char or any whitespace `array_isspace` recognises — space,
                // tab, newline, carriage return, vertical tab, form feed —
                // not just space/tab.
                let needs_quote = s.is_empty()
                    || s.eq_ignore_ascii_case("NULL")
                    || s.chars().any(|c| {
                        matches!(
                            c,
                            ',' | '{'
                                | '}'
                                | '"'
                                | '\\'
                                | ' '
                                | '\t'
                                | '\n'
                                | '\r'
                                | '\x0b'
                                | '\x0c'
                        )
                    });
                if needs_quote {
                    out.push('"');
                    for c in s.chars() {
                        if c == '"' || c == '\\' {
                            out.push('\\');
                        }
                        out.push(c);
                    }
                    out.push('"');
                } else {
                    out.push_str(s);
                }
            }
        }
    }
    out.push('}');
    out
}

/// v7.11.14 — render an INT[] in PG's external array form
/// (`{1,2,NULL}`). Integer payloads never need quoting. NULL
/// elements use the literal token `NULL`.
pub fn format_int_array(items: &[Option<i32>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 4);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(n) => out.push_str(&n.to_string()),
        }
    }
    out.push('}');
    out
}

/// v7.11.14 — render a BIGINT[] in PG's external array form
/// (`{1,2,NULL}`).
pub fn format_bigint_array(items: &[Option<i64>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 6);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(n) => out.push_str(&n.to_string()),
        }
    }
    out.push('}');
    out
}

/// v7.37.5 γ — render a BOOL[] in PG external form.
/// PG uses single-letter `t` / `f` for booleans (matching the
/// scalar wire convention).
pub fn format_bool_array(items: &[Option<bool>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 2);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(b) => out.push(if *b { 't' } else { 'f' }),
        }
    }
    out.push('}');
    out
}

/// v7.37.5 γ — render a SMALLINT[] in PG external form.
pub fn format_smallint_array(items: &[Option<i16>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 4);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(n) => out.push_str(&n.to_string()),
        }
    }
    out.push('}');
    out
}

/// v7.37.5 γ — render a FLOAT[] / DOUBLE PRECISION[] in PG
/// external form. PG renders floats via the engine's existing
/// f64 → text path so this just calls Rust's default Display.
pub fn format_float_array(items: &[Option<f64>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 8);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            // PG float8[] elements use float8out too — scientific past
            // the exponent thresholds (`{1e+30,2}`), not every digit.
            Some(x) => out.push_str(&format_float(*x)),
        }
    }
    out.push('}');
    out
}

/// v7.37.5 γ — render a NUMERIC[] in PG external form.
pub fn format_numeric_array(items: &[Option<(i128, u16)>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 6);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some((scaled, scale)) => out.push_str(&format_numeric(*scaled, *scale)),
        }
    }
    out.push('}');
    out
}

/// v7.37.5 γ — render a DATE[] in PG external form. Each
/// non-NULL element is rendered as `YYYY-MM-DD`.
pub fn format_date_array(items: &[Option<i32>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 12);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(d) => out.push_str(&format_date(*d)),
        }
    }
    out.push('}');
    out
}

/// v7.37.5 γ — render a TIMESTAMP[] (`with_tz=false`) or
/// TIMESTAMPTZ[] (`with_tz=true`) in PG external form. Each
/// non-NULL element is double-quoted because the canonical
/// timestamp text contains a space (`2024-06-01 12:00:00`)
/// that would otherwise split on commas wrong.
pub fn format_timestamp_array(items: &[Option<i64>], with_tz: bool) -> String {
    let mut out = String::with_capacity(2 + items.len() * 22);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(t) => {
                out.push('"');
                if with_tz {
                    out.push_str(&format_timestamptz(*t));
                } else {
                    out.push_str(&format_timestamp(*t));
                }
                out.push('"');
            }
        }
    }
    out.push('}');
    out
}

/// v7.37.5 γ — render a UUID[] in PG external form. UUID text is
/// the canonical lowercase 8-4-4-4-12 hyphenated form; no quoting
/// needed (hex + dashes, no spaces / commas).
pub fn format_uuid_array(items: &[Option<[u8; 16]>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 38);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(b) => out.push_str(&spg_storage::format_uuid(b)),
        }
    }
    out.push('}');
    out
}

/// v7.40.0 — REAL[] / TIME[] / TIMETZ[] / INET[] in PG external
/// form. Measured against PG 18.6: none of these element forms can
/// contain a character `array_out` quotes, so each is emitted bare.
/// `xml[]` is NOT here — an XML element can hold a comma, so it goes
/// through `format_text_array` with the rest of the string arrays.
fn bare_array<T, F: Fn(&T) -> String>(items: &[Option<T>], render: F) -> String {
    let mut out = String::with_capacity(2 + items.len() * 8);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(v) => out.push_str(&render(v)),
        }
    }
    out.push('}');
    out
}

pub fn format_real_array(items: &[Option<f32>], style: &RenderStyle) -> String {
    bare_array(items, |x| format_real_styled(*x, style))
}

pub fn format_time_array(items: &[Option<i64>]) -> String {
    bare_array(items, |us| format_time(*us))
}

pub fn format_timetz_array(items: &[Option<(i64, i32)>]) -> String {
    bare_array(items, |(us, off)| format_timetz(*us, *off))
}

pub fn format_inet_array(items: &[Option<(u8, u8, [u8; 16])>]) -> String {
    bare_array(items, |(family, bits, addr)| {
        crate::conversions::format_inet(*family, *bits, addr)
    })
}

/// v7.37.5 γ — render a BYTEA[] in PG external form. Each
/// non-NULL element is `\\x<hex>` (the PG hex output form) and
/// is double-quoted because the leading backslash is a PG array
/// escape character.
pub fn format_bytea_array(items: &[Option<Vec<u8>>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 8);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(b) => {
                out.push('"');
                let hex = format_bytea_hex(b);
                // Escape leading backslash (`\` → `\\`) per PG
                // array-element quoting rules.
                for c in hex.chars() {
                    if c == '\\' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('"');
            }
        }
    }
    out.push('}');
    out
}

/// v7.37.5 β-P4 — render an INTERVAL[] in PG's external array form.
/// Each non-NULL element is double-quoted because interval text
/// contains spaces (`1 day`) and colons (`24:00:00`) that would
/// confuse the comma-separated parse: `{"1 day","24:00:00",NULL}`.
/// Inner `"` doesn't occur in interval text so no escaping is
/// needed; backslashes likewise can't appear.
pub fn format_interval_array(items: &[Option<spg_storage::IntervalSpan>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 12);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(span) => {
                // v7.38.19 — an element can be infinite too, and the
                // word carries no comma or space, so PostgreSQL prints
                // it unquoted: `{infinity,"1 day"}`.
                if span.kind.is_finite() {
                    out.push('"');
                    out.push_str(&format_interval(span.months, span.days, span.micros));
                    out.push('"');
                } else {
                    out.push_str(&format_interval_kinded(0, 0, 0, span.kind));
                }
            }
        }
    }
    out.push('}');
    out
}

/// v7.10.4 — render a BYTEA payload in PG's hex output format
/// (`\x` prefix, lowercase hex pairs). Public so the wire layer
/// can emit the canonical bytea-as-text representation.
/// v7.39 (round 524) — PG's `escape` bytea form: a printable byte as
/// itself, a backslash doubled, everything else `\ooo` octal.
#[must_use]
pub fn format_bytea_escape(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    for &byte in b {
        match byte {
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(byte as char),
            _ => out.push_str(&alloc::format!("\\{byte:03o}")),
        }
    }
    out
}

pub fn format_bytea_hex(b: &[u8]) -> String {
    let mut out = String::with_capacity(2 + 2 * b.len());
    out.push_str("\\x");
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in b {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}

/// Render a `Numeric { scaled, scale }` as its decimal text form.
/// Negative `scaled` prepends `-` to the absolute value's digits; the
/// integer / fractional split is by character count, padding the
/// fractional side with leading zeros to exactly `scale` chars.
/// v7.38 (read01, T6) — render a NUMERIC honoring its special kind. Finite uses
/// `format_numeric`; the specials render PG's full-word spellings.
pub fn format_numeric_kind(kind: spg_storage::NumericKind, scaled: i128, scale: u16) -> String {
    use spg_storage::NumericKind;
    match kind {
        NumericKind::Finite => format_numeric(scaled, scale),
        NumericKind::NaN => String::from("NaN"),
        NumericKind::PosInf => String::from("Infinity"),
        NumericKind::NegInf => String::from("-Infinity"),
    }
}

pub fn format_numeric(scaled: i128, scale: u16) -> String {
    if scale == 0 {
        return format!("{scaled}");
    }
    let negative = scaled < 0;
    let mag_str = scaled.unsigned_abs().to_string();
    let mag_bytes = mag_str.as_bytes();
    let scale_u = scale as usize;
    let mut out = String::with_capacity(mag_str.len() + 3);
    if negative {
        out.push('-');
    }
    if mag_bytes.len() <= scale_u {
        out.push('0');
        out.push('.');
        for _ in mag_bytes.len()..scale_u {
            out.push('0');
        }
        out.push_str(&mag_str);
    } else {
        let split = mag_bytes.len() - scale_u;
        out.push_str(&mag_str[..split]);
        out.push('.');
        out.push_str(&mag_str[split..]);
    }
    out
}

/// v7.39 (round 292) — the shortest decimal for an f64 in PG's sense,
/// as `{:e}` style. The f64 sibling of `shortest_real_sci`.
///
/// Rust's `{:e}` gives the shortest decimal that ROUND-TRIPS; PG wants
/// the shortest that lies STRICTLY INSIDE the rounding interval. The
/// two differ exactly on values whose short form sits on a half-ulp
/// boundary — measured on PG 18.4, `1e23::float8` prints
/// `9.999999999999999e+22`, because 1e23 IS the boundary and
/// ties-to-even parses it back to the same double, so Rust accepts it.
///
/// The f32 version can test the boundary by widening to f64. There is
/// no wider float here, so the test is done in exact INTEGER
/// arithmetic instead: a midpoint is `M · 2^E` with M odd, a candidate
/// is `D · 10^K`, and the two are equal only if their odd parts and
/// their powers of two both match. That forces `5^|K|` to divide a
/// 57-bit number, which bounds |K| — so the whole comparison fits in
/// u128 and needs no bignum at all.
fn shortest_float_sci(x: f64) -> String {
    let (m, e) = f64_mantissa_exp(x);
    // Midpoints to the neighbours, as odd·2^exp. The gap BELOW a power
    // of two is half the gap above it, so that side needs one more bit.
    let (hi_m, hi_e) = (2 * m + 1, e - 1);
    let (lo_m, lo_e) = if m == 1 << 52 && e > f64_min_exp() {
        (4 * m - 1, e - 2)
    } else {
        (2 * m - 1, e - 1)
    };
    for p in 1..=17u32 {
        let cand = alloc::format!("{x:.*e}", (p - 1) as usize);
        let Ok(v) = cand.parse::<f64>() else { continue };
        if v != x {
            continue;
        }
        let Some((d, k)) = sci_to_digits_exp(&cand) else {
            continue;
        };
        if !decimal_eq_binary(d, k, hi_m, hi_e) && !decimal_eq_binary(d, k, lo_m, lo_e) {
            return cand;
        }
    }
    alloc::format!("{x:e}")
}

/// `x` as `mantissa · 2^exp` with the mantissa a positive integer.
/// Only called on finite non-zero values.
fn f64_mantissa_exp(x: f64) -> (u128, i32) {
    let bits = x.abs().to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let frac = u128::from(bits & 0x000f_ffff_ffff_ffff);
    if biased == 0 {
        (frac, -1074) // subnormal: no implicit leading bit
    } else {
        ((1u128 << 52) | frac, biased - 1075)
    }
}

/// The exponent of the smallest normal f64, below which the
/// gap-below-a-power-of-two rule no longer applies.
const fn f64_min_exp() -> i32 {
    -1074
}

/// Split a `{:e}` string into `(digits, exponent)` such that the value
/// is `digits · 10^exponent`. `None` when it does not fit u128 (which
/// cannot happen for the ≤ 17 digits generated here).
fn sci_to_digits_exp(sci: &str) -> Option<(u128, i32)> {
    let epos = sci.find('e')?;
    let (mant, rest) = sci.split_at(epos);
    let exp: i32 = rest[1..].parse().ok()?;
    let mant = mant.strip_prefix('-').unwrap_or(mant);
    let (int_part, frac_part) = match mant.split_once('.') {
        Some((a, b)) => (a, b),
        None => (mant, ""),
    };
    let mut digits: u128 = 0;
    for c in int_part.chars().chain(frac_part.chars()) {
        digits = digits
            .checked_mul(10)?
            .checked_add(u128::from(c as u8 - b'0'))?;
    }
    Some((digits, exp - i32::try_from(frac_part.len()).ok()?))
}

/// Exactly: does `d · 10^k` equal `m · 2^e`, with `m` odd?
///
/// Both sides are split into an odd part and a power of two; they are
/// equal iff both halves match. `10^k = 2^k · 5^k`, so the 5s must
/// divide out exactly — which is what bounds the powers involved.
fn decimal_eq_binary(d: u128, k: i32, m: u128, e: i32) -> bool {
    if d == 0 {
        return false;
    }
    let a = i32::try_from(d.trailing_zeros()).unwrap_or(i32::MAX);
    let d_odd = d >> d.trailing_zeros();
    if k >= 0 {
        // odd part is d_odd · 5^k — bail as soon as it can only exceed m.
        let mut lhs = d_odd;
        for _ in 0..k {
            match lhs.checked_mul(5) {
                Some(v) if v <= m => lhs = v,
                _ => return false,
            }
        }
        lhs == m && a + k == e
    } else {
        // d_odd must be divisible by 5^|k|; the quotient is the odd part.
        let j = -k;
        let mut lhs = d_odd;
        for _ in 0..j {
            if lhs % 5 != 0 {
                return false;
            }
            lhs /= 5;
        }
        lhs == m && a - j == e
    }
}
