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

/// v7.39 (read01 regproc.c) — the reg* input types SPG carries as text:
/// resolve the name and return its canonical rendering, with PG's
/// distinct not-found / ambiguous errors.
fn cast_reg_misc(kind: &str, s: &str) -> Result<Value<'static>, EvalError> {
    let bare = s
        .strip_prefix("pg_catalog.")
        .unwrap_or(s)
        .trim()
        .to_string();
    match kind {
        "regconfig" => {
            const CONFIGS: &[&str] = &[
                "simple", "arabic", "armenian", "basque", "catalan", "danish", "dutch",
                "english", "finnish", "french", "german", "greek", "hindi", "hungarian",
                "indonesian", "irish", "italian", "lithuanian", "nepali", "norwegian",
                "portuguese", "romanian", "russian", "serbian", "spanish", "swedish",
                "tamil", "turkish", "yiddish",
            ];
            if CONFIGS.contains(&bare.as_str()) {
                Ok(Value::text(bare))
            } else {
                Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "text search configuration \"{s}\" does not exist"
                    ),
                })
            }
        }
        "regdictionary" => {
            if bare == "simple" || bare.ends_with("_stem") {
                Ok(Value::text(bare))
            } else {
                Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "text search dictionary \"{s}\" does not exist"
                    ),
                })
            }
        }
        "regproc" => {
            let hits = crate::system_catalog::PG_PROC_FUNCS
                .iter()
                .filter(|(_, n, ..)| *n == bare)
                .count();
            match hits {
                0 => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("function \"{s}\" does not exist"),
                }),
                1 => Ok(Value::text(bare)),
                _ => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("more than one function named \"{bare}\""),
                }),
            }
        }
        "regprocedure" => {
            // `name(argtype, ...)` — resolve the name, canonicalize each
            // argument type, and re-render.
            let Some((fname, rest)) = bare.split_once('(') else {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "expected a left parenthesis in \"{s}\""
                    ),
                });
            };
            let Some(args_txt) = rest.strip_suffix(')') else {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "expected a right parenthesis in \"{s}\""
                    ),
                });
            };
            let fname = fname.trim().to_ascii_lowercase();
            let args: Vec<String> = if args_txt.trim().is_empty() {
                Vec::new()
            } else {
                args_txt
                    .split(',')
                    .map(|a| {
                        crate::conversions::regtype_canonical_name(a.trim())
                            .ok_or_else(|| EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "type \"{}\" does not exist",
                                    a.trim()
                                ),
                            })
                    })
                    .collect::<Result<_, _>>()?
            };
            let nargs = args.len() as i32;
            let known = crate::system_catalog::PG_PROC_FUNCS
                .iter()
                .any(|(_, n, _, na, _)| *n == fname && *na == nargs);
            if !known {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("function \"{s}\" does not exist"),
                });
            }
            Ok(Value::text(alloc::format!("{fname}({})", args.join(","))))
        }
        // regoper / regoperator: SPG has no operator catalog; every core
        // operator symbol is multiply overloaded in PG, so a known symbol
        // reports PG's ambiguity and anything else does not exist.
        _ => {
            let sym: String = bare.chars().filter(|c| !c.is_whitespace()).collect();
            let core_op = !sym.is_empty()
                && sym
                    .chars()
                    .all(|c| "+-*/<>=~!@#%^&|`?".contains(c));
            if core_op {
                Err(EvalError::TypeMismatch {
                    detail: alloc::format!("more than one operator named {sym}"),
                })
            } else {
                Err(EvalError::TypeMismatch {
                    detail: alloc::format!("operator does not exist: {s}"),
                })
            }
        }
    }
}

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
        // v7.38 (read01) — the inet/cidr ::text cast shows the mask even for
        // /32 and /128 (PG's cast-path form, unlike the display default).
        CastTarget::Text => Ok(Value::text(match &v {
            Value::Inet { family, bits, addr } | Value::Cidr { family, bits, addr } => {
                crate::conversions::format_inet_full(*family, *bits, addr)
            }
            // v7.38 (read01, T11) — bpchar → text strips the trailing blanks
            // (unlike the padded wire display).
            Value::BpChar(s) => s.trim_end_matches(' ').to_string(),
            // v7.39 (read01 ruleutils.c) — regclass::text is the name.
            Value::RegClass(_, name) => name.to_string(),
            _ => value_to_text(&v),
        })),
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
                // v7.39 (read01 regproc.c) — regtype canonicalizes the
                // name ('int4' → 'integer') and rejects unknown types
                // (PG 42704).
                if matches!(target, CastTarget::RegType) {
                    return match crate::conversions::regtype_canonical_name(&bare) {
                        Some(c) => Ok(Value::text(c)),
                        None => Err(EvalError::TypeMismatch {
                            detail: alloc::format!("type \"{s}\" does not exist"),
                        }),
                    };
                }
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
            // v7.38 (read01) — a temporal type with a fractional-seconds
            // precision (`time(3)`, `timestamp(0)`, `timestamptz(2)`) rounds the
            // sub-second field to that many digits, like PG. Resolve against the
            // base type (`type_name_to_data_type` does not know the `(N)` form)
            // and round the coerced result below.
            // v7.38 (read01, T20) — an integer casts to `bit(n)` as the low n
            // bits of its two's-complement representation (PG; int→varbit is
            // rejected there, so only fixed-length `bit` is handled here).
            if matches!(v, Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_)) {
                if let Some(width) = bit_cast_width(&name) {
                    return int_to_bit_string(v, width);
                }
            }
            // v7.39 (read01 varbit.c) — internal exact-length form for
            // B'...' literals (an explicit ::bit means bit(1) below).
            if name == "__bit_literal" {
                return match &v {
                    Value::Null => Ok(Value::Null),
                    Value::Text(s) => match crate::conversions::parse_bit_string_text(s) {
                        Some((nb, by)) => Ok(Value::bit_string(nb, by)),
                        None => Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "invalid input syntax for type bit: \"{s}\""
                            ),
                        }),
                    },
                    Value::BitString { .. } => Ok(v),
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "cannot cast {:?} to bit",
                            other.data_type()
                        ),
                    }),
                };
            }
            // v7.39 (read01 varbit.c) — `bit(n)` over a bit string (or a
            // '0101' text form) zero-extends on the RIGHT or truncates to
            // n (PG's bit() cast, unlike the input-time exact-length rule).
            let bit_src: Option<Value<'static>> = match &v {
                Value::BitString { .. } => Some(v.clone()),
                Value::Text(s) if bit_cast_width(&name).is_some() => {
                    match crate::conversions::parse_bit_string_text(s) {
                        Some((nb, by)) => Some(Value::bit_string(nb, by)),
                        None => {
                            let bad = s.chars().find(|c| *c != '0' && *c != '1');
                            return Err(EvalError::TypeMismatch {
                                detail: match bad {
                                    Some(c) => alloc::format!(
                                        "\"{c}\" is not a valid binary digit"
                                    ),
                                    None => alloc::format!(
                                        "invalid input syntax for type bit: \"{s}\""
                                    ),
                                },
                            });
                        }
                    }
                }
                _ => None,
            };
            if let Some(Value::BitString { nbits, bytes }) = &bit_src {
                if let Some(width) = bit_cast_width(&name) {
                    let mut bits: alloc::vec::Vec<bool> = (0..*nbits as usize)
                        .map(|i| bytes[i / 8] & (0x80 >> (i % 8)) != 0)
                        .collect();
                    bits.resize(width as usize, false);
                    let mut out = alloc::vec![0u8; width.div_ceil(8) as usize];
                    for (i, b) in bits.iter().enumerate() {
                        if *b {
                            out[i / 8] |= 0x80 >> (i % 8);
                        }
                    }
                    return Ok(Value::BitString {
                        nbits: width,
                        bytes: alloc::borrow::Cow::Owned(out),
                    });
                }
            }
            // v7.39 (read01 oid.c) — OID is unsigned 32-bit: a negative
            // integer wraps (PG's (Oid) cast semantics: -1 -> 4294967295),
            // beyond u32 errors "OID out of range", bad text is 22P02.
            if name.eq_ignore_ascii_case("oid") {
                let as_i64 = match &v {
                    Value::Null => return Ok(Value::Null),
                    Value::SmallInt(n) => Some(i64::from(*n)),
                    Value::Int(n) => Some(i64::from(*n)),
                    Value::BigInt(n) => Some(*n),
                    Value::Text(t) => match t.trim().parse::<i64>() {
                        Ok(n) => Some(n),
                        Err(_) => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "invalid input syntax for type oid: {:?}",
                                    t.trim()
                                ),
                            });
                        }
                    },
                    _ => None,
                };
                if let Some(n) = as_i64 {
                    // 32-bit wrap for negatives (C cast semantics).
                    if (-(1i64 << 31)..0).contains(&n) {
                        return Ok(Value::BigInt(n + (1i64 << 32)));
                    }
                    if !(0..=u32::MAX as i64).contains(&n) {
                        return Err(EvalError::TypeMismatch {
                            detail: "OID out of range".into(),
                        });
                    }
                    return Ok(Value::BigInt(n));
                }
            }
            // v7.39 (read01 mac8.c) — macaddr8 -> macaddr requires the
            // EUI-64 ff:fe infix; anything else is PG's dedicated error.
            if name.eq_ignore_ascii_case("macaddr") {
                if let Value::Macaddr8(b) = &v {
                    if b[3] == 0xff && b[4] == 0xfe {
                        return Ok(Value::Macaddr([b[0], b[1], b[2], b[5], b[6], b[7]]));
                    }
                    return Err(EvalError::TypeMismatch {
                        detail: "macaddr8 data out of range to convert to macaddr".into(),
                    });
                }
            }
            // v7.39 (read01 regproc.c) — the remaining reg* input types.
            // SPG carries them as their canonical text rendering; name
            // resolution runs against the static pg_proc table / the FTS
            // configuration list.
            {
                let lower_name = name.to_ascii_lowercase();
                match lower_name.as_str() {
                    "regproc" | "regprocedure" | "regoper" | "regoperator"
                    | "regconfig" | "regdictionary" => {
                        let s = match &v {
                            Value::Null => return Ok(Value::Null),
                            Value::Text(s) => s.as_ref().trim().to_string(),
                            other => {
                                return Err(EvalError::TypeMismatch {
                                    detail: alloc::format!(
                                        "::{lower_name} accepts TEXT, got {:?}",
                                        other.data_type()
                                    ),
                                });
                            }
                        };
                        return cast_reg_misc(&lower_name, &s);
                    }
                    _ => {}
                }
            }
            // v7.39 (read01 pseudotypes.c) — casting a value INTO a
            // pseudotype hits PG's dummy input functions (0A000).
            {
                let lower = name.to_ascii_lowercase();
                if matches!(
                    lower.as_str(),
                    "anyarray"
                        | "anyelement"
                        | "anyenum"
                        | "anyrange"
                        | "anymultirange"
                        | "anynonarray"
                        | "anycompatible"
                        | "anycompatiblearray"
                        | "anycompatiblenonarray"
                        | "anycompatiblerange"
                        | "anycompatiblemultirange"
                        | "any"
                        | "trigger"
                        | "event_trigger"
                        | "internal"
                        | "language_handler"
                        | "fdw_handler"
                        | "pg_ddl_command"
                        | "pg_node_tree"
                ) && !matches!(v, Value::Null)
                {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!("cannot accept a value of type {lower}"),
                    });
                }
            }
            // v7.39 (read01 pseudotypes.c) — `::cstring` is PG's I/O-form
            // pseudotype: text in, text out (cstring_in/out are identity).
            // SPG carries it as text; pg_typeof(cstring) reading "text" is
            // a recorded delta alongside the literal projection OIDs.
            // v7.39 (read01 xid8funcs.c) — `::xid` (32-bit, wrapping) and
            // `::xid8` (64-bit, full) parse an integer text and render it
            // back verbatim. SPG carries them as BigInt.
            if name.eq_ignore_ascii_case("xid") || name.eq_ignore_ascii_case("xid8") {
                return Ok(match v {
                    Value::Null => Value::Null,
                    Value::SmallInt(n) => Value::BigInt(i64::from(n)),
                    Value::Int(n) => Value::BigInt(i64::from(n)),
                    Value::BigInt(n) => Value::BigInt(n),
                    Value::Text(s) => {
                        let t = s.trim();
                        match t.parse::<u64>() {
                            Ok(n) => Value::BigInt(n as i64),
                            Err(_) => {
                                return Err(EvalError::TypeMismatch {
                                    detail: alloc::format!(
                                        "invalid input syntax for type {}: \"{s}\"",
                                        name.to_ascii_lowercase()
                                    ),
                                });
                            }
                        }
                    }
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "cannot cast {:?} to {name}",
                                other.data_type()
                            ),
                        });
                    }
                });
            }
            // v7.39 (read01 varchar.c) — `::name` is text truncated to
            // NAMEDATALEN-1 (63) bytes.
            if name.eq_ignore_ascii_case("name") {
                return Ok(match v {
                    Value::Null => Value::Null,
                    other => {
                        let t = match other {
                            Value::Text(s) => s.into_owned(),
                            o => value_to_text(&o),
                        };
                        let mut cut = t;
                        if cut.len() > 63 {
                            let mut idx = 63;
                            while !cut.is_char_boundary(idx) {
                                idx -= 1;
                            }
                            cut.truncate(idx);
                        }
                        Value::text(cut)
                    }
                });
            }
            if name.eq_ignore_ascii_case("cstring") {
                return Ok(match v {
                    Value::Null => Value::Null,
                    Value::Text(s) => Value::Text(s),
                    other => Value::text(value_to_text(&other)),
                });
            }
            // v7.39 (read01 jsonpath.c) — `::jsonpath` parses and prints
            // the canonical form (PG's jsonpath type; SPG carries it as
            // text — the wire OID is a recorded residual with the other
            // literal projection OIDs).
            if name.eq_ignore_ascii_case("jsonpath") {
                return match v {
                    Value::Null => Ok(Value::Null),
                    Value::Text(s) => Ok(Value::text(
                        crate::json::jsonpath_canonical(s.as_ref())?,
                    )),
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "cannot cast {:?} to jsonpath",
                            other.data_type()
                        ),
                    }),
                };
            }
            let temporal_prec = temporal_typmod(&name);
            let resolve_name: alloc::borrow::Cow<'_, str> = if temporal_prec.is_some() {
                alloc::borrow::Cow::Owned(
                    name.split('(').next().unwrap_or(&name).trim().to_string(),
                )
            } else {
                alloc::borrow::Cow::Borrowed(name.as_str())
            };
            // v7.37.5 ship triage — generic typed-cast dispatch.
            // Resolve the ident to a `DataType` and route the value
            // through the existing `coerce_value` text-decoder for
            // every v7.37.5 γ/δ/ε/ζ-A type that already speaks
            // Text→typed via codec.
            let dt =
                crate::conversions::type_name_to_data_type(&resolve_name).ok_or_else(|| {
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
                    // v7.39 (read01 inet family) — inet/cidr ::text carries
                    // the mask even at full length (cast-path form).
                    Value::Inet { family, bits, addr }
                    | Value::Cidr { family, bits, addr } => Value::text(
                        crate::conversions::format_inet_full(family, bits, &addr),
                    ),
                    other => Value::text(value_to_text(&other)),
                },
                (spg_storage::DataType::Varchar(n) | spg_storage::DataType::Char(n), v) => {
                    // v7.37 D.36 — previously only `Value::Text` was handled, so
                    // `99::char(2)` reached coerce_value as an INT and hit a
                    // CHAR/INT storage type-mismatch.
                    // v7.39 (bpchar epic) — a bpchar source enters through its
                    // text cast (trailing blanks stripped): `::varchar` keeps
                    // the stripped form, `::char(m)` re-pads in coerce_value.
                    let s = match v {
                        Value::Text(s) => s.into_owned(),
                        Value::BpChar(s) => s.trim_end_matches(' ').to_string(),
                        other => value_to_text(&other),
                    };
                    let s = if *n > 0 && s.chars().count() > *n as usize {
                        s.chars()
                            .take(*n as usize)
                            .collect::<alloc::string::String>()
                    } else {
                        s
                    };
                    Value::text(s)
                }
                (_, v) => v,
            };
            let coerced =
                crate::conversions::coerce_value(v, dt, &resolve_name, 0).map_err(|e| {
                    EvalError::TypeMismatch {
                        detail: alloc::format!("{e}"),
                    }
                })?;
            Ok(match temporal_prec {
                Some(prec) => round_temporal_to_precision(coerced, prec),
                None => coerced,
            })
        }
    }
}

