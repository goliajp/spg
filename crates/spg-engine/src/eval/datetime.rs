//! Date / time SQL functions split out of `eval.rs` (cut 32):
//! `EXTRACT` (extract_field + the civil_components wrapper),
//! `date_part`, `age`, MySQL `DATE_FORMAT` (date_format_mysql),
//! `UNIX_TIMESTAMP` (unix_timestamp_of), `FROM_UNIXTIME`
//! (from_unixtime), and `date_trunc`. The calendar primitives these
//! lean on stay in `eval.rs`: `civil_from_days` (and its
//! `MONTH_FULL` / `MONTH_ABBR` companions, shared with `eval::strings`)
//! and `days_from_civil` (re-exported there from `eval::format`).

use alloc::format;
use alloc::string::String;

use spg_storage::Value;

use super::{EvalError, MONTH_ABBR, MONTH_FULL, civil_from_days, days_from_civil};

/// Pull an integer component (year / month / ... / microsecond) out
/// of a `DATE` or `TIMESTAMP`. Returns NULL on a NULL source, errors
/// when the source isn't a calendar type.
pub(super) fn extract_field(
    field: spg_sql::ast::ExtractField,
    v: &Value,
) -> Result<Value<'static>, EvalError> {
    use spg_sql::ast::ExtractField as F;
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    // INTERVAL has its own decomposition — `YEAR` / `MONTH` come from
    // the months part, the rest from the microseconds part. PG matches
    // this convention (months is normalised modulo 12 for MONTH).
    if let Value::Interval {
        months,
        days,
        micros,
    } = *v
    {
        let years = months / 12;
        let mons = months % 12;
        let secs_total = micros / 1_000_000;
        let frac = micros % 1_000_000;
        let result = match field {
            F::Year => i64::from(years),
            F::Month => i64::from(mons),
            // v7.37.5 β — `days` is its own dimension now. PG semantics:
            // `extract(day from INTERVAL '24 hours')` = 0 (the micros
            // dimension never bleeds into the day count).
            F::Day => i64::from(days),
            F::Hour => (secs_total / 3600) % 24,
            F::Minute => (secs_total / 60) % 60,
            F::Second => secs_total % 60,
            F::Microsecond => (secs_total % 60) * 1_000_000 + frac,
            // total seconds in the interval (months count as 30 days,
            // days count their own 86_400, PG's justify_interval
            // convention).
            F::Epoch => i64::from(months) * 30 * 86_400 + i64::from(days) * 86_400 + secs_total,
        };
        return Ok(Value::BigInt(result));
    }
    let (days, day_micros) = match *v {
        Value::Date(d) => (d, 0_i64),
        Value::Timestamp(t) => {
            let days = t.div_euclid(86_400_000_000);
            let day_micros = t.rem_euclid(86_400_000_000);
            (i32::try_from(days).unwrap_or(i32::MAX), day_micros)
        }
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "EXTRACT requires DATE / TIMESTAMP / INTERVAL, got {:?}",
                    v.data_type()
                ),
            });
        }
    };
    let (y, m, d) = civil_components(days);
    let secs = day_micros / 1_000_000;
    let hh = secs / 3600;
    let mm = (secs / 60) % 60;
    let ss = secs % 60;
    let frac = day_micros % 1_000_000;
    let result = match field {
        F::Year => i64::from(y),
        F::Month => i64::from(m),
        F::Day => i64::from(d),
        F::Hour => hh,
        F::Minute => mm,
        F::Second => ss,
        F::Microsecond => ss * 1_000_000 + frac,
        // seconds since the unix epoch (truncated; PG returns
        // numeric with fraction — mailrs casts ::BIGINT anyway).
        F::Epoch => i64::from(days) * 86_400 + secs,
    };
    Ok(Value::BigInt(result))
}

/// Internal wrapper around the file-private `civil_from_days` so the
/// public surface area doesn't change. Returns `(year, month, day)`.
fn civil_components(days: i32) -> (i32, u32, u32) {
    civil_from_days(days)
}

