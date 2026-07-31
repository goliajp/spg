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
/// v7.39 (round 515) — the type names each family below accepts, declared
/// ONCE so the value path and the NULL-target check cannot drift apart.
///
/// They already had, three times. Round 509 added a check that a cast target
/// names something, with its own hand-kept list; round 512 then added
/// `Value::Cid` without registering `cid` there, round 514 found that and
/// `xid`, and round 515 found six more — `'english'::regconfig` resolved
/// while `NULL::regconfig` did not. The duplication was the defect, so it
/// is gone rather than patched a third time.
pub(crate) const REG_MISC_TYPES: &[&str] = &[
    "regproc",
    "regprocedure",
    "regoper",
    "regoperator",
    "regconfig",
    "regdictionary",
    "regcollation",
];

/// The catalog-shaped scalars — see `cast_catalog_scalar`.
pub(crate) const CATALOG_SCALAR_TYPES: &[&str] = &[
    "cid",
    "xid",
    "oidvector",
    "int2vector",
    "aclitem",
    "refcursor",
    "pg_snapshot",
    "txid_snapshot",
    "jsonpath",
];

/// The pseudotypes and the statistics / GiST internals: a NULL is NULL, a
/// value is "cannot accept a value of type X".
pub(crate) const OPAQUE_TYPES: &[&str] = &[
    "anyarray",
    "anyelement",
    "anyenum",
    "anyrange",
    "anymultirange",
    "anynonarray",
    "anycompatible",
    "anycompatiblearray",
    "anycompatiblenonarray",
    "anycompatiblerange",
    "anycompatiblemultirange",
    "any",
    "trigger",
    "event_trigger",
    "internal",
    "language_handler",
    "fdw_handler",
    "pg_ddl_command",
    "pg_node_tree",
    "pg_ndistinct",
    "pg_mcv_list",
    "pg_dependencies",
    "pg_brin_minmax_multi_summary",
    "pg_brin_bloom_summary",
    "gtsvector",
];