/// v7.38 (read01, T20) — width of a `bit` cast target: bare `bit` is `bit(1)`,
/// `bit(N)` is N. `None` for `varbit` / `bit varying` (PG rejects int→varbit) and
/// any non-bit name.
fn bit_cast_width(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let trimmed = lower.trim();
    if trimmed == "bit" {
        return Some(1);
    }
    let rest = trimmed.strip_prefix("bit")?.trim_start();
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    inner.trim().parse::<u32>().ok()
}

/// v7.38 (read01, T20) — build a `bit(width)` value from an integer: the low
/// `width` bits of the two's-complement, packed MSB-first / left-aligned (the
/// on-wire bit layout). Widths past 64 sign-extend.
fn int_to_bit_string(v: Value<'static>, width: u32) -> Result<Value<'static>, EvalError> {
    let n: i64 = match v {
        Value::Int(x) => i64::from(x),
        Value::BigInt(x) => x,
        Value::SmallInt(x) => i64::from(x),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: "int_to_bit_string: non-integer source".into(),
            });
        }
    };
    let w = width as usize;
    let mut bytes = alloc::vec![0u8; w.div_ceil(8)];
    for i in 0..w {
        let p = w - 1 - i; // bit position counted from the LSB
        let bit = if p >= 64 {
            u8::from(n < 0) // sign-extend beyond the integer's width
        } else {
            ((n >> p) & 1) as u8
        };
        if bit != 0 {
            bytes[i / 8] |= 1 << (7 - (i % 8));
        }
    }
    Ok(Value::bit_string(width, bytes))
}

