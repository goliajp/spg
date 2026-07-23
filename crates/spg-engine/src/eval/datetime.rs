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
    field: &spg_sql::ast::ExtractField,
    v: &Value,
    src_name: &str,
) -> Result<Value<'static>, EvalError> {
    use spg_sql::ast::ExtractField as F;
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    // v7.39 (round 253) — PG resolves field names at runtime: an unknown
    // one is 22023 with the source type in the message.
    if let F::Other(name) = field {
        return Err(EvalError::TypeMismatch {
            detail: format!("unit \"{name}\" not recognized for type {src_name}"),
        });
    }
    // Integral results are NUMERIC (PG 14+ EXTRACT returns numeric;
    // date_part demotes to double precision at its own boundary).
    let num0 = |n: i64| Value::Numeric {
        scaled: i128::from(n),
        scale: 0,
        kind: spg_storage::NumericKind::Finite,
    };
    // v7.39 (round 253) — EXTRACT from TIMETZ (probed live): the
    // time-of-day fields read the LOCAL clock, epoch subtracts the
    // offset, and the timezone fields report the signed offset parts.
    if let Value::TimeTz { us, offset_secs } = *v {
        let secs = us / 1_000_000;
        let frac = us % 1_000_000;
        let num = |scaled: i128, scale: u16| {
            Ok(Value::Numeric {
                scaled,
                scale,
                kind: spg_storage::NumericKind::Finite,
            })
        };
        return match field {
            F::Hour => num(i128::from(secs / 3600), 0),
            F::Minute => num(i128::from((secs / 60) % 60), 0),
            F::Second => num(i128::from(secs % 60) * 1_000_000 + i128::from(frac), 6),
            F::Millisecond => num(i128::from(secs % 60) * 1_000_000 + i128::from(frac), 3),
            F::Microsecond => num(i128::from(secs % 60) * 1_000_000 + i128::from(frac), 0),
            F::Epoch => num(i128::from(us) - i128::from(offset_secs) * 1_000_000, 6),
            F::Timezone => num(i128::from(offset_secs), 0),
            F::TimezoneHour => num(i128::from(offset_secs / 3600), 0),
            F::TimezoneMinute => num(i128::from((offset_secs / 60) % 60), 0),
            other => Err(EvalError::TypeMismatch {
                detail: format!(
                    "unit \"{}\" not supported for type {src_name}",
                    format!("{other}").to_lowercase()
                ),
            }),
        };
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
        // v7.38 (read01 sweep) — the fraction-bearing fields keep sub-second
        // precision as NUMERIC (PG), instead of truncating to an integer.
        // Epoch / Second render microseconds (scale 6); Millisecond scale 3.
        match field {
            F::Epoch => {
                let total_secs =
                    i64::from(months) * 30 * 86_400 + i64::from(days) * 86_400 + secs_total;
                return Ok(Value::Numeric {
                    scaled: i128::from(total_secs) * 1_000_000 + i128::from(frac),
                    scale: 6,
                    kind: spg_storage::NumericKind::Finite,
                });
            }
            F::Second => {
                return Ok(Value::Numeric {
                    scaled: i128::from(secs_total % 60) * 1_000_000 + i128::from(frac),
                    scale: 6,
                    kind: spg_storage::NumericKind::Finite,
                });
            }
            F::Millisecond => {
                return Ok(Value::Numeric {
                    scaled: i128::from(secs_total % 60) * 1_000_000 + i128::from(frac),
                    scale: 3,
                    kind: spg_storage::NumericKind::Finite,
                });
            }
            _ => {}
        }
        let result = match field {
            F::Year => i64::from(years),
            F::Month => i64::from(mons),
            // v7.37.5 β — `days` is its own dimension now. PG semantics:
            // `extract(day from INTERVAL '24 hours')` = 0 (the micros
            // dimension never bleeds into the day count).
            F::Day => i64::from(days),
            // PG does NOT roll interval hours into days: the `days`
            // dimension is independent, so `extract(hour from INTERVAL
            // '25 hours')` = 25 (not 1). Only MINUTE / SECOND wrap,
            // because the HH:MM:SS time component keeps MM / SS < 60
            // while HH is unbounded.
            F::Hour => secs_total / 3600,
            F::Minute => (secs_total / 60) % 60,
            F::Second => secs_total % 60,
            F::Microsecond => (secs_total % 60) * 1_000_000 + frac,
            // total seconds in the interval (months count as 30 days,
            // days count their own 86_400, PG's justify_interval
            // convention).
            F::Epoch => i64::from(months) * 30 * 86_400 + i64::from(days) * 86_400 + secs_total,
            F::Quarter => i64::from(mons) / 3 + 1,
            F::Decade => i64::from(years) / 10,
            F::Century => i64::from(years) / 100,
            F::Millennium => i64::from(years) / 1000,
            F::Millisecond => (secs_total % 60) * 1_000 + frac / 1_000,
            // v7.39 (round 253) — probed live: WEEK on an interval is
            // days/7 (truncating toward zero: 13d -> 1, -8d -> -1).
            F::Week => i64::from(days) / 7,
            F::Dow
            | F::Isodow
            | F::Doy
            | F::Isoyear
            | F::Julian
            | F::Timezone
            | F::TimezoneHour
            | F::TimezoneMinute => {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "unit \"{}\" not supported for type {src_name}",
                        format!("{field}").to_lowercase()
                    ),
                });
            }
            F::Other(_) => unreachable!("handled above"),
        };
        return Ok(num0(result));
    }
    // v7.38 (read01) — EXTRACT from TIME. `Value::Time` is micros within the
    // day; only the time-of-day fields apply, and PG returns them all as
    // NUMERIC. A date field is rejected with PG's exact wording.
    if let Value::Time(micros) = *v {
        let secs = micros / 1_000_000;
        let frac = micros % 1_000_000;
        let num = |scaled: i128, scale: u16| {
            Ok(Value::Numeric {
                scaled,
                scale,
                kind: spg_storage::NumericKind::Finite,
            })
        };
        return match field {
            F::Hour => num(i128::from(secs / 3600), 0),
            F::Minute => num(i128::from((secs / 60) % 60), 0),
            F::Second => num(i128::from(secs % 60) * 1_000_000 + i128::from(frac), 6),
            F::Millisecond => num(i128::from(secs % 60) * 1_000_000 + i128::from(frac), 3),
            F::Microsecond => num(i128::from(secs % 60) * 1_000_000 + i128::from(frac), 0),
            F::Epoch => num(i128::from(secs) * 1_000_000 + i128::from(frac), 6),
            other => Err(EvalError::TypeMismatch {
                detail: format!(
                    "unit \"{}\" not supported for type {src_name}",
                    alloc::format!("{other}").to_lowercase()
                ),
            }),
        };
    }
    // v7.39 (round 253) — a DATE has no time-of-day: PG rejects those
    // fields (0A000) where SPG silently answered 0.
    if matches!(*v, Value::Date(_))
        && matches!(
            field,
            F::Hour
                | F::Minute
                | F::Second
                | F::Millisecond
                | F::Microsecond
                | F::Timezone
                | F::TimezoneHour
                | F::TimezoneMinute
        )
    {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "unit \"{}\" not supported for type {src_name}",
                format!("{field}").to_lowercase()
            ),
        });
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
    // v7.38 (read01 sweep) — fraction-bearing fields keep sub-second precision
    // as NUMERIC (PG). Epoch / Second scale 6 (microseconds); Millisecond scale 3.
    match field {
        F::Epoch => {
            let total_secs = i64::from(days) * 86_400 + secs;
            // v7.39 (read01 round 104) — epoch from a DATE is a whole-second
            // integer (PG scale 0: `1704067200`); a TIMESTAMP carries the
            // microsecond scale 6 (`1704067200.000000`). SPG used scale 6 for
            // both.
            if matches!(*v, Value::Date(_)) {
                return Ok(Value::Numeric {
                    scaled: i128::from(total_secs),
                    scale: 0,
                    kind: spg_storage::NumericKind::Finite,
                });
            }
            return Ok(Value::Numeric {
                scaled: i128::from(total_secs) * 1_000_000 + i128::from(frac),
                scale: 6,
                kind: spg_storage::NumericKind::Finite,
            });
        }
        F::Second => {
            return Ok(Value::Numeric {
                scaled: i128::from(ss) * 1_000_000 + i128::from(frac),
                scale: 6,
                kind: spg_storage::NumericKind::Finite,
            });
        }
        F::Millisecond => {
            return Ok(Value::Numeric {
                scaled: i128::from(ss) * 1_000_000 + i128::from(frac),
                scale: 3,
                kind: spg_storage::NumericKind::Finite,
            });
        }
        _ => {}
    }
    let result = match field {
        // v7.39 (GUC knife 6, BC) — PG reports the era year: there is
        // no year 0, so astronomical year <= 0 is BC year 1-y,
        // reported negative (0044-03-15 BC -> -44, not -43).
        F::Year => {
            if y <= 0 {
                i64::from(y) - 1
            } else {
                i64::from(y)
            }
        }
        F::Month => i64::from(m),
        F::Day => i64::from(d),
        F::Hour => hh,
        F::Minute => mm,
        F::Second => ss,
        F::Microsecond => ss * 1_000_000 + frac,
        // seconds since the unix epoch (truncated; PG returns
        // numeric with fraction — mailrs casts ::BIGINT anyway).
        F::Epoch => i64::from(days) * 86_400 + secs,
        // 1970-01-01 was a Thursday: dow 4, isodow 4.
        F::Dow => i64::from((days + 4).rem_euclid(7)),
        F::Isodow => i64::from((days + 3).rem_euclid(7)) + 1,
        F::Doy => i64::from(days - days_from_civil(y, 1, 1)) + 1,
        F::Week => iso_week_and_year(days, y).0,
        F::Isoyear => {
            let iso = iso_week_and_year(days, y).1;
            // Same no-year-zero reporting as F::Year.
            if iso <= 0 { iso - 1 } else { iso }
        }
        F::Quarter => i64::from((m - 1) / 3) + 1,
        F::Decade => i64::from(y).div_euclid(10),
        F::Century => era_bucket(y, 100),
        F::Millennium => era_bucket(y, 1000),
        // JD of 1970-01-01 is 2440588. A DATE is the integer day; a
        // TIMESTAMP carries the day fraction — PG renders it as the
        // numeric division's scale-20 form (`2460385.50000000000000000000`).
        F::Julian => {
            if matches!(*v, Value::Timestamp(_)) {
                let jd = i128::from(days) + 2_440_588;
                let scaled =
                    jd * 10_i128.pow(20) + i128::from(day_micros) * 10_i128.pow(20) / 86_400_000_000;
                return Ok(Value::Numeric {
                    scaled,
                    scale: 20,
                    kind: spg_storage::NumericKind::Finite,
                });
            }
            i64::from(days) + 2_440_588
        }
        F::Millisecond => ss * 1_000 + frac / 1_000,
        // SPG sessions run UTC — the offset is honestly zero. The
        // statically-typed `timestamp` rejection lives at the eval arm
        // (the tstz/timestamp split needs the expression's declared
        // type, which a Value alone cannot carry).
        F::Timezone | F::TimezoneHour | F::TimezoneMinute => 0,
        F::Other(_) => unreachable!("handled above"),
    };
    Ok(num0(result))
}