fn cast_reg_misc(kind: &str, s: &str) -> Result<Value<'static>, EvalError> {
    let bare = s
        .strip_prefix("pg_catalog.")
        .unwrap_or(s)
        .trim()
        .to_string();
    match kind {
        "regconfig" => {
            const CONFIGS: &[&str] = &[
                "simple",
                "arabic",
                "armenian",
                "basque",
                "catalan",
                "danish",
                "dutch",
                "english",
                "finnish",
                "french",
                "german",
                "greek",
                "hindi",
                "hungarian",
                "indonesian",
                "irish",
                "italian",
                "lithuanian",
                "nepali",
                "norwegian",
                "portuguese",
                "romanian",
                "russian",
                "serbian",
                "spanish",
                "swedish",
                "tamil",
                "turkish",
                "yiddish",
            ];
            if CONFIGS.contains(&bare.as_str()) {
                Ok(Value::text(bare))
            } else {
                Err(EvalError::TypeMismatch {
                    detail: alloc::format!("text search configuration \"{s}\" does not exist"),
                })
            }
        }
        // v7.39 (round 513) — `regcollation`. PG lowercases an UNQUOTED
        // identifier before it looks, which is why `'C'::regcollation` is
        // "collation \"c\" for encoding \"UTF8\" does not exist" there while
        // `'\"C\"'::regcollation` resolves — measured, and the reason the
        // quoted form is the one anybody writes. The rendering keeps the
        // quotes PG puts back on a name that needs them.
        "regcollation" => {
            let quoted = bare.starts_with('"') && bare.ends_with('"') && bare.len() >= 2;
            let name = if quoted {
                bare[1..bare.len() - 1].to_string()
            } else {
                bare.to_ascii_lowercase()
            };
            const COLLATIONS: &[&str] = &["C", "POSIX", "default", "ucs_basic"];
            match COLLATIONS.iter().find(|c| **c == name) {
                // PG re-quotes anything that is not a plain lowercase word.
                Some(c) => Ok(Value::text(if c.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_')
                    && *c != "default"
                {
                    (*c).to_string()
                } else {
                    alloc::format!("\"{c}\"")
                })),
                None => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "collation \"{name}\" for encoding \"UTF8\" does not exist"
                    ),
                }),
            }
        }
        "regdictionary" => {
            if bare == "simple" || bare.ends_with("_stem") {
                Ok(Value::text(bare))
            } else {
                Err(EvalError::TypeMismatch {
                    detail: alloc::format!("text search dictionary \"{s}\" does not exist"),
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
                    detail: alloc::format!("expected a left parenthesis in \"{s}\""),
                });
            };
            let Some(args_txt) = rest.strip_suffix(')') else {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("expected a right parenthesis in \"{s}\""),
                });
            };
            let fname = fname.trim().to_ascii_lowercase();
            let args: Vec<String> = if args_txt.trim().is_empty() {
                Vec::new()
            } else {
                args_txt
                    .split(',')
                    .map(|a| {
                        crate::conversions::regtype_canonical_name(a.trim()).ok_or_else(|| {
                            EvalError::TypeMismatch {
                                detail: alloc::format!("type \"{}\" does not exist", a.trim()),
                            }
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
            let core_op = !sym.is_empty() && sym.chars().all(|c| "+-*/<>=~!@#%^&|`?".contains(c));
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

/// v7.39 (round 355, M13) — `BINARY expr` / `CAST(expr AS BINARY[(n)])`.
fn cast_mysql_binary(v: Value<'static>, name: &str) -> Result<Value<'static>, EvalError> {
    let limit: Option<usize> = name
        .split_once('(')
        .and_then(|(_, rest)| rest.trim_end_matches(')').trim().parse().ok());
    let text = match &v {
        Value::Null => return Ok(Value::Null),
        Value::Text(t) => t.to_string(),
        Value::BpChar(t) => t.to_string(),
        other => crate::eval::values::value_to_text(other),
    };
    Ok(Value::text(match limit {
        // Byte-wise, which is the point of the type.
        Some(n) if text.len() > n => {
            let mut cut = n;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text[..cut].to_string()
        }
        _ => text,
    }))
}

/// v7.39 (round 352, M8) — `CAST(x AS SIGNED)` / `CAST(x AS UNSIGNED)`.
fn cast_mysql_integer(v: Value<'static>, unsigned: bool) -> Result<Value<'static>, EvalError> {
    // v7.39 (round 527) — an EXACT integer source must not round-trip
    // through f64. It loses precision above 2^53, and the float→int cast
    // SATURATES, so `CAST(18446744073709551615 AS UNSIGNED)` answered
    // 9223372036854775807 — a different number, with nothing to say so.
    // The value stores, compares and sums correctly at full width
    // (measured against MariaDB 11); only the cast reduced it.
    let exact: Option<i128> = match &v {
        Value::Bool(b) => Some(i128::from(u8::from(*b))),
        Value::SmallInt(x) => Some(i128::from(*x)),
        Value::Int(x) => Some(i128::from(*x)),
        Value::BigInt(x) => Some(i128::from(*x)),
        Value::Numeric {
            scaled,
            scale: 0,
            kind: spg_storage::NumericKind::Finite,
        } => Some(*scaled),
        _ => None,
    };
    let rounded: i128 = match exact {
        Some(n) => n,
        None => {
            let n: f64 = match &v {
                Value::Null => return Ok(Value::Null),
                Value::Float(x) => *x,
                Value::Real(x) => f64::from(*x),
                #[allow(clippy::cast_precision_loss)]
                Value::Numeric { scaled, scale, .. } => {
                    *scaled as f64 / 10_f64.powi(i32::from(*scale))
                }
                Value::Text(t) | Value::BpChar(t) => crate::eval::mysql_leading_number(t),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!("cannot cast {} to integer", crate::conversions::pg_type_name_for_error_opt(other.data_type())),
                    });
                }
            };
            // Half away from zero, which is what MariaDB does
            // (2.5 → 3, -2.5 → -3).
            let r = if n >= 0.0 { (n + 0.5).floor() } else { (n - 0.5).ceil() };
            #[allow(clippy::cast_possible_truncation)]
            let as_i64 = r as i64;
            i128::from(as_i64)
        }
    };
    if unsigned {
        // MariaDB wraps a negative through the full u64 range.
        let wrapped: u64 = if rounded < 0 {
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            {
                rounded as i64 as u64
            }
        } else {
            u64::try_from(rounded).unwrap_or(u64::MAX)
        };
        // Above i64::MAX the value only fits the numeric carrier, which
        // is the same one a BIGINT UNSIGNED column already uses.
        return Ok(if wrapped > i64::MAX as u64 {
            Value::Numeric {
                scaled: i128::from(wrapped),
                scale: 0,
                kind: spg_storage::NumericKind::Finite,
            }
        } else {
            #[allow(clippy::cast_possible_wrap)]
            Value::BigInt(wrapped as i64)
        });
    }
    Ok(Value::BigInt(
        i64::try_from(rounded).unwrap_or(if rounded < 0 { i64::MIN } else { i64::MAX }),
    ))
}

/// v7.39 (round 544) — the integer a bytea's bytes spell: big-endian,
/// right-aligned into `width` bytes, sign-extended from the leading
/// byte when the value already fills the width. Measured on PG18:
/// `'\x05'::bytea::int8` is 5, `'\x'::bytea::int4` is 0,
/// `'\xffffffff'::bytea::int4` is -1.
#[inline(never)]
fn bytea_to_integer(v: &Value<'static>, width: usize) -> Result<Value<'static>, EvalError> {
    let Value::Bytes(b) = v else {
        return Err(EvalError::TypeMismatch {
            detail: alloc::string::String::from("expected bytea"),
        });
    };
    if b.len() > width {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("bytea of {} bytes is too wide for the target", b.len()),
        });
    }
    let negative = b.len() == width && b.first().is_some_and(|f| *f & 0x80 != 0);
    let mut acc: i64 = if negative { -1 } else { 0 };
    for byte in b.iter() {
        acc = (acc << 8) | i64::from(*byte);
    }
    Ok(if width == 4 {
        Value::Int(i32::try_from(acc).unwrap_or(0))
    } else {
        Value::BigInt(acc)
    })
}

/// Round a numeric operand (`scaled` × 10^-`scale`) to the nearest
/// integer, half-away-from-zero — PG's `numeric → int` coercion rule.
fn numeric_round_to_i128(scaled: i128, scale: u16) -> i128 {
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
    cast_value_in(v, target, false)
}

/// v7.39 (round 352, M8) — `cast_value` with the session dialect, for the
/// targets the two disagree about (`SIGNED` / `UNSIGNED` exist only in
/// MySQL: PG says `type "signed" does not exist`, measured).
pub fn cast_value_in(
    v: Value<'static>,
    target: CastTarget,
    mysql: bool,
) -> Result<Value<'static>, EvalError> {
    cast_value_ref_in(v, &target, mysql)
}

/// v7.39 (round 607) — the same dispatch, taking the target by REFERENCE.
///
/// `eval_cast_arm` cloned the target for every row. For the settled variants
/// that clone is free, which is why `id::FLOAT` allocated nothing a row while
/// `id::REAL` — the same conversion under a name the parser leaves as
/// `Named(String)` — allocated one just to hand the name over, and seven more
/// re-deriving its lowercase form inside.
pub fn cast_value_ref_in(
    v: Value<'static>,
    target: &CastTarget,
    mysql: bool,
) -> Result<Value<'static>, EvalError> {
    // v7.39 (round 509) — PG validates the cast TARGET whatever the operand
    // is: `NULL::nosuchtype` is an error there, not NULL. This returned early
    // before ever looking at the target, so a misspelt type name silently
    // produced NULL and `pg_typeof(NULL::nosuchtype)` answered `unknown`. A
    // value operand DID error, so the gap was exactly the NULL case, in both
    // spellings (`::t` and `CAST(… AS t)`).
    //
    // Only `Named` can fail to resolve; every other CastTarget is a variant
    // the parser already settled. So a NULL keeps its short-circuit
    // everywhere else and a Named target runs the real path, which is the
    // only thing that knows every name that resolves. Writing a second
    // resolver to check the name against looked simpler and was wrong: it
    // missed `::binary` (the MySQL prefix's desugar), a table's row type,
    // and the pseudotypes, all of which resolve further down this arm.
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
            Value::RegClass(_, name) | Value::RegProc(_, name) => name.to_string(),
            _ => value_to_text(&v),
        })),
        // v7.39 (round 254) — the integer targets refuse a NUMERIC special
        // outright (PG: `cannot convert NaN to integer`); without this the
        // arms below read the special's canonical mantissa and answered 0.
        // The float / numeric targets pass it through instead — handled in
        // their own arms, which now consult `kind`.
        // v7.39 (round 343) — an OID-typed reference casts to an integer
        // the way PG's do (`'t'::regclass::bigint` is 27830 there). SPG
        // reported `cannot cast None to bigint`: the integer path read the
        // value's storage DataType, which these two deliberately do not
        // have, and the message leaked that `None` to the client.
        CastTarget::BigInt | CastTarget::Int
            if matches!(v, Value::RegClass(..) | Value::RegProc(..)) =>
        {
            let (Value::RegClass(oid, _) | Value::RegProc(oid, _)) = v else {
                unreachable!("guarded above")
            };
            Ok(if matches!(target, CastTarget::BigInt) {
                Value::BigInt(oid)
            } else {
                Value::Int(i32::try_from(oid).unwrap_or(i32::MAX))
            })
        }
        // v7.39 (round 544) — bytea reads back as the integer its bytes
        // spell, big-endian and right-aligned. Measured on PG18:
        // '\x05'::bytea::int8 is 5, '\x'::bytea::int4 is 0.
        CastTarget::Int if matches!(v, Value::Bytes(_)) => bytea_to_integer(&v, 4),
        CastTarget::BigInt if matches!(v, Value::Bytes(_)) => bytea_to_integer(&v, 8),
        CastTarget::Int => cast_numeric_special_reject(&v, "integer")
            .unwrap_or_else(|| cast_numeric_to_int(v)),
        CastTarget::BigInt => cast_numeric_special_reject(&v, "bigint")
            .unwrap_or_else(|| cast_numeric_to_bigint(v)),
        CastTarget::Float => cast_numeric_to_float(v),
        CastTarget::Bool => cast_to_bool(v),
        CastTarget::Date => cast_to_date(v),
        // TIMESTAMP and TIMESTAMPTZ share a runtime representation
        // (i64 microseconds UTC) but NOT an input rule, and conflating
        // the two silently stored the wrong instant. `::timestamp`
        // keeps the wall clock a literal's offset was written against
        // (round 289); `::timestamptz` CONVERTS by it. The evaluator's
        // context-aware arm intercepts timestamptz before this point,
        // so the difference only showed on the paths that reach here
        // directly — INSERT VALUES folds its literals through
        // `literal_expr_to_value_in`, and stored 10:00 for
        // `'2020-01-01 10:00:00+02'::timestamptz` where PG stores
        // 08:00 (round 310).
        // v7.39 (round 423) — `CAST(x AS DATETIME)` / `AS TIMESTAMP` reach
        // here as the dedicated variant rather than `Named`, so the MySQL
        // "bare temporal type has fractional precision 0" rule has to be
        // applied here too: MariaDB drops the fraction, PG keeps every
        // microsecond.
        CastTarget::Timestamp => {
            let out = cast_to_timestamp(v)?;
            Ok(if mysql {
                round_temporal_to_precision(out, 0, true)
            } else {
                out
            })
        }
        CastTarget::Timestamptz => cast_to_timestamptz(v),
        // v7.9.25 — `expr::INTERVAL`. Currently only TEXT → Interval
        // is supported (the mailrs idiom: `$1::INTERVAL` where the
        // bound param is a string like `'7 days'`).
        // v7.39 (round 544) — a time-of-day IS an interval of that
        // length. Measured: '10:20:30.123456'::time::interval reads
        // 10:20:30.123456 on PG18, fractional seconds and all.
        CastTarget::Interval => match v {
            Value::Time(us) => Ok(Value::Interval {
                months: 0,
                days: 0,
                micros: us,
            }),
            other => cast_to_interval(other),
        },
        // v7.9.25 — `::json` keeps the input text verbatim (PG's json
        // type preserves whitespace / key order / duplicates).
        CastTarget::Json => match v {
            Value::Json(s) => Ok(Value::json(s)),
            Value::Text(s) => Ok(Value::json(s)),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::json only accepts TEXT-shape inputs, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            }),
        },
        // v7.38 (read01) — `::jsonb` canonicalises like PG: object keys
        // sorted (length, then bytes) + duplicates collapsed last-wins,
        // `, ` / `: ` whitespace, and numbers normalised. Invalid JSON
        // falls back to the verbatim text (validation stays a separate
        // concern from this representation fix).
        CastTarget::Jsonb => match v {
            // v7.39 (read01 jsonb) — the explicit ::jsonb cast validates:
            // invalid tokens (NaN / Infinity / malformed) error like PG
            // instead of passing the raw text through.
            Value::Json(s) | Value::Text(s) => match crate::json::canonicalize_jsonb(s.as_ref()) {
                Ok(c) => Ok(Value::json(c)),
                Err(_) => Err(EvalError::TypeMismatch {
                    detail: alloc::string::String::from("invalid input syntax for type json"),
                }),
            },
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::jsonb only accepts TEXT-shape inputs, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    && let Some(name) = crate::conversions::regtype_oid_to_name_owned(n)
                {
                    Ok(Value::text(name))
                } else {
                    Ok(Value::text(alloc::format!("{n}")))
                }
            }
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::regtype / ::regclass accepts TEXT (name) or integer (oid), got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            }),
        },
        // v7.10.11 — `::TEXT[]`. Decode PG external array form
        // when input is Text; pass through unchanged when it is
        // already TextArray. Anything else is a type mismatch.
        CastTarget::TextArray => match v {
            Value::TextArray(items) => Ok(Value::TextArray(items)),
            Value::Text(s) => {
                if let Some(r) = try_cast_2d_array(&s, |row| {
                    decode_text_array_external(row).map(Value::TextArray)
                }) {
                    return r;
                }
                decode_text_array_external(&s).map(Value::TextArray)
            }
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
                    "::TEXT[] only accepts TEXT / array inputs, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "::tsvector only accepts TEXT / tsvector inputs, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            }),
        },
        CastTarget::TsQuery => match v {
            Value::TsQuery(ast) => Ok(Value::TsQuery(ast)),
            Value::Text(s) => decode_tsquery_external(&s).map(Value::TsQuery),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::tsquery only accepts TEXT / tsquery inputs, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "::uuid only accepts TEXT / uuid inputs, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
            // v7.39 (round 544) — an integer's two's-complement bytes,
            // big-endian, at the source type's width. Measured on PG18:
            // 5::int2 -> \x0005, 5::int4 -> \x00000005,
            // 5::int8 -> \x0000000000000005, (-1)::int4 -> \xffffffff.
            Value::SmallInt(n) => Ok(Value::bytes(n.to_be_bytes().to_vec())),
            Value::Int(n) => Ok(Value::bytes(n.to_be_bytes().to_vec())),
            Value::BigInt(n) => Ok(Value::bytes(n.to_be_bytes().to_vec())),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::bytea only accepts TEXT / bytea / integer inputs, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            }),
        },
        CastTarget::Named(name) => {
            // v7.39 (round 613) — a plain scalar spelling goes straight to
            // the tail. See `PLAIN_NAMED_TARGETS` for why that is the same
            // thing as walking the arm, and the pin for the check that says
            // so mechanically.
            if let Some(dt) = plain_named_target(name) {
                return finish_named_cast(v, dt, name, None, mysql);
            }
            // v7.38 (read01) — a temporal type with a fractional-seconds
            // precision (`time(3)`, `timestamp(0)`, `timestamptz(2)`) rounds the
            // sub-second field to that many digits, like PG. Resolve against the
            // base type (`type_name_to_data_type` does not know the `(N)` form)
            // and round the coerced result below.
            // v7.38 (read01, T20) — an integer casts to `bit(n)` as the low n
            // bits of its two's-complement representation (PG; int→varbit is
            // rejected there, so only fixed-length `bit` is handled here).
            if matches!(v, Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_)) {
                if let Some(width) = bit_cast_width(name) {
                    return int_to_bit_string(v, width.0);
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
                            detail: alloc::format!("invalid input syntax for type bit: \"{s}\""),
                        }),
                    },
                    Value::BitString { .. } => Ok(v),
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!("cannot cast {} to bit", crate::conversions::pg_type_name_for_error_opt(other.data_type())),
                    }),
                };
            }
            // v7.39 (read01 varbit.c) — `bit(n)` over a bit string (or a
            // '0101' text form) zero-extends on the RIGHT or truncates to
            // n (PG's bit() cast, unlike the input-time exact-length rule).
            let bit_src: Option<Value<'static>> = match &v {
                Value::BitString { .. } => Some(v.clone()),
                Value::Text(s) if bit_cast_width(name).is_some() => {
                    match crate::conversions::parse_bit_string_text(s) {
                        Some((nb, by)) => Some(Value::bit_string(nb, by)),
                        None => {
                            let bad = s.chars().find(|c| *c != '0' && *c != '1');
                            return Err(EvalError::TypeMismatch {
                                detail: match bad {
                                    Some(c) => {
                                        alloc::format!("\"{c}\" is not a valid binary digit")
                                    }
                                    None => {
                                        alloc::format!("invalid input syntax for type bit: \"{s}\"")
                                    }
                                },
                            });
                        }
                    }
                }
                _ => None,
            };
            if let Some(Value::BitString { nbits, bytes }) = &bit_src {
                if let Some((width, pads)) = bit_cast_width(name) {
                    // varbit truncates but never pads.
                    if !pads && *nbits <= width {
                        return Ok(Value::BitString {
                            nbits: *nbits,
                            bytes: alloc::borrow::Cow::Owned(bytes.to_vec()),
                        });
                    }
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
            // v7.39 (round 607) — matched against the static list rather than
            // through an owned lowercase copy. The copy was built for every
            // row and thrown away on every row that is not one of these.
            if let Some(lower_name) = REG_MISC_TYPES
                .iter()
                .copied()
                .find(|k| name.eq_ignore_ascii_case(k))
            {
                let s = match &v {
                    Value::Null => return Ok(Value::Null),
                    Value::Text(s) => s.as_ref().trim().to_string(),
                    // v7.39 (round 634) — an OID reaches these types too.
                    // PG registers int2/int4/int8/oid -> regproc as IMPLICIT
                    // casts and renders an oid with no matching entry as the
                    // number itself: `1::INT::REGPROC` is `1`, and
                    // `1247::OID::REGPROC` is `1247`. SPG refused the whole
                    // integer family with "accepts TEXT".
                    Value::SmallInt(n) => return Ok(Value::text(n.to_string())),
                    Value::Int(n) => return Ok(Value::text(n.to_string())),
                    Value::BigInt(n) => return Ok(Value::text(n.to_string())),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "::{lower_name} accepts TEXT, got {}",
                                crate::conversions::pg_type_name_for_error_opt(other.data_type())
                            ),
                        });
                    }
                };
                return cast_reg_misc(lower_name, &s);
            }
            // v7.39 (round 514) — the remaining catalog-shaped types. Each
            // validates its own text form and keeps it, which is what PG's
            // input functions do; the wordings below are PG18 readings.
            if let Some(out) = cast_catalog_scalar(name, &v)? {
                return Ok(out);
            }
            // v7.39 (round 511) — `'(0,1)'::tid`, so a caller can name a row
            // it read a ctid from earlier. PG's text form is the only input
            // shape it has.
            if name.eq_ignore_ascii_case("tid") {
                return match &v {
                    Value::Tid(..) => Ok(v),
                    Value::Text(t) => parse_tid_text(t).ok_or_else(|| EvalError::TypeMismatch {
                        detail: alloc::format!("invalid input syntax for type tid: \"{t}\""),
                    }),
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "cannot cast type {} to tid",
                            crate::eval::strings::pg_typeof_name(other)
                        ),
                    }),
                };
            }
            // v7.39 (read01 pseudotypes.c) — casting a value INTO a
            // pseudotype hits PG's dummy input functions (0A000).
            if let Some(lower) = OPAQUE_TYPES
                .iter()
                .copied()
                .find(|k| name.eq_ignore_ascii_case(k))
            {
                // v7.39 (round 509) — a pseudotype is a REAL type name,
                // so `NULL::anyarray` is NULL on PG, not an error. Only a
                // VALUE hits the dummy input function. Before this the
                // NULL case fell through to the type table below, which
                // does not carry the pseudotypes, and once NULL stopped
                // short-circuiting the whole cast it started reporting
                // them as unknown types.
                return if matches!(v, Value::Null) {
                    Ok(Value::Null)
                } else {
                    Err(EvalError::TypeMismatch {
                        detail: alloc::format!("cannot accept a value of type {lower}"),
                    })
                };
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
                            detail: alloc::format!("cannot cast {} to {name}", crate::conversions::pg_type_name_for_error_opt(other.data_type())),
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
                    Value::Text(s) => Ok(Value::text(crate::json::jsonpath_canonical(s.as_ref())?)),
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!("cannot cast {} to jsonpath", crate::conversions::pg_type_name_for_error_opt(other.data_type())),
                    }),
                };
            }
            // v7.39 (round 355, M13) — MySQL's `BINARY` / `BINARY(n)`.
            // It is a COLLATION coercion, not a type change: MariaDB
            // renders `BINARY 'abc'` as `abc` (and `HEX()` of it as
            // 616263), so the value passes through unchanged; `(n)`
            // truncates to n bytes (`CAST('abc' AS BINARY(2))` is `ab`,
            // measured). What it really buys is byte-wise comparison,
            // which `compare_is_case_insensitive` now refuses to fold.
            if mysql && (name.eq_ignore_ascii_case("binary") || name.to_ascii_lowercase().starts_with("binary("))
            {
                return cast_mysql_binary(v, name);
            }
            // v7.39 (round 352, M8) — MySQL's SIGNED / UNSIGNED targets.
            // Measured on MariaDB 11: a string gives its LEADING number
            // (`'12abc'` → 12, `'abc'` → 0); a fractional value ROUNDS
            // half-away-from-zero (1.5 → 2, 2.5 → 3, -2.5 → -3) rather
            // than truncating; and UNSIGNED wraps a negative through u64
            // (`-1` → 18446744073709551615).
            // PG has no such type — `type "signed" does not exist` — so the
            // reading is gated on the dialect, not just on the spelling.
            if mysql && (name.eq_ignore_ascii_case("signed") || name.eq_ignore_ascii_case("unsigned"))
            {
                return cast_mysql_integer(v, name.eq_ignore_ascii_case("unsigned"));
            }
            // v7.39 (round 423) — a bare MySQL temporal type carries
            // fractional precision 0, so `CAST(x AS DATETIME)` drops the
            // fraction (measured on MariaDB 11). PG's `::timestamp` keeps
            // every microsecond, so the default is dialect-gated.
            let temporal_prec = temporal_typmod(name)
                .or_else(|| (mysql && is_bare_temporal_type(name)).then_some(0));
            let resolve_name: alloc::borrow::Cow<'_, str> = if temporal_prec.is_some() {
                alloc::borrow::Cow::Owned(
                    name.split('(').next().unwrap_or(name).trim().to_string(),
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
                    // v7.39 (round 272) — a numeric typmod outside PG's
                    // bounds gets PG's own wording rather than being
                    // reported as an unknown type.
                    // v7.39 (round 620) — and an unknown one is PG's
                    // wording, which also earns it PG's SQLSTATE (42704
                    // UNDEFINED_OBJECT; `unsupported cast target` fell
                    // through to the generic 42000).
                    EvalError::TypeMismatch {
                        detail: crate::conversions::numeric_typmod_error(&resolve_name)
                            .unwrap_or_else(|| unknown_type_error_text(name)),
                    }
                })?;
            finish_named_cast(v, dt, &resolve_name, temporal_prec, mysql)
        }
    }
}