/// `date_part(field_text, source)` — function form of `EXTRACT(field FROM
/// source)`. Same component dispatch (DATE / TIMESTAMP / INTERVAL) and
/// same `BigInt` return shape; PG returns double precision but we keep the
/// integer convention so the runner's `query I` shape works unchanged.
pub(super) fn date_part(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    use spg_sql::ast::ExtractField as F;
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("date_part() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(&args[0], Value::Null) || matches!(&args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Text(field_name) = &args[0] else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "date_part() needs a text field, got {:?}",
                args[0].data_type()
            ),
        });
    };
    let field = match field_name.to_ascii_lowercase().as_str() {
        "year" => F::Year,
        "month" => F::Month,
        "day" => F::Day,
        "hour" => F::Hour,
        "minute" => F::Minute,
        "second" => F::Second,
        "microsecond" | "microseconds" => F::Microsecond,
        "epoch" => F::Epoch,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "unknown date_part field {other:?}; \
                     supported: year, month, day, hour, minute, second, microsecond"
                ),
            });
        }
    };
    extract_field(field, &args[1])
}

/// `age(t1, t2)` — return `t1 - t2` as an INTERVAL. v2.12 produces a
/// micros-only interval (no months normalisation) because PG's
/// month-justification rule is sensitive to the day-of-month walk and
/// adds material complexity for marginal corpus value.
///
/// `age(t)` (single-arg form) is intentionally unsupported in v2.12:
/// the dispatcher errors instead of guessing a clock source. Callers
/// who want PG's `age(t)` semantics should write `age(CURRENT_DATE, t)`
/// explicitly so the clock reference is visible at the SQL layer.
pub(super) fn age(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.is_empty() || args.len() > 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("age() takes 1 or 2 args, got {}", args.len()),
        });
    }
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    // v7.37.17 (17.6 siblings) — PG's age(xid) overload, used by
    // autovacuum-wraparound monitoring (`age(relfrozenxid)`).
    // SPG's u64 tx ids never wrap, so the wraparound distance is
    // honestly 0 ("no vacuum urgency") for every xid.
    if args.len() == 1 {
        if let Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_) = &args[0] {
            return Ok(Value::Int(0));
        }
    }
    // Coerce to TIMESTAMP micros — DATE lifts to midnight; TIMESTAMP
    // stays as-is; anything else errors.
    let to_micros = |v: &Value| -> Result<i64, EvalError> {
        match v {
            Value::Timestamp(t) => Ok(*t),
            Value::Date(d) => Ok(i64::from(*d) * 86_400_000_000),
            other => Err(EvalError::TypeMismatch {
                detail: format!("age() needs DATE or TIMESTAMP, got {:?}", other.data_type()),
            }),
        }
    };
    // v7.37.17 (17.6 siblings) — PG's single-arg form:
    //   age(ts) == age(date_trunc('day', current_timestamp), ts)
    // ie the "wall-clock" age relative to midnight today. SPG
    // uses UNIX_EPOCH-based micros without a wall clock; without
    // a plausible "today" we anchor at 2020-01-01 UTC which is
    // predictable (any test using single-arg age can subtract).
    // v7.38 will thread real current_timestamp through.
    let (a, b) = if args.len() == 1 {
        // 2020-01-01 UTC as micros since epoch — no wall-clock
        // dependency, deterministic across runs.
        const ANCHOR_2020_UTC: i64 = 1_577_836_800_000_000;
        (ANCHOR_2020_UTC, to_micros(&args[0])?)
    } else {
        (to_micros(&args[0])?, to_micros(&args[1])?)
    };
    let delta = a.checked_sub(b).ok_or(EvalError::TypeMismatch {
        detail: "age() subtraction overflows i64 microseconds".into(),
    })?;
    // v7.38 P1 (轴 1 pg_regress closure) — PG canonical AGE output
    // splits whole days out of the microsecond residue so
    // `AGE('2024-03-01'::TIMESTAMP, '2024-01-01'::TIMESTAMP)` reads
    // `60 days` (not `1440:00:00`). Truncate toward zero so the day
    // component carries delta's sign and micros retains the same sign
    // — matches PG's interval normalisation for symmetric AGE diffs.
    const US_PER_DAY: i64 = 86_400_000_000;
    let days_i64 = delta / US_PER_DAY;
    let micros = delta % US_PER_DAY;
    let days = i32::try_from(days_i64).map_err(|_| EvalError::TypeMismatch {
        detail: "age() day count exceeds i32".into(),
    })?;
    Ok(Value::Interval {
        months: 0,
        days,
        micros,
    })
}

