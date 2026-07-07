//! `expr::TYPE` CAST evaluation (cut 29 — extracted from `eval.rs`).
//!
//! Implements PG-style runtime coercion: the giant `cast_value`
//! dispatcher plus its per-target helpers (numeric / bool / array /
//! date / timestamp / interval / vector). Date and timestamp casts
//! defer to the calendar parsers (`parse_date_literal` /
//! `parse_timestamp_literal`) that stay in `eval.rs`; tsvector /
//! tsquery casts defer to the FTS codecs re-exported from
//! `eval::textsearch`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::CastTarget;
use spg_storage::Value;

use super::math::{f64_powi, f64_round_half_even};
use super::{
    EvalError, decode_tsquery_external, decode_tsvector_external, parse_date_literal,
    parse_timestamp_literal, value_to_text,
};

/// Round a numeric operand (`scaled` × 10^-`scale`) to the nearest
/// integer, half-away-from-zero — PG's `numeric → int` coercion rule.
fn numeric_round_to_i128(scaled: i128, scale: u8) -> i128 {
    let factor = 10_i128.pow(u32::from(scale));
    let neg = scaled < 0;
    let abs = scaled.unsigned_abs() as i128;
    let q = abs / factor;
    let r = abs % factor;
    let mag = if 2 * r >= factor { q + 1 } else { q };
    if neg { -mag } else { mag }
}