/// v7.39 (round 613) — the tail of the `Named` arm: stringify for the text
/// targets, coerce, and round a temporal precision. Split out so the fast
/// path below reaches exactly this code rather than a copy of it.
fn finish_named_cast(
    v: Value<'static>,
    dt: spg_storage::DataType,
    resolve_name: &str,
    temporal_prec: Option<u8>,
    mysql: bool,
) -> Result<Value<'static>, EvalError> {
    // PG semantics: any value casts to varchar(n) / char(n) through its text
    // representation (`99::char(2)` → '99'), and an EXPLICIT cast truncates
    // to n characters — only column assignment errors on overflow. Stringify
    // a non-text source first, then truncate up front so the coerce path's
    // length contract never fires here.
    let v = match (&dt, v) {
        // v7.38 (read01) — an explicit cast to TEXT stringifies any
        // value (`text(42)` → '42'), matching `42::text`. (coerce_value
        // deliberately rejects a bare INT→TEXT so INSERT stays strict.)
        (spg_storage::DataType::Text, v) => match v {
            Value::Text(s) => Value::Text(s),
            // v7.39 (read01 inet family) — inet/cidr ::text carries
            // the mask even at full length (cast-path form).
            Value::Inet { family, bits, addr } | Value::Cidr { family, bits, addr } => {
                Value::text(crate::conversions::format_inet_full(family, bits, &addr))
            }
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
        crate::conversions::coerce_value(v, dt, resolve_name, 0).map_err(|e| match e {
            // v7.39 (read01 round 113) — pass an already-classed engine
            // error through unchanged. Re-stringifying via Display would
            // double the "eval: type mismatch: " class prefix (the wire
            // strips only the outermost one), leaking it into the message
            // — visible now that jsonb → numeric casts error with PG's
            // exact "cannot cast jsonb string to type numeric" wording.
            crate::EngineError::Eval(ev) => ev,
            // v7.39 (round 622, S05a) — `coerce_value` is the INSERT-time
            // COLUMN coercion, and a cast borrows it. Its rejection is
            // phrased for a column, so `SELECT 1::INET` answered
            //
            //   type mismatch in column "inet" (position 0): expected INET,
            //   got INT
            //
            // naming a column that does not exist, at a position that means
            // nothing, in the storage layer's own vocabulary. PG says
            // `cannot cast type integer to inet`. The column phrasing stays
            // where it belongs — an INSERT still says which column — and a
            // failed cast now says what it failed to cast, like every other
            // arm in this file already did.
            //
            // The two type names come off the error itself, which already
            // carries them as `DataType`. Naming them BEFORE the call — the
            // obvious way to write this, since the value and the target both
            // move into it — costs two `String`s on every SUCCESSFUL cast,
            // and the panel caught exactly that: `id::NUMERIC` 23.75 ->
            // 55.48 ms, `id::REAL` 21.55 -> 50.48. This is the same eager
            // error construction round 614 removed from 28 call sites,
            // rebuilt by hand a round later.
            crate::EngineError::Storage(spg_storage::StorageError::TypeMismatch {
                expected,
                actual,
                ..
            }) => EvalError::TypeMismatch {
                detail: alloc::format!(
                    "cannot cast {} to {}",
                    crate::conversions::pg_type_name_for_error(actual),
                    crate::conversions::pg_type_name_for_error(expected)
                ),
            },
            other => EvalError::TypeMismatch {
                detail: alloc::format!("{other}"),
            },
        })?;
    Ok(match temporal_prec {
        Some(prec) => round_temporal_to_precision(coerced, prec, mysql),
        None => coerced,
    })
}

/// v7.39 (round 613) — the plain scalar spellings, with the type each one
/// resolves to.
///
/// Round 612 measured the `Named` arm re-deriving everything for every row:
/// `s::VARCHAR` cost 30.6 ms over 200k rows where `s::TEXT` — the identical
/// conversion, under a spelling the parser settles into a `CastTarget`
/// variant — cost 11.2, and probes split the difference across the whole arm
/// rather than any one place in it. These names reach the tail directly.
///
/// Both halves of that shortcut are checked mechanically by the pin, not by
/// eye: every entry's type is asserted to equal `type_name_to_data_type`'s
/// answer, and every entry is asserted absent from each arm above the
/// resolve (the reg-misc / catalog-scalar / opaque lists, `tid`, `xid`,
/// `xid8`, `jsonpath`, the MySQL `binary` / `signed` / `unsigned` names, and
/// the bit and temporal spellings). A name that grows a special case has to
/// leave this table, and the pin says so.
const PLAIN_NAMED_TARGETS: &[(&str, spg_storage::DataType)] = &[
    ("text", spg_storage::DataType::Text),
    ("varchar", spg_storage::DataType::Varchar(0)),
    ("character varying", spg_storage::DataType::Varchar(0)),
    (
        "numeric",
        spg_storage::DataType::Numeric {
            precision: 0,
            scale: 0,
        },
    ),
    (
        "decimal",
        spg_storage::DataType::Numeric {
            precision: 0,
            scale: 0,
        },
    ),
    ("real", spg_storage::DataType::Real),
    ("float4", spg_storage::DataType::Real),
    ("float8", spg_storage::DataType::Float),
    ("double precision", spg_storage::DataType::Float),
    ("int2", spg_storage::DataType::SmallInt),
    ("smallint", spg_storage::DataType::SmallInt),
    ("int4", spg_storage::DataType::Int),
    ("integer", spg_storage::DataType::Int),
    ("int8", spg_storage::DataType::BigInt),
    ("bool", spg_storage::DataType::Bool),
    ("boolean", spg_storage::DataType::Bool),
    ("date", spg_storage::DataType::Date),
    ("bytea", spg_storage::DataType::Bytes),
    ("uuid", spg_storage::DataType::Uuid),
];

/// v7.39 (round 613) — the heads that may carry a typmod and are still
/// plain: `varchar(20)`, `char(4)`, `numeric(10,2)`. The type comes from
/// `type_name_to_data_type` over the WHOLE name, so the typmod is parsed
/// exactly where it always was; only the walk down the arm is skipped. The
/// pin checks each head against every arm above the resolve, and that none
/// of them is a bit or temporal spelling.
pub(crate) const PLAIN_NAMED_HEADS: &[&str] = &[
    "varchar",
    "character varying",
    "char",
    "character",
    "bpchar",
    "numeric",
    "decimal",
];

/// The type a plain scalar spelling resolves to, or `None` when the name
/// needs the whole arm.
pub(crate) fn plain_named_target(name: &str) -> Option<spg_storage::DataType> {
    if let Some(dt) = PLAIN_NAMED_TARGETS
        .iter()
        .find(|(k, _)| name.eq_ignore_ascii_case(k))
        .map(|(_, dt)| *dt)
    {
        return Some(dt);
    }
    let head = name.split('(').next()?.trim();
    if name.len() == head.len()
        || !PLAIN_NAMED_HEADS
            .iter()
            .any(|k| head.eq_ignore_ascii_case(k))
    {
        return None;
    }
    crate::conversions::type_name_to_data_type(name)
}

/// The scalar type names the `Named` arm resolves without a catalog.
fn is_known_scalar_name(lower: &str) -> bool {
    REG_MISC_TYPES.contains(&lower)
        || CATALOG_SCALAR_TYPES.contains(&lower)
        || OPAQUE_TYPES.contains(&lower)
        || matches!(lower, "tid" | "record" | "cstring" | "regnamespace" | "regrole")
}

/// v7.39 (round 509) — does this name a type at all?
///
/// PG validates the cast TARGET whatever the operand is: `NULL::nosuchtype`
/// is an error there, not NULL. `cast_value_in` short-circuits a NULL before
/// it ever looks at the target, so the check has to happen in the caller —
/// and `eval_cast_arm` is the caller that has a catalog, which is what
/// enums, domains, composites and table row types need.
///
/// This lists what the `Named` arm below resolves WITHOUT a catalog. Keeping
/// the two in step is a real hazard: a first cut of this check missed three
/// live spellings — `::binary` (the MySQL prefix's desugar), a table's row
/// type, and the pseudotypes — and the e2e suite caught every one. It is the
/// check on this function.
/// v7.39 (round 620) — PG's wording for a cast target that names no type.
///
/// SPG said ``unsupported cast target `::nosuchtype` ``, which reads as "SPG
/// has not got round to that one" when what happened is that no such type
/// exists anywhere. PG says `type "nosuchtype" does not exist`, and because
/// the wire classifies by message text, saying it also moves the code off the
/// generic 42000 onto 42704 UNDEFINED_OBJECT.
pub(crate) fn unknown_type_error_text(name: &str) -> alloc::string::String {
    alloc::format!("type \"{name}\" does not exist")
}

pub(crate) fn builtin_target_resolves(name: &str, mysql: bool) -> bool {
    if name == "__bit_literal" || bit_cast_width(name).is_some() {
        return true;
    }
    crate::conversions::with_lower_name(name, |lower| {
        builtin_target_resolves_lower(name, lower, mysql)
    })
}

fn builtin_target_resolves_lower(name: &str, lower: &str, mysql: bool) -> bool {
    // The three families, read from the same declarations the value path
    // dispatches on — see their doc comment for why that matters.
    if is_known_scalar_name(lower) {
        return true;
    }
    // v7.39 (round 515) — `<element>[]`, which this parser names
    // `<element>_array`. PG has an array type for every scalar, so the rule
    // is the stem's: `NULL::cstring[]`, `NULL::aclitem[]` and
    // `NULL::"char"[]` all resolve there. A general rule rather than three
    // entries, because the next scalar added would otherwise need a fourth.
    if let Some(stem) = lower.strip_suffix("_array")
        && (is_known_scalar_name(stem)
            || crate::conversions::type_name_to_data_type(stem).is_some())
    {
        return true;
    }
    if mysql && matches!(lower, "binary" | "signed" | "unsigned") {
        return true;
    }
    let base = if temporal_typmod(name).is_some() || (mysql && is_bare_temporal_type(name)) {
        name.split('(').next().unwrap_or(name).trim()
    } else {
        name
    };
    crate::conversions::type_name_to_data_type(base).is_some()
        || crate::conversions::numeric_typmod_error(base).is_some()
}

/// v7.39 (round 511) — PG's `(block,offset)` text form for a tid.
fn parse_tid_text(t: &str) -> Option<Value<'static>> {
    let inner = t.trim().strip_prefix('(')?.strip_suffix(')')?;
    let (b, o) = inner.split_once(',')?;
    Some(Value::Tid(
        b.trim().parse::<u32>().ok()?,
        o.trim().parse::<u32>().ok()?,
    ))
}

