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
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
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
    // v7.39 (read01 oracle_compat.c) — bytea trim variants work on BYTES
    // (byteatrim/ltrim/rtrim): trim any byte present in the set argument,
    // returning bytea — the text path would eat the \x prefix.
    if let [Value::Bytes(b), Value::Bytes(set)] = args {
        let setb: alloc::collections::BTreeSet<u8> = set.iter().copied().collect();
        let mut lo = 0usize;
        let mut hi = b.len();
        if matches!(side, TrimSide::Left | TrimSide::Both) {
            while lo < hi && setb.contains(&b[lo]) {
                lo += 1;
            }
        }
        if matches!(side, TrimSide::Right | TrimSide::Both) {
            while hi > lo && setb.contains(&b[hi - 1]) {
                hi -= 1;
            }
        }
        return Ok(Value::Bytes(alloc::borrow::Cow::Owned(b[lo..hi].to_vec())));
    }
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
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "between",
    "bigint",
    "binary",
    "bit",
    "boolean",
    "both",
    "case",
    "cast",
    "char",
    "character",
    "check",
    "coalesce",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "dec",
    "decimal",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "exists",
    "extract",
    "false",
    "fetch",
    "float",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "greatest",
    "group",
    "grouping",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "inout",
    "int",
    "integer",
    "intersect",
    "interval",
    "into",
    "is",
    "isnull",
    "join",
    "json",
    "json_array",
    "json_arrayagg",
    "json_exists",
    "json_object",
    "json_objectagg",
    "json_query",
    "json_scalar",
    "json_serialize",
    "json_table",
    "json_value",
    "lateral",
    "leading",
    "least",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "merge_action",
    "national",
    "natural",
    "nchar",
    "none",
    "normalize",
    "not",
    "notnull",
    "null",
    "nullif",
    "numeric",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "out",
    "outer",
    "overlaps",
    "overlay",
    "placing",
    "position",
    "precision",
    "primary",
    "real",
    "references",
    "returning",
    "right",
    "row",
    "select",
    "session_user",
    "setof",
    "similar",
    "smallint",
    "some",
    "substring",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "time",
    "timestamp",
    "to",
    "trailing",
    "treat",
    "trim",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "values",
    "varchar",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
    "xmlattributes",
    "xmlconcat",
    "xmlelement",
    "xmlexists",
    "xmlforest",
    "xmlnamespaces",
    "xmlparse",
    "xmlpi",
    "xmlroot",
    "xmlserialize",
    "xmltable",
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