/// PG-style `expr::TYPE` coercion. NULL always casts as NULL.
pub fn cast_value(v: Value<'static>, target: CastTarget) -> Result<Value<'static>, EvalError> {
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    match target {
        CastTarget::Vector => cast_to_vector(v),
        CastTarget::Text => Ok(Value::text(value_to_text(&v))),
        CastTarget::Int => cast_numeric_to_int(v),
        CastTarget::BigInt => cast_numeric_to_bigint(v),
        CastTarget::Float => cast_numeric_to_float(v),
        CastTarget::Bool => cast_to_bool(v),
        CastTarget::Date => cast_to_date(v),
        // TIMESTAMP and TIMESTAMPTZ have identical runtime
        // representation (i64 microseconds UTC).
        CastTarget::Timestamp | CastTarget::Timestamptz => cast_to_timestamp(v),
        // v7.9.25 — `expr::INTERVAL`. Currently only TEXT → Interval
        // is supported (the mailrs idiom: `$1::INTERVAL` where the
        // bound param is a string like `'7 days'`).
        CastTarget::Interval => cast_to_interval(v),
        // v7.9.25 — `::json` keeps the input text verbatim (PG's json
        // type preserves whitespace / key order / duplicates).
        CastTarget::Json => match v {
            Value::Json(s) => Ok(Value::json(s)),
            Value::Text(s) => Ok(Value::json(s)),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::json only accepts TEXT-shape inputs, got {:?}",
                    other.data_type()
                ),
            }),
        },
        // v7.38 (read01) — `::jsonb` canonicalises like PG: object keys
        // sorted (length, then bytes) + duplicates collapsed last-wins,
        // `, ` / `: ` whitespace, and numbers normalised. Invalid JSON
        // falls back to the verbatim text (validation stays a separate
        // concern from this representation fix).
        CastTarget::Jsonb => match v {
            Value::Json(s) | Value::Text(s) => Ok(Value::json(
                crate::json::canonicalize_jsonb(s.as_ref())
                    .unwrap_or_else(|_| s.as_ref().to_string()),
            )),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::jsonb only accepts TEXT-shape inputs, got {:?}",
                    other.data_type()
                ),
            }),
        },
        // v7.17.0 Phase 5.3 — `::regtype` / `::regclass`. PG
        // semantics: each is a textual catalog-name surfacing as
        // a numeric OID at the wire layer that renders back as
        // the original name. SPG has no OID space, but pg_dump /
        // mailrs / Django code uses the cast purely for textual
        // round-trip — feeding `'public.t'::regclass::text` into
        // a downstream `format(…)` or string concat. We map to
        // that textual contract: Text in → Text out (the schema-
        // qualifier `public.` is stripped to match PG's default
        // search_path-aware rendering); numeric in → re-cast to
        // Text as best-effort; anything else errors.
        //
        // Pre-3.3 / pre-5.3 (v7.9.26) the cast surfaced a clean
        // error; this lifts to accept-and-textify so the dominant
        // dump-loader pattern unblocks. SPG-shaped queries that
        // genuinely need an OID for runtime joins are still
        // documented as unsupported.
        CastTarget::RegType | CastTarget::RegClass => match v {
            Value::Text(s) => {
                // Strip an optional `<schema>.` prefix — PG's
                // regclass render drops it when the schema is on
                // the search_path; SPG is single-schema so
                // dropping is always safe.
                let bare = s.rsplit('.').next().unwrap_or(&s).to_string();
                Ok(Value::text(bare))
            }
            // A numeric OID → its type name for `::regtype` (the common
            // `atttypid::regtype` column-type-name shape). `::regclass`
            // needs a catalog reverse-lookup for user relations, which
            // this cast has no access to, so it keeps rendering the OID.
            Value::Int(_) | Value::BigInt(_) => {
                let n = match v {
                    Value::Int(n) => i64::from(n),
                    Value::BigInt(n) => n,
                    _ => unreachable!(),
                };
                if matches!(target, CastTarget::RegType)
                    && let Some(name) = crate::conversions::regtype_oid_to_name(n)
                {
                    Ok(Value::text::<String>(name.into()))
                } else {
                    Ok(Value::text(alloc::format!("{n}")))
                }
            }
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::regtype / ::regclass accepts TEXT (name) or integer (oid), got {:?}",
                    other.data_type()
                ),
            }),
        },
        // v7.10.11 — `::TEXT[]`. Decode PG external array form
        // when input is Text; pass through unchanged when it is
        // already TextArray. Anything else is a type mismatch.
        CastTarget::TextArray => match v {
            Value::TextArray(items) => Ok(Value::TextArray(items)),
            Value::Text(s) => decode_text_array_external(&s).map(Value::TextArray),
            // Other scalar arrays cast element-wise, each element
            // rendered as its own text (NULLs preserved). PG allows
            // `ARRAY[1,2,3]::text[]`.
            Value::IntArray(items) => Ok(Value::TextArray(
                items
                    .into_iter()
                    .map(|o| o.map(|n| alloc::format!("{n}")))
                    .collect(),
            )),
            Value::BigIntArray(items) => Ok(Value::TextArray(
                items
                    .into_iter()
                    .map(|o| o.map(|n| alloc::format!("{n}")))
                    .collect(),
            )),
            Value::SmallIntArray(items) => Ok(Value::TextArray(
                items
                    .into_iter()
                    .map(|o| o.map(|n| alloc::format!("{n}")))
                    .collect(),
            )),
            Value::BoolArray(items) => Ok(Value::TextArray(
                items
                    .into_iter()
                    .map(|o| o.map(|b| String::from(if b { "t" } else { "f" })))
                    .collect(),
            )),
            Value::FloatArray(items) => Ok(Value::TextArray(
                items
                    .into_iter()
                    .map(|o| o.map(|x| value_to_text(&Value::Float(x))))
                    .collect(),
            )),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::TEXT[] only accepts TEXT / array inputs, got {:?}",
                    other.data_type()
                ),
            }),
        },
        // v7.11.13 — `::INT[]` / `::BIGINT[]`. Decode PG external
        // form `{1,2,3}` when input is Text; widen TextArray /
        // IntArray as appropriate.
        CastTarget::IntArray => cast_to_int_array(v),
        CastTarget::BigIntArray => cast_to_bigint_array(v),
        // v7.12.0 — `::tsvector` / `::tsquery`. Decodes PG external
        // form when input is Text; passes through unchanged when the
        // input is already the target type. Other inputs are a type
        // mismatch. Lexer / Porter stemmer arrive in v7.12.1; the
        // external-form cast at v7.12.0 is the path pg_dump and
        // direct-literal callers use.
        CastTarget::TsVector => match v {
            Value::TsVector(items) => Ok(Value::TsVector(items)),
            Value::Text(s) => decode_tsvector_external(&s).map(Value::TsVector),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::tsvector only accepts TEXT / tsvector inputs, got {:?}",
                    other.data_type()
                ),
            }),
        },
        CastTarget::TsQuery => match v {
            Value::TsQuery(ast) => Ok(Value::TsQuery(ast)),
            Value::Text(s) => decode_tsquery_external(&s).map(Value::TsQuery),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::tsquery only accepts TEXT / tsquery inputs, got {:?}",
                    other.data_type()
                ),
            }),
        },
        // v7.17.0 — `::uuid`. Identity for `uuid → uuid`; parse
        // text via the shared `parse_uuid_str`. Anything else is a
        // type mismatch — PG also rejects e.g. INT → UUID without
        // an explicit text bridge.
        CastTarget::Uuid => match v {
            Value::Uuid(b) => Ok(Value::Uuid(b)),
            Value::Text(s) => match spg_storage::parse_uuid_str(&s) {
                Some(b) => Ok(Value::Uuid(b)),
                None => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("invalid input syntax for type uuid: {s:?}"),
                }),
            },
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::uuid only accepts TEXT / uuid inputs, got {:?}",
                    other.data_type()
                ),
            }),
        },
        // v7.18 — `::bytea`. Identity for `Bytes → Bytes`; decode
        // Text via the engine's PG-format bytea decoder (`\x`
        // hex form + `\NNN` escape form). Anything else is a type
        // mismatch — same shape as PG's contract. Closes the
        // mailrs D-pre #3 reverse-acceptance gap.
        CastTarget::Bytea => match v {
            Value::Bytes(b) => Ok(Value::bytes(b)),
            Value::Text(s) => match crate::conversions::decode_bytea_literal(&s) {
                Ok(b) => Ok(Value::bytes(b)),
                Err(msg) => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("invalid input syntax for type bytea: {msg}"),
                }),
            },
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::bytea only accepts TEXT / bytea inputs, got {:?}",
                    other.data_type()
                ),
            }),
        },
        CastTarget::Named(name) => {
            // v7.37.5 ship triage — generic typed-cast dispatch.
            // Resolve the ident to a `DataType` and route the value
            // through the existing `coerce_value` text-decoder for
            // every v7.37.5 γ/δ/ε/ζ-A type that already speaks
            // Text→typed via codec.
            let dt = crate::conversions::type_name_to_data_type(&name).ok_or_else(|| {
                EvalError::TypeMismatch {
                    detail: alloc::format!("unsupported cast target `::{name}`"),
                }
            })?;
            // PG semantics: any value casts to varchar(n) / char(n) through
            // its text representation (`99::char(2)` → '99'), and an EXPLICIT
            // cast truncates to n characters — only column assignment errors on
            // overflow. Stringify a non-text source first, then truncate up
            // front so the coerce path's length contract never fires here.
            let v = match (&dt, v) {
                // v7.38 (read01) — an explicit cast to TEXT stringifies any
                // value (`text(42)` → '42'), matching `42::text`. (coerce_value
                // deliberately rejects a bare INT→TEXT so INSERT stays strict.)
                (spg_storage::DataType::Text, v) => match v {
                    Value::Text(s) => Value::Text(s),
                    other => Value::text(value_to_text(&other)),
                },
                (
                    spg_storage::DataType::Varchar(n) | spg_storage::DataType::Char(n),
                    v,
                ) => {
                    // v7.37 D.36 — previously only `Value::Text` was handled, so
                    // `99::char(2)` reached coerce_value as an INT and hit a
                    // CHAR/INT storage type-mismatch.
                    let s = match v {
                        Value::Text(s) => s.into_owned(),
                        other => value_to_text(&other),
                    };
                    let s = if *n > 0 && s.chars().count() > *n as usize {
                        s.chars().take(*n as usize).collect::<alloc::string::String>()
                    } else {
                        s
                    };
                    Value::text(s)
                }
                (_, v) => v,
            };
            crate::conversions::coerce_value(v, dt, &name, 0).map_err(|e| EvalError::TypeMismatch {
                detail: alloc::format!("{e}"),
            })
        }
    }
}