/// v7.39 (round 514) — the catalog-shaped scalar types: the ids, the oid
/// vectors, an ACL item, a cursor name and a transaction snapshot.
///
/// `Some` when `name` is one of them, so the caller can fall through to
/// everything else. Every error wording is a PG18 reading — they differ per
/// type and per ELEMENT (`::oidvector` complains about `oid`,
/// `::int2vector` about `smallint`), which is why they are spelled out
/// rather than shared.
fn cast_catalog_scalar(
    name: &str,
    v: &Value<'_>,
) -> Result<Option<Value<'static>>, EvalError> {
    crate::conversions::with_lower_name(name, |lower| cast_catalog_scalar_lower(lower, v))
}

fn cast_catalog_scalar_lower(
    lower: &str,
    v: &Value<'_>,
) -> Result<Option<Value<'static>>, EvalError> {
    // v7.39 (round 515) — `<element>[]` runs the element's own check over
    // each member and keeps the literal, which is what PG does: measured,
    // `'{a,b}'::aclitem[]` is "unrecognized key word: \"a\"".
    if let Some(stem) = lower.strip_suffix("_array")
        && (CATALOG_SCALAR_TYPES.contains(&stem) || OPAQUE_TYPES.contains(&stem))
    {
        let Value::Text(t) = v else {
            return Ok(None);
        };
        let body = t.trim();
        let inner = body
            .strip_prefix('{')
            .and_then(|b| b.strip_suffix('}'))
            .unwrap_or(body);
        for part in inner.split(',').filter(|p| !p.trim().is_empty()) {
            cast_catalog_scalar(stem, &Value::text(part.trim().to_string()))?;
        }
        return Ok(Some(Value::text(body.to_string())));
    }
    if !CATALOG_SCALAR_TYPES.contains(&lower) {
        return Ok(None);
    }
    let text = match v {
        Value::Text(t) => t.to_string(),
        Value::Cid(c) if lower == "cid" => return Ok(Some(Value::Cid(*c))),
        Value::Xid(x) if lower == "xid" => return Ok(Some(Value::Xid(*x))),
        // v7.39 (round 641) — PG has no cast between an integer and a
        // transaction id in either direction: `5::xid` is "cannot cast
        // type integer to xid" and `'5'::xid::int` is the mirror of it,
        // measured. The unknown-literal spelling `'5'::xid` is a
        // different thing — that is the type's input function, and it is
        // the Text arm above. Only `xid` is carved out here; `cid`,
        // `oid` and the vector types keep taking an integer.
        Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) if lower == "xid" => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "cannot cast type {} to xid",
                    crate::eval::strings::pg_typeof_name(v)
                ),
            });
        }
        Value::SmallInt(n) => alloc::format!("{n}"),
        Value::Int(n) => alloc::format!("{n}"),
        Value::BigInt(n) => alloc::format!("{n}"),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "cannot cast type {} to {lower}",
                    crate::eval::strings::pg_typeof_name(other)
                ),
            });
        }
    };
    let t = text.trim();
    let bad = |ty: &str, what: &str| EvalError::TypeMismatch {
        detail: alloc::format!("invalid input syntax for type {ty}: \"{what}\""),
    };
    let out = match lower {
        "cid" => Value::Cid(t.parse::<u32>().map_err(|_| bad("cid", t))?),
        "xid" => Value::Xid(t.parse::<u32>().map_err(|_| bad("xid", t))?),
        // Space-separated element lists, validated element by element and
        // kept in their own spelling.
        "oidvector" | "int2vector" => {
            let elem_ty = if lower == "oidvector" { "oid" } else { "smallint" };
            for part in t.split_whitespace() {
                let ok = if elem_ty == "oid" {
                    part.parse::<u32>().is_ok()
                } else {
                    part.parse::<i16>().is_ok()
                };
                if !ok {
                    return Err(bad(elem_ty, part));
                }
            }
            Value::text(t.to_string())
        }
        // `grantee=privileges/grantor`, and PG checks the key word first:
        // anything before the `=` that is not a role name, `group` or
        // `user` is "unrecognized key word".
        "aclitem" => {
            let Some((who, rest)) = t.split_once('=') else {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("unrecognized key word: \"{t}\""),
                });
            };
            if !rest.contains('/') {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("a name must follow the \"/\" sign"),
                });
            }
            let _ = who;
            Value::text(t.to_string())
        }
        // A cursor name is just a name.
        "refcursor" => Value::text(t.to_string()),
        // `xmin:xmax:xip_list` — two numbers and a comma-separated tail.
        "pg_snapshot" | "txid_snapshot" => {
            let parts: alloc::vec::Vec<&str> = t.splitn(3, ':').collect();
            let shaped = parts.len() == 3
                && parts[0].parse::<u64>().is_ok()
                && parts[1].parse::<u64>().is_ok()
                && (parts[2].is_empty()
                    || parts[2].split(',').all(|x| x.parse::<u64>().is_ok()));
            if !shaped {
                return Err(bad(lower, t));
            }
            Value::text(t.to_string())
        }
        // PG normalises a path on input: `$.a` reads back `$."a"`. The
        // engine already has the parser its operators use.
        "jsonpath" => Value::text(crate::json::jsonpath_canonical(t)?),
        _ => unreachable!("guarded above"),
    };
    Ok(Some(out))
}