/// v7.17.0 Phase 3.P0-29 — MySQL `DATE_FORMAT(t, fmt)`.
///
/// Format tokens (MySQL 8.0 surface):
///   * `%Y` — 4-digit year  `%y` — 2-digit year
///   * `%m` — 01-12 month   `%c` — 1-12 month (no zero pad)
///   * `%d` — 01-31 day     `%e` — 1-31 day (no zero pad)
///   * `%H` — 00-23 hour    `%h` / `%I` — 01-12 hour
///   * `%i` — 00-59 MINUTE (NB: `%M` is month name in MySQL — easy
///     footgun if we mirror PG's `to_char` tokens by accident)
///   * `%s` / `%S` — 00-59 second
///   * `%f` — 000000-999999 microseconds (always 6 digits)
///   * `%p` — AM / PM
///   * `%M` — January-December (full month name)
///   * `%b` — Jan-Dec (abbreviated month name)
///   * `%%` — literal `%`
///
/// Unknown `%X` tokens pass through verbatim (MySQL emits the `%`
/// then the unknown letter).
pub(super) fn date_format_mysql(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    use core::fmt::Write as _;
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("date_format() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(&args[0], Value::Null) || matches!(&args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Text(fmt) = &args[1] else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "date_format() needs a text format, got {:?}",
                args[1].data_type()
            ),
        });
    };
    let (days, day_micros) = match &args[0] {
        Value::Date(d) => (*d, 0_i64),
        Value::Timestamp(t) => {
            let days = t.div_euclid(86_400_000_000);
            (
                i32::try_from(days).unwrap_or(i32::MAX),
                t.rem_euclid(86_400_000_000),
            )
        }
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "date_format() needs DATE or TIMESTAMP, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let (y, mo, d) = civil_from_days(days);
    let secs = day_micros / 1_000_000;
    let frac = day_micros % 1_000_000;
    let hh24 = u32::try_from(secs / 3600).unwrap_or(0);
    let mi = u32::try_from((secs / 60) % 60).unwrap_or(0);
    let ss = u32::try_from(secs % 60).unwrap_or(0);
    let hh12 = match hh24 % 12 {
        0 => 12,
        x => x,
    };
    let ampm = if hh24 < 12 { "AM" } else { "PM" };
    let us = u32::try_from(frac).unwrap_or(0);

    let mut out = String::with_capacity(fmt.len() + 8);
    let bytes = fmt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        if i + 1 >= bytes.len() {
            // Trailing `%` with no specifier — emit verbatim.
            out.push('%');
            i += 1;
            continue;
        }
        let token = bytes[i + 1];
        match token {
            b'Y' => {
                let _ = write!(out, "{y:04}");
            }
            b'y' => {
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                let yy = (y.rem_euclid(100)) as u32;
                let _ = write!(out, "{yy:02}");
            }
            b'm' => {
                let _ = write!(out, "{mo:02}");
            }
            b'c' => {
                let _ = write!(out, "{mo}");
            }
            b'd' => {
                let _ = write!(out, "{d:02}");
            }
            b'e' => {
                let _ = write!(out, "{d}");
            }
            b'H' => {
                let _ = write!(out, "{hh24:02}");
            }
            b'h' | b'I' => {
                let _ = write!(out, "{hh12:02}");
            }
            b'i' => {
                // MINUTE — distinct from PG's `MI` and from MySQL's
                // own `%M` (month name).
                let _ = write!(out, "{mi:02}");
            }
            b's' | b'S' => {
                let _ = write!(out, "{ss:02}");
            }
            b'f' => {
                let _ = write!(out, "{us:06}");
            }
            b'p' => {
                out.push_str(ampm);
            }
            b'M' => {
                out.push_str(MONTH_FULL[(mo - 1) as usize]);
            }
            b'b' => {
                out.push_str(MONTH_ABBR[(mo - 1) as usize]);
            }
            b'%' => {
                out.push('%');
            }
            other => {
                // Unknown specifier — MySQL emits the letter
                // verbatim (without the `%`).
                out.push(other as char);
            }
        }
        i += 2;
    }
    Ok(Value::text(out))
}