/// v7.39 (round 253) — the PG type name for EXTRACT error messages,
/// from the VALUE's variant. `timestamptz` shares `Value::Timestamp`,
/// so the function form can only say "timestamp without time zone";
/// the EXTRACT arm upgrades the name from the expression's declared
/// type when it is statically known.
pub(super) fn value_src_type_name(v: &Value) -> &'static str {
    match v {
        Value::Date(_) => "date",
        Value::Time(_) => "time without time zone",
        Value::TimeTz { .. } => "time with time zone",
        Value::Interval { .. } => "interval",
        _ => "timestamp without time zone",
    }
}

/// PG counts centuries/millennia from year 1 with no year 0:
/// 2001-2100 is century 21; proleptic year 0 (1 BC) is century -1.
fn era_bucket(y: i32, unit: i32) -> i64 {
    if y > 0 {
        i64::from((y - 1) / unit) + 1
    } else {
        i64::from(y / unit) - 1
    }
}

/// ISO 8601 week number and week-numbering year for a day count
/// (days since 1970-01-01) whose civil year is `y`. Week 1 is the
/// week containing January 4th; weeks run Monday-Sunday.
pub(crate) fn iso_week_and_year(days: i32, y: i32) -> (i64, i64) {
    let isodow = (days + 3).rem_euclid(7) + 1; // 1 = Monday
    let doy = days - days_from_civil(y, 1, 1) + 1;
    let iso_weeks_in = |year: i32| -> i32 {
        // 53-week years: Jan 1 is Thursday, or leap year with
        // Jan 1 on Wednesday (equivalently Dec 31 is Thursday).
        let jan1 = days_from_civil(year, 1, 1);
        let dec31 = days_from_civil(year, 12, 31);
        let jan1_isodow = (jan1 + 3).rem_euclid(7) + 1;
        let dec31_isodow = (dec31 + 3).rem_euclid(7) + 1;
        if jan1_isodow == 4 || dec31_isodow == 4 {
            53
        } else {
            52
        }
    };
    let w = (doy - isodow + 10).div_euclid(7);
    if w < 1 {
        (i64::from(iso_weeks_in(y - 1)), i64::from(y - 1))
    } else if w > iso_weeks_in(y) {
        (1, i64::from(y + 1))
    } else {
        (i64::from(w), i64::from(y))
    }
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
        // PG accepts plural spellings as aliases for the singular fields
        // (matches the EXTRACT parser); quarter/dow/doy/isoyear have no plural.
        "year" | "years" => F::Year,
        "month" | "months" => F::Month,
        "day" | "days" => F::Day,
        "hour" | "hours" => F::Hour,
        "minute" | "minutes" => F::Minute,
        "second" | "seconds" => F::Second,
        "microsecond" | "microseconds" => F::Microsecond,
        "epoch" => F::Epoch,
        "dow" => F::Dow,
        "isodow" => F::Isodow,
        "doy" => F::Doy,
        "week" | "weeks" => F::Week,
        "isoyear" => F::Isoyear,
        "quarter" => F::Quarter,
        "decade" | "decades" => F::Decade,
        "century" | "centuries" => F::Century,
        "millennium" | "millenniums" | "millennia" => F::Millennium,
        "julian" => F::Julian,
        "millisecond" | "milliseconds" => F::Millisecond,
        "timezone" => F::Timezone,
        "timezone_hour" => F::TimezoneHour,
        "timezone_minute" => F::TimezoneMinute,
        other => F::Other(String::from(other)),
    };
    // v7.39 (round 275) — date_part() and EXTRACT do NOT share a
    // field/type matrix, and SPG was wrong in BOTH directions because it
    // borrowed EXTRACT's. Measured on PG 18.4:
    //   date_part('hour', DATE)      → 0        EXTRACT(hour FROM DATE) → error
    //   date_part('timezone', DATE)  → error naming TIMESTAMP, not date
    //   date_part('timezone', TIME)  → error naming TIME
    // The error naming *timestamp* for a DATE argument is the tell: the
    // function form promotes a date to timestamp-at-midnight first, and
    // everything follows from that one promotion. A TIME is not promoted.
    let promoted;
    let mut from_date = false;
    let (arg, src_name) = match &args[1] {
        Value::Date(d) => {
            promoted = Value::Timestamp(i64::from(*d) * 86_400_000_000);
            from_date = true;
            (&promoted, "timestamp without time zone")
        }
        other => (other, value_src_type_name(other)),
    };
    // The timezone fields are rejected on the promoted value, because a
    // timestamp carries no zone to report.
    //
    // Only for a value we KNOW came from a date. SPG stores timestamptz
    // in the same `Value::Timestamp`, so a bare Timestamp here may be
    // either type and `date_part('timezone', <timestamptz>)` legitimately
    // answers 0. Telling the two apart needs the argument's STATIC
    // declared type, which EXTRACT has (eval.rs:2448) and this call path
    // does not — the function dispatch receives values, not expressions.
    // That plumbing is its own round; recorded in the phase-2 ledger.
    if matches!(field, F::Timezone | F::TimezoneHour | F::TimezoneMinute) && from_date {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "unit \"{}\" not supported for type timestamp without time zone",
                format!("{field}").to_lowercase()
            ),
        });
    }
    // v7.38 (read01) — date_part() returns `double precision`, whereas EXTRACT
    // returns `numeric` (PG 14+). extract_field builds the numeric form; demote
    // it to f64 so `date_part('epoch', …)` renders like PG's double (no trailing
    // `.000000`) and pg_typeof reports double precision.
    Ok(match extract_field(&field, arg, src_name)? {
        Value::Numeric { scaled, scale, .. } =>
        {
            #[allow(clippy::cast_precision_loss)]
            Value::Float(scaled as f64 / 10f64.powi(i32::from(scale)))
        }
        other => other,
    })
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
    // ie the "wall-clock" age relative to midnight today.
    // v7.39 (read01 round 97) — when a clock IS set, `rewrite_clock_calls`
    // rewrites `age(t)` to the two-arg form with today's real midnight before
    // eval, so this single-arg path is only reached with NO clock (the
    // embedded engine). There we anchor at 2020-01-01 UTC — predictable and
    // deterministic (any test using single-arg age can subtract).
    let (a, b) = if args.len() == 1 {
        // 2020-01-01 UTC as micros since epoch — no wall-clock
        // dependency, deterministic across runs.
        const ANCHOR_2020_UTC: i64 = 1_577_836_800_000_000;
        (ANCHOR_2020_UTC, to_micros(&args[0])?)
    } else {
        (to_micros(&args[0])?, to_micros(&args[1])?)
    };
    // PG's AGE is a *calendar* difference: it breaks the span into
    // years/months/days by walking the civil calendar with borrows, so
    // `age('2024-03-15','2024-01-10')` reads `2 mons 5 days`, not
    // `65 days`. Compute the positive span (hi - lo) field-by-field, then
    // negate every field if the arguments were reversed.
    const US_PER_DAY: i64 = 86_400_000_000;
    let neg = a < b;
    let (hi, lo) = if neg { (b, a) } else { (a, b) };
    let split = |us: i64| -> (i32, i64) {
        (
            i32::try_from(us.div_euclid(US_PER_DAY)).unwrap_or(i32::MAX),
            us.rem_euclid(US_PER_DAY),
        )
    };
    let (hd, ht) = split(hi);
    let (ld, lt) = split(lo);
    let (y1, m1, d1) = civil_from_days(hd);
    let (y2, m2, d2) = civil_from_days(ld);
    // days in month (m) of year (y) — via civil day arithmetic, no leap
    // table needed.
    let dim = |y: i32, m: u32| -> i64 {
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        i64::from(days_from_civil(ny, nm, 1) - days_from_civil(y, m, 1))
    };
    let mut micros = ht - lt;
    let mut mday = i64::from(d1) - i64::from(d2);
    let mut mon = i64::from(m1) - i64::from(m2);
    let mut year = i64::from(y1) - i64::from(y2);
    if micros < 0 {
        micros += US_PER_DAY;
        mday -= 1;
    }
    while mday < 0 {
        mon -= 1;
        mday += dim(y2, m2);
    }
    while mon < 0 {
        mon += 12;
        year -= 1;
    }
    let mut months = year * 12 + mon;
    if neg {
        months = -months;
        mday = -mday;
        micros = -micros;
    }
    Ok(Value::Interval {
        months: i32::try_from(months).map_err(|_| EvalError::TypeMismatch {
            detail: "age() month count exceeds i32".into(),
        })?,
        days: i32::try_from(mday).map_err(|_| EvalError::TypeMismatch {
            detail: "age() day count exceeds i32".into(),
        })?,
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
        // MySQL accepts string datetimes anywhere a DATETIME is
        // expected.
        Value::Text(s) => {
            let t = parse_text_datetime(s).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("date_format(): cannot parse datetime {s:?}"),
            })?;
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
            // v7.39 (round 357, M17) — the specifiers that were echoed as
            // bare letters, so a format string came back with `W` where
            // MariaDB writes `Monday`. Every value below is from the
            // MariaDB 11 run over nine dates (2024-01-01/07/15,
            // 2023-01-01, 2024-12-31, 2024-02-29, 2024-03-02/03,
            // 2021-01-01), not from the manual.
            b'k' => {
                let _ = write!(out, "{hh24}");
            }
            b'l' => {
                let _ = write!(out, "{hh12}");
            }
            b'W' => {
                out.push_str(DAY_FULL[weekday_sunday0(days) as usize]);
            }
            b'a' => {
                out.push_str(DAY_ABBR[weekday_sunday0(days) as usize]);
            }
            b'w' => {
                let _ = write!(out, "{}", weekday_sunday0(days));
            }
            b'j' => {
                let _ = write!(out, "{:03}", day_of_year(y, mo, d));
            }
            // `15th`, `1st`, `2nd`, `3rd` — English ordinal.
            b'D' => {
                let _ = write!(out, "{d}{}", ordinal_suffix(d));
            }
            b'r' => {
                let _ = write!(out, "{hh12:02}:{mi:02}:{ss:02} {ampm}");
            }
            b'T' => {
                let _ = write!(out, "{hh24:02}:{mi:02}:{ss:02}");
            }
            // Week numbers. `%U` counts from Sunday and `%u` from Monday,
            // both 00-based; `%V`/`%X` are the Sunday-start pair whose
            // week 1 begins on the year's FIRST SUNDAY, and `%v`/`%x` are
            // ISO-8601. All four verified against the measured table.
            b'U' => {
                let _ = write!(out, "{:02}", week_of_year(y, mo, d, days, false));
            }
            b'u' => {
                let _ = write!(out, "{:02}", week_of_year(y, mo, d, days, true));
            }
            b'V' => {
                let _ = write!(out, "{:02}", sunday_week(days).1);
            }
            b'X' => {
                let _ = write!(out, "{:04}", sunday_week(days).0);
            }
            b'v' => {
                let _ = write!(out, "{:02}", iso_week(days).1);
            }
            b'x' => {
                let _ = write!(out, "{:04}", iso_week(days).0);
            }
            // SPG keeps one clock, in UTC.
            b'Z' => {
                out.push_str("UTC");
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

const DAY_FULL: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const DAY_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// v7.39 (round 357, M17) — weekday with Sunday = 0, which is what `%w`
/// reports. 1970-01-01 was a Thursday.
fn weekday_sunday0(days: i32) -> u32 {
    u32::try_from((days + 4).rem_euclid(7)).unwrap_or(0)
}

fn ordinal_suffix(d: u32) -> &'static str {
    match (d % 10, d % 100) {
        (1, 1 | 21 | 31) => "st",
        (2, 2 | 22) => "nd",
        (3, 3 | 23) => "rd",
        _ => "th",
    }
}

fn day_of_year(y: i32, mo: u32, d: u32) -> u32 {
    const CUM: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    CUM[(mo - 1) as usize] + d + u32::from(leap && mo > 2)
}

/// v7.39 (round 378) — MySQL `WEEK(date, mode)` / `YEARWEEK(date, mode)`
/// with the full 0..7 mode set. Returns `(year, week)`. The mode bits:
/// `1` = weeks start Monday (else Sunday); `2` = "with year" (a leading
/// partial week rolls into the previous year's last week instead of being
/// week 0); `4` = week 1 is the first week that contains the year's first
/// week-start day (else the first week with 4+ days this year). Clean-room
/// from those documented rules, validated against MariaDB 11 across the
/// measured dates (2020/2021 Jan 1, mid-year, Dec 31). PG has no WEEK mode.
pub(crate) fn mysql_calc_week(days: i32, mode: u32) -> (i32, u32) {
    // MySQL normalises the mode first: when Monday-first is NOT set, the
    // "first weekday" bit is flipped, so mode 0 means "week 1 is the first
    // week containing a Sunday", mode 4 means "first week with 4+ days",
    // etc. (this is MySQL's `week_mode`).
    let mode = {
        let m = mode & 7;
        if m & 1 == 0 {
            m ^ 4
        } else {
            m
        }
    };
    let monday_first = mode & 1 != 0;
    let mut week_year = mode & 2 != 0;
    let first_weekday = mode & 4 != 0;
    // Weekday of a day number, 0 = the week's first day (Monday or Sunday).
    let weekday = |dn: i32| -> i32 {
        if monday_first {
            (dn + 3).rem_euclid(7)
        } else {
            (dn + 4).rem_euclid(7)
        }
    };
    let days_in_year = |yr: i32| -> i32 {
        if (yr % 4 == 0 && yr % 100 != 0) || yr % 400 == 0 {
            366
        } else {
            365
        }
    };
    // Does the year's first week start with a leading partial (so the
    // year's first `wd` days belong to the previous week)? Mirrors MySQL's
    // `(first_weekday && weekday != 0) || (!first_weekday && weekday >= 4)`.
    let partial_before = |wd: i32| -> bool {
        if first_weekday {
            wd != 0
        } else {
            wd >= 4
        }
    };
    let (mut year, mo, d) = civil_from_days(days);
    let mut first_daynr = days_from_civil(year, 1, 1);
    let mut wd = weekday(first_daynr);
    if mo == 1 && i32::try_from(d).unwrap_or(1) <= 7 - wd {
        if !week_year && partial_before(wd) {
            return (year, 0);
        }
        week_year = true;
        year -= 1;
        let diy = days_in_year(year);
        first_daynr -= diy;
        wd = (wd + 7 - diy.rem_euclid(7)).rem_euclid(7);
    }
    let offset = if partial_before(wd) {
        days - (first_daynr + (7 - wd))
    } else {
        days - (first_daynr - wd)
    };
    if week_year && offset >= 52 * 7 {
        let diy = days_in_year(year);
        let wd_end = (wd + diy).rem_euclid(7);
        if !partial_before(wd_end) {
            return (year + 1, 1);
        }
    }
    (year, u32::try_from(offset / 7 + 1).unwrap_or(1))
}

/// `%U` / `%u`: 00-based week, counting from the year's first Sunday (or
/// Monday). Verified against MariaDB for all nine measured dates.
fn week_of_year(y: i32, mo: u32, d: u32, days: i32, monday_first: bool) -> u32 {
    let doy = day_of_year(y, mo, d);
    let w = if monday_first {
        (weekday_sunday0(days) + 6) % 7
    } else {
        weekday_sunday0(days)
    };
    (doy + 6 - w) / 7
}

/// `%V` / `%X`: week 1 begins on the year's FIRST SUNDAY; a date before
/// it belongs to the previous year's last week (measured: 2024-01-01 is
/// week 53 of 2023).
fn sunday_week(days: i32) -> (i32, u32) {
    reckon_week(days, 0)
}

/// `%v` / `%x`: ISO-8601 — weeks start on Monday and week 1 is the one
/// holding the year's first Thursday.
fn iso_week(days: i32) -> (i32, u32) {
    reckon_week(days, 1)
}

/// Shared week reckoning. `start` is the weekday the week begins on
/// (0 = Sunday, 1 = Monday). For the Sunday form week 1 starts on the
/// first such weekday of the year; for the Monday form it is ISO's
/// first-Thursday rule.
fn reckon_week(days: i32, start: u32) -> (i32, u32) {
    let (y, _, _) = civil_from_days(days);
    for probe in [y + 1, y, y - 1] {
        let first = days_from_civil(probe, 1, 1);
        let first_wd = weekday_sunday0(first);
        // Days from Jan 1 to the first `start` weekday.
        let offset = i32::try_from((start + 7 - first_wd) % 7).unwrap_or(0);
        let week1 = if start == 1 {
            // ISO: week 1 holds the first Thursday, i.e. it starts on the
            // Monday on or before Jan 4.
            let jan4 = days_from_civil(probe, 1, 4);
            jan4 - i32::try_from((weekday_sunday0(jan4) + 6) % 7).unwrap_or(0)
        } else {
            first + offset
        };
        if days >= week1 {
            let w = u32::try_from((days - week1) / 7 + 1).unwrap_or(1);
            return (probe, w);
        }
    }
    (y, 1)
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
        Value::Timestamp(t) => Ok(unix_seconds(*t)),
        Value::Date(d) => Ok(Value::BigInt(i64::from(*d) * 86_400)),
        // v7.39 (round 356, M14) — MySQL reads a date/datetime STRING
        // here, which is how the function is nearly always called:
        // `UNIX_TIMESTAMP('2024-01-15 00:00:00')` is 1705276800 and
        // `UNIX_TIMESTAMP('2024-01-15')` the same (measured on MariaDB
        // 11, session time zone UTC). It refused every string outright.
        // A fractional part is kept: `'…10:30:45.5'` is 1705314645.5.
        Value::Text(t) | Value::BpChar(t) => Ok(text_unix_seconds(t)),
        // …and the bare numeric YYYYMMDD form (`20240115`).
        Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_) => {
            let n = match &args[0] {
                Value::Int(n) => i64::from(*n),
                Value::SmallInt(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                _ => unreachable!("guarded above"),
            };
            Ok(text_unix_seconds(&alloc::format!("{n}")))
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!(
                "unix_timestamp() needs DATE or TIMESTAMP, got {:?}",
                other.data_type()
            ),
        }),
    }
}