fn cast_to_int_array(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::IntArray(items) => Ok(Value::IntArray(items)),
        Value::BigIntArray(items) => {
            let mut out: Vec<Option<i32>> = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    None => out.push(None),
                    Some(n) => match i32::try_from(n) {
                        Ok(x) => out.push(Some(x)),
                        Err(_) => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!("::INT[] element {n} overflows i32"),
                            });
                        }
                    },
                }
            }
            Ok(Value::IntArray(out))
        }
        Value::Text(s) => decode_int_array_external(&s).map(Value::IntArray),
        Value::TextArray(items) => {
            let mut out: Vec<Option<i32>> = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    None => out.push(None),
                    Some(s) => match s.parse::<i32>() {
                        Ok(n) => out.push(Some(n)),
                        Err(_) => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!("::INT[] cannot parse {s:?}"),
                            });
                        }
                    },
                }
            }
            Ok(Value::IntArray(out))
        }
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!("::INT[] does not accept {:?}", other.data_type()),
        }),
    }
}

fn cast_to_bigint_array(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::BigIntArray(items) => Ok(Value::BigIntArray(items)),
        Value::IntArray(items) => Ok(Value::BigIntArray(
            items.into_iter().map(|x| x.map(i64::from)).collect(),
        )),
        Value::Text(s) => decode_bigint_array_external(&s).map(Value::BigIntArray),
        Value::TextArray(items) => {
            let mut out: Vec<Option<i64>> = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    None => out.push(None),
                    Some(s) => match s.parse::<i64>() {
                        Ok(n) => out.push(Some(n)),
                        Err(_) => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!("::BIGINT[] cannot parse {s:?}"),
                            });
                        }
                    },
                }
            }
            Ok(Value::BigIntArray(out))
        }
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!("::BIGINT[] does not accept {:?}", other.data_type()),
        }),
    }
}

