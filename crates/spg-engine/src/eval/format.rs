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
    let mut s = String::with_capacity(base.len() + 6);
    s.push_str(&base);
    let total_min = (offset_micros / 60_000_000).abs();
    let (h, m) = (total_min / 60, total_min % 60);
    s.push(if offset_micros < 0 { '-' } else { '+' });
    s.push_str(&alloc::format!("{h:02}"));
    if m != 0 {
        s.push(':');
        s.push_str(&alloc::format!("{m:02}"));
    }
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
    let sci = alloc::format!("{x:e}"); // e.g. "1.234e15", "1e-5", "-2.5e-10"
    let epos = sci.find('e').expect("{:e} always has an 'e'");
    let exp_val: i32 = sci[epos + 1..].parse().unwrap_or(0);
    if (-4..=14).contains(&exp_val) {
        return alloc::format!("{x}"); // fixed-point shortest
    }
    let mant = &sci[..epos];
    let exp = &sci[epos + 1..];
    let (sign, digits) = match exp.strip_prefix('-') {
        Some(d) => ('-', d),
        None => ('+', exp),
    };
    alloc::format!("{mant}e{sign}{digits:0>2}")
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
    let sci = alloc::format!("{x:e}");
    let epos = sci.find('e').expect("{:e} always has an 'e'");
    let exp_val: i32 = sci[epos + 1..].parse().unwrap_or(0);
    if (-4..=5).contains(&exp_val) {
        return alloc::format!("{x}");
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
    let s = s.trim();
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
        return parse_month_name_date(s);
    }
    // v7.38 (read01) — year-first numeric form with `-`, `/` or `.` separators
    // and non-zero-padded month/day (`2020-1-5`, `2020/01/5`, `2020.1.05`), all
    // of which PG accepts. Requires exactly three all-digit fields, the first
    // being the 4-digit year, so it stays unambiguous (no MDY/DMY guessing).
    let mut parts = s.splitn(3, |c| c == '-' || c == '/' || c == '.');
    let (ys, ms, ds) = (parts.next()?, parts.next()?, parts.next()?);
    if ds.contains(['-', '/', '.', ' ']) {
        return None; // trailing separator / extra field / garbage
    }
    if ys.len() != 4 || ms.is_empty() || ms.len() > 2 || ds.is_empty() || ds.len() > 2 {
        return None;
    }
    if [ys, ms, ds]
        .iter()
        .any(|p| !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let y: i32 = ys.parse().ok()?;
    let m: u32 = ms.parse().ok()?;
    let d: u32 = ds.parse().ok()?;
    // PG validates the day against the actual (leap-aware) month length:
    // `'2024-02-30'` / `'2024-04-31'` / `'2023-02-29'` all raise "date/time
    // field value out of range". Without this the parser would silently roll
    // the overflow forward (Feb 30 → Mar 1) and corrupt data.
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
fn parse_month_name_date(s: &str) -> Option<i32> {
    let tokens: alloc::vec::Vec<&str> = s
        .split(|c: char| c == ' ' || c == ',' || c == '-')
        .filter(|t| !t.is_empty())
        .collect();
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
    let (mut month, mut year, mut day) = (None, None, None);
    for t in tokens {
        if let Some(m) = month_of(t) {
            if month.replace(m).is_some() {
                return None; // two month names
            }
        } else if t.bytes().all(|b| b.is_ascii_digit()) {
            match t.len() {
                4 if year.is_none() => year = t.parse().ok(),
                1 | 2 if day.is_none() => day = t.parse().ok(),
                _ => return None,
            }
        } else {
            return None; // a non-month alphabetic token (`Foo`)
        }
    }
    let (m, y, d): (u32, i32, u32) = (month?, year?, day?);
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
    let trimmed = s.trim();
    // PG special timestamp values. `infinity` / `-infinity` use the i64
    // sentinels (they compare greater/less than every finite timestamp).
    if trimmed.eq_ignore_ascii_case("epoch") {
        return Some(0);
    }
    if trimmed.eq_ignore_ascii_case("infinity") || trimmed.eq_ignore_ascii_case("+infinity") {
        return Some(i64::MAX);
    }
    if trimmed.eq_ignore_ascii_case("-infinity") {
        return Some(i64::MIN);
    }
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
        parts.push(format!("{days} {}", unit(i64::from(days), "day", "days")));
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
pub fn format_numeric_array(items: &[Option<(i128, u8)>]) -> String {
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
/// v7.38 (read01, T6) — render a NUMERIC honoring its special kind. Finite uses
/// `format_numeric`; the specials render PG's full-word spellings.
pub fn format_numeric_kind(kind: spg_storage::NumericKind, scaled: i128, scale: u8) -> String {
    use spg_storage::NumericKind;
    match kind {
        NumericKind::Finite => format_numeric(scaled, scale),
        NumericKind::NaN => String::from("NaN"),
        NumericKind::PosInf => String::from("Infinity"),
        NumericKind::NegInf => String::from("-Infinity"),
    }
}

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