/// Whole seconds when there is no fraction, a double when there is —
/// MariaDB answers 1705314645.5 for a `.5` datetime.
fn unix_seconds(micros: i64) -> Value<'static> {
    let frac = micros.rem_euclid(1_000_000);
    if frac == 0 {
        Value::BigInt(micros.div_euclid(1_000_000))
    } else {
        #[allow(clippy::cast_precision_loss)]
        Value::Float(micros as f64 / 1_000_000.0)
    }
}

/// v7.39 (round 356, M14) — a date / datetime string (or a bare
/// `YYYYMMDD`) as epoch seconds. Anything unreadable is NULL, which is
/// what MariaDB answers for `'not a date'` and for `''` — not an error.
fn text_unix_seconds(t: &str) -> Value<'static> {
    let trimmed = t.trim();
    // `20240115` — the numeric form, spelled either way.
    if trimmed.len() == 8 && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        let iso = alloc::format!("{}-{}-{}", &trimmed[..4], &trimmed[4..6], &trimmed[6..]);
        return crate::eval::parse_date_literal(&iso)
            .map_or(Value::Null, |d| Value::BigInt(i64::from(d) * 86_400));
    }
    if let Some(micros) = crate::eval::parse_timestamp_literal(trimmed) {
        return unix_seconds(micros);
    }
    crate::eval::parse_date_literal(trimmed)
        .map_or(Value::Null, |d| Value::BigInt(i64::from(d) * 86_400))
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
        Value::Numeric { scaled, scale, .. } => {
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
pub(super) fn date_trunc(
    args: &[Value<'_>],
    ctx: &super::EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    // v7.39 (round 221) — PG 12+ `date_trunc(unit, tstz, zone)`: truncate in
    // the ZONE's local calendar, return the UTC instant of that local
    // boundary (DST-correct via the tzdb's reverse lookup).
    if args.len() == 3 {
        if args.iter().any(|a| matches!(a, Value::Null)) {
            return Ok(Value::Null);
        }
        let Value::Text(zone) = &args[2] else {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "date_trunc() zone must be text, got {:?}",
                    args[2].data_type()
                ),
            });
        };
        let z = zone.trim();
        let ts = text_or_temporal_micros(&args[1], "date_trunc")?;
        let off = ctx.zone_offset_at(z, ts).ok_or_else(|| EvalError::TypeMismatch {
            detail: format!("date_trunc({z:?}): time zone not recognized"),
        })?;
        let local = Value::Timestamp(ts + off);
        let truncated = date_trunc(&[args[0].clone(), local], ctx)?;
        let Value::Timestamp(tl) = truncated else {
            return Ok(truncated);
        };
        // Local boundary back to UTC (reverse lookup handles a boundary
        // that lands on the other side of a DST transition).
        let utc = ctx.zone_local_to_utc(z, tl).unwrap_or(tl - off);
        return Ok(Value::Timestamp(utc));
    }
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
    // v7.39 (read01 timestamp.c) — the INTERVAL overload truncates the
    // interval's fields below the unit (PG interval_trunc).
    if let Value::Interval {
        months,
        days,
        micros,
    } = &args[1]
    {
        let unit_lc = unit.to_ascii_lowercase();
        let (mut mo, mut dd, mut us) = (*months, *days, *micros);
        match unit_lc.as_str() {
            "millennium" => {
                mo = mo / 12000 * 12000;
                dd = 0;
                us = 0;
            }
            "century" => {
                mo = mo / 1200 * 1200;
                dd = 0;
                us = 0;
            }
            "decade" => {
                mo = mo / 120 * 120;
                dd = 0;
                us = 0;
            }
            "year" => {
                mo = mo / 12 * 12;
                dd = 0;
                us = 0;
            }
            "quarter" => {
                mo = mo / 3 * 3;
                dd = 0;
                us = 0;
            }
            "month" => {
                dd = 0;
                us = 0;
            }
            "day" => us = 0,
            "hour" => us = us / 3_600_000_000 * 3_600_000_000,
            "minute" => us = us / 60_000_000 * 60_000_000,
            "second" => us = us / 1_000_000 * 1_000_000,
            "milliseconds" | "millisecond" => us = us / 1_000 * 1_000,
            "microseconds" | "microsecond" => {}
            other => {
                return Err(EvalError::TypeMismatch {
                    detail: format!("unit \"{other}\" not supported for type interval"),
                });
            }
        }
        return Ok(Value::Interval {
            months: mo,
            days: dd,
            micros: us,
        });
    }
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
    const DAY: i64 = 86_400_000_000;
    let truncated = match unit_lc.as_str() {
        "millennium" => {
            // Millennia run 2001-3000; truncate to the first year.
            let my = if y > 0 {
                (y - 1) / 1000 * 1000 + 1
            } else {
                y / 1000 * 1000 - 999
            };
            i64::from(days_from_civil(my, 1, 1)) * DAY
        }
        "century" => {
            let cy = if y > 0 {
                (y - 1) / 100 * 100 + 1
            } else {
                y / 100 * 100 - 99
            };
            i64::from(days_from_civil(cy, 1, 1)) * DAY
        }
        "decade" => i64::from(days_from_civil(y.div_euclid(10) * 10, 1, 1)) * DAY,
        "year" => i64::from(days_from_civil(y, 1, 1)) * DAY,
        "quarter" => {
            let qm = (m - 1) / 3 * 3 + 1;
            i64::from(days_from_civil(y, qm, 1)) * DAY
        }
        "month" => i64::from(days_from_civil(y, m, 1)) * DAY,
        // ISO week starts Monday; 1970-01-01 was a Thursday (isodow 4).
        "week" => {
            let isodow = (day_i32 + 3).rem_euclid(7); // 0 = Monday
            i64::from(day_i32 - isodow) * DAY
        }
        "day" => days * DAY,
        "hour" => days * DAY + (day_micros / 3_600_000_000) * 3_600_000_000,
        "minute" => days * DAY + (day_micros / 60_000_000) * 60_000_000,
        "second" => days * DAY + (day_micros / 1_000_000) * 1_000_000,
        "milliseconds" | "millisecond" => days * DAY + (day_micros / 1_000) * 1_000,
        "microseconds" | "microsecond" => micros,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "unknown date_trunc unit {other:?}; supported: millennium, \
                     century, decade, year, quarter, month, week, day, hour, \
                     minute, second, milliseconds, microseconds"
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
        "JANUARY",
        "FEBRUARY",
        "MARCH",
        "APRIL",
        "MAY",
        "JUNE",
        "JULY",
        "AUGUST",
        "SEPTEMBER",
        "OCTOBER",
        "NOVEMBER",
        "DECEMBER",
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
                Some(v) => {
                    year = if v >= 70 {
                        1900 + v as i32
                    } else {
                        2000 + v as i32
                    }
                }
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
                    let tag: alloc::string::String = inp[ip..ip + 2]
                        .iter()
                        .collect::<alloc::string::String>()
                        .to_ascii_uppercase();
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
                let rest: alloc::string::String = inp[ip..]
                    .iter()
                    .collect::<alloc::string::String>()
                    .to_ascii_uppercase();
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
            let h: i64 = parts.next().and_then(|x| x.parse().ok()).ok_or_else(|| {
                EvalError::TypeMismatch {
                    detail: format!("time_format(): cannot parse time {s:?}"),
                }
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
            let (ly, lm, _) =
                civil_from_days(i32::try_from(lo.div_euclid(86_400_000_000)).unwrap_or(0));
            let (hy, hm, _) =
                civil_from_days(i32::try_from(hi.div_euclid(86_400_000_000)).unwrap_or(0));
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

/// v7.37.17 (17.6 siblings) — MySQL GET_FORMAT({DATE|TIME|DATETIME},
/// {'EUR'|'USA'|'JIS'|'ISO'|'INTERNAL'}): the fixed format-string
/// table from the MySQL manual. The parser lowers the bare type
/// keyword onto a string literal.
pub(super) fn get_format_mysql(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("get_format() takes 2 args, got {}", args.len()),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let (Value::Text(kind), Value::Text(region)) = (&args[0], &args[1]) else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "get_format() takes (type keyword, region text), got ({:?}, {:?})",
                args[0].data_type(),
                args[1].data_type()
            ),
        });
    };
    let fmt = match (
        kind.to_ascii_lowercase().as_str(),
        region.to_ascii_uppercase().as_str(),
    ) {
        ("date", "USA") => "%m.%d.%Y",
        ("date", "JIS" | "ISO") => "%Y-%m-%d",
        ("date", "EUR") => "%d.%m.%Y",
        ("date", "INTERNAL") => "%Y%m%d",
        ("datetime" | "timestamp", "USA" | "JIS" | "ISO") => "%Y-%m-%d %H.%i.%s",
        ("datetime" | "timestamp", "EUR") => "%Y-%m-%d %H.%i.%s",
        ("datetime" | "timestamp", "INTERNAL") => "%Y%m%d%H%i%s",
        ("time", "USA") => "%h:%i:%s %p",
        ("time", "JIS" | "ISO") => "%H:%i:%s",
        ("time", "EUR") => "%H.%i.%s",
        ("time", "INTERNAL") => "%H%i%s",
        // Unknown region → NULL (MySQL behaviour).
        _ => return Ok(Value::Null),
    };
    Ok(Value::text(String::from(fmt)))
}