fn decode_int_array_external(s: &str) -> Result<Vec<Option<i32>>, EvalError> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .ok_or_else(|| EvalError::TypeMismatch {
            detail: alloc::format!("INT[] literal {s:?} must be enclosed in '{{...}}'"),
        })?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|part| {
            let p = part.trim();
            if p.eq_ignore_ascii_case("NULL") {
                Ok(None)
            } else {
                p.parse::<i32>()
                    .map(Some)
                    .map_err(|_| EvalError::TypeMismatch {
                        detail: alloc::format!("INT[] element {p:?} is not an i32"),
                    })
            }
        })
        .collect()
}

fn decode_bigint_array_external(s: &str) -> Result<Vec<Option<i64>>, EvalError> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .ok_or_else(|| EvalError::TypeMismatch {
            detail: alloc::format!("BIGINT[] literal {s:?} must be enclosed in '{{...}}'"),
        })?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|part| {
            let p = part.trim();
            if p.eq_ignore_ascii_case("NULL") {
                Ok(None)
            } else {
                p.parse::<i64>()
                    .map(Some)
                    .map_err(|_| EvalError::TypeMismatch {
                        detail: alloc::format!("BIGINT[] element {p:?} is not an i64"),
                    })
            }
        })
        .collect()
}

/// v7.10.11 — same decoder as `decode_text_array_literal` in
/// `lib.rs`, but lives here so the eval-time cast path stays
/// inside `spg-engine::eval`. Kept in lock-step with the engine
/// `coerce_value` decoder by tests.
fn decode_text_array_external(s: &str) -> Result<Vec<Option<String>>, EvalError> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .ok_or_else(|| EvalError::TypeMismatch {
            detail: alloc::format!("TEXT[] literal {s:?} must be enclosed in '{{...}}'"),
        })?;
    let mut out: Vec<Option<String>> = Vec::new();
    if inner.trim().is_empty() {
        return Ok(out);
    }
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i <= bytes.len() {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let mut buf = String::new();
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    buf.push(bytes[i + 1] as char);
                    i += 2;
                } else {
                    buf.push(bytes[i] as char);
                    i += 1;
                }
            }
            if i >= bytes.len() {
                return Err(EvalError::TypeMismatch {
                    detail: "unterminated quoted element in TEXT[] literal".into(),
                });
            }
            i += 1;
            out.push(Some(buf));
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            let raw = inner[start..i].trim();
            if raw.eq_ignore_ascii_case("NULL") {
                out.push(None);
            } else {
                out.push(Some(raw.to_string()));
            }
        }
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] != b',' {
            return Err(EvalError::TypeMismatch {
                detail: "expected ',' between TEXT[] elements".into(),
            });
        }
        i += 1;
    }
    Ok(out)
}

fn cast_to_interval(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Interval {
            months,
            days,
            micros,
        } => Ok(Value::Interval {
            months,
            days,
            micros,
        }),
        Value::Text(s) => {
            let (months, days, micros) =
                spg_sql::parser::parse_interval_text(&s).ok_or_else(|| {
                    EvalError::TypeMismatch {
                        detail: alloc::format!("cannot parse {s:?} as INTERVAL"),
                    }
                })?;
            Ok(Value::Interval {
                months,
                days,
                micros,
            })
        }
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "::INTERVAL only accepts TEXT-shape inputs, got {:?}",
                other.data_type()
            ),
        }),
    }
}