/// v7.38 (read01, T20) — width of a `bit` cast target: bare `bit` is `bit(1)`,
/// `bit(N)` is N. `None` for `varbit` / `bit varying` (PG rejects int→varbit) and
/// any non-bit name.
fn bit_cast_width(name: &str) -> Option<(u32, bool)> {
    crate::conversions::with_lower_name(name, bit_cast_width_lower)
}

fn bit_cast_width_lower(lower: &str) -> Option<(u32, bool)> {
    let trimmed = lower.trim();
    if trimmed == "bit" {
        return Some((1, true));
    }
    // v7.39 (round 281) — `varbit(n)` / `bit varying(n)` adjust on an
    // explicit cast too, but only DOWN: PG truncates a too-long value
    // and leaves a shorter one alone, where `bit(n)` also pads.
    for (prefix, pads) in [("varbit", false), ("bit varying", false), ("bit", true)] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let rest = rest.trim_start();
            if let Some(inner) = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')'))
                && let Ok(n) = inner.trim().parse::<u32>()
            {
                return Some((n, pads));
            }
        }
    }
    None
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
    crate::conversions::with_lower_name(name, |lower| {
        let (base, rest) = lower.split_once('(')?;
        if !matches!(
            base.trim(),
            "time" | "timetz" | "timestamp" | "timestamptz" | "datetime"
        ) {
            return None;
        }
        let digits = rest.trim_start();
        let end = digits
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(digits.len());
        digits[..end].parse::<u8>().ok()
    })
}

