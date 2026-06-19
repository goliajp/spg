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
) -> Result<Value, EvalError> {
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
            F::Epoch => {
                i64::from(months) * 30 * 86_400 + i64::from(days) * 86_400 + secs_total
            }
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
pub(super) fn date_part(args: &[Value]) -> Result<Value, EvalError> {
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
pub(super) fn age(args: &[Value]) -> Result<Value, EvalError> {
    if args.is_empty() || args.len() > 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("age() takes 1 or 2 args, got {}", args.len()),
        });
    }
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
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
    if args.len() == 1 {
        return Err(EvalError::TypeMismatch {
            detail: "single-arg age() is unsupported in v2.12 \
                     (use age(CURRENT_DATE, t) explicitly)"
                .into(),
        });
    }
    let a = to_micros(&args[0])?;
    let b = to_micros(&args[1])?;
    let delta = a.checked_sub(b).ok_or(EvalError::TypeMismatch {
        detail: "age() subtraction overflows i64 microseconds".into(),
    })?;
    Ok(Value::Interval {
        months: 0,
        days: 0,
        micros: delta,
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
pub(super) fn date_format_mysql(args: &[Value]) -> Result<Value, EvalError> {
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
    Ok(Value::Text(out))
}

/// v7.17.0 Phase 3.P0-29 — `UNIX_TIMESTAMP(t)` returns epoch
/// seconds (BIGINT) for a TIMESTAMP / DATE.
///
/// Bare `UNIX_TIMESTAMP()` (no args) is folded to a BigInt literal
/// by clock_replacement_for at the rewrite layer — never reaches
/// this arm.
pub(super) fn unix_timestamp_of(args: &[Value]) -> Result<Value, EvalError> {
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
pub(super) fn from_unixtime(args: &[Value]) -> Result<Value, EvalError> {
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
pub(super) fn date_trunc(args: &[Value]) -> Result<Value, EvalError> {
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