fn cast_to_date(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Date(d) => Ok(Value::Date(d)),
        // Integer literals carry days since the Unix epoch — used by
        // the `CURRENT_DATE` AST rewrite to inject the wall clock.
        Value::Int(n) => Ok(Value::Date(n)),
        Value::BigInt(n) => {
            i32::try_from(n)
                .map(Value::Date)
                .map_err(|_| EvalError::TypeMismatch {
                    detail: "bigint days-since-epoch out of DATE range".into(),
                })
        }
        // Timestamp truncates to its day boundary.
        Value::Timestamp(t) => {
            let days = t.div_euclid(86_400_000_000);
            i32::try_from(days)
                .map(Value::Date)
                .map_err(|_| EvalError::TypeMismatch {
                    detail: "timestamp out of DATE range".into(),
                })
        }
        Value::Text(s) => {
            if let Some(d) = parse_date_literal(&s) {
                return Ok(Value::Date(d));
            }
            // PG accepts a full timestamp string in a DATE cast and
            // truncates to the day (verified vs live PG18.4:
            // `'2020-01-01 12:00:00'::date` → 2020-01-01; a bad time
            // like `'... 25:00:00'` still raises). Reuse the timestamp
            // parser — it validates the time-of-day + optional TZ — then
            // floor to the date via the same path as the Timestamp arm.
            if let Some(t) = parse_timestamp_literal(&s) {
                let days = t.div_euclid(86_400_000_000);
                return i32::try_from(days)
                    .map(Value::Date)
                    .map_err(|_| EvalError::TypeMismatch {
                        detail: "timestamp out of DATE range".into(),
                    });
            }
            Err(EvalError::TypeMismatch {
                detail: format!("cannot parse {s:?} as DATE (expected YYYY-MM-DD)"),
            })
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {:?} to DATE", other.data_type()),
        }),
    }
}

fn cast_to_timestamp(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Timestamp(t) => Ok(Value::Timestamp(t)),
        // Int / BigInt carry microseconds since the Unix epoch — used
        // by the `NOW()` / `CURRENT_TIMESTAMP` AST rewrite to inject
        // the wall clock as a plain integer literal.
        Value::Int(n) => Ok(Value::Timestamp(i64::from(n))),
        Value::BigInt(n) => Ok(Value::Timestamp(n)),
        // DATE → TIMESTAMP picks midnight on the date.
        Value::Date(d) => Ok(Value::Timestamp(i64::from(d) * 86_400_000_000)),
        Value::Text(s) => {
            parse_timestamp_literal(&s)
                .map(Value::Timestamp)
                .ok_or(EvalError::TypeMismatch {
                    detail: format!(
                        "cannot parse {s:?} as TIMESTAMP \
                     (expected YYYY-MM-DD[ HH:MM:SS[.ffffff]])"
                    ),
                })
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {:?} to TIMESTAMP", other.data_type()),
        }),
    }
}

fn cast_numeric_to_int(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Int(n) => Ok(Value::Int(n)),
        Value::BigInt(n) => i32::try_from(n)
            .map(Value::Int)
            .map_err(|_| EvalError::TypeMismatch {
                detail: format!("bigint {n} does not fit in int"),
            }),
        // PG rounds (half-to-even) coercing a real number to an integer, and
        // errors on a non-finite or out-of-range value (`'inf'::int`,
        // `1e20::int`) rather than saturating.
        #[allow(clippy::cast_possible_truncation)]
        Value::Float(x) => {
            let r = f64_round_half_even(x);
            if !r.is_finite() || r < -2_147_483_648.0 || r > 2_147_483_647.0 {
                return Err(EvalError::TypeMismatch {
                    detail: "integer out of range".into(),
                });
            }
            Ok(Value::Int(r as i32))
        }
        Value::Numeric { scaled, scale } => {
            let rounded = numeric_round_to_i128(scaled, scale);
            i32::try_from(rounded)
                .map(Value::Int)
                .map_err(|_| EvalError::TypeMismatch {
                    detail: format!("numeric {rounded} does not fit in int"),
                })
        }
        Value::Text(s) => crate::conversions::parse_pg_int(&s)
            .and_then(|n| i32::try_from(n).ok())
            .map(Value::Int)
            .ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("cannot parse {s:?} as int"),
            }),
        Value::Bool(b) => Ok(Value::Int(i32::from(b))),
        // PG `bit`/`varbit` → int is the MSB-first bit value.
        #[allow(clippy::cast_possible_truncation)]
        Value::BitString { nbits, bytes } => {
            Ok(Value::Int(crate::conversions::bit_string_to_i64(nbits, &bytes) as i32))
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {:?} to int", other.data_type()),
        }),
    }
}