/// Round a TIME / TIMESTAMP value's microsecond field to `prec` fractional-
/// second digits (`prec` 0..=6), half-away-from-zero as PG's AdjustTimestamp.
/// v7.39 (round 423) — `truncate` selects MySQL's reduction mode. PG's
/// AdjustTimestamp ROUNDS half-away-from-zero (`::timestamp(1)` of `.256` is
/// `.3`); MariaDB TRUNCATES toward zero (`.2`, measured). Same function, one
/// flag, because everything else about the reduction is identical.
fn round_temporal_to_precision(v: Value<'static>, prec: u8, truncate: bool) -> Value<'static> {
    if prec >= 6 {
        return v;
    }
    let scale = 10i64.pow(u32::from(6 - prec));
    let reduce = |micros: i64| -> i64 {
        if truncate {
            // Toward zero, so a negative time-of-day loses the same digits.
            (micros / scale) * scale
        } else {
            let half = scale / 2;
            if micros >= 0 {
                ((micros + half) / scale) * scale
            } else {
                -(((-micros + half) / scale) * scale)
            }
        }
    };
    match v {
        Value::Timestamp(m) => Value::Timestamp(reduce(m)),
        Value::Time(m) => Value::Time(reduce(m)),
        other => other,
    }
}

/// v7.39 (round 423) — is `name` a bare temporal type (no `(N)` modifier)?
/// MySQL gives those fractional precision ZERO — `CAST(x AS DATETIME)` drops
/// the fraction entirely — where PG's `::timestamp` keeps full microseconds.
fn is_bare_temporal_type(name: &str) -> bool {
    let t = name.trim();
    ["time", "timestamp", "datetime"]
        .iter()
        .any(|k| t.eq_ignore_ascii_case(k))
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
        Value::Text(s) => {
            if let Some(r) = try_cast_2d_array(&s, |row| {
                decode_int_array_external(row).map(Value::IntArray)
            }) {
                return r;
            }
            decode_int_array_external(&s).map(Value::IntArray)
        }
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
            detail: alloc::format!("::INT[] does not accept {}", crate::conversions::pg_type_name_for_error_opt(other.data_type())),
        }),
    }
}

