//! String / text SQL functions split out of `eval.rs` (cut 28):
//! `left` / `right` (string_left_right), `lpad` / `rpad` (string_pad),
//! `trim` / `ltrim` / `rtrim` (string_trim + TrimSide), `format`
//! (format_string), `to_char`, plus the `pg_typeof` name lookup and
//! the `value_to_format_text` coercion shared by concat / replace /
//! split_part / position dispatch in eval.rs. The date helpers
//! (`civil_from_days`, `MONTH_FULL` / `MONTH_ABBR`) stay in eval.rs
//! since `date_format_mysql` and the timestamp paths share them.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_storage::Value;

use super::{EvalError, MONTH_ABBR, MONTH_FULL, civil_from_days};

// PG trim family: which side to strip.
#[derive(Debug, Clone, Copy)]
pub(super) enum TrimSide {
    Left,
    Right,
    Both,
}

/// PG `left(s, n)` / `right(s, n)` shared implementation. Both
/// support negative n which means "all but |n| chars from the
/// opposite side". n=0 → ''. Codepoint-counted. NULL → NULL.
pub(super) fn string_left_right(
    args: &[Value<'_>],
    is_left: bool,
    fn_name: &str,
) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("{fn_name}() takes 2 args, got {}", args.len()),
        });
    }
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let s = value_to_format_text(&args[0]);
    let n = match &args[1] {
        Value::SmallInt(x) => i64::from(*x),
        Value::Int(x) => i64::from(*x),
        Value::BigInt(x) => *x,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "{fn_name}(): n must be integer, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    if n == 0 {
        return Ok(Value::text(String::new()));
    }
    let (start, end) = if is_left {
        if n > 0 {
            (0usize, (n.min(len)) as usize)
        } else {
            // left(s, -k) → drop last |k| chars; keep [0..len - k]
            let drop = (-n).min(len);
            (0usize, (len - drop) as usize)
        }
    } else if n > 0 {
        // right(s, k) → keep last k chars; start = max(0, len-k)
        let start = (len - n).max(0);
        (start as usize, len as usize)
    } else {
        // right(s, -k) → drop first |k| chars; keep [k..len]
        let drop = (-n).min(len);
        (drop as usize, len as usize)
    };
    if start >= end {
        return Ok(Value::text(String::new()));
    }
    Ok(Value::text(chars[start..end].iter().collect::<String>()))
}

/// PG `lpad` / `rpad` shared implementation. Length is the
/// target codepoint count. When the input is longer than `length`,
/// truncate keeping the LEFT side (both lpad and rpad agree with
/// PG here). When shorter, pad with `fill` (default SPACE) cycling
/// for multi-char fills, on the appropriate side. Empty fill +
/// needs padding → returns input verbatim (potentially
/// truncated). NULL on any arg → NULL.
pub(super) fn string_pad(
    args: &[Value<'_>],
    is_left: bool,
    fn_name: &str,
) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 && args.len() != 3 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("{fn_name}() takes 2 or 3 args, got {}", args.len()),
        });
    }
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let s = value_to_format_text(&args[0]);
    let target = match &args[1] {
        Value::SmallInt(x) => i64::from(*x),
        Value::Int(x) => i64::from(*x),
        Value::BigInt(x) => *x,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "{fn_name}(): length must be integer, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let fill = if args.len() == 3 {
        value_to_format_text(&args[2])
    } else {
        String::from(" ")
    };
    if target <= 0 {
        return Ok(Value::text(String::new()));
    }
    let target = target as usize;
    let s_chars: Vec<char> = s.chars().collect();
    if s_chars.len() >= target {
        // Truncate from the right (PG keeps LEFT side for both
        // lpad and rpad).
        return Ok(Value::text(s_chars[..target].iter().collect::<String>()));
    }
    if fill.is_empty() {
        return Ok(Value::text(s));
    }
    let pad_needed = target - s_chars.len();
    let fill_chars: Vec<char> = fill.chars().collect();
    let mut padding = String::with_capacity(pad_needed * 4);
    for i in 0..pad_needed {
        padding.push(fill_chars[i % fill_chars.len()]);
    }
    if is_left {
        Ok(Value::text(padding + &s))
    } else {
        Ok(Value::text(s + &padding))
    }
}

/// PG `trim` / `ltrim` / `rtrim` / `btrim` shared implementation.
/// Accepts 1 or 2 args; coerces both to text via the standard
/// `value_to_format_text` helper; treats the chars arg as a SET
/// of UTF-8 codepoints (not a substring). NULL on either arg
/// poisons the result.
pub(super) fn string_trim(
    args: &[Value<'_>],
    side: TrimSide,
    fn_name: &str,
) -> Result<Value<'static>, EvalError> {
    let (input, chars_str) = match args {
        [v] => (v.clone(), String::from(" ")),
        [v, c] => (v.clone(), {
            // NULL chars poisons.
            if matches!(c, Value::Null) {
                return Ok(Value::Null);
            }
            value_to_format_text(c)
        }),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("{fn_name}() takes 1 or 2 args, got {}", args.len()),
            });
        }
    };
    if matches!(input, Value::Null) {
        return Ok(Value::Null);
    }
    let s = value_to_format_text(&input);
    let charset: alloc::collections::BTreeSet<char> = chars_str.chars().collect();
    let chars: Vec<char> = s.chars().collect();
    let mut start = 0usize;
    let mut end = chars.len();
    if matches!(side, TrimSide::Left | TrimSide::Both) {
        while start < end && charset.contains(&chars[start]) {
            start += 1;
        }
    }
    if matches!(side, TrimSide::Right | TrimSide::Both) {
        while end > start && charset.contains(&chars[end - 1]) {
            end -= 1;
        }
    }
    Ok(Value::text(chars[start..end].iter().collect::<String>()))
}