fn cast_numeric_to_bigint(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Int(n) => Ok(Value::BigInt(i64::from(n))),
        Value::BigInt(n) => Ok(Value::BigInt(n)),
        // PG rounds (half-to-even) coercing a real number to bigint, and errors
        // on a non-finite or out-of-range value rather than saturating.
        #[allow(clippy::cast_possible_truncation)]
        Value::Float(x) => {
            let r = f64_round_half_even(x);
            if !r.is_finite() || r < -9_223_372_036_854_775_808.0 || r >= 9_223_372_036_854_775_808.0
            {
                return Err(EvalError::TypeMismatch {
                    detail: "bigint out of range".into(),
                });
            }
            Ok(Value::BigInt(r as i64))
        }
        Value::Numeric { scaled, scale } => {
            let rounded = numeric_round_to_i128(scaled, scale);
            i64::try_from(rounded)
                .map(Value::BigInt)
                .map_err(|_| EvalError::TypeMismatch {
                    detail: format!("numeric {rounded} does not fit in bigint"),
                })
        }
        Value::Text(s) => crate::conversions::parse_pg_int(&s)
            .map(Value::BigInt)
            .ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("cannot parse {s:?} as bigint"),
            }),
        Value::Bool(b) => Ok(Value::BigInt(i64::from(b))),
        // PG `bit`/`varbit` → bigint is the MSB-first bit value.
        Value::BitString { nbits, bytes } => {
            Ok(Value::BigInt(crate::conversions::bit_string_to_i64(nbits, &bytes)))
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {:?} to bigint", other.data_type()),
        }),
    }
}

fn cast_numeric_to_float(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Int(n) => Ok(Value::Float(f64::from(n))),
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Ok(Value::Float(n as f64)),
        Value::Float(x) => Ok(Value::Float(x)),
        // PG's numeric→double precision is an implicit cast; a
        // `Value::Numeric` (from `::numeric`, a numeric column, or
        // numeric arithmetic) must convert to f64, not error.
        #[allow(clippy::cast_precision_loss)]
        Value::Numeric { scaled, scale } => {
            Ok(Value::Float((scaled as f64) / f64_powi(10.0, i32::from(scale))))
        }
        Value::Text(s) => {
            s.trim()
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| EvalError::TypeMismatch {
                    detail: format!("cannot parse {s:?} as float"),
                })
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {:?} to float", other.data_type()),
        }),
    }
}

fn cast_to_bool(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Bool(b) => Ok(Value::Bool(b)),
        Value::Int(n) => Ok(Value::Bool(n != 0)),
        Value::BigInt(n) => Ok(Value::Bool(n != 0)),
        Value::Text(s) => {
            // PG boolin accepts any unambiguous prefix of true/false/yes/no
            // plus on/off/1/0 (case-insensitive, trimmed); `o` alone is
            // ambiguous (on vs off) and errors.
            let lo = s.trim().to_ascii_lowercase();
            match lo.as_str() {
                "1" | "t" | "tr" | "tru" | "true" | "y" | "ye" | "yes" | "on" => {
                    Ok(Value::Bool(true))
                }
                "0" | "f" | "fa" | "fal" | "fals" | "false" | "n" | "no" | "of" | "off" => {
                    Ok(Value::Bool(false))
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: format!("cannot parse {s:?} as bool"),
                }),
            }
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {:?} to bool", other.data_type()),
        }),
    }
}

/// Parse a `Value::text("[1.0, 2.0, 3.0]")` into a `Value::vector(..)`. Mirrors
/// pgvector's `'[..]'::vector` cast. NULL casts as NULL.
pub fn cast_to_vector(v: Value) -> Result<Value<'static>, EvalError> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Vector(v) => Ok(Value::vector(v.into_owned())),
        Value::Text(s) => parse_vector_text(&s)
            .map(Value::vector)
            .ok_or(EvalError::TypeMismatch {
                detail: format!("cannot parse {s:?} as a vector literal"),
            }),
        other => Err(EvalError::TypeMismatch {
            detail: format!("::vector requires text input, got {:?}", other.data_type()),
        }),
    }
}

/// Parse `"[1.0, 2.0, -3]"` into `Vec<f32>`. Returns `None` on malformed input.
pub fn parse_vector_text(s: &str) -> Option<Vec<f32>> {
    let trimmed = s.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    let trimmed_inner = inner.trim();
    if trimmed_inner.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for part in trimmed_inner.split(',') {
        let f: f32 = part.trim().parse().ok()?;
        out.push(f);
    }
    Some(out)
}