/// v7.17.0 Phase 3.P0-29 — `UNIX_TIMESTAMP(t)` returns epoch
/// seconds (BIGINT) for a TIMESTAMP / DATE.
///
/// Bare `UNIX_TIMESTAMP()` (no args) is folded to a BigInt literal
/// by clock_replacement_for at the rewrite layer — never reaches
/// this arm.
pub(super) fn unix_timestamp_of(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 1 {
        return Err(EvalError::TypeMismatch {
            detail: format!("unix_timestamp() takes 0 or 1 arg, got {}", args.len()),
        });
    }
    match &args[0] {
        Value::Null => Ok(Value::Null),
        Value::Timestamp(t) => Ok(Value::BigInt(t.div_euclid(1_000_000))),
        Value::Date(d) => Ok(Value::BigInt(i64::from(*d) * 86_400)),
        other => Err(EvalError::TypeMismatch {
            detail: format!(
                "unix_timestamp() needs DATE or TIMESTAMP, got {:?}",
                other.data_type()
            ),
        }),
    }
}

/// v7.17.0 Phase 3.P0-29 — `FROM_UNIXTIME(n)` returns a TIMESTAMP
/// at `n` seconds past the Unix epoch. `FROM_UNIXTIME(n, fmt)`
/// applies MySQL date_format on top, returning TEXT.
pub(super) fn from_unixtime(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if !(1..=2).contains(&args.len()) {
        return Err(EvalError::TypeMismatch {
            detail: format!("from_unixtime() takes 1 or 2 args, got {}", args.len()),
        });
    }
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let secs: i64 = match &args[0] {
        Value::SmallInt(n) => i64::from(*n),
        Value::Int(n) => i64::from(*n),
        Value::BigInt(n) => *n,
        Value::Float(x) => *x as i64,
        Value::Numeric { scaled, scale } => {
            let denom = 10_i128.pow(u32::from(*scale));
            i64::try_from(scaled.div_euclid(denom)).unwrap_or(i64::MAX)
        }
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "from_unixtime() needs a numeric epoch second count, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let ts = Value::Timestamp(secs.saturating_mul(1_000_000));
    if args.len() == 1 {
        Ok(ts)
    } else {
        date_format_mysql(&[ts, args[1].clone()])
    }
}

/// `date_trunc(unit, timestamp)` — round a `TIMESTAMP` down to the
/// requested calendar boundary (year / month / day / hour / minute /
/// second). Returns the truncated `TIMESTAMP`. NULL on either side
/// propagates to NULL.
pub(super) fn date_trunc(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("date_trunc() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(&args[0], Value::Null) || matches!(&args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Text(unit) = &args[0] else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "date_trunc() needs a text unit, got {:?}",
                args[0].data_type()
            ),
        });
    };
    // Both DATE and TIMESTAMP sources are accepted. DATE lifts to
    // midnight first; the result is always TIMESTAMP.
    let micros = match &args[1] {
        Value::Timestamp(t) => *t,
        Value::Date(d) => i64::from(*d) * 86_400_000_000,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "date_trunc() needs DATE or TIMESTAMP, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let unit_lc = unit.to_ascii_lowercase();
    let days = micros.div_euclid(86_400_000_000);
    let day_micros = micros.rem_euclid(86_400_000_000);
    let day_i32 = i32::try_from(days).unwrap_or(i32::MAX);
    let (y, m, _) = civil_from_days(day_i32);
    let truncated = match unit_lc.as_str() {
        "year" => i64::from(days_from_civil(y, 1, 1)) * 86_400_000_000,
        "month" => i64::from(days_from_civil(y, m, 1)) * 86_400_000_000,
        "day" => days * 86_400_000_000,
        "hour" => days * 86_400_000_000 + (day_micros / 3_600_000_000) * 3_600_000_000,
        "minute" => days * 86_400_000_000 + (day_micros / 60_000_000) * 60_000_000,
        "second" => days * 86_400_000_000 + (day_micros / 1_000_000) * 1_000_000,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "unknown date_trunc unit {other:?}; \
                     supported: year, month, day, hour, minute, second"
                ),
            });
        }
    };
    Ok(Value::Timestamp(truncated))
}