/// v7.17.0 Phase 3.8 — PG `format(fmtstr, args…)` with
/// sprintf-style conversion specifiers. Subset covered:
///   * `%s` — text rendering of the arg
///   * `%I` — quoted SQL identifier (always double-quoted; embedded
///     `"` doubled per SQL grammar)
///   * `%L` — quoted SQL literal (single-quoted; embedded `'`
///     doubled; NULL → literal `NULL`)
///   * `%%` — literal `%`
///   * `%n$X` — argument position (1-based) before the specifier
///     character (e.g. `%2$s` picks the 2nd arg)
pub(super) fn format_string(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.is_empty() {
        return Err(EvalError::TypeMismatch {
            detail: "format() takes at least 1 arg (format string)".into(),
        });
    }
    let fmt = match &args[0] {
        Value::Text(s) => s.clone(),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "format(): first arg must be text, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let arg_values = &args[1..];
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    // Position cursor — next implicit arg picked when no `n$`
    // prefix is given. PG's format uses a 1-based cursor that
    // advances on each implicit-position spec.
    let mut implicit_cursor: usize = 0;
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // Parse optional `n$` position prefix.
        let mut explicit_pos: Option<usize> = None;
        // Buffer the digits so we can roll back if no `$` follows.
        let mut digit_buf = String::new();
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
                digit_buf.push(d);
                chars.next();
            } else {
                break;
            }
        }
        if !digit_buf.is_empty() && matches!(chars.peek(), Some(&'$')) {
            chars.next(); // consume `$`
            explicit_pos =
                Some(
                    digit_buf
                        .parse::<usize>()
                        .map_err(|_| EvalError::TypeMismatch {
                            detail: format!("format(): invalid arg position {digit_buf:?}"),
                        })?,
                );
            digit_buf.clear();
        }
        // Specifier character.
        let spec = match chars.next() {
            Some(c) => c,
            None => {
                return Err(EvalError::TypeMismatch {
                    detail: "format(): trailing `%` with no specifier".into(),
                });
            }
        };
        // Anything left in digit_buf (no `$`) was actually
        // pre-spec digits we now have to emit verbatim. PG would
        // treat them as width hint; v7.17 doesn't implement
        // width, but we don't want to silently drop the digits.
        // Strategy: ignore width for now and emit just the
        // converted value.
        let _ = digit_buf;
        if spec == '%' {
            out.push('%');
            continue;
        }
        let arg_index = match explicit_pos {
            Some(p) => p.saturating_sub(1),
            None => {
                let i = implicit_cursor;
                implicit_cursor += 1;
                i
            }
        };
        let arg = arg_values.get(arg_index).cloned().unwrap_or(Value::Null);
        match spec {
            's' => match arg {
                Value::Null => {} // PG: NULL renders as empty for %s.
                v => out.push_str(&value_to_format_text(&v)),
            },
            'I' => match arg {
                Value::Null => {
                    return Err(EvalError::TypeMismatch {
                        detail: "format(): NULL is not a valid identifier (%I)".into(),
                    });
                }
                v => {
                    let s = value_to_format_text(&v);
                    out.push('"');
                    for ch in s.chars() {
                        if ch == '"' {
                            out.push('"');
                            out.push('"');
                        } else {
                            out.push(ch);
                        }
                    }
                    out.push('"');
                }
            },
            'L' => match arg {
                Value::Null => out.push_str("NULL"),
                v => {
                    let s = value_to_format_text(&v);
                    out.push('\'');
                    for ch in s.chars() {
                        if ch == '\'' {
                            out.push('\'');
                            out.push('\'');
                        } else {
                            out.push(ch);
                        }
                    }
                    out.push('\'');
                }
            },
            other => {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "format(): unknown specifier '%{other}' \
                         (v7.17 supports %s %I %L %%)"
                    ),
                });
            }
        }
    }
    Ok(Value::text(out))
}