/// PG `timezone(zone, ts)` — the function form of `AT TIME ZONE`.
/// UTC / GMT and explicit '±HH[:MM]' offsets shift for real: the
/// input is treated as UTC-stored micros and the result is the
/// zone-local naive timestamp (the dominant timestamptz →
/// timestamp display direction; SPG's single timestamp
/// representation cannot distinguish the reverse). v7.39 (round
/// 221) — named IANA zones (`Asia/Tokyo`) resolve through the
/// host tzdb (`ctx.zone_offset_at`, DST-correct per instant);
/// only a host with no zoneinfo still errors honestly.
pub(super) fn timezone_pg(
    args: &[Value<'_>],
    ctx: &super::EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("timezone() takes 2 args, got {}", args.len()),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    // v7.39 (round 221) — `timetz AT TIME ZONE zone`: re-anchor the
    // time-of-day to the target zone's offset, preserving the instant
    // (PG: `12:00:00+05` AT TIME ZONE 'UTC' → `07:00:00+00`).
    if let Value::TimeTz { .. } = &args[1] {
        return timetz_at_zone(args, ctx);
    }
    let Value::Text(zone) = &args[0] else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "timezone() zone must be text, got {:?}",
                args[0].data_type()
            ),
        });
    };
    let z = zone.trim();
    // PG's AT TIME ZONE has a sign quirk that a uniform offset can't capture: a
    // NUMERIC offset (`+09`) is added to the naive time, but a NAMED zone (`JST`)
    // subtracts its UTC offset. `tz_abbrev_offset` already stores the applied
    // (naive→UTC) offset, so the two spellings keep their own branches here.
    let offset = if z.eq_ignore_ascii_case("utc") || z.eq_ignore_ascii_case("gmt") {
        0
    } else if let Some(off) = parse_tz_offset(z) {
        off
    } else if let Ok(h) = z.parse::<i64>() {
        if h.abs() > 14 {
            return Err(EvalError::TypeMismatch {
                detail: format!("timezone(): offset {h} out of range"),
            });
        }
        h * 3_600_000_000
    } else if let Some(applied) = tz_abbrev_offset(z) {
        applied
    } else {
        // v7.39 (round 221) — a named IANA zone resolves through the host
        // tzdb, DST-correct at the input instant. The instant is needed
        // first, so this branch reads it early (same call as below —
        // text_or_temporal_micros is pure).
        let ts = text_or_temporal_micros(&args[1], "timezone")?;
        let Some(off) = ctx.zone_offset_at(z, ts) else {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "timezone({z:?}): time zone not recognized (no tzdata entry \
                     on this host); use UTC, a fixed abbreviation (EST/PST/JST/\
                     CET/…), or an explicit '+HH:MM' offset"
                ),
            });
        };
        return Ok(Value::Timestamp(ts + off));
    };
    let ts = text_or_temporal_micros(&args[1], "timezone")?;
    Ok(Value::Timestamp(ts + offset))
}