/// v7.37.17 (17.6 siblings) — MySQL STR_TO_DATE(str, format): the
/// inverse of DATE_FORMAT. Unparseable input returns NULL (MySQL
/// raises a warning, not an error). A format with no time
/// specifiers produces a DATE; any time specifier produces a
/// TIMESTAMP.
pub(super) fn str_to_date_mysql(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("str_to_date() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(&args[0], Value::Null) || matches!(&args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let (Value::Text(input), Value::Text(fmt)) = (&args[0], &args[1]) else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "str_to_date() takes (text, text), got ({:?}, {:?})",
                args[0].data_type(),
                args[1].data_type()
            ),
        });
    };
    const MONTH_FULL_UP: [&str; 12] = [
        "JANUARY", "FEBRUARY", "MARCH", "APRIL", "MAY", "JUNE", "JULY",
        "AUGUST", "SEPTEMBER", "OCTOBER", "NOVEMBER", "DECEMBER",
    ];
    let inp: alloc::vec::Vec<char> = input.chars().collect();
    let f: alloc::vec::Vec<char> = fmt.chars().collect();
    let mut year: i32 = 1970;
    let mut month: u32 = 1;
    let mut day: u32 = 1;
    let mut hour: u32 = 0;
    let mut minute: u32 = 0;
    let mut second: u32 = 0;
    let mut micros: u32 = 0;
    let mut pm: Option<bool> = None;
    let mut has_time = false;
    let mut ip = 0usize;
    let mut fp = 0usize;
    // Reads 1..=max ASCII digits.
    let read_num = |ip: &mut usize, max: usize, inp: &[char]| -> Option<u32> {
        let start = *ip;
        while *ip < inp.len() && *ip - start < max && inp[*ip].is_ascii_digit() {
            *ip += 1;
        }
        if *ip == start {
            return None;
        }
        inp[start..*ip]
            .iter()
            .collect::<alloc::string::String>()
            .parse()
            .ok()
    };
    while fp < f.len() {
        if f[fp] != '%' {
            // Literal — whitespace in the format matches any run of
            // whitespace; other chars must match exactly.
            if f[fp].is_whitespace() {
                while ip < inp.len() && inp[ip].is_whitespace() {
                    ip += 1;
                }
                fp += 1;
                continue;
            }
            if ip < inp.len() && inp[ip] == f[fp] {
                ip += 1;
                fp += 1;
                continue;
            }
            return Ok(Value::Null);
        }
        if fp + 1 >= f.len() {
            return Ok(Value::Null);
        }
        let spec = f[fp + 1];
        fp += 2;
        match spec {
            'Y' => match read_num(&mut ip, 4, &inp) {
                Some(v) => year = v as i32,
                None => return Ok(Value::Null),
            },
            'y' => match read_num(&mut ip, 2, &inp) {
                // MySQL 2-digit year: 70..=99 → 19xx, else 20xx.
                Some(v) => year = if v >= 70 { 1900 + v as i32 } else { 2000 + v as i32 },
                None => return Ok(Value::Null),
            },
            'm' | 'c' => match read_num(&mut ip, 2, &inp) {
                Some(v @ 1..=12) => month = v,
                _ => return Ok(Value::Null),
            },
            'd' | 'e' => match read_num(&mut ip, 2, &inp) {
                Some(v @ 1..=31) => day = v,
                _ => return Ok(Value::Null),
            },
            'H' => match read_num(&mut ip, 2, &inp) {
                Some(v @ 0..=23) => {
                    hour = v;
                    has_time = true;
                }
                _ => return Ok(Value::Null),
            },
            'h' | 'I' => match read_num(&mut ip, 2, &inp) {
                Some(v @ 1..=12) => {
                    hour = v;
                    has_time = true;
                }
                _ => return Ok(Value::Null),
            },
            'i' => match read_num(&mut ip, 2, &inp) {
                Some(v @ 0..=59) => {
                    minute = v;
                    has_time = true;
                }
                _ => return Ok(Value::Null),
            },
            's' | 'S' => match read_num(&mut ip, 2, &inp) {
                Some(v @ 0..=59) => {
                    second = v;
                    has_time = true;
                }
                _ => return Ok(Value::Null),
            },
            'f' => {
                let start = ip;
                match read_num(&mut ip, 6, &inp) {
                    Some(v) => {
                        // Right-pad to microseconds.
                        let ndigits = ip - start;
                        let mut scaled = v;
                        for _ in ndigits..6 {
                            scaled *= 10;
                        }
                        micros = scaled;
                        has_time = true;
                    }
                    None => return Ok(Value::Null),
                }
            }
            'p' => {
                if ip + 2 <= inp.len() {
                    let tag: alloc::string::String =
                        inp[ip..ip + 2].iter().collect::<alloc::string::String>().to_ascii_uppercase();
                    match tag.as_str() {
                        "AM" => pm = Some(false),
                        "PM" => pm = Some(true),
                        _ => return Ok(Value::Null),
                    }
                    ip += 2;
                    has_time = true;
                } else {
                    return Ok(Value::Null);
                }
            }
            'M' | 'b' => {
                // Month name (full or abbreviated) — match the
                // longest month prefix.
                let rest: alloc::string::String = inp[ip..].iter().collect::<alloc::string::String>().to_ascii_uppercase();
                let mut matched = None;
                for (idx, name) in MONTH_FULL_UP.iter().enumerate() {
                    if rest.starts_with(name) {
                        matched = Some((idx as u32 + 1, name.len()));
                        break;
                    }
                    if rest.starts_with(&name[..3]) {
                        matched = Some((idx as u32 + 1, 3));
                        // Keep looking for a full-name match (June
                        // vs Jun) — full names win.
                    }
                }
                match matched {
                    Some((m, len)) => {
                        month = m;
                        ip += len;
                    }
                    None => return Ok(Value::Null),
                }
            }
            '%' => {
                if ip < inp.len() && inp[ip] == '%' {
                    ip += 1;
                } else {
                    return Ok(Value::Null);
                }
            }
            _ => return Ok(Value::Null),
        }
    }
    if let Some(is_pm) = pm {
        hour = match (hour % 12, is_pm) {
            (h, true) => h + 12,
            (h, false) => h,
        };
    }
    let days = days_from_civil(year, month, day);
    if has_time {
        let micros_total = i64::from(days) * 86_400_000_000
            + i64::from(hour) * 3_600_000_000
            + i64::from(minute) * 60_000_000
            + i64::from(second) * 1_000_000
            + i64::from(micros);
        Ok(Value::Timestamp(micros_total))
    } else {
        Ok(Value::Date(days))
    }
}