fn cast_to_bigint_array(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::BigIntArray(items) => Ok(Value::BigIntArray(items)),
        Value::IntArray(items) => Ok(Value::BigIntArray(
            items.into_iter().map(|x| x.map(i64::from)).collect(),
        )),
        Value::Text(s) => {
            if let Some(r) = try_cast_2d_array(&s, |row| {
                decode_bigint_array_external(row).map(Value::BigIntArray)
            }) {
                return r;
            }
            decode_bigint_array_external(&s).map(Value::BigIntArray)
        }
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
            detail: alloc::format!("::BIGINT[] does not accept {}", crate::conversions::pg_type_name_for_error_opt(other.data_type())),
        }),
    }
}

/// Cast a possibly-2-D array literal: parse each top-level row with `elem` (the
/// 1-D element decoder) and fold into a 2-D value; `None` when the literal is 1-D.
fn try_cast_2d_array(
    s: &str,
    elem: impl Fn(&str) -> Result<Value<'static>, EvalError>,
) -> Option<Result<Value<'static>, EvalError>> {
    let rows = crate::eval::values::split_2d_rows(s)?;
    let mut row_vals: alloc::vec::Vec<Value<'static>> = alloc::vec::Vec::with_capacity(rows.len());
    for r in &rows {
        match elem(r) {
            Ok(v) => row_vals.push(v),
            Err(e) => return Some(Err(e)),
        }
    }
    Some(
        crate::eval::values::build_2d_from_rows(&row_vals).ok_or_else(|| EvalError::TypeMismatch {
            detail: crate::conversions::malformed_array_literal(s),
        }),
    )
}

fn decode_int_array_external(s: &str) -> Result<Vec<Option<i32>>, EvalError> {
    let trimmed = s.trim();
    // v7.39 (read01 jsonfuncs.c) — the json_to_record/populate desugar
    // routes JSON array text ("[1,2]") through this cast; accept the
    // bracket form alongside PG's brace form.
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .or_else(|| trimmed.strip_prefix('[').and_then(|x| x.strip_suffix(']')))
        .ok_or_else(|| EvalError::TypeMismatch {
            detail: crate::conversions::malformed_array_literal(s),
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
                        detail: alloc::format!("invalid input syntax for type integer: {p:?}"),
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
        .or_else(|| trimmed.strip_prefix('[').and_then(|x| x.strip_suffix(']')))
        .ok_or_else(|| EvalError::TypeMismatch {
            // v7.39 (round 325) — was "BIGmalformed array literal", a
            // stray edit that shipped: the message a client saw for
            // `'abc'::bigint[]` began with three letters of BIGINT.
            detail: crate::conversions::malformed_array_literal(s),
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
                        detail: alloc::format!("invalid input syntax for type bigint: {p:?}"),
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
        .or_else(|| trimmed.strip_prefix('[').and_then(|x| x.strip_suffix(']')))
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
                        // v7.39 (round 324, V42) — PG's wording.
                        detail: alloc::format!("invalid input syntax for type interval: \"{s}\""),
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
                "::INTERVAL only accepts TEXT-shape inputs, got {}",
                crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
            detail: format!("cannot cast {} to DATE", crate::conversions::pg_type_name_for_error_opt(other.data_type())),
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
        Value::Date(d) => Ok(Value::Timestamp(crate::conversions::date_days_to_micros(d))),
        Value::Text(s) => {
            // v7.39 (round 289) — the target has no zone, so PG ignores
            // any the literal carries: `'…+02'::timestamp` keeps the
            // wall clock rather than converting to UTC.
            crate::eval::format::parse_timestamp_literal_wall_ordered(
                &s,
                crate::eval::format::DateOrder::Mdy,
            )
            .map(Value::Timestamp)
            .ok_or_else(|| EvalError::TypeMismatch {
                // v7.39 (round 324, V42) — PG's wording, and PG's split
                // between "invalid input syntax" and "date/time field
                // value out of range".
                detail: crate::eval::format::datetime_input_error_text(&s, "timestamp"),
            })
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {} to TIMESTAMP", crate::conversions::pg_type_name_for_error_opt(other.data_type())),
        }),
    }
}

/// v7.39 (round 310) — `::timestamptz` from text: an offset in the
/// literal is APPLIED, unlike the zone-less sibling which discards it.
/// Naive input (no offset) is read as UTC, which is what the
/// context-aware arm already assumed when it fell through to here.
fn cast_to_timestamptz(v: Value) -> Result<Value, EvalError> {
    let Value::Text(s) = &v else {
        return cast_to_timestamp(v);
    };
    crate::eval::format::parse_timestamp_literal_tz_ordered(
        s,
        crate::eval::format::DateOrder::Mdy,
    )
    .map(|(micros, _had_tz)| Value::Timestamp(micros))
    .ok_or_else(|| EvalError::TypeMismatch {
        // v7.39 (round 324, V42) — and with the RIGHT type name: this arm
        // used to report `TIMESTAMP` for a `::timestamptz` cast.
        detail: crate::eval::format::datetime_input_error_text(s, "timestamp with time zone"),
    })
}

/// v7.39 (round 254) — PG refuses to cast a NUMERIC special into any
/// integer type: `cannot convert NaN to integer` / `cannot convert
/// infinity to bigint` (an infinity is named without its sign, probed
/// live). Returns `None` for an ordinary value so the caller runs its
/// normal conversion.
fn cast_numeric_special_reject(v: &Value, target: &str) -> Option<Result<Value<'static>, EvalError>> {
    let Value::Numeric { kind, .. } = v else {
        return None;
    };
    if *kind == spg_storage::NumericKind::Finite {
        return None;
    }
    let what = if *kind == spg_storage::NumericKind::NaN {
        "NaN"
    } else {
        "infinity"
    };
    Some(Err(EvalError::TypeMismatch {
        detail: alloc::format!("cannot convert {what} to {target}"),
    }))
}

fn cast_numeric_to_int(v: Value) -> Result<Value, EvalError> {
    match v {
        // v7.39 (round 633) — SMALLINT. `1::SMALLINT::INT` answered
        // "cannot cast smallint to int": the arm was simply absent, next to
        // the Int and BigInt ones. Widening a smallint is about as ordinary
        // as a cast gets, and PG has it registered as an IMPLICIT cast.
        // Same omission shape as the sum accumulator missing SmallInt in
        // round 626 — a variant list written out by hand, one entry short.
        Value::SmallInt(n) => Ok(Value::Int(i32::from(n))),
        Value::Int(n) => Ok(Value::Int(n)),
        Value::BigInt(n) => i32::try_from(n)
            .map(Value::Int)
            // v7.39 (read01 round 79) — PG's wording, which the Float arm two
            // arms down was already using: "integer out of range". Drivers match
            // on it. Three arms of one function had two different messages.
            .map_err(|_| EvalError::TypeMismatch {
                detail: "integer out of range".into(),
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
        // v7.39 (read01 round 112) — `real` (float4) rounds/range-checks the
        // same way float8 does; only the float8 arm existed.
        #[allow(clippy::cast_possible_truncation)]
        Value::Real(x) => {
            let r = f64_round_half_even(f64::from(x));
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
                    detail: "integer out of range".into(),
                })
        }
        Value::Text(s) => crate::conversions::parse_pg_int(&s)
            .and_then(|n| i32::try_from(n).ok())
            .map(Value::Int)
            .ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("invalid input syntax for type integer: {s:?}"),
            }),
        Value::Bool(b) => Ok(Value::Int(i32::from(b))),
        // v7.39 (read01 char.c) — ("char")::int is the byte value.
        Value::Char1(b) => Ok(Value::Int(i32::from(b))),
        // PG `bit`/`varbit` → int is the MSB-first bit value.
        #[allow(clippy::cast_possible_truncation)]
        Value::BitString { nbits, bytes } => Ok(Value::Int(crate::conversions::bit_string_to_i64(
            nbits, &bytes,
        ) as i32)),
        // v7.39 (read01 round 113) — jsonb → int: decode the JSON scalar, then
        // round via the numeric arm above. String/array/object/boolean error.
        Value::Json(s) => match crate::conversions::jsonb_scalar_for_cast(&s, "integer")? {
            crate::conversions::JsonbScalar::Numeric(n) => cast_numeric_to_int(n),
            crate::conversions::JsonbScalar::Bool(_) => Err(
                crate::conversions::jsonb_cast_type_error("boolean", "integer"),
            ),
            crate::conversions::JsonbScalar::Null => Ok(Value::Null),
        },
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {} to int", crate::conversions::pg_type_name_for_error_opt(other.data_type())),
        }),
    }
}