/// v7.39 (round 221) — `timezone(zone, timetz)`: keep the instant, swap the
/// carried offset (PG's timetz AT TIME ZONE). The wall-clock moves by the
/// offset delta; the result is a `timetz` at the target offset.
fn timetz_at_zone(
    args: &[Value<'_>],
    ctx: &super::EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let Value::Text(zone) = &args[0] else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "timezone() zone must be text, got {:?}",
                args[0].data_type()
            ),
        });
    };
    let Value::TimeTz { us, offset_secs } = &args[1] else {
        unreachable!("caller matched TimeTz");
    };
    let z = zone.trim();
    // Resolve the target offset. A DST-varying named zone needs an instant;
    // timetz carries none, so PG uses the current date — mirror via the
    // session clock when available, else epoch (fixed zones unaffected).
    let now = ctx.clock.map_or(0, |c| c());
    let target_secs = if z.eq_ignore_ascii_case("utc") || z.eq_ignore_ascii_case("gmt") {
        0
    } else if let Some(off) = parse_tz_offset(z) {
        (off / 1_000_000) as i32
    } else if let Some(off) = tz_abbrev_offset(z).or_else(|| ctx.zone_offset_at(z, now)) {
        (off / 1_000_000) as i32
    } else {
        return Err(EvalError::TypeMismatch {
            detail: format!("timezone({z:?}): time zone not recognized"),
        });
    };
    // Same instant, new offset: shift wall-clock by the delta, wrap to 24h.
    const DAY: i64 = 86_400_000_000;
    let delta_us = (i64::from(target_secs) - i64::from(*offset_secs)) * 1_000_000;
    let new_us = (us + delta_us).rem_euclid(DAY);
    Ok(Value::TimeTz {
        us: new_us,
        offset_secs: target_secs,
    })
}