/// v7.37.17 (17.6 siblings) — MySQL TIME_FORMAT(time, format): the
/// time-of-day slice of DATE_FORMAT. Accepts a TIMESTAMP (its
/// time-of-day) or an 'HH:MM[:SS[.ffffff]]' text value.
pub(super) fn time_format_mysql(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("time_format() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(&args[0], Value::Null) || matches!(&args[1], Value::Null) {
        return Ok(Value::Null);
    }
    // Normalise to a timestamp on day 0, then reuse date_format.
    let day_micros: i64 = match &args[0] {
        Value::Timestamp(t) => t.rem_euclid(86_400_000_000),
        Value::Text(s) => {
            let mut parts = s.trim().splitn(3, ':');
            let h: i64 = parts
                .next()
                .and_then(|x| x.parse().ok())
                .ok_or_else(|| EvalError::TypeMismatch {
                    detail: format!("time_format(): cannot parse time {s:?}"),
                })?;
            let m: i64 = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            let (sec, us) = match parts.next() {
                None => (0_i64, 0_i64),
                Some(rest) => match rest.split_once('.') {
                    None => (rest.parse().unwrap_or(0), 0),
                    Some((s_int, s_frac)) => {
                        let mut frac = String::from(s_frac);
                        while frac.len() < 6 {
                            frac.push('0');
                        }
                        frac.truncate(6);
                        (s_int.parse().unwrap_or(0), frac.parse().unwrap_or(0))
                    }
                },
            };
            h * 3_600_000_000 + m * 60_000_000 + sec * 1_000_000 + us
        }
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "time_format() needs TIME text or TIMESTAMP, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    date_format_mysql(&[Value::Timestamp(day_micros), args[1].clone().into_owned()])
}

/// Micros-per-unit for the fixed-length MySQL interval units.
fn unit_micros(unit: &str) -> Option<i64> {
    Some(match unit {
        "microsecond" => 1,
        "second" => 1_000_000,
        "minute" => 60_000_000,
        "hour" => 3_600_000_000,
        "day" => 86_400_000_000,
        "week" => 7 * 86_400_000_000,
        _ => return None,
    })
}

/// pub(super) surface for the apply_function dispatch (adddate /
/// subdate / date_sub share the coercion).
pub(super) fn text_or_temporal_micros(v: &Value<'_>, fn_name: &str) -> Result<i64, EvalError> {
    ts_of(v, fn_name)
}

fn ts_of(v: &Value<'_>, fn_name: &str) -> Result<i64, EvalError> {
    match v {
        Value::Timestamp(t) => Ok(*t),
        Value::Date(d) => Ok(i64::from(*d) * 86_400_000_000),
        // MySQL accepts string datetimes everywhere a DATETIME is
        // expected — parse 'YYYY-MM-DD[ HH:MM:SS[.ffffff]]'.
        Value::Text(s) => parse_text_datetime(s).ok_or_else(|| EvalError::TypeMismatch {
            detail: format!("{fn_name}(): cannot parse datetime {s:?}"),
        }),
        other => Err(EvalError::TypeMismatch {
            detail: format!(
                "{fn_name}() needs DATE or TIMESTAMP, got {:?}",
                other.data_type()
            ),
        }),
    }
}

/// Parse 'YYYY-MM-DD[ HH:MM:SS[.ffffff]]' into epoch micros.
fn parse_text_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date_part, time_part) = match s.split_once(' ') {
        Some((d, t)) => (d, Some(t)),
        None => match s.split_once('T') {
            Some((d, t)) => (d, Some(t)),
            None => (s, None),
        },
    };
    let mut dp = date_part.splitn(3, '-');
    let y: i32 = dp.next()?.parse().ok()?;
    let mo: u32 = dp.next()?.parse().ok()?;
    let d: u32 = dp.next()?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    let mut micros = i64::from(days) * 86_400_000_000;
    if let Some(t) = time_part {
        let mut tp = t.splitn(3, ':');
        let h: i64 = tp.next()?.parse().ok()?;
        let mi: i64 = tp.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let (sec, us): (i64, i64) = match tp.next() {
            None => (0, 0),
            Some(rest) => match rest.split_once('.') {
                None => (rest.parse().ok()?, 0),
                Some((si, sf)) => {
                    let mut frac = String::from(sf);
                    while frac.len() < 6 {
                        frac.push('0');
                    }
                    frac.truncate(6);
                    (si.parse().ok()?, frac.parse().ok()?)
                }
            },
        };
        if !(0..=23).contains(&h) || !(0..=59).contains(&mi) || !(0..=59).contains(&sec) {
            return None;
        }
        micros += h * 3_600_000_000 + mi * 60_000_000 + sec * 1_000_000 + us;
    }
    Some(micros)
}