/// Helper: render a Value as text for format()'s %s / %I / %L
/// payload. Reuses the regular text-coercion table.
/// v7.17.0 Phase 3.P0-31 — map a `Value` to the canonical PG
/// type-name string returned by `pg_typeof`. Lowercase, matches
/// what real PostgreSQL emits (NOT SPG's UPPERCASE Display shape).
pub(super) fn pg_typeof_name(v: &Value) -> &'static str {
    match v {
        Value::SmallInt(_) => "smallint",
        Value::Int(_) => "integer",
        Value::BigInt(_) => "bigint",
        Value::Float(_) => "double precision",
        Value::Text(_) => "text",
        Value::Bool(_) => "boolean",
        Value::Vector(_) | Value::Sq8Vector(_) | Value::HalfVector(_) => "vector",
        Value::Numeric { .. } => "numeric",
        Value::Date(_) => "date",
        Value::Timestamp(_) => "timestamp without time zone",
        Value::Interval { .. } => "interval",
        Value::Json(_) => {
            // SPG carries JSON and JSONB in the same Value::Json
            // variant; without a column ty hint we cannot tell
            // them apart at value level. Return "json" as the
            // conservative answer (PG's pg_typeof on a literal
            // `'{}'::json` returns "json"; the jsonb case is
            // covered when an explicit ::jsonb cast lands as
            // Value::Json too — see below override at call site).
            //
            // The eval-arm above for pg_typeof handles the
            // disambiguation via Expr-shape probing.
            "json"
        }
        Value::Bytes(_) => "bytea",
        Value::TextArray(_) => "text[]",
        Value::IntArray(_) => "integer[]",
        Value::BigIntArray(_) => "bigint[]",
        Value::TsVector(_) => "tsvector",
        Value::TsQuery(_) => "tsquery",
        Value::Uuid(_) => "uuid",
        Value::Null => "unknown",
        // Value is #[non_exhaustive]; future variants land here
        // until the table is updated.
        _ => "unknown",
    }
}

pub(super) fn value_to_format_text(v: &Value) -> String {
    match v {
        Value::Text(s) | Value::Json(s) => s.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(x) => format!("{x}"),
        Value::Bool(b) => {
            if *b {
                "t".into()
            } else {
                "f".into()
            }
        }
        Value::Null => String::new(),
        other => format!("{other:?}"),
    }
}

pub(super) fn to_char(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    use core::fmt::Write as _;
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("to_char() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(&args[0], Value::Null) || matches!(&args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Text(fmt) = &args[1] else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "to_char() needs a text format, got {:?}",
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
                    "to_char() needs DATE or TIMESTAMP, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let (y, mo, d) = civil_from_days(days);
    let secs = day_micros / 1_000_000;
    let frac = day_micros % 1_000_000;
    // div_euclid keeps every value non-negative — the casts below are
    // sign-safe by construction. `secs ∈ [0, 86400)`, `frac ∈ [0,
    // 1_000_000)`, so all three quantities fit in u32.
    let hh24 = u32::try_from(secs / 3600).unwrap_or(0);
    let mi = u32::try_from((secs / 60) % 60).unwrap_or(0);
    let ss = u32::try_from(secs % 60).unwrap_or(0);
    let hh12 = match hh24 % 12 {
        0 => 12,
        x => x,
    };
    let ampm = if hh24 < 12 { "AM" } else { "PM" };
    let ms = u32::try_from(frac / 1_000).unwrap_or(0); // millisecond
    let us = u32::try_from(frac).unwrap_or(0); // microsecond (0..1_000_000)

    let mut out = String::with_capacity(fmt.len() + 8);
    let bytes = fmt.as_bytes();
    let mut i = 0;
    // write! against a String never fails — discard the Result.
    while i < bytes.len() {
        // Try the longest prefixes first so "YYYY" wins over "YY".
        let rest = &bytes[i..];
        if rest.starts_with(b"YYYY") {
            let _ = write!(out, "{y:04}");
            i += 4;
        } else if rest.starts_with(b"YY") {
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let yy = (y.rem_euclid(100)) as u32;
            let _ = write!(out, "{yy:02}");
            i += 2;
        } else if rest.starts_with(b"Month") {
            out.push_str(MONTH_FULL[(mo - 1) as usize]);
            i += 5;
        } else if rest.starts_with(b"Mon") {
            out.push_str(MONTH_ABBR[(mo - 1) as usize]);
            i += 3;
        } else if rest.starts_with(b"MM") {
            let _ = write!(out, "{mo:02}");
            i += 2;
        } else if rest.starts_with(b"DD") {
            let _ = write!(out, "{d:02}");
            i += 2;
        } else if rest.starts_with(b"HH24") {
            let _ = write!(out, "{hh24:02}");
            i += 4;
        } else if rest.starts_with(b"HH12") {
            let _ = write!(out, "{hh12:02}");
            i += 4;
        } else if rest.starts_with(b"MI") {
            let _ = write!(out, "{mi:02}");
            i += 2;
        } else if rest.starts_with(b"SS") {
            let _ = write!(out, "{ss:02}");
            i += 2;
        } else if rest.starts_with(b"MS") {
            let _ = write!(out, "{ms:03}");
            i += 2;
        } else if rest.starts_with(b"US") {
            let _ = write!(out, "{us:06}");
            i += 2;
        } else if rest.starts_with(b"AM") || rest.starts_with(b"PM") {
            out.push_str(ampm);
            i += 2;
        } else {
            // Pass any non-placeholder byte through verbatim.
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Ok(Value::text(out))
}