/// PG `quote_literal(text)`: wrap `s` in single quotes, doubling any
/// embedded single quote. When the string contains a backslash, PG
/// emits the `E'…'` escape-string form with backslashes doubled, so
/// the result stays a valid literal regardless of the reader's
/// `standard_conforming_strings` setting — e.g. `quote_literal('c:\p')`
/// → `E'c:\\p'`. This is the shared body for both `quote_literal` and
/// the non-null branch of `quote_nullable`.
pub(super) fn pg_quote_literal(s: &str) -> String {
    let has_backslash = s.contains('\\');
    let mut out = String::with_capacity(s.len() + 4);
    if has_backslash {
        out.push('E');
    }
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

pub(super) fn format_string(
    args: &[Value<'_>],
    style: &super::format::RenderStyle,
) -> Result<Value<'static>, EvalError> {
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
        // PG conversion spec: `% [n$] [-] [width] type`. The pre-`$` digits are
        // the arg position; otherwise they are the field width.
        let mut width_digits = String::new();
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
        } else {
            width_digits = digit_buf.clone();
        }
        // `-` flag (left-justify) then width — but only when the width wasn't
        // already captured as the pre-`$` digits above. The width may be a
        // literal number or `*`, which pulls it from the next argument (PG:
        // `format('%*s', 5, 'x')` right-pads 'x' to width 5).
        let mut left_justify = false;
        let mut width_from_arg = false;
        if width_digits.is_empty() {
            if matches!(chars.peek(), Some(&'-')) {
                chars.next();
                left_justify = true;
            }
            if matches!(chars.peek(), Some(&'*')) {
                chars.next();
                width_from_arg = true;
            } else {
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        width_digits.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
        }
        let width: usize = if width_from_arg {
            // The `*` consumes one implicit argument as the width. PG: a
            // negative width means left-justify with the absolute width.
            let w_arg = arg_values
                .get(implicit_cursor)
                .cloned()
                .unwrap_or(Value::Null);
            implicit_cursor += 1;
            let w = match w_arg {
                Value::SmallInt(n) => i64::from(n),
                Value::Int(n) => i64::from(n),
                Value::BigInt(n) => n,
                _ => 0,
            };
            if w < 0 {
                left_justify = true;
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let uw = w.unsigned_abs() as usize;
            uw
        } else {
            width_digits.parse().unwrap_or(0)
        };
        // Specifier character.
        let spec = match chars.next() {
            Some(c) => c,
            None => {
                return Err(EvalError::TypeMismatch {
                    detail: "format(): trailing `%` with no specifier".into(),
                });
            }
        };
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
        // Build the converted text for this conversion, then apply the field
        // width (minimum width; PG never truncates, pads with spaces).
        let converted: String = match spec {
            's' => match arg {
                Value::Null => String::new(), // PG: NULL renders as empty for %s.
                v => value_to_format_text_styled(&v, style),
            },
            'I' => match arg {
                Value::Null => {
                    return Err(EvalError::TypeMismatch {
                        detail: "format(): NULL is not a valid identifier (%I)".into(),
                    });
                }
                v => pg_quote_ident(&value_to_format_text_styled(&v, style)),
            },
            'L' => match arg {
                Value::Null => "NULL".into(),
                v => {
                    let s = value_to_format_text_styled(&v, style);
                    let mut q = String::with_capacity(s.len() + 2);
                    q.push('\'');
                    for ch in s.chars() {
                        if ch == '\'' {
                            q.push('\'');
                        }
                        q.push(ch);
                    }
                    q.push('\'');
                    q
                }
            },
            other => {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "format(): unknown specifier '%{other}' \
                         (supports %s %I %L %%)"
                    ),
                });
            }
        };
        let vis_len = converted.chars().count();
        if vis_len < width {
            let pad = " ".repeat(width - vis_len);
            if left_justify {
                out.push_str(&converted);
                out.push_str(&pad);
            } else {
                out.push_str(&pad);
                out.push_str(&converted);
            }
        } else {
            out.push_str(&converted);
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
        Value::Real(_) => "real",
        Value::Text(_) => "text",
        Value::Bool(_) => "boolean",
        Value::Vector(_) | Value::Sq8Vector(_) | Value::HalfVector(_) => "vector",
        Value::Numeric { .. } | Value::NumericBig(_) => "numeric",
        Value::Date(_) => "date",
        Value::Time(_) => "time without time zone",
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
        // v7.38 (read01) — the constructor now unifies numeric-ladder element
        // types (`ARRAY[1, 2.5]` → numeric[], `ARRAY[1, 2.5::float8]` →
        // double precision[]); report them instead of falling to "unknown".
        Value::SmallIntArray(_) => "smallint[]",
        Value::NumericArray(_) => "numeric[]",
        Value::FloatArray(_) => "double precision[]",
        Value::TsVector(_) => "tsvector",
        Value::TsQuery(_) => "tsquery",
        Value::Uuid(_) => "uuid",
        // SPG carries both `bit` and `bit varying` in one BitString
        // variant (no fixed-vs-varying tag), so it reports the varying
        // spelling — the same as its data_type() — rather than "unknown".
        // A `bit` literal reads as "bit varying" here vs PG's "bit"; that
        // needs a bit-vs-varbit value tag SPG doesn't yet keep.
        Value::BitString { .. } => "bit varying",
        // v7.38 (read01) — the rest of SPG's scalar value types. These all
        // reported "unknown" before, which drivers and ORMs read as "no type".
        Value::Money(_) => "money",
        Value::Inet { .. } => "inet",
        Value::Cidr { .. } => "cidr",
        Value::Macaddr(_) => "macaddr",
        Value::Macaddr8(_) => "macaddr8",
        Value::PgLsn(_) => "pg_lsn",
        Value::Xml(_) => "xml",
        Value::Hstore(_) => "hstore",
        Value::BpChar(_) => "character",
        // An anonymous `row(...)` / whole-row reference is PG's `record`.
        Value::Composite(_) => "record",
        Value::Point(_) => "point",
        Value::Lseg(..) => "lseg",
        Value::Path { .. } => "path",
        Value::PgBox(..) => "box",
        Value::Polygon(_) => "polygon",
        Value::Line { .. } => "line",
        Value::Circle { .. } => "circle",
        Value::Range { kind, .. } => match kind {
            spg_storage::RangeKind::Int4 => "int4range",
            spg_storage::RangeKind::Int8 => "int8range",
            spg_storage::RangeKind::Num => "numrange",
            spg_storage::RangeKind::Ts => "tsrange",
            spg_storage::RangeKind::TsTz => "tstzrange",
            spg_storage::RangeKind::Date => "daterange",
        },
        Value::BoolArray(_) => "boolean[]",
        Value::DateArray(_) => "date[]",
        Value::TimestampArray(_) => "timestamp without time zone[]",
        Value::TimestamptzArray(_) => "timestamp with time zone[]",
        Value::IntervalArray(_) => "interval[]",
        Value::UuidArray(_) => "uuid[]",
        Value::JsonArray(_) => "json[]",
        Value::JsonbArray(_) => "jsonb[]",
        Value::BytesArray(_) => "bytea[]",
        Value::VarcharArray(_) => "character varying[]",
        Value::CharArray(_) => "character[]",
        Value::MoneyArray(_) => "money[]",
        Value::Null => "unknown",
        // Value is #[non_exhaustive]; future variants land here
        // until the table is updated.
        _ => "unknown",
    }
}

