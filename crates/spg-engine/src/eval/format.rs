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

use super::civil_from_days;

/// Render a `Date` (days since epoch) as `YYYY-MM-DD`. Negative values
/// for pre-1970 dates render with a leading `-` on the year.
pub fn format_date(days: i32) -> String {
    let (y, m, d) = civil_from_days(days);
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
    let base = format_timestamp(micros);
    let mut s = String::with_capacity(base.len() + 3);
    s.push_str(&base);
    s.push_str("+00");
    s
}

/// v7.17.0 Phase 3.P0-35 — PG `money` canonical text form, en_US
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
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    // Split into day + intra-day part with proper floor division so
    // negative timestamps render right too.
    let days = micros.div_euclid(MICROS_PER_DAY);
    let day_micros = micros.rem_euclid(MICROS_PER_DAY);
    let day_i32 = i32::try_from(days).unwrap_or(i32::MAX);
    let (y, m, d) = civil_from_days(day_i32);
    let secs = day_micros / 1_000_000;
    let frac = day_micros % 1_000_000;
    let hh = secs / 3600;
    let mm = (secs / 60) % 60;
    let ss = secs % 60;
    if frac == 0 {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
    } else {
        // Strip trailing zeros from the 6-digit fractional component.
        let raw = format!("{frac:06}");
        let trimmed = raw.trim_end_matches('0');
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{trimmed}")
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
    let bytes = s.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let y: i32 = s[0..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Parse `YYYY-MM-DD[ HH:MM:SS[.ffffff]]` into a `Timestamp`
/// (microseconds since Unix epoch). The time portion is optional;
/// missing → midnight. The fractional portion accepts 1–6 digits and
/// pads with zeros to microseconds.
pub fn parse_timestamp_literal(s: &str) -> Option<i64> {
    let trimmed = s.trim();
    let (date_part, time_part) = match trimmed.find([' ', 'T']) {
        Some(i) => (&trimmed[..i], Some(&trimmed[i + 1..])),
        None => (trimmed, None),
    };
    let days = parse_date_literal(date_part)?;
    let (day_micros, tz_offset_micros) = match time_part {
        None => (0, 0),
        Some(t) => parse_time_of_day_micros(t)?,
    };
    // PG semantics: a TIMESTAMPTZ literal with an explicit offset
    // is normalised to UTC for storage. `'12:00:00+09'` means
    // 12:00:00 in a UTC+09 zone → 03:00:00 UTC → subtract the
    // positive offset (or add the negative one). Storage is i64
    // microseconds UTC for both TIMESTAMP and TIMESTAMPTZ (see
    // spg-storage::DataType::Timestamptz docs); the wire-level
    // round-trip then re-applies the session timezone on the
    // SELECT side when format_timestamp is asked for a TZ-aware
    // render.
    Some(i64::from(days) * 86_400_000_000 + day_micros - tz_offset_micros)
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
/// surfaces as "cannot parse … as TIMESTAMP").
fn parse_time_of_day_micros(t: &str) -> Option<(i64, i64)> {
    let t = t.trim();
    // Detect & strip optional TZ suffix. Anchor on the first
    // `+` / `-` AFTER position 8 (so the leading sign on a
    // negative offset can't be mistaken for an `HH:MM:SS-OO`
    // boundary if the time itself is somehow malformed).
    // ` UTC` and trailing `Z` also count as zero-offset TZ tags.
    let (core, tz_micros) = if let Some(rest) = t.strip_suffix('Z') {
        (rest, 0i64)
    } else if let Some(rest) = t.strip_suffix(" UTC").or_else(|| t.strip_suffix("UTC")) {
        (rest, 0i64)
    } else if let Some((idx, sign_byte)) = find_offset_sign(t) {
        let suffix = &t[idx..];
        let micros = parse_tz_offset_suffix(suffix, sign_byte == b'+')?;
        (&t[..idx], micros)
    } else {
        (t, 0i64)
    };
    let (time, frac_str) = match core.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (core, None),
    };
    let bytes = time.as_bytes();
    if bytes.len() != 8 || bytes[2] != b':' || bytes[5] != b':' {
        return None;
    }
    let hh: i64 = time[0..2].parse().ok()?;
    let mm: i64 = time[3..5].parse().ok()?;
    let ss: i64 = time[6..8].parse().ok()?;
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
    // Start past `HH:MM:SS` (8 bytes).
    if bytes.len() < 9 {
        return None;
    }
    for i in 8..bytes.len() {
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
    if years != 0 {
        parts.push(format!(
            "{years} {}",
            unit(i64::from(years), "year", "years")
        ));
    }
    if mons != 0 {
        parts.push(format!("{mons} {}", unit(i64::from(mons), "mon", "mons")));
    }
    if days != 0 {
        parts.push(format!(
            "{days} {}",
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
        let sign = if neg { "-" } else { "" };
        if frac == 0 {
            parts.push(format!("{sign}{hh:02}:{mm:02}:{ss:02}"));
        } else {
            let raw = format!("{frac:06}");
            let trimmed = raw.trim_end_matches('0');
            parts.push(format!("{sign}{hh:02}:{mm:02}:{ss:02}.{trimmed}"));
        }
    }
    if parts.is_empty() {
        "0".into()
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
                let needs_quote = s.is_empty()
                    || s.eq_ignore_ascii_case("NULL")
                    || s.chars()
                        .any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\' | ' ' | '\t'));
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
                out.push('"');
                out.push_str(&format_interval(span.months, span.days, span.micros));
                out.push('"');
            }
        }
    }
    out.push('}');
    out
}

/// v7.10.4 — render a BYTEA payload in PG's hex output format
/// (`\x` prefix, lowercase hex pairs). Public so the wire layer
/// can emit the canonical bytea-as-text representation.
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
pub fn format_numeric(scaled: i128, scale: u8) -> String {
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