/// Shift a timestamp by n calendar months (day-of-month clamps to
/// the target month's length, MySQL behaviour).
fn add_months(ts: i64, n: i64) -> i64 {
    let days = ts.div_euclid(86_400_000_000);
    let day_micros = ts.rem_euclid(86_400_000_000);
    let (y, m, d) = civil_from_days(i32::try_from(days).unwrap_or(i32::MAX));
    let total = i64::from(y) * 12 + i64::from(m) - 1 + n;
    let ny = i32::try_from(total.div_euclid(12)).unwrap_or(i32::MAX);
    let nm = u32::try_from(total.rem_euclid(12)).unwrap_or(0) + 1;
    let month_len = |y: i32, m: u32| -> u32 {
        match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            _ => {
                if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                    29
                } else {
                    28
                }
            }
        }
    };
    let nd = d.min(month_len(ny, nm));
    i64::from(days_from_civil(ny, nm, nd)) * 86_400_000_000 + day_micros
}

/// v7.37.17 (17.6 siblings) — MySQL TIMESTAMPADD(unit, n, datetime).
/// The parser lowers the bare unit keyword onto a string literal.
pub(super) fn timestampadd_mysql(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 3 {
        return Err(EvalError::TypeMismatch {
            detail: format!("timestampadd() takes 3 args, got {}", args.len()),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let Value::Text(unit) = &args[0] else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "timestampadd() unit must be a keyword, got {:?}",
                args[0].data_type()
            ),
        });
    };
    let n = match &args[1] {
        Value::Int(v) => i64::from(*v),
        Value::SmallInt(v) => i64::from(*v),
        Value::BigInt(v) => *v,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "timestampadd() count must be integer, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let ts = ts_of(&args[2], "timestampadd")?;
    let unit_lc = unit.to_ascii_lowercase();
    let out = if let Some(per) = unit_micros(&unit_lc) {
        ts.saturating_add(n.saturating_mul(per))
    } else {
        match unit_lc.as_str() {
            "month" => add_months(ts, n),
            "quarter" => add_months(ts, n.saturating_mul(3)),
            "year" => add_months(ts, n.saturating_mul(12)),
            other => {
                return Err(EvalError::TypeMismatch {
                    detail: format!("timestampadd(): unknown unit {other:?}"),
                });
            }
        }
    };
    Ok(Value::Timestamp(out))
}

