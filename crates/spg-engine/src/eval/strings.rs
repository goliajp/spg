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

use super::{EvalError, MONTH_ABBR, MONTH_FULL, civil_from_days, days_from_civil};

/// Full weekday names, indexed Monday = 0 .. Sunday = 6 (matching
/// `(days + 3).rem_euclid(7)` since 1970-01-01 was a Thursday).
const DAY_FULL: [&str; 7] = [
    "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday",
];
/// Abbreviated weekday names, same Monday = 0 indexing.
const DAY_ABBR: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
/// Roman numeral months (PG `RM` / `rm`), index month-1.
const MONTH_ROMAN: [&str; 12] = [
    "I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X", "XI", "XII",
];

/// Apply a PG `to_char` case template to a mixed-case canonical name:
/// `"Tuesday"` → itself for `Xx`, uppercase for `XX`, lowercase for
/// `xx`. `blank_to` (when `Some`) right-pads with spaces to the fixed
/// PG field width (9 for full day/month names, 4 for roman months).
fn cased_name(canonical: &str, upper: bool, lower: bool, blank_to: Option<usize>) -> String {
    let mut s = if upper {
        canonical.to_ascii_uppercase()
    } else if lower {
        canonical.to_ascii_lowercase()
    } else {
        canonical.to_string()
    };
    if let Some(width) = blank_to {
        while s.len() < width {
            s.push(' ');
        }
    }
    s
}

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
/// PostgreSQL keywords whose `pg_get_keywords().catcode <> 'U'`
/// (reserved / type-func-name / col-name categories). `quote_ident`
/// / `quote_identifier` quote any of these even when the character
/// class is otherwise identifier-safe. Sorted ascending for
/// `binary_search`. Captured live from PG 18.4.
const PG_QUOTE_KEYWORDS: &[&str] = &[
    "all", "analyse", "analyze", "and", "any", "array", "as", "asc",
    "asymmetric", "authorization", "between", "bigint", "binary", "bit", "boolean", "both",
    "case", "cast", "char", "character", "check", "coalesce", "collate", "collation",
    "column", "concurrently", "constraint", "create", "cross", "current_catalog", "current_date", "current_role",
    "current_schema", "current_time", "current_timestamp", "current_user", "dec", "decimal", "default", "deferrable",
    "desc", "distinct", "do", "else", "end", "except", "exists", "extract",
    "false", "fetch", "float", "for", "foreign", "freeze", "from", "full",
    "grant", "greatest", "group", "grouping", "having", "ilike", "in", "initially",
    "inner", "inout", "int", "integer", "intersect", "interval", "into", "is",
    "isnull", "join", "json", "json_array", "json_arrayagg", "json_exists", "json_object", "json_objectagg",
    "json_query", "json_scalar", "json_serialize", "json_table", "json_value", "lateral", "leading", "least",
    "left", "like", "limit", "localtime", "localtimestamp", "merge_action", "national", "natural",
    "nchar", "none", "normalize", "not", "notnull", "null", "nullif", "numeric",
    "offset", "on", "only", "or", "order", "out", "outer", "overlaps",
    "overlay", "placing", "position", "precision", "primary", "real", "references", "returning",
    "right", "row", "select", "session_user", "setof", "similar", "smallint", "some",
    "substring", "symmetric", "system_user", "table", "tablesample", "then", "time", "timestamp",
    "to", "trailing", "treat", "trim", "true", "union", "unique", "user",
    "using", "values", "varchar", "variadic", "verbose", "when", "where", "window",
    "with", "xmlattributes", "xmlconcat", "xmlelement", "xmlexists", "xmlforest", "xmlnamespaces", "xmlparse",
    "xmlpi", "xmlroot", "xmlserialize", "xmltable",
];

/// True when `s` must be double-quoted to survive as a SQL
/// identifier, mirroring PG's `quote_identifier`: an unquoted
/// identifier must be non-empty, start with `[a-z_]`, contain only
/// `[a-z0-9_]`, and not collide with a non-unreserved keyword.
fn ident_needs_quotes(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return true; // empty → always quote ("")
    };
    if !(first.is_ascii_lowercase() || first == '_') {
        return true;
    }
    if s.chars()
        .any(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
    {
        return true;
    }
    // All-lowercase identifier-safe text: quote iff it is a keyword
    // PG would otherwise reinterpret.
    PG_QUOTE_KEYWORDS.binary_search(&s).is_ok()
}