fn cast_numeric_to_bigint(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Int(n) => Ok(Value::BigInt(i64::from(n))),
        // v7.39 (round 633) — SMALLINT, missing here for the same reason.
        Value::SmallInt(n) => Ok(Value::BigInt(i64::from(n))),
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
        // v7.39 (read01 round 112) — `real` (float4) → bigint, matching float8.
        #[allow(clippy::cast_possible_truncation)]
        Value::Real(x) => {
            let r = f64_round_half_even(f64::from(x));
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
                // v7.39 (round 324, V42) — PG's wording.
                detail: format!("invalid input syntax for type bigint: \"{s}\""),
            }),
        Value::Bool(b) => Ok(Value::BigInt(i64::from(b))),
        // PG `bit`/`varbit` → bigint is the MSB-first bit value.
        Value::BitString { nbits, bytes } => Ok(Value::BigInt(
            crate::conversions::bit_string_to_i64(nbits, &bytes),
        )),
        // v7.39 (read01 round 113) — jsonb → bigint.
        Value::Json(s) => match crate::conversions::jsonb_scalar_for_cast(&s, "bigint")? {
            crate::conversions::JsonbScalar::Numeric(n) => cast_numeric_to_bigint(n),
            crate::conversions::JsonbScalar::Bool(_) => Err(
                crate::conversions::jsonb_cast_type_error("boolean", "bigint"),
            ),
            crate::conversions::JsonbScalar::Null => Ok(Value::Null),
        },
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {} to bigint", crate::conversions::pg_type_name_for_error_opt(other.data_type())),
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
        // v7.39 (round 254) — a special crosses to its IEEE twin.
        Value::Numeric { kind, .. } if kind != spg_storage::NumericKind::Finite => {
            Ok(Value::Float(match kind {
                spg_storage::NumericKind::NaN => f64::NAN,
                spg_storage::NumericKind::PosInf => f64::INFINITY,
                _ => f64::NEG_INFINITY,
            }))
        }
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
        // v7.39 (read01 round 113) — jsonb → double precision.
        Value::Json(s) => {
            match crate::conversions::jsonb_scalar_for_cast(&s, "double precision")? {
                crate::conversions::JsonbScalar::Numeric(n) => cast_numeric_to_float(n),
                crate::conversions::JsonbScalar::Bool(_) => Err(
                    crate::conversions::jsonb_cast_type_error("boolean", "double precision"),
                ),
                crate::conversions::JsonbScalar::Null => Ok(Value::Null),
            }
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {} to float", crate::conversions::pg_type_name_for_error_opt(other.data_type())),
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
        // v7.39 (read01 round 113) — jsonb → boolean accepts only JSON
        // true/false; a JSON number/string/array/object errors.
        Value::Json(s) => match crate::conversions::jsonb_scalar_for_cast(&s, "boolean")? {
            crate::conversions::JsonbScalar::Bool(b) => Ok(Value::Bool(b)),
            crate::conversions::JsonbScalar::Numeric(_) => Err(
                crate::conversions::jsonb_cast_type_error("numeric", "boolean"),
            ),
            crate::conversions::JsonbScalar::Null => Ok(Value::Null),
        },
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {} to bool", crate::conversions::pg_type_name_for_error_opt(other.data_type())),
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
            .ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("cannot parse {s:?} as a vector literal"),
            }),
        other => Err(EvalError::TypeMismatch {
            detail: format!("::vector requires text input, got {}", crate::conversions::pg_type_name_for_error_opt(other.data_type())),
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

#[cfg(test)]
mod round613_plain_named_targets {
    use super::*;

    /// v7.39 (round 613) — the shortcut is only equivalent to walking the
    /// arm while these hold. Checked here rather than by eye, so a name that
    /// grows a special case above the resolve fails the gate instead of
    /// silently taking the wrong path.
    fn assert_no_arm_above_the_resolve_claims(name: &str) {
        assert!(
            !REG_MISC_TYPES.iter().any(|k| name.eq_ignore_ascii_case(k)),
            "{name} is a reg-misc type"
        );
        assert!(
            !CATALOG_SCALAR_TYPES
                .iter()
                .any(|k| name.eq_ignore_ascii_case(k)),
            "{name} is a catalog scalar"
        );
        assert!(
            !OPAQUE_TYPES.iter().any(|k| name.eq_ignore_ascii_case(k)),
            "{name} is a pseudotype"
        );
        for special in [
            "__bit_literal",
            "tid",
            "xid",
            "xid8",
            "jsonpath",
            "binary",
            "signed",
            "unsigned",
        ] {
            assert!(
                !name.eq_ignore_ascii_case(special),
                "{name} has its own arm ({special})"
            );
        }
        assert!(bit_cast_width(name).is_none(), "{name} is a bit spelling");
        assert!(
            temporal_typmod(name).is_none(),
            "{name} carries a temporal precision"
        );
        assert!(
            !is_bare_temporal_type(name),
            "{name} is a bare temporal type"
        );
        assert!(
            matches!(
                cast_catalog_scalar(name, &Value::text("x")),
                Ok(None)
            ),
            "{name} is claimed by the catalog-scalar arm"
        );
    }

    #[test]
    fn every_plain_target_resolves_to_the_type_the_table_claims() {
        for (name, dt) in PLAIN_NAMED_TARGETS {
            assert_eq!(
                crate::conversions::type_name_to_data_type(name),
                Some(*dt),
                "{name} does not resolve to the type the table gives it"
            );
            assert_eq!(plain_named_target(name), Some(*dt));
            // The spelling is matched without regard to case.
            assert_eq!(plain_named_target(&name.to_uppercase()), Some(*dt));
            assert_no_arm_above_the_resolve_claims(name);
        }
    }

    #[test]
    fn every_typmod_head_is_plain_and_resolves_through_the_type_table() {
        for head in PLAIN_NAMED_HEADS {
            assert_no_arm_above_the_resolve_claims(head);
            for spelled in [alloc::format!("{head}(4)"), alloc::format!("{head}(10,2)")] {
                assert_no_arm_above_the_resolve_claims(&spelled);
                assert_eq!(
                    plain_named_target(&spelled),
                    crate::conversions::type_name_to_data_type(&spelled),
                    "{spelled} takes a different type through the shortcut"
                );
            }
            // A bare head with no typmod only shortcuts when it is in the
            // exact table; the head list alone must not claim it.
            let bare = plain_named_target(head);
            let exact = PLAIN_NAMED_TARGETS
                .iter()
                .find(|(k, _)| head.eq_ignore_ascii_case(k))
                .map(|(_, dt)| *dt);
            assert_eq!(bare, exact, "{head} bare");
        }
    }

    #[test]
    fn a_name_with_its_own_arm_is_not_shortcut() {
        for name in [
            "regproc", "aclitem", "anyarray", "tid", "xid", "jsonpath", "bit", "bit(4)",
            "timestamp", "timestamp(2)", "time(3)", "nosuchtype", "int4range",
        ] {
            assert_eq!(plain_named_target(name), None, "{name} was shortcut");
        }
    }
}