pub(super) fn value_to_format_text(v: &Value) -> String {
    value_to_format_text_styled(v, &super::format::RenderStyle::default())
}

/// v7.39 (GUC knife 4) — the styled variant: concat / concat_ws /
/// format(%s) textify via PG's out-functions, which honour DateStyle /
/// IntervalStyle / extra_float_digits.
pub(super) fn value_to_format_text_styled(v: &Value, style: &super::format::RenderStyle) -> String {
    match v {
        Value::Text(s) | Value::Json(s) => s.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(x) => super::format::format_float_styled(*x, style),
        // PG renders numeric in concat/format/text-coercion as its exact
        // decimal (`x || 2.5::numeric` → `x2.5`), not a debug dump.
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => super::format::format_numeric_kind(*kind, *scaled, *scale),
        Value::Bool(b) => {
            if *b {
                "t".into()
            } else {
                "f".into()
            }
        }
        Value::Null => String::new(),
        // Every other type (Date / Timestamp / Interval / arrays / Bytea /
        // UUID / Time / Money / Range / Hstore / 2D arrays / …) renders via
        // the canonical value→text renderer — the same PG-faithful form SELECT
        // and the wire layer emit — rather than leaking a Rust debug dump.
        other => super::values::value_to_text_styled(other, style),
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
        Value::Numeric { scaled, scale, .. } => {
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
        } else if rest.starts_with(b"YYY") {
            // v — trailing-N-digit year forms (PG: interval '1 year' →
            // YYY '001', YY '01', Y '1'; YY of 123 years → '23').
            (pad(yyyy % 1000, 3, fm), 3)
        } else if rest.starts_with(b"YY") {
            (pad(yyyy % 100, 2, fm), 2)
        } else if rest.starts_with(b"Y") {
            (pad(yyyy % 10, 1, fm), 1)
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
        } else if rest.starts_with(b"SSSS") {
            // v7.37 — seconds of the time-of-day part; must precede `SS`.
            (alloc::format!("{}", hh24 * 3600 + mi * 60 + ss), 4)
        } else if rest.starts_with(b"FF") && rest.get(2).is_some_and(u8::is_ascii_digit) {
            // v7.37 — `FF1`..`FF6`: first N digits of the fractional second.
            let n = usize::from(rest[2] - b'0');
            let frac = alloc::format!("{us:06}");
            (frac[..n.min(6)].to_string(), 3)
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
/// Format `x` with exactly `d` fractional digits (round-half-away-from-zero),
/// no sign. `d == 0` yields no decimal point.
fn format_fixed_abs(x: f64, d: usize) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let pow = libm::pow(10.0, d as f64);
    #[allow(clippy::cast_possible_truncation)]
    let scaled = libm::round(x.abs() * pow) as i128;
    if d == 0 {
        return alloc::format!("{scaled}");
    }
    let unit = 10_i128.pow(d as u32);
    let ip = scaled / unit;
    let fp = (scaled % unit).abs();
    alloc::format!("{ip}.{fp:0width$}", width = d)
}

/// PG `V` scale: multiply by 10^(digit count after `V`) and render as an
/// integer (the `V` drops the decimal point). Field width = all digit slots
/// (before + after V) plus a sign column; non-FM left-pads with blanks.
fn to_char_v_scale(n: f64, before: &str, after: &str, fill_mode: bool) -> String {
    let count_slots = |s: &str| s.chars().filter(|c| matches!(c, '9' | '0')).count();
    let vdigits = count_slots(after);
    let total_slots = count_slots(before) + vdigits;
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let scaled = libm::round(n.abs() * libm::pow(10.0, vdigits as f64)) as i128;
    let neg = n < 0.0 && scaled != 0;
    let core = if neg {
        alloc::format!("-{scaled}")
    } else {
        alloc::format!("{scaled}")
    };
    if fill_mode {
        core
    } else {
        left_pad_spaces(&core, total_slots + 1)
    }
}

/// PG `EEEE` scientific notation. `mant_fmt` is the format preceding `EEEE`
/// (e.g. `9.9`); its post-decimal digit count sets the mantissa precision.
/// Mantissa is normalised to one leading digit; rounding is not
/// re-normalised (PG: `9.99` with `9.9EEEE` → `10.0e+00`). Exponent is a
/// signed two-digit field. Non-FM keeps a leading blank for the sign.
fn to_char_scientific(n: f64, mant_fmt: &str, fill_mode: bool) -> String {
    let neg = n < 0.0 && n != 0.0;
    let a = n.abs();
    let exp: i32 = if a == 0.0 {
        0
    } else {
        #[allow(clippy::cast_possible_truncation)]
        {
            libm::floor(libm::log10(a)) as i32
        }
    };
    let frac_digits = mant_fmt.find(['.', 'D', 'd']).map_or(0, |dot| {
        mant_fmt[dot + 1..]
            .chars()
            .filter(|c| matches!(c, '9' | '0'))
            .count()
    });
    let mantissa = if a == 0.0 {
        0.0
    } else {
        a / libm::pow(10.0, f64::from(exp))
    };
    let mant_str = format_fixed_abs(mantissa, frac_digits);
    let sign = if neg {
        "-"
    } else if fill_mode {
        ""
    } else {
        " "
    };
    let esign = if exp < 0 { '-' } else { '+' };
    alloc::format!("{sign}{mant_str}e{esign}{:02}", exp.abs())
}

fn to_char_roman(n: f64, fill_mode: bool) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let v = libm::round(n) as i64;
    if !(1..=3999).contains(&v) {
        return core::iter::repeat_n('#', 15).collect();
    }
    const VALS: [(i64, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
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

/// v7.38 (read01, T23) — the numeric `EEEE` scientific marker has two PG
/// placement rules, both rejected at parse time (before any output):
///   * combining `EEEE` with a sign/fill/scale/roman flag
///     (`FM`,`S`,`MI`,`PL`,`SG`,`PR`,`RN`,`V`,`B`) → "incompatible with
///     other formats";
///   * any further *format token* after `EEEE` (a digit `9`/`0`, a
///     decimal/group `.`/`,`/`D`/`G`, a currency `L`, a scale `V`, a sign
///     flag, or a second `EEEE`) → "EEEE must be the last pattern used".
/// Plain literals after it — spaces, `$`, or `"…"`-quoted text — are fine.
/// Quoted spans and `\`-escaped characters are literals, so we scan a
/// de-quoted copy of the mask; the incompatible check wins when a flag
/// precedes `EEEE` and a token also follows it (PG's left-to-right pass).
fn check_eeee_format(fmt: &str) -> Result<(), EvalError> {
    // Build the significant (non-literal) characters, uppercased, dropping
    // `"…"` quoted spans and `\`-escaped characters the way PG's lexer does.
    let mut sig = String::with_capacity(fmt.len());
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => {
                for q in chars.by_ref() {
                    if q == '"' {
                        break;
                    }
                }
            }
            _ => sig.push(c.to_ascii_uppercase()),
        }
    }
    let Some(epos) = sig.find("EEEE") else {
        return Ok(());
    };
    let before = &sig[..epos];
    if ["FM", "MI", "PL", "SG", "PR", "RN"]
        .iter()
        .any(|f| before.contains(f))
        || before.contains(['S', 'V', 'B'])
    {
        return Err(EvalError::TypeMismatch {
            detail: String::from(
                "\"EEEE\" is incompatible with other formats: \"EEEE\" may \
                 only be used together with digit and decimal point patterns",
            ),
        });
    }
    let after = &sig[epos + 4..];
    if after.contains([
        '9', '0', '.', ',', 'D', 'G', 'L', 'V', 'S', 'M', 'I', 'P', 'R', 'N', 'B', 'F', 'H', 'E',
    ]) {
        return Err(EvalError::TypeMismatch {
            detail: String::from("\"EEEE\" must be the last pattern used"),
        });
    }
    Ok(())
}

// v7.38 (read01 P6.01) — `exact` carries the input's exact (scaled, scale)
// when it came in as `numeric`, so the digit-slot path can build the value
// from integer arithmetic instead of the lossy f64 `n`. `n` is still used by
// the RN / EEEE / V paths (roman / scientific / V-scale), which are inherently
// float-shaped, and as the fallback when the exact form overflows i128.
fn to_char_numeric(n: f64, exact: Option<(i128, u8)>, fmt: &str) -> String {
    let fill_mode = fmt.len() >= 2 && fmt[..2].eq_ignore_ascii_case("FM");
    let pat = if fill_mode { &fmt[2..] } else { fmt };
    // `RN` / `rn`: Roman numerals (handled before the digit-slot machinery).
    if pat.eq_ignore_ascii_case("RN") {
        return to_char_roman(n, fill_mode);
    }
    // `EEEE`: scientific notation. The mantissa format is whatever precedes
    // `EEEE`; the digit count after its decimal sets the mantissa precision.
    if let Some(epos) = pat.to_ascii_uppercase().find("EEEE") {
        return to_char_scientific(n, &pat[..epos], fill_mode);
    }
    // `V`: scale — multiply by 10^(digits after V) and drop the decimal.
    if let Some(vpos) = pat.find(['V', 'v']) {
        return to_char_v_scale(n, &pat[..vpos], &pat[vpos + 1..], fill_mode);
    }
    // `PR` suffix: PG's accounting-negative notation — a negative value is
    // wrapped in angle brackets with no minus sign (`<1234.50>`), a
    // non-negative one gets a trailing space where the `>` would sit.
    let has_pr = pat.len() >= 2 && pat[pat.len() - 2..].eq_ignore_ascii_case("PR");
    let mut pat = if has_pr { &pat[..pat.len() - 2] } else { pat };
    // v7.37 — `TH` / `th` ordinal suffix and a trailing `%` literal. Both are
    // stripped here and re-applied post-pass (like PR), so the slot machinery
    // never sees them. `TH` (upper) → uppercase suffix; `th` → lowercase.
    let th_suffix: Option<bool> =
        if pat.len() >= 2 && pat[pat.len() - 2..].eq_ignore_ascii_case("TH") {
            let upper = pat.ends_with("TH");
            pat = &pat[..pat.len() - 2];
            Some(upper)
        } else {
            None
        };
    let has_pct = pat.ends_with('%');
    if has_pct {
        pat = &pat[..pat.len() - 1];
    }
    // v7.37 — leading `L` currency locale symbol (C locale → `$`). Stripped
    // here; the rest formats normally and `$` is prepended post-pass.
    // v7.38 (read01) — a leading literal `$` (`FM$9,999.00`) anchors the dollar
    // sign at the front too, matching PG (`$1,234.50`).
    // v7.39 (read01 formatting.c) — the currency symbol comes from the
    // locale: in the C locale PG's L is a single SPACE, while a literal
    // `$` in the picture stays a dollar sign.
    let has_locale_currency = pat.starts_with(['L', 'l']);
    let has_lit_currency = !has_locale_currency && pat.starts_with('$');
    if has_locale_currency || has_lit_currency {
        pat = &pat[1..];
    }
    // v7.39 (read01 formatting.c) — leading `SG` writes the sign itself
    // (always + or -, no blank column), like PG's NUM_SG action.
    let has_leading_sg = pat.len() >= 2 && pat[..2].eq_ignore_ascii_case("SG");
    if has_leading_sg {
        pat = &pat[2..];
    }
    // v7.37 — trailing sign / literal suffixes (stripped here, applied
    // post-pass; the sign moves out of the leading column). Mutually
    // exclusive by construction. `MI` = minus-if-negative, `PL` =
    // plus-if-positive, `SG` = always-signed, a lone trailing `S` = trailing
    // sign, `$` = literal currency.
    let ends_kw = |p: &str, kw: &str| p.len() >= 2 && p[p.len() - 2..].eq_ignore_ascii_case(kw);
    let has_mi = ends_kw(pat, "MI");
    if has_mi {
        pat = &pat[..pat.len() - 2];
    }
    let has_pl = !has_mi && ends_kw(pat, "PL");
    if has_pl {
        pat = &pat[..pat.len() - 2];
    }
    let has_sg = !has_mi && !has_pl && ends_kw(pat, "SG");
    if has_sg {
        pat = &pat[..pat.len() - 2];
    }
    let has_trailing_s = !has_sg
        && (pat.ends_with('S') || pat.ends_with('s'))
        && !pat.ends_with("SS")
        && !pat.ends_with("ss");
    if has_trailing_s {
        pat = &pat[..pat.len() - 1];
    }
    let has_dollar = pat.ends_with('$');
    if has_dollar {
        pat = &pat[..pat.len() - 1];
    }
    let trailing_sign = has_mi || has_pl || has_sg || has_trailing_s;
    let has_sign_tok = !trailing_sign && pat.chars().any(|c| c == 'S' || c == 's');

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
    // column (PG keeps a slot for the sign in fixed width). MI / SG and a
    // trailing `S` position the sign at the end, so PG drops the reserved
    // leading column (`PL` and `$` keep it).
    let sign_col = usize::from(!(has_mi || has_sg || has_trailing_s));
    let int_field_width = int_pat
        .chars()
        .filter(|c| is_slot(*c) || is_group(*c))
        .count()
        + sign_col;
    // Right-most integer slot char (units position) and left-most
    // `0` slot position from the right (units = 0).
    let int_slot_chars: alloc::vec::Vec<char> = int_pat.chars().filter(|c| is_slot(*c)).collect();
    let units_slot = int_slot_chars.last().copied().unwrap_or('9');
    // Force leading zeros up to (and including) the left-most `0`
    // slot: its distance from the units position sets the minimum
    // integer width. `009` → width 3, `990` → width 1.
    let zero_pad = int_slot_chars
        .iter()
        .position(|c| *c == '0')
        .map_or(0, |i| int_slot_chars.len() - i);

    // Round to the requested scale, split into integer / fraction. When the
    // input is exact numeric (P6.01), rescale it to `frac_digits` decimals in
    // pure i128 arithmetic so high-precision values keep every digit; only
    // fall back to the lossy f64 path for floats or on i128 overflow.
    let pow = 10_i128.pow(frac_digits as u32);
    #[allow(clippy::cast_possible_truncation)]
    let f64_scaled = || libm::round(n.abs() * pow as f64) as i128;
    let exact_scaled = exact.and_then(|(in_scaled, in_scale)| {
        let abs = in_scaled.unsigned_abs();
        let fd = u32::try_from(frac_digits).ok()?;
        let insc = u32::from(in_scale);
        let rescaled: u128 = if fd >= insc {
            10_u128
                .checked_pow(fd - insc)
                .and_then(|m| abs.checked_mul(m))?
        } else {
            // Drop excess fraction digits, rounding half away from zero.
            let divisor = 10_u128.checked_pow(insc - fd)?;
            (abs / divisor) + u128::from(abs % divisor >= divisor.div_ceil(2))
        };
        i128::try_from(rescaled).ok()
    });
    let (scaled, neg) = match exact_scaled {
        Some(s) => (s, exact.is_some_and(|(v, _)| v < 0) && s != 0),
        None => {
            let s = f64_scaled();
            (s, n < 0.0 && s != 0)
        }
    };
    let int_part = scaled / pow;
    let frac_part = scaled % pow;
    let value_is_zero = scaled == 0;
    let sign_str: &str = if has_pr || trailing_sign {
        // PR and the trailing sign modes render the sign as a post-pass below.
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
        if show_zero {
            "0".to_string()
        } else {
            String::new()
        }
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
            out = alloc::format!("{}<{trimmed}>", " ".repeat(lead.saturating_sub(1)));
        } else if !fill_mode {
            out.push(' ');
        }
    }
    // v7.37 — trailing sign / currency suffixes (see the strip block above).
    if has_mi {
        if neg {
            out.push('-');
        } else if !fill_mode {
            out.push(' ');
        }
    } else if has_pl || has_sg || has_trailing_s {
        out.push(if neg { '-' } else { '+' });
    }
    if has_dollar {
        out.push('$');
    }
    // v7.37 — ordinal suffix (`TH`/`th`) based on the integer value, then a
    // trailing `%` literal.
    if let Some(upper) = th_suffix {
        let suf = ordinal_suffix(int_part);
        if upper {
            out.push_str(&suf.to_ascii_uppercase());
        } else {
            out.push_str(suf);
        }
    }
    if has_pct {
        out.push('%');
    }
    if has_leading_sg {
        // SG owns the sign COLUMN: replace the single leading blank or
        // minus the slot machinery wrote (later pre-decimal blanks stay,
        // PG: SG9.9 of .5 = "+ .5"), or prepend when there is none.
        let sign = if neg { '-' } else { '+' };
        if out.starts_with(' ') || out.starts_with('-') {
            out.replace_range(..1, &sign.to_string());
        } else {
            out.insert(0, sign);
        }
    }
    if has_locale_currency {
        out.insert(0, ' ');
    } else if has_lit_currency {
        out.insert(0, '$');
    }
    out
}

/// The English ordinal suffix (`st`/`nd`/`rd`/`th`) for `n`, matching PG's
/// `TH` / `th` numeric-format modifier. 11/12/13 are always `th`.
fn ordinal_suffix(n: i128) -> &'static str {
    let n = n.unsigned_abs();
    if (11..=13).contains(&(n % 100)) {
        return "th";
    }
    match n % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    }
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
    if let Value::Interval {
        months,
        days,
        micros,
    } = &args[0]
    {
        return Ok(Value::text(to_char_interval(
            i64::from(*months),
            i64::from(*days),
            i128::from(*micros),
            fmt,
        )));
    }
    // Numeric form: to_char(number, 'FM9999.00' / '999,990.9' / …).
    if let Some(n) = numeric_value_for_to_char(&args[0]) {
        check_eeee_format(fmt)?;
        // v7.38 (read01 P6.01) — thread the exact (scaled, scale) for numeric
        // inputs so to_char never rounds a high-precision value through f64.
        let exact = match &args[0] {
            Value::Numeric { scaled, scale, .. } => Some((*scaled, *scale)),
            _ => None,
        };
        return Ok(Value::text(to_char_numeric(n, exact, fmt)));
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
    let iso_dow = (dow_mon0 as i64) + 1; // ID: Monday = 1
    let julian = i64::from(days) + 2_440_588; // J
    let century: i64 = if y > 0 {
        i64::from((y - 1) / 100) + 1
    } else {
        i64::from(y / 100) - 1
    }; // CC
    // v7.39 (read01 formatting.c) — year fields render the ERA year (PG's
    // ADJUST_YEAR): there is no year 0, so astronomical year <= 0 displays
    // as 1 - y (44 BC is stored as -43 and prints 0044; the era tokens
    // still read the raw sign).
    let disp_y: i64 = if y <= 0 {
        1 - i64::from(y)
    } else {
        i64::from(y)
    };

    let mut out = String::with_capacity(fmt.len() + 8);
    let bytes = fmt.as_bytes();
    let mut i = 0;
    // `FM` prefix suppresses the leading zeros / blank padding of the
    // one field it precedes.
    let mut fm = false;
    // v7.37 — the numeric value emitted by the immediately preceding field,
    // so a following `TH`/`th` renders its ordinal suffix (e.g. `DDth` → 21st).
    let mut last_num: Option<i128> = None;
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
        // Ordinal pending from the previous field (see `last_num`); `take`
        // clears it so a non-`TH` field drops the pending suffix.
        let pending_ord = last_num.take();
        let mut next_num: Option<i128> = None;
        // Numeric fields honour FM by dropping the zero pad.
        macro_rules! num {
            ($val:expr, $width:literal) => {{
                if fm {
                    let _ = write!(out, "{}", $val);
                } else {
                    let _ = write!(out, "{:0width$}", $val, width = $width);
                }
                next_num = Some(i128::from($val));
            }};
        }
        let mut consumed = 2usize;
        if rest.starts_with(b"Y,YYY") {
            // v7.37 — special comma-grouped year token (2026 → "2,026").
            out.push_str(&group_thousands(&alloc::format!("{disp_y}")));
            consumed = 5;
        } else if rest.starts_with(b"YYYY") {
            num!(disp_y, 4);
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
            let _ = write!(out, "{:03}", disp_y.rem_euclid(1000));
            consumed = 3;
        } else if rest.starts_with(b"DDD") {
            num!(day_of_year, 3);
            consumed = 3;
        } else if rest.starts_with(b"Month") {
            out.push_str(&cased_name(
                MONTH_FULL[(mo - 1) as usize],
                false,
                false,
                pad(9),
            ));
            consumed = 5;
        } else if rest.starts_with(b"MONTH") {
            out.push_str(&cased_name(
                MONTH_FULL[(mo - 1) as usize],
                true,
                false,
                pad(9),
            ));
            consumed = 5;
        } else if rest.starts_with(b"month") {
            out.push_str(&cased_name(
                MONTH_FULL[(mo - 1) as usize],
                false,
                true,
                pad(9),
            ));
            consumed = 5;
        } else if rest.starts_with(b"Mon") {
            out.push_str(&cased_name(
                MONTH_ABBR[(mo - 1) as usize],
                false,
                false,
                None,
            ));
            consumed = 3;
        } else if rest.starts_with(b"MON") {
            out.push_str(&cased_name(
                MONTH_ABBR[(mo - 1) as usize],
                true,
                false,
                None,
            ));
            consumed = 3;
        } else if rest.starts_with(b"mon") {
            out.push_str(&cased_name(
                MONTH_ABBR[(mo - 1) as usize],
                false,
                true,
                None,
            ));
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
            let _ = write!(out, "{:02}", disp_y.rem_euclid(100));
        } else if rest.starts_with(b"IW") {
            num!(iso_week, 2);
        } else if rest.starts_with(b"IY") {
            let _ = write!(out, "{:02}", iso_year.rem_euclid(100));
        } else if rest.starts_with(b"IDDD") {
            // v7.37 — ISO day of year (day within the ISO 8601 week-year),
            // = (iso_week - 1) * 7 + iso_dow. Must precede the `ID` arm.
            let _ = write!(out, "{:03}", (iso_week - 1) * 7 + iso_dow);
            consumed = 4;
        } else if rest.starts_with(b"ID") {
            let _ = write!(out, "{iso_dow}");
        } else if rest.starts_with(b"MM") {
            num!(mo, 2);
        } else if rest.starts_with(b"DD") {
            num!(d, 2);
        } else if rest.starts_with(b"MI") {
            num!(mi, 2);
        } else if rest.starts_with(b"SSSS") {
            // v7.37 — seconds past midnight (0..86399), no zero padding.
            // Must precede the `SS` arm (which `SSSS` also prefix-matches).
            let _ = write!(out, "{}", hh24 * 3600 + mi * 60 + ss);
            consumed = 4;
        } else if rest.starts_with(b"FF") && rest.get(2).is_some_and(u8::is_ascii_digit) {
            // v7.37 — `FF1`..`FF6`: the first N digits of the fractional
            // second (from the 6-digit microsecond field). Previously the
            // `FFn` pattern was emitted verbatim.
            let n = usize::from(rest[2] - b'0');
            let frac = alloc::format!("{us:06}");
            out.push_str(&frac[..n.min(6)]);
            consumed = 3;
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
            out.push_str(&cased_name(
                MONTH_ROMAN[(mo - 1) as usize],
                true,
                false,
                pad(4),
            ));
        } else if rest.starts_with(b"rm") {
            out.push_str(&cased_name(
                MONTH_ROMAN[(mo - 1) as usize],
                false,
                true,
                pad(4),
            ));
        } else if rest.starts_with(b"HH") {
            num!(hh12, 2);
        } else if rest.starts_with(b"A.M.") || rest.starts_with(b"P.M.") {
            // v7.37 — dotted meridiem (uppercase). PG renders the actual
            // half-day regardless of which spelling was requested.
            out.push_str(if hh24 < 12 { "A.M." } else { "P.M." });
            consumed = 4;
        } else if rest.starts_with(b"a.m.") || rest.starts_with(b"p.m.") {
            out.push_str(if hh24 < 12 { "a.m." } else { "p.m." });
            consumed = 4;
        } else if rest.starts_with(b"B.C.") || rest.starts_with(b"A.D.") {
            // v7.37 — dotted era. Proleptic year <= 0 is BC; PG shows the
            // actual era regardless of the requested spelling.
            out.push_str(if i64::from(y) <= 0 { "B.C." } else { "A.D." });
            consumed = 4;
        } else if rest.starts_with(b"b.c.") || rest.starts_with(b"a.d.") {
            out.push_str(if i64::from(y) <= 0 { "b.c." } else { "a.d." });
            consumed = 4;
        } else if rest.starts_with(b"AM") || rest.starts_with(b"PM") {
            out.push_str(ampm);
        } else if rest.starts_with(b"am") || rest.starts_with(b"pm") {
            out.push_str(if hh24 < 12 { "am" } else { "pm" });
        } else if rest.starts_with(b"BC") || rest.starts_with(b"AD") {
            // v7.37 — era indicator (uppercase, no dots).
            out.push_str(if i64::from(y) <= 0 { "BC" } else { "AD" });
        } else if rest.starts_with(b"bc") || rest.starts_with(b"ad") {
            out.push_str(if i64::from(y) <= 0 { "bc" } else { "ad" });
        } else if (rest.starts_with(b"TH") || rest.starts_with(b"th")) && pending_ord.is_some() {
            // v7.37 — ordinal suffix for the preceding numeric field
            // (`DDth` → 21st, `HH12th` → 02nd). Only fires right after a
            // number; a bare `th`/`TH` still passes through as a literal.
            let suf = ordinal_suffix(pending_ord.unwrap_or(0));
            if rest.starts_with(b"TH") {
                out.push_str(&suf.to_ascii_uppercase());
            } else {
                out.push_str(suf);
            }
        } else if rest.starts_with(b"Y") || rest.starts_with(b"I") {
            // Single-digit year / ISO-year (last digit).
            let base = if rest[0] == b'I' { iso_year } else { disp_y };
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
            next_num = Some(i128::from(dow_sun1));
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
        last_num = next_num;
        fm = false;
        i += consumed;
    }
    Ok(Value::text(out))
}