/// v7.38 (T-tstz Phase 2) — the micro-offset a fixed time-zone spelling adds to
/// a UTC instant to reach zone-local wall-clock (JST → +9h, EST → -5h). Accepts
/// `UTC` / `GMT`, an explicit `±HH[:MM]` / bare-hour offset, and the fixed
/// abbreviations in `tz_abbrev_offset`. Returns `None` for a named IANA zone
/// (no tzdata) — the caller decides whether that errors or falls back to UTC.
///
/// Note the sign: `tz_abbrev_offset` stores the *applied* offset (`-utc_offset`,
/// the `AT TIME ZONE` naive→UTC direction), so it is negated here to give the
/// UTC→local rendering direction this function promises.
pub(crate) fn resolve_zone_offset(z: &str) -> Option<i64> {
    let z = z.trim();
    if z.is_empty() || z.eq_ignore_ascii_case("utc") || z.eq_ignore_ascii_case("gmt") {
        return Some(0);
    }
    if let Some(off) = parse_tz_offset(z) {
        return Some(off);
    }
    // Bare numeric hours: `+5`, `5`, `-9`.
    if let Ok(h) = z.parse::<i64>() {
        return (h.abs() <= 14).then_some(h * 3_600_000_000);
    }
    tz_abbrev_offset(z).map(|applied| -applied)
}