/// PG `quote_ident` / `quote_identifier`: return `s` unchanged when
/// it is a safe unquoted identifier, otherwise wrap it in double
/// quotes with embedded `"` doubled.
pub(super) fn pg_quote_ident(s: &str) -> String {
    if !ident_needs_quotes(s) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

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
                    out.push_str(&pg_quote_ident(&s));
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

/// Coerce a numeric operand to f64 for the `to_char(number, fmt)`
/// form. Returns `None` for non-numeric values so the date/timestamp
/// path takes over.
fn numeric_value_for_to_char(v: &Value) -> Option<f64> {
    match v {
        Value::SmallInt(n) => Some(f64::from(*n)),
        Value::Int(n) => Some(f64::from(*n)),
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Some(*n as f64),
        Value::Float(x) => Some(*x),
        #[allow(clippy::cast_precision_loss)]
        Value::Numeric { scaled, scale } => {
            let mut div = 1.0_f64;
            for _ in 0..*scale {
                div *= 10.0;
            }
            Some((*scaled as f64) / div)
        }
        _ => None,
    }
}

/// A PG-faithful subset of the numeric `to_char` format. Supported
/// tokens: digit slots `9` (leading-zero-blanked) and `0`
/// (zero-forced); the decimal separators `.` / `D`; the group
/// separators `,` / `G`; the explicit sign `S`; and the `FM`
/// fill-mode prefix that drops padding and trims trailing fraction
/// zeros. Matches PG on sign placement (spaces pad to the far left,
/// the sign sits immediately left of the first digit), leading-zero
/// suppression for values < 1, and `#` field-overflow.
///
/// Unsupported (rendered as literals / ignored, documented as
/// known-limitations): `MI` / `PR` / `SG` alternate signs, `RN`
/// roman numerals, `EEEE` scientific, `V` scale, `TH` / `th`
/// ordinals, currency `L` / `$`, and a trailing (rather than
/// leading) `S`.
/// `to_char(interval, fmt)`. Unlike the timestamp form, interval fields carry
/// their own sign and don't wrap: `HH24` of `interval '25 hours'` is `25`, not
/// `01`. `MM`/`YYYY` come straight from the months component (14 months →
/// `0001-02`); the time part decomposes into `HH24:MI:SS`. Calendar-only codes
/// (day/month names, DOW, week, Julian) are meaningless for an interval and
/// pass through as literals rather than erroring. Known cosmetic divergence:
/// PG renders a negative `DD` field unpadded (`-1`, not `-01`); we zero-pad
/// every numeric field consistently after the sign.
fn to_char_interval(months: i64, days: i64, micros: i128, fmt: &str) -> String {
    use core::fmt::Write as _;
    let yyyy = months / 12;
    let mm = months % 12;
    let hh24 = i64::try_from(micros / 3_600_000_000).unwrap_or(0);
    let mi = i64::try_from((micros / 60_000_000) % 60).unwrap_or(0);
    let ss = i64::try_from((micros / 1_000_000) % 60).unwrap_or(0);
    let ms = i64::try_from((micros / 1_000) % 1_000).unwrap_or(0);
    let us = i64::try_from(micros % 1_000_000).unwrap_or(0);
    let hh12 = match hh24.rem_euclid(12) {
        0 => 12,
        x => x,
    };
    let ampm = if hh24.rem_euclid(24) < 12 { "AM" } else { "PM" };
    // Sign-aware zero pad: PG renders `-2` hours as `-02`.
    let pad = |v: i64, w: usize, fm: bool| -> String {
        if fm {
            alloc::format!("{v}")
        } else if v < 0 {
            alloc::format!("-{:0width$}", -v, width = w)
        } else {
            alloc::format!("{:0width$}", v, width = w)
        }
    };
    let mut out = String::with_capacity(fmt.len() + 8);
    let bytes = fmt.as_bytes();
    let mut i = 0;
    let mut fm = false;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if rest.starts_with(b"FM") {
            fm = true;
            i += 2;
            continue;
        }
        if bytes[i] == b'"' {
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            out.push_str(&fmt[start..i]);
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        let (frag, consumed): (String, usize) = if rest.starts_with(b"YYYY") {
            (pad(yyyy, 4, fm), 4)
        } else if rest.starts_with(b"HH24") {
            (pad(hh24, 2, fm), 4)
        } else if rest.starts_with(b"HH12") {
            (pad(hh12, 2, fm), 4)
        } else if rest.starts_with(b"US") {
            (pad(us, 6, fm), 2)
        } else if rest.starts_with(b"MS") {
            (pad(ms, 3, fm), 2)
        } else if rest.starts_with(b"HH") {
            (pad(hh12, 2, fm), 2)
        } else if rest.starts_with(b"MI") {
            (pad(mi, 2, fm), 2)
        } else if rest.starts_with(b"SS") {
            (pad(ss, 2, fm), 2)
        } else if rest.starts_with(b"DD") {
            (pad(days, 2, fm), 2)
        } else if rest.starts_with(b"MM") {
            (pad(mm, 2, fm), 2)
        } else if rest.starts_with(b"AM") || rest.starts_with(b"PM") {
            (ampm.to_string(), 2)
        } else {
            // Any other byte (punctuation, spaces, calendar-name letters)
            // passes through literally.
            let mut buf = String::new();
            let _ = write!(buf, "{}", bytes[i] as char);
            (buf, 1)
        };
        out.push_str(&frag);
        fm = false;
        i += consumed;
    }
    out
}

/// PG `to_char(n, 'RN')` — Roman numerals. Valid for 1..=3999; anything else
/// (including 0 and negatives) renders as 15 `#`. Without `FM` the result is
/// right-justified in a 15-character field; `FM` trims it.
fn to_char_roman(n: f64, fill_mode: bool) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let v = libm::round(n) as i64;
    if !(1..=3999).contains(&v) {
        return core::iter::repeat('#').take(15).collect();
    }
    const VALS: [(i64, &str); 13] = [
        (1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"),
        (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I"),
    ];
    let mut out = String::new();
    let mut rem = v;
    for (val, sym) in VALS {
        while rem >= val {
            out.push_str(sym);
            rem -= val;
        }
    }
    if fill_mode {
        out
    } else {
        alloc::format!("{out:>15}")
    }
}

fn to_char_numeric(n: f64, fmt: &str) -> String {
    let fill_mode = fmt.len() >= 2 && fmt[..2].eq_ignore_ascii_case("FM");
    let pat = if fill_mode { &fmt[2..] } else { fmt };
    // `RN` / `rn`: Roman numerals (handled before the digit-slot machinery).
    if pat.eq_ignore_ascii_case("RN") {
        return to_char_roman(n, fill_mode);
    }
    // `PR` suffix: PG's accounting-negative notation — a negative value is
    // wrapped in angle brackets with no minus sign (`<1234.50>`), a
    // non-negative one gets a trailing space where the `>` would sit.
    let has_pr = pat.len() >= 2 && pat[pat.len() - 2..].eq_ignore_ascii_case("PR");
    let pat = if has_pr { &pat[..pat.len() - 2] } else { pat };
    let has_sign_tok = pat.chars().any(|c| c == 'S' || c == 's');

    // Split around the decimal separator ('.', 'D', or 'd').
    let dec_pos = pat
        .char_indices()
        .find(|(_, c)| *c == '.' || *c == 'D' || *c == 'd')
        .map(|(i, c)| (i, c.len_utf8()));
    let (int_pat, frac_pat, has_decimal) = match dec_pos {
        Some((i, w)) => (&pat[..i], &pat[i + w..], true),
        None => (pat, "", false),
    };

    let is_slot = |c: char| matches!(c, '9' | '0');
    let is_group = |c: char| matches!(c, ',' | 'G' | 'g');
    let int_slots = int_pat.chars().filter(|c| is_slot(*c)).count();
    let frac_digits = frac_pat.chars().filter(|c| is_slot(*c)).count();
    let has_group = int_pat.chars().any(is_group);
    // The field width reserved for the integer side plus one sign
    // column (PG always keeps a slot for the sign in fixed width).
    let int_field_width = int_pat.chars().filter(|c| is_slot(*c) || is_group(*c)).count() + 1;
    // Right-most integer slot char (units position) and left-most
    // `0` slot position from the right (units = 0).
    let int_slot_chars: alloc::vec::Vec<char> =
        int_pat.chars().filter(|c| is_slot(*c)).collect();
    let units_slot = int_slot_chars.last().copied().unwrap_or('9');
    // Force leading zeros up to (and including) the left-most `0`
    // slot: its distance from the units position sets the minimum
    // integer width. `009` → width 3, `990` → width 1.
    let zero_pad = int_slot_chars
        .iter()
        .position(|c| *c == '0')
        .map_or(0, |i| int_slot_chars.len() - i);

    // Round to the requested scale, split into integer / fraction.
    let pow = 10_i128.pow(frac_digits as u32);
    #[allow(clippy::cast_possible_truncation)]
    let scaled = libm::round(n.abs() * pow as f64) as i128;
    let int_part = scaled / pow;
    let frac_part = scaled % pow;
    let value_is_zero = scaled == 0;
    // A value that rounds to exactly zero carries no sign in PG.
    let neg = n < 0.0 && !value_is_zero;
    let sign_str: &str = if has_pr {
        // PR shows the sign via brackets, applied as a post-pass below.
        ""
    } else if neg {
        "-"
    } else if has_sign_tok {
        "+"
    } else {
        ""
    };

    // --- Field overflow: integer digits exceed the digit slots. ---
    let int_digit_len = if int_part == 0 {
        0
    } else {
        alloc::format!("{int_part}").len()
    };
    if int_digit_len > int_slots {
        let mut core = String::new();
        core.push_str(sign_str);
        for _ in 0..int_slots {
            core.push('#');
        }
        let mut out = if fill_mode {
            core
        } else {
            left_pad_spaces(&core, int_field_width)
        };
        if has_decimal {
            out.push('.');
            for _ in 0..frac_digits {
                out.push('#');
            }
        }
        return out;
    }

    // --- Integer body (significant digits, no leading blanks). ---
    let mut body = if int_part == 0 {
        // Show a "0" unless it is a leading zero being blanked: a
        // '9' units slot with a decimal point (PG shows `.50`), and
        // in fixed width a whole-zero value is likewise blanked.
        let show_zero = if fill_mode {
            value_is_zero || units_slot == '0'
        } else {
            units_slot == '0' || !has_decimal
        };
        if show_zero { "0".to_string() } else { String::new() }
    } else {
        alloc::format!("{int_part}")
    };
    // Force leading zeros up to the left-most '0' slot.
    while !body.is_empty() && body.chars().count() < zero_pad {
        body.insert(0, '0');
    }
    if has_group && body.chars().count() > 3 {
        body = group_thousands(&body);
    }

    // --- Assemble sign + body, then pad / trim per mode. ---
    let mut out = if fill_mode {
        alloc::format!("{sign_str}{body}")
    } else {
        left_pad_spaces(&alloc::format!("{sign_str}{body}"), int_field_width)
    };

    if has_decimal {
        let mut fs = alloc::format!("{frac_part:0width$}", width = frac_digits);
        if fill_mode {
            // FM trims trailing zeros down to the `0` slots but keeps
            // the decimal point (PG renders `5.` for to_char(5,'FM9.99')).
            let keep = frac_pat.chars().filter(|c| *c == '0').count();
            while fs.chars().count() > keep && fs.ends_with('0') {
                fs.pop();
            }
            out.push('.');
            out.push_str(&fs);
        } else if frac_digits > 0 {
            out.push('.');
            out.push_str(&fs);
        }
    }
    if has_pr {
        if neg {
            // Consume the reserved sign column with `<` and append `>`.
            let trimmed = out.trim_start();
            let lead = out.chars().count() - trimmed.chars().count();
            out = alloc::format!(
                "{}<{trimmed}>",
                " ".repeat(lead.saturating_sub(1))
            );
        } else if !fill_mode {
            out.push(' ');
        }
    }
    out
}

/// Left-pad `s` with spaces so its char count reaches `width`
/// (right-alignment); returns `s` unchanged when already wide enough.
fn left_pad_spaces(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        return s.to_string();
    }
    let mut out = String::with_capacity(width);
    for _ in 0..width - len {
        out.push(' ');
    }
    out.push_str(s);
    out
}

/// Insert commas every three digits from the right of a pure-digit
/// integer string.
fn group_thousands(int_str: &str) -> String {
    let bytes: alloc::vec::Vec<char> = int_str.chars().collect();
    let mut out = String::new();
    let len = bytes.len();
    for (idx, c) in bytes.iter().enumerate() {
        if idx > 0 && (len - idx) % 3 == 0 {
            out.push(',');
        }
        out.push(*c);
    }
    out
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
    // Interval form: to_char(interval, 'HH24:MI:SS' / 'DD' / 'YYYY-MM' / …).
    if let Value::Interval { months, days, micros } = &args[0] {
        return Ok(Value::text(to_char_interval(
            i64::from(*months),
            i64::from(*days),
            i128::from(*micros),
            fmt,
        )));
    }
    // Numeric form: to_char(number, 'FM9999.00' / '999,990.9' / …).
    if let Some(n) = numeric_value_for_to_char(&args[0]) {
        return Ok(Value::text(to_char_numeric(n, fmt)));
    }
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
                    "to_char() needs a number, DATE or TIMESTAMP, got {:?}",
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

    // Calendar-derived fields (PG doc semantics). 1970-01-01 was a
    // Thursday (index 3 in a Monday=0 week).
    let dow_mon0 = usize::try_from((i64::from(days) + 3).rem_euclid(7)).unwrap_or(0);
    let day_of_year = i64::from(days - days_from_civil(y, 1, 1)) + 1; // DDD
    let (iso_week, iso_year) = super::datetime::iso_week_and_year(days, y); // IW / IYYY
    let quarter = i64::from((mo - 1) / 3) + 1; // Q
    let week_of_year = (day_of_year - 1) / 7 + 1; // WW
    let week_of_month = i64::from((d - 1) / 7) + 1; // W
    let dow_sun1 = (i64::from(days) + 4).rem_euclid(7) + 1; // D: Sunday = 1
    let iso_dow = i64::from(dow_mon0 as i64) + 1; // ID: Monday = 1
    let julian = i64::from(days) + 2_440_588; // J
    let century: i64 = if y > 0 {
        i64::from((y - 1) / 100) + 1
    } else {
        i64::from(y / 100) - 1
    }; // CC

    let mut out = String::with_capacity(fmt.len() + 8);
    let bytes = fmt.as_bytes();
    let mut i = 0;
    // `FM` prefix suppresses the leading zeros / blank padding of the
    // one field it precedes.
    let mut fm = false;
    // write! against a String never fails — discard the Result.
    while i < bytes.len() {
        // Try the longest prefixes first so "YYYY" wins over "YY".
        let rest = &bytes[i..];
        // FM toggles fill-mode for the *next* field only; consume and
        // loop without emitting so it applies to whatever follows.
        if rest.starts_with(b"FM") {
            fm = true;
            i += 2;
            continue;
        }
        // Double-quoted text is a literal — PG strips the quotes and emits the
        // content verbatim (e.g. `HH24"h"MI"m"` → `14h30m`, `YYYY"年"` → `2024年`).
        if bytes[i] == b'"' {
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            out.push_str(&fmt[start..i]);
            if i < bytes.len() {
                i += 1; // consume the closing quote
            }
            continue;
        }
        // Blank-padded name fields (Day / Month / RM): FM strips the
        // trailing blanks; the width is the longest member (9 for
        // day/month names, 4 for roman months).
        let pad = |width: usize| if fm { None } else { Some(width) };
        // Numeric fields honour FM by dropping the zero pad.
        macro_rules! num {
            ($val:expr, $width:literal) => {{
                if fm {
                    let _ = write!(out, "{}", $val);
                } else {
                    let _ = write!(out, "{:0width$}", $val, width = $width);
                }
            }};
        }
        let mut consumed = 2usize;
        if rest.starts_with(b"YYYY") {
            num!(y, 4);
            consumed = 4;
        } else if rest.starts_with(b"IYYY") {
            num!(iso_year, 4);
            consumed = 4;
        } else if rest.starts_with(b"HH24") {
            num!(hh24, 2);
            consumed = 4;
        } else if rest.starts_with(b"HH12") {
            num!(hh12, 2);
            consumed = 4;
        } else if rest.starts_with(b"IYY") {
            let _ = write!(out, "{:03}", iso_year.rem_euclid(1000));
            consumed = 3;
        } else if rest.starts_with(b"YYY") {
            let _ = write!(out, "{:03}", i64::from(y).rem_euclid(1000));
            consumed = 3;
        } else if rest.starts_with(b"DDD") {
            num!(day_of_year, 3);
            consumed = 3;
        } else if rest.starts_with(b"Month") {
            out.push_str(&cased_name(MONTH_FULL[(mo - 1) as usize], false, false, pad(9)));
            consumed = 5;
        } else if rest.starts_with(b"MONTH") {
            out.push_str(&cased_name(MONTH_FULL[(mo - 1) as usize], true, false, pad(9)));
            consumed = 5;
        } else if rest.starts_with(b"month") {
            out.push_str(&cased_name(MONTH_FULL[(mo - 1) as usize], false, true, pad(9)));
            consumed = 5;
        } else if rest.starts_with(b"Mon") {
            out.push_str(&cased_name(MONTH_ABBR[(mo - 1) as usize], false, false, None));
            consumed = 3;
        } else if rest.starts_with(b"MON") {
            out.push_str(&cased_name(MONTH_ABBR[(mo - 1) as usize], true, false, None));
            consumed = 3;
        } else if rest.starts_with(b"mon") {
            out.push_str(&cased_name(MONTH_ABBR[(mo - 1) as usize], false, true, None));
            consumed = 3;
        } else if rest.starts_with(b"Day") {
            out.push_str(&cased_name(DAY_FULL[dow_mon0], false, false, pad(9)));
            consumed = 3;
        } else if rest.starts_with(b"DAY") {
            out.push_str(&cased_name(DAY_FULL[dow_mon0], true, false, pad(9)));
            consumed = 3;
        } else if rest.starts_with(b"day") {
            out.push_str(&cased_name(DAY_FULL[dow_mon0], false, true, pad(9)));
            consumed = 3;
        } else if rest.starts_with(b"Dy") {
            out.push_str(&cased_name(DAY_ABBR[dow_mon0], false, false, None));
        } else if rest.starts_with(b"DY") {
            out.push_str(&cased_name(DAY_ABBR[dow_mon0], true, false, None));
        } else if rest.starts_with(b"dy") {
            out.push_str(&cased_name(DAY_ABBR[dow_mon0], false, true, None));
        } else if rest.starts_with(b"YY") {
            let _ = write!(out, "{:02}", i64::from(y).rem_euclid(100));
        } else if rest.starts_with(b"IW") {
            num!(iso_week, 2);
        } else if rest.starts_with(b"IY") {
            let _ = write!(out, "{:02}", iso_year.rem_euclid(100));
        } else if rest.starts_with(b"ID") {
            let _ = write!(out, "{iso_dow}");
        } else if rest.starts_with(b"MM") {
            num!(mo, 2);
        } else if rest.starts_with(b"DD") {
            num!(d, 2);
        } else if rest.starts_with(b"MI") {
            num!(mi, 2);
        } else if rest.starts_with(b"SS") {
            num!(ss, 2);
        } else if rest.starts_with(b"MS") {
            let _ = write!(out, "{ms:03}");
        } else if rest.starts_with(b"US") {
            let _ = write!(out, "{us:06}");
        } else if rest.starts_with(b"WW") {
            num!(week_of_year, 2);
        } else if rest.starts_with(b"CC") {
            num!(century, 2);
        } else if rest.starts_with(b"RM") {
            out.push_str(&cased_name(MONTH_ROMAN[(mo - 1) as usize], true, false, pad(4)));
        } else if rest.starts_with(b"rm") {
            out.push_str(&cased_name(MONTH_ROMAN[(mo - 1) as usize], false, true, pad(4)));
        } else if rest.starts_with(b"HH") {
            num!(hh12, 2);
        } else if rest.starts_with(b"AM") || rest.starts_with(b"PM") {
            out.push_str(ampm);
        } else if rest.starts_with(b"am") || rest.starts_with(b"pm") {
            out.push_str(if hh24 < 12 { "am" } else { "pm" });
        } else if rest.starts_with(b"Y") || rest.starts_with(b"I") {
            // Single-digit year / ISO-year (last digit).
            let base = if rest[0] == b'I' { iso_year } else { i64::from(y) };
            let _ = write!(out, "{}", base.rem_euclid(10));
            consumed = 1;
        } else if rest.starts_with(b"Q") {
            let _ = write!(out, "{quarter}");
            consumed = 1;
        } else if rest.starts_with(b"W") {
            let _ = write!(out, "{week_of_month}");
            consumed = 1;
        } else if rest.starts_with(b"D") {
            let _ = write!(out, "{dow_sun1}");
            consumed = 1;
        } else if rest.starts_with(b"J") {
            let _ = write!(out, "{julian}");
            consumed = 1;
        } else {
            // Pass any non-placeholder byte through verbatim.
            out.push(bytes[i] as char);
            consumed = 1;
            i += consumed;
            // A literal byte doesn't consume the pending FM.
            continue;
        }
        fm = false;
        i += consumed;
    }
    Ok(Value::text(out))
}