/// v7.37.17 (17.6 siblings) — MySQL TIMESTAMPDIFF(unit, from, to):
/// the count of COMPLETE units from `from` to `to` (signed).
pub(super) fn timestampdiff_mysql(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 3 {
        return Err(EvalError::TypeMismatch {
            detail: format!("timestampdiff() takes 3 args, got {}", args.len()),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let Value::Text(unit) = &args[0] else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "timestampdiff() unit must be a keyword, got {:?}",
                args[0].data_type()
            ),
        });
    };
    let from = ts_of(&args[1], "timestampdiff")?;
    let to = ts_of(&args[2], "timestampdiff")?;
    let unit_lc = unit.to_ascii_lowercase();
    let out = if let Some(per) = unit_micros(&unit_lc) {
        (to - from) / per
    } else {
        let months_between = |from: i64, to: i64| -> i64 {
            // Count complete months: advance from `from` while the
            // shifted point stays ≤ `to` (mirrored for negatives).
            let sign = if to >= from { 1 } else { -1 };
            let (lo, hi) = if sign > 0 { (from, to) } else { (to, from) };
            let (ly, lm, _) = civil_from_days(
                i32::try_from(lo.div_euclid(86_400_000_000)).unwrap_or(0),
            );
            let (hy, hm, _) = civil_from_days(
                i32::try_from(hi.div_euclid(86_400_000_000)).unwrap_or(0),
            );
            let mut approx =
                (i64::from(hy) * 12 + i64::from(hm)) - (i64::from(ly) * 12 + i64::from(lm));
            // Adjust down while the full-month shift overshoots.
            while approx > 0 && add_months(lo, approx) > hi {
                approx -= 1;
            }
            sign * approx
        };
        match unit_lc.as_str() {
            "month" => months_between(from, to),
            "quarter" => months_between(from, to) / 3,
            "year" => months_between(from, to) / 12,
            other => {
                return Err(EvalError::TypeMismatch {
                    detail: format!("timestampdiff(): unknown unit {other:?}"),
                });
            }
        }
    };
    Ok(Value::BigInt(out))
}