/// v7.38 (read01, T-timezone) — the applied micro-offset for a fixed
/// (non-DST-varying) time-zone abbreviation, as PG treats them. The value is
/// `-utc_offset` (a naive TIMESTAMP in the zone is `ts + applied` in UTC), so
/// EST (UTC-5) → +5h. Half-hour zones are included where unambiguous. Ambiguous
/// abbreviations (e.g. IST) are intentionally omitted.
fn tz_abbrev_offset(z: &str) -> Option<i64> {
    const H: i64 = 3_600_000_000;
    let m = |h: i64, min: i64| h * H + min * 60_000_000;
    let off = match z.to_ascii_uppercase().as_str() {
        "EST" => m(5, 0),
        "EDT" => m(4, 0),
        "CST" => m(6, 0),
        "CDT" => m(5, 0),
        "MST" => m(7, 0),
        "MDT" => m(6, 0),
        "PST" => m(8, 0),
        "PDT" => m(7, 0),
        "AKST" => m(9, 0),
        "AKDT" => m(8, 0),
        "HST" => m(10, 0),
        "JST" | "KST" => m(-9, 0),
        "CET" | "WEST" => m(-1, 0),
        "CEST" | "EET" | "SAST" => m(-2, 0),
        "EEST" | "MSK" => m(-3, 0),
        "WET" | "GMT" | "UTC" => 0,
        "BST" | "IST_IE" => m(-1, 0),
        "AEST" => m(-10, 0),
        "AEDT" => m(-11, 0),
        "NZST" => m(-12, 0),
        "ACST" => m(-9, -30),
        _ => return None,
    };
    Some(off)
}