/// Extract the fractional-seconds precision from a temporal cast name like
/// `time(3)` / `timestamp(0)` / `timestamptz(2)`; `None` for any non-temporal
/// type or a bare temporal type with no `(N)`.
fn temporal_typmod(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    let (base, rest) = lower.split_once('(')?;
    if !matches!(
        base.trim(),
        "time" | "timetz" | "timestamp" | "timestamptz" | "datetime"
    ) {
        return None;
    }
    let digits: alloc::string::String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<u8>().ok()
}

/// Round a TIME / TIMESTAMP value's microsecond field to `prec` fractional-
/// second digits (`prec` 0..=6), half-away-from-zero as PG's AdjustTimestamp.
fn round_temporal_to_precision(v: Value<'static>, prec: u8) -> Value<'static> {
    if prec >= 6 {
        return v;
    }
    let scale = 10i64.pow(u32::from(6 - prec));
    let round = |micros: i64| -> i64 {
        let half = scale / 2;
        if micros >= 0 {
            ((micros + half) / scale) * scale
        } else {
            -(((-micros + half) / scale) * scale)
        }
    };
    match v {
        Value::Timestamp(m) => Value::Timestamp(round(m)),
        Value::Time(m) => Value::Time(round(m)),
        other => other,
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
    // v7.39 (read01 jsonfuncs.c) — the json_to_record/populate desugar
    // routes JSON array text ("[1,2]") through this cast; accept the
    // bracket form alongside PG's brace form.
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .or_else(|| {
            trimmed
                .strip_prefix('[')
                .and_then(|x| x.strip_suffix(']'))
        })
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
        .or_else(|| {
            trimmed
                .strip_prefix('[')
                .and_then(|x| x.strip_suffix(']'))
        })
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
        .or_else(|| {
            trimmed
                .strip_prefix('[')
                .and_then(|x| x.strip_suffix(']'))
        })
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
            // PG error split: numeric-shaped input whose field values
            // fail the calendar checks is "out of range" (plus PG's
            // DateStyle hint); anything else is an input-syntax error.
            if super::format::date_text_is_field_shaped(&s) {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "date/time field value out of range: {s:?}\n\
                         HINT:  Perhaps you need a different \"DateStyle\" setting."
                    ),
                });
            }
            Err(EvalError::TypeMismatch {
                detail: format!("invalid input syntax for type date: {s:?}"),
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
        // v7.39 (read01 timestamp.c) — sentinel-aware (the plain multiply
        // overflowed on ±infinity dates).
        Value::Date(d) => Ok(Value::Timestamp(
            crate::conversions::date_days_to_micros(d),
        )),
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
            if !r.is_finite() || !(-2_147_483_648.0..=2_147_483_647.0).contains(&r) {
                return Err(EvalError::TypeMismatch {
                    detail: "integer out of range".into(),
                });
            }
            Ok(Value::Int(r as i32))
        }
        Value::Numeric { scaled, scale, .. } => {
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
                detail: format!("invalid input syntax for type integer: {s:?}"),
            }),
        Value::Bool(b) => Ok(Value::Int(i32::from(b))),
        // PG `bit`/`varbit` → int is the MSB-first bit value.
        #[allow(clippy::cast_possible_truncation)]
        Value::BitString { nbits, bytes } => Ok(Value::Int(crate::conversions::bit_string_to_i64(
            nbits, &bytes,
        ) as i32)),
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
            if !r.is_finite()
                || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&r)
            {
                return Err(EvalError::TypeMismatch {
                    detail: "bigint out of range".into(),
                });
            }
            Ok(Value::BigInt(r as i64))
        }
        Value::Numeric { scaled, scale, .. } => {
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
        Value::BitString { nbits, bytes } => Ok(Value::BigInt(
            crate::conversions::bit_string_to_i64(nbits, &bytes),
        )),
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
        Value::Numeric { scaled, scale, .. } => Ok(Value::Float(
            (scaled as f64) / f64_powi(10.0, i32::from(scale)),
        )),
        Value::Text(s) => {
            let t = s.trim();
            // Unparseable → invalid syntax; parseable-but-out-of-range (overflow
            // to ±∞ / nonzero underflow to 0) → out of range, the way PG's
            // float8in does, rather than silently yielding Infinity/0. Shared
            // with the Named-cast coerce path so `::float` and `::float8` agree.
            if t.parse::<f64>().is_err() {
                return Err(EvalError::TypeMismatch {
                    detail: format!("cannot parse {s:?} as float"),
                });
            }
            crate::conversions::parse_float8(t)
                .map(Value::Float)
                .ok_or_else(|| EvalError::TypeMismatch {
                    detail: format!("\"{t}\" is out of range for type double precision"),
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
                    detail: format!("invalid input syntax for type boolean: {:?}", s.trim()),
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
