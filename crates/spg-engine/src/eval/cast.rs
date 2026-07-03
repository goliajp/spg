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

use super::{
    EvalError, decode_tsquery_external, decode_tsvector_external, parse_date_literal,
    parse_timestamp_literal, value_to_text,
};

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
        // v7.9.25 — `::json` / `::jsonb`. Routes Text → Json
        // (validation is the producer's responsibility, same as
        // the column-INSERT path).
        CastTarget::Json | CastTarget::Jsonb => match v {
            Value::Json(s) => Ok(Value::json(s)),
            Value::Text(s) => Ok(Value::json(s)),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::json / ::jsonb only accepts TEXT-shape inputs, got {:?}",
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
            Value::Int(n) => Ok(Value::text(alloc::format!("{n}"))),
            Value::BigInt(n) => Ok(Value::text(alloc::format!("{n}"))),
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
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::TEXT[] only accepts TEXT / TEXT[] inputs, got {:?}",
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
            // PG semantics: an EXPLICIT varchar(n) / char(n) cast
            // truncates to n characters — only column assignment
            // errors on overflow. Truncate up front so the coerce
            // path's length contract never fires here.
            let v = match (&dt, v) {
                (
                    spg_storage::DataType::Varchar(n) | spg_storage::DataType::Char(n),
                    Value::Text(s),
                ) if *n > 0 && s.chars().count() > *n as usize => {
                    Value::text(s.chars().take(*n as usize).collect::<alloc::string::String>())
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
        Value::Text(s) => parse_date_literal(&s)
            .map(Value::Date)
            .ok_or(EvalError::TypeMismatch {
                detail: format!("cannot parse {s:?} as DATE (expected YYYY-MM-DD)"),
            }),
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
        #[allow(clippy::cast_possible_truncation)]
        Value::Float(x) => Ok(Value::Int(x as i32)),
        Value::Text(s) => {
            s.trim()
                .parse::<i32>()
                .map(Value::Int)
                .map_err(|_| EvalError::TypeMismatch {
                    detail: format!("cannot parse {s:?} as int"),
                })
        }
        Value::Bool(b) => Ok(Value::Int(i32::from(b))),
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {:?} to int", other.data_type()),
        }),
    }
}

fn cast_numeric_to_bigint(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Int(n) => Ok(Value::BigInt(i64::from(n))),
        Value::BigInt(n) => Ok(Value::BigInt(n)),
        #[allow(clippy::cast_possible_truncation)]
        Value::Float(x) => Ok(Value::BigInt(x as i64)),
        Value::Text(s) => {
            s.trim()
                .parse::<i64>()
                .map(Value::BigInt)
                .map_err(|_| EvalError::TypeMismatch {
                    detail: format!("cannot parse {s:?} as bigint"),
                })
        }
        Value::Bool(b) => Ok(Value::BigInt(i64::from(b))),
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
            let lo = s.trim().to_ascii_lowercase();
            match lo.as_str() {
                "true" | "t" | "yes" | "y" | "1" | "on" => Ok(Value::Bool(true)),
                "false" | "f" | "no" | "n" | "0" | "off" => Ok(Value::Bool(false)),
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