/// Parse a '+HH:MM' / '-HH:MM' timezone offset into micros.
fn parse_tz_offset(s: &str) -> Option<i64> {
    let s = s.trim();
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1_i64, &s[1..]),
        b'-' => (-1_i64, &s[1..]),
        _ => return None,
    };
    let (h, m) = rest.split_once(':')?;
    let h: i64 = h.parse().ok()?;
    let m: i64 = m.parse().ok()?;
    if h > 14 || m > 59 {
        return None;
    }
    Some(sign * (h * 3_600_000_000 + m * 60_000_000))
}

/// v7.37.17 (17.6 siblings) — MySQL CONVERT_TZ(dt, from_tz, to_tz).
/// Offset forms ('+HH:MM') shift for real. Named zones return NULL —
/// faithful to MySQL's behaviour when the mysql.time_zone tables
/// aren't loaded (SPG carries no tzdata).
pub(super) fn convert_tz_mysql(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 3 {
        return Err(EvalError::TypeMismatch {
            detail: format!("convert_tz() takes 3 args, got {}", args.len()),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let ts = text_or_temporal_micros(&args[0], "convert_tz")?;
    let (Value::Text(from), Value::Text(to)) = (&args[1], &args[2]) else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "convert_tz() timezones must be text, got ({:?}, {:?})",
                args[1].data_type(),
                args[2].data_type()
            ),
        });
    };
    let (Some(from_off), Some(to_off)) = (parse_tz_offset(from), parse_tz_offset(to)) else {
        // Named zone without tzdata → NULL, like MySQL with
        // unloaded time-zone tables.
        return Ok(Value::Null);
    };
    Ok(Value::Timestamp(ts - from_off + to_off))
}
