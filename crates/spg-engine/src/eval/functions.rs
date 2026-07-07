//! SQL scalar-function dispatch (`apply_function`) split out of `eval.rs`
//! (cut 34): the big lowercased-name → builtin match that `eval_expr`
//! routes `Expr::Function` through. The per-builtin implementations live
//! in the sibling `eval` submodules (strings / math / regexp / encoding /
//! textsearch / datetime / cast / inet / ...) and in `eval.rs`; this
//! module reaches all of them — plus the shared value helpers and types —
//! through a single `use super::*` (the glob keeps the dispatch table's
//! wide call surface from needing dozens of explicit imports).

use super::*;

/// Dispatch on lowercased function name. v1.4 implements only a handful of
/// scalar functions; aggregates land in v1.5 alongside GROUP BY.
/// v7.36 (perf) — Step VM entry. Caller has already lowercased
/// the function name at compile time (`Step::Function { name_lower
/// }`), so dispatch skips the per-call `to_ascii_lowercase()`
/// allocation. Equivalent to `apply_function` for any already-
/// lowercase input.
// v7.37.9 T3 S6 — args relaxed from `&[Value<'static>]` to
// `&[Value<'_>]` so the Step VM's borrow-bearing stack slice can be
// passed in directly (no per-row Vec materialise). The dispatch body
// reads args by reference and constructs the result owned, so the
// lifetime relaxation is a pure signature widening — no behaviour
// change.
pub(super) fn apply_function_lower(
    name_lower: &str,
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    apply_function_dispatch(name_lower, args, ctx)
}

/// v7.37 D.24 — does a POSIX regex pattern contain a *capturing* parenthesized
/// subexpression? `(...)` is capturing unless it opens `(?...)` (non-capturing /
/// lookaround / inline flags). An escaped `\(` is a literal paren, and `(`
/// inside a `[...]` character class is literal. Used to decide whether
/// `substring(string FROM pattern)` can be answered with the whole match
/// (no capture group) or needs first-group extraction (a regex-engine gap).
fn pattern_has_capturing_group(pat: &str) -> bool {
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    let mut in_class = false;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 2;
                continue;
            }
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' if !in_class => {
                if chars.get(i + 1) != Some(&'?') {
                    return true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

pub(super) fn apply_function(
    name: &str,
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    apply_function_dispatch(&name.to_ascii_lowercase(), args, ctx)
}

/// v7.38 (read01 P6.31) — count the nodes of a tsquery AST the way PG's
/// numnode() does: every lexeme term and every operator (AND / OR / NOT /
/// PHRASE) counts as one node.
fn count_tsquery_nodes(ast: &spg_storage::TsQueryAst) -> i32 {
    use spg_storage::TsQueryAst as Q;
    match ast {
        Q::Term { .. } => 1,
        Q::Not(inner) => 1 + count_tsquery_nodes(inner),
        Q::And(l, r) | Q::Or(l, r) => 1 + count_tsquery_nodes(l) + count_tsquery_nodes(r),
        Q::Phrase { left, right, .. } => {
            1 + count_tsquery_nodes(left) + count_tsquery_nodes(right)
        }
    }
}

/// v7.38 (read01 P6.31) — the GIN-indexable part of a tsquery, per PG's
/// querytree(): a NOT node is not indexable ("fake"); AND keeps whichever side
/// is indexable; OR is indexable only if BOTH sides are. `None` means the whole
/// query is non-indexable (PG prints "T").
fn querytree_indexable(ast: &spg_storage::TsQueryAst) -> Option<spg_storage::TsQueryAst> {
    use spg_storage::TsQueryAst as Q;
    match ast {
        Q::Term { .. } => Some(ast.clone()),
        Q::Not(_) => None,
        Q::And(l, r) => match (querytree_indexable(l), querytree_indexable(r)) {
            (Some(a), Some(b)) => Some(Q::And(alloc::boxed::Box::new(a), alloc::boxed::Box::new(b))),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        },
        Q::Or(l, r) => match (querytree_indexable(l), querytree_indexable(r)) {
            (Some(a), Some(b)) => Some(Q::Or(alloc::boxed::Box::new(a), alloc::boxed::Box::new(b))),
            _ => None,
        },
        Q::Phrase { left, right, distance } => {
            match (querytree_indexable(left), querytree_indexable(right)) {
                (Some(a), Some(b)) => Some(Q::Phrase {
                    left: alloc::boxed::Box::new(a),
                    right: alloc::boxed::Box::new(b),
                    distance: *distance,
                }),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            }
        }
    }
}

fn apply_function_dispatch(
    name: &str,
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    // v7.38 (read01 sweep) — PG resolves an unknown-type string literal in a
    // numeric function's argument to numeric (`abs('-7')`, `sqrt('16')`,
    // `round('3.567', 2)`). For the single-numeric-first-argument math
    // functions, coerce a Text arg[0] that parses as a number and re-dispatch;
    // a non-numeric string falls through to the normal type error unchanged.
    // `trunc` is intentionally excluded: PG leaves `trunc('unknown')` ambiguous
    // ("function trunc(unknown) is not unique"), so SPG lets it error too.
    const NUMERIC_ARG0_FNS: &[&str] = &[
        "abs", "sqrt", "cbrt", "sign", "ceil", "ceiling", "floor", "exp", "ln",
        "round",
    ];
    if NUMERIC_ARG0_FNS.contains(&name)
        && let Some(Value::Text(s)) = args.first()
        && let Ok(coerced) = crate::conversions::coerce_value(
            Value::text(s.as_ref()),
            spg_storage::DataType::Numeric { precision: 0, scale: 0 },
            "",
            0,
        )
    {
        let mut new_args: alloc::vec::Vec<Value> = args.to_vec();
        new_args[0] = coerced;
        return apply_function_dispatch(name, &new_args, ctx);
    }
    match name {
        // v7.38 P0 元机制 A — SQL-facing handles for the injection
        // points framework. Tests call these to attach an action
        // (`wait` / `error[:msg]` / `notice[:msg]`) to a registered
        // point, wake parked threads, or detach. Release builds
        // (feature off) reject the calls outright so a production
        // SPG can't be coerced into deadlocking.
        "spg_injection_attach" => spg_injection_attach(args),
        "spg_injection_wakeup" => spg_injection_wakeup(args),
        "spg_injection_detach" => spg_injection_detach(args),
        // v7.17.0 Phase 1.1 — SEQUENCE accessor functions.
        "nextval" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("nextval() takes 1 arg, got {}", args.len()),
                });
            }
            let seq_name = match &args[0] {
                Value::Text(s) => s.to_string(),
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "nextval() argument must be TEXT, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let resolver = ctx
                .sequence_resolver
                .ok_or_else(|| EvalError::TypeMismatch {
                    detail: "nextval() requires a sequence resolver (read-only context)".into(),
                })?;
            let v = resolver(SequenceOp::Next(seq_name))?;
            Ok(Value::BigInt(v))
        }
        "currval" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("currval() takes 1 arg, got {}", args.len()),
                });
            }
            let seq_name = match &args[0] {
                Value::Text(s) => s.to_string(),
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "currval() argument must be TEXT, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let resolver = ctx
                .sequence_resolver
                .ok_or_else(|| EvalError::TypeMismatch {
                    detail: "currval() requires a sequence resolver (read-only context)".into(),
                })?;
            let v = resolver(SequenceOp::Curr(seq_name))?;
            Ok(Value::BigInt(v))
        }
        "setval" => {
            if args.len() != 2 && args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("setval() takes 2 or 3 args, got {}", args.len()),
                });
            }
            let seq_name = match &args[0] {
                Value::Text(s) => s.to_string(),
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "setval() name argument must be TEXT, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let value = match &args[1] {
                Value::SmallInt(n) => i64::from(*n),
                Value::Int(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "setval() value argument must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let is_called = if args.len() == 3 {
                match &args[2] {
                    Value::Bool(b) => *b,
                    Value::Null => return Ok(Value::Null),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!(
                                "setval() is_called argument must be BOOL, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                }
            } else {
                true
            };
            let resolver = ctx
                .sequence_resolver
                .ok_or_else(|| EvalError::TypeMismatch {
                    detail: "setval() requires a sequence resolver (read-only context)".into(),
                })?;
            let v = resolver(SequenceOp::Set {
                name: seq_name,
                value,
                is_called,
            })?;
            Ok(Value::BigInt(v))
        }
        // v7.22 (round-13) — char_length / character_length are the
        // SQL-standard spellings PG accepts everywhere; pg_dump
        // CHECK predicates carry them verbatim.
        "length" | "char_length" | "character_length" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("length() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                // Geometric `length(lseg)` — the segment's Euclidean length.
                Value::Lseg(a, b) => {
                    let (dx, dy) = (b.x - a.x, b.y - a.y);
                    Ok(Value::Float(f64_sqrt(dx * dx + dy * dy)))
                }
                // `length(path)` — total length of the polyline; a closed path
                // adds the wrap-around segment (last -> first).
                Value::Path { points, closed } => {
                    let seg = |a: &spg_storage::Point2D, b: &spg_storage::Point2D| {
                        let (dx, dy) = (b.x - a.x, b.y - a.y);
                        f64_sqrt(dx * dx + dy * dy)
                    };
                    let mut total = 0.0;
                    for w in points.windows(2) {
                        total += seg(&w[0], &w[1]);
                    }
                    if *closed && points.len() >= 2 {
                        total += seg(&points[points.len() - 1], &points[0]);
                    }
                    Ok(Value::Float(total))
                }
                Value::Text(s) => {
                    // v7.36 (perf — mailrs Ask 1) — ASCII fast path.
                    // `s.is_ascii()` is SIMD-vectorised; for the 1 KB
                    // ASCII bodies in the storage / contact baselines
                    // it's ~50 ns vs the 1 µs `s.chars().count()`
                    // walk. PG `length(text)` returns character count,
                    // which equals byte count when ASCII.
                    let n = if s.is_ascii() {
                        i32::try_from(s.len()).unwrap_or(i32::MAX)
                    } else {
                        i32::try_from(s.chars().count()).unwrap_or(i32::MAX)
                    };
                    Ok(Value::Int(n))
                }
                // v7.10.4 — PG semantics: length(bytea) returns
                // byte count (= octet_length). Without this branch
                // mailrs's INSERT … SELECT length(body) … against a
                // BYTEA column would type-mismatch.
                Value::Bytes(b) => {
                    let n = i32::try_from(b.len()).unwrap_or(i32::MAX);
                    Ok(Value::Int(n))
                }
                // PG length(bit) is the number of bits.
                Value::BitString { nbits, .. } => {
                    Ok(Value::Int(i32::try_from(*nbits).unwrap_or(i32::MAX)))
                }
                // PG length(tsvector) is the number of lexemes.
                Value::TsVector(lexemes) => {
                    Ok(Value::Int(i32::try_from(lexemes.len()).unwrap_or(i32::MAX)))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!("length() needs text or bytea, got {:?}", other.data_type()),
                }),
            }
        }
        // v7.10.4 — `OCTET_LENGTH(x)` returns byte count for both
        // TEXT (UTF-8 byte length) and BYTEA. PG-spec name; aliases
        // to length() for bytea by design.
        "octet_length" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("octet_length() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let n = i32::try_from(s.len()).unwrap_or(i32::MAX);
                    Ok(Value::Int(n))
                }
                Value::Bytes(b) => {
                    let n = i32::try_from(b.len()).unwrap_or(i32::MAX);
                    Ok(Value::Int(n))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "octet_length() needs text or bytea, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — SQL:2003 OVERLAY(string PLACING
        // replacement FROM start [FOR length]). Splices `replacement`
        // into `string` at 1-based `start`, replacing `length` chars
        // (or replacement.len() chars if `length` is omitted).
        //
        // Real implementation, multi-byte-safe via chars().
        "overlay" => {
            if args.len() < 3 || args.len() > 4 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("overlay() takes 3 or 4 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let s = match &args[0] {
                Value::Text(s) => s.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "overlay(): source must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let placing = match &args[1] {
                Value::Text(s) => s.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "overlay(): replacement must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let start = match &args[2] {
                Value::Int(n) => *n as i64,
                Value::BigInt(n) => *n,
                Value::SmallInt(n) => i64::from(*n),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "overlay(): start must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if start < 1 {
                return Err(EvalError::TypeMismatch {
                    detail: "overlay(): start must be >= 1".into(),
                });
            }
            let placing_char_count = placing.chars().count();
            let length = if args.len() == 4 {
                match &args[3] {
                    Value::Int(n) => *n as i64,
                    Value::BigInt(n) => *n,
                    Value::SmallInt(n) => i64::from(*n),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "overlay(): length must be integer, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                }
            } else {
                placing_char_count as i64
            };
            let start_idx = (start - 1) as usize;
            let end_idx = start_idx.saturating_add(length.max(0) as usize);
            let mut out = alloc::string::String::new();
            for (i, ch) in s.chars().enumerate() {
                if i < start_idx {
                    out.push(ch);
                } else if i == start_idx {
                    out.push_str(placing);
                }
                if i >= end_idx {
                    out.push(ch);
                }
            }
            // Handle start_idx at end of source.
            if start_idx >= s.chars().count() {
                out.push_str(placing);
            }
            Ok(Value::text(out))
        }
        // v7.37.17 (17.6 siblings) — set_bit / get_bit / set_byte /
        // get_byte for bytea manipulation. PG-standard low-level
        // byte / bit access.
        "get_byte" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("get_byte() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let b = match &args[0] {
                Value::Bytes(b) => b,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "get_byte(): needs bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let idx = match &args[1] {
                Value::Int(n) => *n as i64,
                Value::BigInt(n) => *n,
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "get_byte(): index must be integer".into(),
                    });
                }
            };
            if idx < 0 || (idx as usize) >= b.len() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "get_byte(): index {idx} out of range 0..{}",
                        b.len()
                    ),
                });
            }
            Ok(Value::Int(i32::from(b[idx as usize])))
        }
        "get_bit" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("get_bit() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            // PG `get_bit(bit/varbit, n)`: bit strings are MSB-first with bit 0
            // the leftmost bit, and the range is the bit length (not bytes*8).
            if let Value::BitString { nbits, bytes } = &args[0] {
                let bit_idx = match &args[1] {
                    Value::Int(n) => i64::from(*n),
                    Value::BigInt(n) => *n,
                    _ => {
                        return Err(EvalError::TypeMismatch {
                            detail: "get_bit(): index must be integer".into(),
                        });
                    }
                };
                if bit_idx < 0 || (bit_idx as u64) >= u64::from(*nbits) {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "get_bit(): index {bit_idx} out of range 0..{nbits}"
                        ),
                    });
                }
                let byte_idx = (bit_idx as usize) / 8;
                let bit_off = (bit_idx as usize) % 8;
                let bit = (bytes[byte_idx] >> (7 - bit_off)) & 1;
                return Ok(Value::Int(i32::from(bit)));
            }
            let b = match &args[0] {
                Value::Bytes(b) => b,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "get_bit(): needs bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let bit_idx = match &args[1] {
                Value::Int(n) => *n as i64,
                Value::BigInt(n) => *n,
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "get_bit(): index must be integer".into(),
                    });
                }
            };
            if bit_idx < 0 || (bit_idx as usize) >= b.len() * 8 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "get_bit(): index {bit_idx} out of range 0..{}",
                        b.len() * 8
                    ),
                });
            }
            let byte_idx = (bit_idx as usize) / 8;
            let bit_off = (bit_idx as usize) % 8;
            let bit = (b[byte_idx] >> bit_off) & 1;
            Ok(Value::Int(i32::from(bit)))
        }
        // v7.37.17 (17.6 siblings) — set_byte(bytea, index, val) /
        // set_bit(bytea, bit_index, val). Complements get_byte /
        // get_bit; returns modified bytea copy.
        "set_byte" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("set_byte() takes 3 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let b = match &args[0] {
                Value::Bytes(b) => b.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "set_byte(): needs bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let idx = match &args[1] {
                Value::Int(n) => *n as i64,
                Value::BigInt(n) => *n,
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "set_byte(): index must be integer".into(),
                    });
                }
            };
            let val = match &args[2] {
                Value::Int(n) => *n as i64,
                Value::BigInt(n) => *n,
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "set_byte(): value must be integer".into(),
                    });
                }
            };
            if idx < 0 || (idx as usize) >= b.len() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "set_byte(): index {idx} out of range 0..{}",
                        b.len()
                    ),
                });
            }
            if !(0..=255).contains(&val) {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("set_byte(): value {val} not in 0..=255"),
                });
            }
            let mut out = b.to_vec();
            out[idx as usize] = val as u8;
            Ok(Value::Bytes(out.into()))
        }
        "set_bit" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("set_bit() takes 3 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            // PG `set_bit(bit/varbit, n, v)`: MSB-first, range is the bit
            // length; returns the modified bit string (not a bytea).
            if let Value::BitString { nbits, bytes } = &args[0] {
                let bit_idx = match &args[1] {
                    Value::Int(n) => i64::from(*n),
                    Value::BigInt(n) => *n,
                    _ => {
                        return Err(EvalError::TypeMismatch {
                            detail: "set_bit(): index must be integer".into(),
                        });
                    }
                };
                let val = match &args[2] {
                    Value::Int(n) => i64::from(*n),
                    Value::BigInt(n) => *n,
                    _ => {
                        return Err(EvalError::TypeMismatch {
                            detail: "set_bit(): value must be integer".into(),
                        });
                    }
                };
                if bit_idx < 0 || (bit_idx as u64) >= u64::from(*nbits) {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "set_bit(): index {bit_idx} out of range 0..{nbits}"
                        ),
                    });
                }
                if val != 0 && val != 1 {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!("set_bit(): value {val} must be 0 or 1"),
                    });
                }
                let mut out = bytes.to_vec();
                let byte_idx = (bit_idx as usize) / 8;
                let bit_off = (bit_idx as usize) % 8;
                let mask = 1u8 << (7 - bit_off);
                if val == 1 {
                    out[byte_idx] |= mask;
                } else {
                    out[byte_idx] &= !mask;
                }
                return Ok(Value::BitString { nbits: *nbits, bytes: out.into() });
            }
            let b = match &args[0] {
                Value::Bytes(b) => b.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "set_bit(): needs bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let bit_idx = match &args[1] {
                Value::Int(n) => *n as i64,
                Value::BigInt(n) => *n,
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "set_bit(): index must be integer".into(),
                    });
                }
            };
            let val = match &args[2] {
                Value::Int(n) => *n as i64,
                Value::BigInt(n) => *n,
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "set_bit(): value must be integer".into(),
                    });
                }
            };
            if bit_idx < 0 || (bit_idx as usize) >= b.len() * 8 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "set_bit(): index {bit_idx} out of range 0..{}",
                        b.len() * 8
                    ),
                });
            }
            if val != 0 && val != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("set_bit(): value {val} must be 0 or 1"),
                });
            }
            let mut out = b.to_vec();
            let byte_idx = (bit_idx as usize) / 8;
            let bit_off = (bit_idx as usize) % 8;
            let mask = 1u8 << bit_off;
            if val == 1 {
                out[byte_idx] |= mask;
            } else {
                out[byte_idx] &= !mask;
            }
            Ok(Value::Bytes(out.into()))
        }
        // v7.37.17 (17.6 siblings) — index-property probes that
        // psql \d output + monitoring exporters emit.
        //
        //   pg_index_has_property(indexoid, name)       → bool
        //   pg_indexam_has_property(amoid, name)        → bool
        //   pg_index_column_has_property(indexoid, col, name) → bool
        //
        // SPG's BTree AM supports all standard properties (ordered
        // + scan / bitmapscan / search); custom AMs (Hash/GiST etc.)
        // land with v7.39 indexes epic. Returning true here matches
        // BTree's PG behavior for the common property queries
        // ("returnable", "orderable", "search_array", etc.).
        "pg_index_has_property"
        | "pg_indexam_has_property"
        | "pg_index_column_has_property" => Ok(Value::Bool(true)),
        // v7.37.17 (17.6 siblings) — schema-visibility probes.
        // SPG uses a single `public` namespace, so anything the caller
        // can reference is visible. psql \d + ORMs check these.
        "pg_type_is_visible"
        | "pg_table_is_visible"
        | "pg_function_is_visible"
        | "pg_operator_is_visible"
        | "pg_opclass_is_visible"
        | "pg_ts_config_is_visible"
        | "pg_ts_dict_is_visible"
        | "pg_ts_parser_is_visible"
        | "pg_ts_template_is_visible" => Ok(Value::Bool(true)),
        // pg_get_serial_sequence is already handled above; keep
        // this location as the anchor for the visibility family.
        //
        // pg_relation_is_publishable / pg_get_publication_tables —
        // logical-decoding auxiliary probes. Return true / NULL
        // (SPG's publication surface handles the real lookup;
        // these are the wire-protocol-agnostic scalar aliases).
        "pg_relation_is_publishable" => Ok(Value::Bool(true)),
        "pg_get_publication_tables" => Ok(Value::Null),
        // pg_stat_get_activity(pid) — returns a row from the
        // activity view. Return NULL for the scalar surface;
        // real callers use the pg_stat_activity view.
        "pg_stat_get_activity" | "pg_stat_get_backend_activity" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — pg_stat_get_bgwriter_* +
        // pg_stat_get_wal_* + pg_stat_get_archiver families.
        // Aggregate cluster-wide counter probes emitted by
        // postgres_exporter's default scrape.
        "pg_stat_get_bgwriter_timed_checkpoints"
        | "pg_stat_get_bgwriter_requested_checkpoints"
        | "pg_stat_get_bgwriter_buf_written_checkpoints"
        | "pg_stat_get_bgwriter_buf_written_clean"
        | "pg_stat_get_bgwriter_maxwritten_clean"
        | "pg_stat_get_buf_written_backend"
        | "pg_stat_get_buf_fsync_backend"
        | "pg_stat_get_buf_alloc"
        | "pg_stat_get_checkpoint_write_time"
        | "pg_stat_get_checkpoint_sync_time"
        | "pg_stat_get_wal_records"
        | "pg_stat_get_wal_fpi"
        | "pg_stat_get_wal_bytes"
        | "pg_stat_get_wal_buffers_full"
        | "pg_stat_get_wal_write"
        | "pg_stat_get_wal_sync"
        | "pg_stat_get_wal_write_time"
        | "pg_stat_get_wal_sync_time"
        | "pg_stat_get_archiver_archived_count"
        | "pg_stat_get_archiver_failed_count"
        | "pg_stat_get_analyze_count"
        | "pg_stat_get_autoanalyze_count"
        | "pg_stat_get_vacuum_count"
        | "pg_stat_get_autovacuum_count"
        | "pg_stat_get_live_tuples"
        | "pg_stat_get_dead_tuples"
        | "pg_stat_get_mod_since_analyze"
        | "pg_stat_get_ins_since_vacuum"
        | "pg_stat_get_tuples_inserted"
        | "pg_stat_get_tuples_updated"
        | "pg_stat_get_tuples_deleted"
        | "pg_stat_get_tuples_hot_updated"
        | "pg_stat_get_tuples_newpage_updated"
        | "pg_stat_get_numscans"
        | "pg_stat_get_tuples_returned"
        | "pg_stat_get_tuples_fetched"
        | "pg_stat_get_blocks_fetched"
        | "pg_stat_get_blocks_hit"
        | "pg_stat_get_xact_tuples_inserted"
        | "pg_stat_get_xact_tuples_updated"
        | "pg_stat_get_xact_tuples_deleted"
        | "pg_stat_get_xact_tuples_hot_updated"
        | "pg_stat_get_xact_tuples_newpage_updated"
        | "pg_stat_get_xact_numscans"
        | "pg_stat_get_xact_tuples_returned"
        | "pg_stat_get_xact_tuples_fetched"
        | "pg_stat_get_xact_blocks_fetched"
        | "pg_stat_get_xact_blocks_hit" => Ok(Value::BigInt(0)),
        // Timestamp-shaped ones.
        "pg_stat_get_bgwriter_stat_reset_time"
        | "pg_stat_get_archiver_last_archived_time"
        | "pg_stat_get_archiver_last_failed_time"
        | "pg_stat_get_archiver_stat_reset_time"
        | "pg_stat_get_last_analyze_time"
        | "pg_stat_get_last_autoanalyze_time"
        | "pg_stat_get_last_vacuum_time"
        | "pg_stat_get_last_autovacuum_time" => Ok(Value::Null),
        // Text-shaped ones.
        "pg_stat_get_archiver_last_archived_wal"
        | "pg_stat_get_archiver_last_failed_wal" => {
            Ok(Value::text::<String>(String::new()))
        }
        // v7.37.17 (17.6 siblings) — pg_stat_get_db_* family.
        // Per-database counter probes; postgres_exporter emits
        // these in the "high cardinality" scrape mode. Return
        // 0 (or NULL for timestamp-shaped ones) so scrapes
        // succeed with zero-baseline metrics.
        "pg_stat_get_db_xact_commit"
        | "pg_stat_get_db_xact_rollback"
        | "pg_stat_get_db_blocks_fetched"
        | "pg_stat_get_db_blocks_hit"
        | "pg_stat_get_db_tuples_returned"
        | "pg_stat_get_db_tuples_fetched"
        | "pg_stat_get_db_tuples_inserted"
        | "pg_stat_get_db_tuples_updated"
        | "pg_stat_get_db_tuples_deleted"
        | "pg_stat_get_db_conflict_all"
        | "pg_stat_get_db_conflict_tablespace"
        | "pg_stat_get_db_conflict_lock"
        | "pg_stat_get_db_conflict_snapshot"
        | "pg_stat_get_db_conflict_bufferpin"
        | "pg_stat_get_db_conflict_startup_deadlock"
        | "pg_stat_get_db_conflict_logicalslot"
        | "pg_stat_get_db_deadlocks"
        | "pg_stat_get_db_checksum_failures"
        | "pg_stat_get_db_active_time"
        | "pg_stat_get_db_idle_in_transaction_time"
        | "pg_stat_get_db_session_time"
        | "pg_stat_get_db_sessions"
        | "pg_stat_get_db_sessions_abandoned"
        | "pg_stat_get_db_sessions_fatal"
        | "pg_stat_get_db_sessions_killed"
        | "pg_stat_get_db_temp_bytes"
        | "pg_stat_get_db_temp_files"
        | "pg_stat_get_db_numbackends"
        | "pg_stat_get_db_blk_read_time"
        | "pg_stat_get_db_blk_write_time" => Ok(Value::BigInt(0)),
        // Timestamp-shaped db-stat probes.
        "pg_stat_get_db_stat_reset_time"
        | "pg_stat_get_db_checksum_last_failure" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — pg_stat_get_snapshot_timestamp
        // returns the timestamp when the stats snapshot was taken.
        // Monitoring dashboards + postgres_exporter check this to
        // detect frozen stats. Return the 2020-01-01 anchor
        // consistent with pg_backend_start_time; v7.38 wall-clock
        // plumbing replaces this with a real per-scrape timestamp.
        "pg_stat_get_snapshot_timestamp" | "pg_stat_get_stat_snapshot_timestamp" => {
            const ANCHOR_2020_UTC: i64 = 1_577_836_800_000_000;
            Ok(Value::Timestamp(ANCHOR_2020_UTC))
        }
        // v7.37.17 (17.6 siblings) — array_to_json(arr [, pretty])
        // returns JSON text representation of an array. `pretty`
        // when true adds newlines between elements. Multi-dim arrays
        // queue with v7.40 array-model widening.
        "array_to_json" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_to_json() takes 1 or 2 args, got {}",
                        args.len()
                    ),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            let pretty = matches!(args.get(1), Some(Value::Bool(true)));
            let sep = if pretty { ",\n " } else { "," };
            let opener = if pretty { "[\n " } else { "[" };
            let closer = if pretty { "\n]" } else { "]" };
            let mut out = alloc::string::String::from(opener);
            let items = match &args[0] {
                Value::TextArray(items) => {
                    let strs: alloc::vec::Vec<alloc::string::String> = items
                        .iter()
                        .map(|o| match o {
                            None => alloc::string::String::from("null"),
                            Some(s) => {
                                let escaped =
                                    s.replace('\\', "\\\\").replace('"', "\\\"");
                                alloc::format!("\"{escaped}\"")
                            }
                        })
                        .collect();
                    strs
                }
                Value::IntArray(items) => items
                    .iter()
                    .map(|n| match n {
                        None => alloc::string::String::from("null"),
                        Some(x) => alloc::format!("{x}"),
                    })
                    .collect(),
                Value::BigIntArray(items) => items
                    .iter()
                    .map(|n| match n {
                        None => alloc::string::String::from("null"),
                        Some(x) => alloc::format!("{x}"),
                    })
                    .collect(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "array_to_json(): needs array, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                out.push_str(item);
            }
            out.push_str(closer);
            Ok(Value::json(out))
        }
        // v7.37.17 (17.6 siblings) — Interval canonicalizers.
        // PG's `justify_days`, `justify_hours`, `justify_interval`
        // normalize an interval's {months, days, micros} tuple:
        //
        //   justify_days(interval)     : days ≥ 30 → months += days/30
        //   justify_hours(interval)    : micros ≥ 24h → days += micros/24h
        //   justify_interval(interval) : both, iteratively
        //
        // Only the +wrap direction; negative slots stay signed in PG,
        // so we mirror that. NULL passthrough.
        "justify_days" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("justify_days() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Interval { months, days, micros } => {
                    let extra_months: i32 = days.div_euclid(30);
                    let leftover_days: i32 = days.rem_euclid(30);
                    Ok(Value::Interval {
                        months: months + extra_months,
                        days: leftover_days,
                        micros: *micros,
                    })
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "justify_days() needs interval, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "justify_hours" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("justify_hours() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Interval { months, days, micros } => {
                    const DAY_US: i64 = 24 * 60 * 60 * 1_000_000;
                    let extra_days_i64 = micros.div_euclid(DAY_US);
                    let leftover_micros = micros.rem_euclid(DAY_US);
                    let extra_days: i32 =
                        i32::try_from(extra_days_i64).unwrap_or(i32::MAX);
                    Ok(Value::Interval {
                        months: *months,
                        days: days + extra_days,
                        micros: leftover_micros,
                    })
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "justify_hours() needs interval, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "justify_interval" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("justify_interval() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Interval { months, days, micros } => {
                    const DAY_US: i64 = 24 * 60 * 60 * 1_000_000;
                    let extra_days_i64 = micros.div_euclid(DAY_US);
                    let leftover_micros = micros.rem_euclid(DAY_US);
                    let extra_days: i32 =
                        i32::try_from(extra_days_i64).unwrap_or(i32::MAX);
                    let total_days = days + extra_days;
                    let extra_months: i32 = total_days.div_euclid(30);
                    let leftover_days: i32 = total_days.rem_euclid(30);
                    Ok(Value::Interval {
                        months: months + extra_months,
                        days: leftover_days,
                        micros: leftover_micros,
                    })
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "justify_interval() needs interval, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — snapshot export / import
        // family. SPG doesn't yet ship a snapshot text serializer
        // (queues with v7.38 MVCC Phase C); return NULL / true so
        // callers can defensively probe.
        "pg_export_snapshot" | "pg_snapshot" => Ok(Value::Null),
        // pg_import_snapshot / pg_import_serialized_snapshot don't
        // return anything meaningful in scalar form.
        "pg_import_snapshot" | "pg_import_serialized_snapshot" => Ok(Value::Null),
        // pg_visible_in_snapshot(xid, snapshot) — visibility probe
        // used by pg_visibility. Under SPG's synchronous commit +
        // no MVCC-yet model, any tx we know about is visible.
        "pg_visible_in_snapshot" => Ok(Value::Bool(true)),
        // pg_last_xid — returns the last committed xid. Similar
        // shape to txid_current; use next_tx_id.
        "pg_last_xid" => Ok(Value::BigInt(0)),
        // v7.37.17 (17.6 siblings) — WAL utility probes. SPG uses
        // seq_no not PG-style LSN; return NULL until seq_no ↔ LSN
        // mapping ships with the replication-protocol RFC.
        //
        //   pg_walfile_name(lsn)         → NULL
        //   pg_walfile_name_offset(lsn)  → NULL
        //   pg_split_walfile_name(name)  → NULL
        //   pg_ls_slru()                 → NULL
        //   pg_ls_replslotdir()          → NULL
        //   pg_promote(...)              → true (accept-and-no-op —
        //                                    SPG is single-writer)
        //   pg_reload_conf()             → true (SPG config is
        //                                       CLI/env-only, no
        //                                       reload semantics)
        //   pg_rotate_logfile()          → true (no-op)
        //   pg_rotate_logfile_v2()       → true
        //   pg_switch_wal()              → NULL (returns LSN of
        //                                       switched location)
        "pg_walfile_name"
        | "pg_walfile_name_offset"
        | "pg_split_walfile_name"
        | "pg_ls_slru"
        | "pg_ls_replslotdir"
        | "pg_switch_wal"
        | "pg_control_system"
        | "pg_control_recovery"
        | "pg_control_checkpoint"
        | "pg_control_init" => Ok(Value::Null),
        "pg_promote" | "pg_reload_conf" | "pg_rotate_logfile" | "pg_rotate_logfile_v2" => {
            Ok(Value::Bool(true))
        }
        // v7.37.17 (17.6 siblings) — logging / config-file probes.
        // psql \d + tools query these. SPG uses stderr for logs and
        // env/CLI for config, so return NULL / empty text.
        "pg_current_logfile" => Ok(Value::text::<String>(String::new())),
        "pg_hba_file_rules" | "pg_ident_file_mappings" | "pg_config" => {
            Ok(Value::Null)
        }
        // SRF settings/metadata readers — psql tab-completion
        // (pg_get_keywords), reloptions expansion
        // (pg_options_to_table), timezone catalogs, partition-tree
        // walker, extended-stats reader, logical-decoding dirs.
        // Scalar surface: NULL keeps the callers parseable.
        "pg_get_keywords"
        | "pg_options_to_table"
        | "pg_show_all_settings"
        | "pg_show_all_file_settings"
        | "pg_timezone_names"
        | "pg_timezone_abbrevs"
        | "pg_partition_tree"
        | "pg_mcv_list_items"
        | "pg_ls_logicalmapdir"
        | "pg_ls_logicalsnapdir" => Ok(Value::Null),
        // Replication-origin family — pg_recvlogical / pglogical
        // probe. SPG has no logical replication yet.
        "pg_replication_origin_advance"
        | "pg_replication_origin_create"
        | "pg_replication_origin_drop"
        | "pg_replication_origin_oid"
        | "pg_replication_origin_progress"
        | "pg_replication_origin_session_is_setup"
        | "pg_replication_origin_session_progress"
        | "pg_replication_origin_session_reset"
        | "pg_replication_origin_session_setup"
        | "pg_replication_origin_xact_reset"
        | "pg_replication_origin_xact_setup"
        | "pg_show_replication_origin_status" => Ok(Value::Null),
        // Replication-slot admin. These are usually functions that
        // return SETOF records; scalar-surface NULL is fine.
        "pg_create_physical_replication_slot"
        | "pg_create_logical_replication_slot"
        | "pg_copy_physical_replication_slot"
        | "pg_copy_logical_replication_slot"
        | "pg_drop_replication_slot"
        | "pg_replication_slot_advance" => Ok(Value::Null),
        // Logical-decoding consumers — SETOF change streams +
        // message emission. SPG's replication protocol RFC
        // (MAGIC_SUB/MAGIC_REPL) owns the real semantics; NULL
        // keeps wal2json/debezium-style probe queries parseable.
        "pg_logical_slot_get_changes"
        | "pg_logical_slot_peek_changes"
        | "pg_logical_slot_get_binary_changes"
        | "pg_logical_slot_peek_binary_changes" => Ok(Value::Null),
        // pg_logical_emit_message(transactional, prefix, content) —
        // writes a message WAL record, returns its LSN. SPG's WAL
        // has no message record type; return the same '0/0' text
        // LSN as the pg_current_wal_lsn family.
        "pg_logical_emit_message" => {
            Ok(Value::text::<String>("0/0".into()))
        }
        // Collation versioning — SPG collates via Rust's Unicode
        // tables, so the actual version is the Unicode version we
        // ship (aligned with unicode_version() at 15.0). Never
        // reports a mismatch warning because there is exactly one
        // collation provider.
        "pg_collation_actual_version"
        | "pg_database_collation_actual_version" => {
            match args.first() {
                Some(Value::Null) => Ok(Value::Null),
                _ => Ok(Value::text::<String>("15.0".into())),
            }
        }
        // pg_import_system_collations(schema) — count of collations
        // imported from the OS. SPG has a single builtin provider;
        // nothing to import → 0.
        "pg_import_system_collations" => Ok(Value::Int(0)),
        // Trigger-function names that appear in CREATE TRIGGER
        // statements from pg_dump. Calling them directly as scalars
        // (some ORMs probe existence this way) returns NULL.
        "suppress_redundant_updates_trigger"
        | "tsvector_update_trigger"
        | "tsvector_update_trigger_column" => Ok(Value::Null),
        // pg_nextoid(rel, col, index) — binary-upgrade-only oid
        // allocator; binary_upgrade_* setters are pg_upgrade
        // internals. NULL keeps pg_upgrade-generated dumps moving.
        "pg_nextoid"
        | "binary_upgrade_set_next_pg_type_oid"
        | "binary_upgrade_set_next_array_pg_type_oid"
        | "binary_upgrade_set_next_heap_pg_class_oid"
        | "binary_upgrade_set_next_index_pg_class_oid"
        | "binary_upgrade_set_next_toast_pg_class_oid"
        | "binary_upgrade_set_next_pg_enum_oid"
        | "binary_upgrade_set_next_pg_authid_oid"
        | "binary_upgrade_set_record_init_privs"
        | "binary_upgrade_set_missing_value"
        | "binary_upgrade_create_empty_extension" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — pg_stat_reset* family. Returns
        // void (NULL) — monitoring / admin dashboards call these on
        // schedule to reset counters. SPG's counters are session-
        // scoped and reset on Engine::new() today; a real reset arm
        // that clears query_stats + storage counters queues with
        // v7.38 observability epic.
        "pg_stat_reset"
        | "pg_stat_reset_shared"
        | "pg_stat_reset_single_table_counters"
        | "pg_stat_reset_single_function_counters"
        | "pg_stat_reset_slru"
        | "pg_stat_reset_replication_slot"
        | "pg_stat_reset_subscription_stats" => Ok(Value::Null),
        // pg_stat_clear_snapshot — clear the per-session stats
        // snapshot. Same treatment: return void.
        "pg_stat_clear_snapshot" => Ok(Value::Null),
        // pg_stat_force_next_flush — force next stats flush to
        // shared memory. SPG's stats are synchronous; no-op.
        "pg_stat_force_next_flush" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — transaction ID probes. SPG
        // uses u64 tx IDs that never wrap; these return the current
        // tx ID as BigInt. txid_ names are pre-PG 13 aliases for
        // the pg_ names.
        "txid_current"
        | "txid_current_if_assigned"
        | "pg_current_xact_id"
        | "pg_current_xact_id_if_assigned" => {
            // Use next_tx_id as a best-effort scalar surface.
            let id = ctx
                .catalog
                .and_then(|_| Some(1i64))
                .unwrap_or(0i64);
            Ok(Value::BigInt(id))
        }
        // txid_current_snapshot / pg_current_snapshot — full snapshot
        // structure with xmin/xmax/xip_list. Return NULL for the
        // scalar surface until we have a snapshot text type.
        "txid_current_snapshot"
        | "pg_current_snapshot"
        | "pg_snapshot_xmin"
        | "pg_snapshot_xmax"
        | "pg_snapshot_xip"
        | "txid_snapshot_xmin"
        | "txid_snapshot_xmax"
        | "txid_snapshot_xip" => Ok(Value::Null),
        // txid_status / pg_xact_status — SPG commits synchronously,
        // so any non-null tx ID we know about is committed.
        "txid_status" | "pg_xact_status" => {
            Ok(Value::text::<String>("committed".into()))
        }
        // pg_notification_queue_usage — % of notification queue in
        // use. Return 0.0 (SPG has no notification queue yet).
        "pg_notification_queue_usage" => Ok(Value::Float(0.0)),
        // v7.37.17 (17.6 siblings) — jsonb_object_keys returns the
        // top-level keys of a jsonb object. PG has this as a SRF
        // (set-returning function) — SPG's scalar surface returns
        // TextArray. Errors on non-object input (matches PG). Empty
        // object → empty array.
        "jsonb_object_keys" | "json_object_keys" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                // Text accepted alongside Json so the FROM-position
                // SRF rewrite (string literal argument, no jsonb cast
                // context) reaches this arm.
                Value::Json(s) | Value::Text(s) => {
                    let parsed =
                        crate::json::parse(s).map_err(|e| EvalError::TypeMismatch {
                            detail: format!("{name}(): JSON parse failed: {e}"),
                        })?;
                    match parsed {
                        crate::json::JsonValue::Object(members) => {
                            let keys: alloc::vec::Vec<Option<alloc::string::String>> = members
                                .into_iter()
                                .map(|(k, _)| Some(k))
                                .collect();
                            Ok(Value::TextArray(keys))
                        }
                        _ => Err(EvalError::TypeMismatch {
                            detail: format!(
                                "{name}(): expected JSON object, got other JSON type"
                            ),
                        }),
                    }
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "{name}() needs jsonb, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — jsonb_strip_nulls(jsonb)
        // removes null-valued keys from object members (recurses
        // into nested objects + arrays). Array items are preserved
        // even if null (matches PG semantics).
        "jsonb_strip_nulls" | "json_strip_nulls" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                // v7.37 D.49 — accept a TEXT arg (PG casts an unknown-type string
                // literal to jsonb).
                Value::Json(s) | Value::Text(s) => {
                    let mut parsed =
                        crate::json::parse(s).map_err(|e| EvalError::TypeMismatch {
                            detail: format!("{name}(): JSON parse failed: {e}"),
                        })?;
                    fn strip(v: &mut crate::json::JsonValue) {
                        match v {
                            crate::json::JsonValue::Object(members) => {
                                members.retain(
                                    |(_, val)| !matches!(val, crate::json::JsonValue::Null),
                                );
                                for (_, val) in members.iter_mut() {
                                    strip(val);
                                }
                            }
                            crate::json::JsonValue::Array(items) => {
                                for it in items.iter_mut() {
                                    strip(it);
                                }
                            }
                            _ => {}
                        }
                    }
                    strip(&mut parsed);
                    fn write_json(v: &crate::json::JsonValue, out: &mut alloc::string::String) {
                        use core::fmt::Write;
                        match v {
                            crate::json::JsonValue::Null => out.push_str("null"),
                            crate::json::JsonValue::Bool(true) => out.push_str("true"),
                            crate::json::JsonValue::Bool(false) => out.push_str("false"),
                            crate::json::JsonValue::Number(x) => {
                                let _ = write!(out, "{x}");
                            }
                            crate::json::JsonValue::NumberText(s) => out.push_str(s),
                            crate::json::JsonValue::String(s) => {
                                out.push('"');
                                for c in s.chars() {
                                    match c {
                                        '"' => out.push_str("\\\""),
                                        '\\' => out.push_str("\\\\"),
                                        '\n' => out.push_str("\\n"),
                                        '\r' => out.push_str("\\r"),
                                        '\t' => out.push_str("\\t"),
                                        c if (c as u32) < 0x20 => {
                                            let _ = write!(out, "\\u{:04x}", c as u32);
                                        }
                                        c => out.push(c),
                                    }
                                }
                                out.push('"');
                            }
                            crate::json::JsonValue::Array(items) => {
                                out.push('[');
                                for (i, it) in items.iter().enumerate() {
                                    if i > 0 {
                                        out.push(',');
                                    }
                                    write_json(it, out);
                                }
                                out.push(']');
                            }
                            crate::json::JsonValue::Object(entries) => {
                                out.push('{');
                                for (i, (k, val)) in entries.iter().enumerate() {
                                    if i > 0 {
                                        out.push(',');
                                    }
                                    write_json(
                                        &crate::json::JsonValue::String(k.clone()),
                                        out,
                                    );
                                    out.push(':');
                                    write_json(val, out);
                                }
                                out.push('}');
                            }
                        }
                    }
                    let mut out = alloc::string::String::new();
                    write_json(&parsed, &mut out);
                    let result = Value::json(out);
                    // jsonb_strip_nulls yields canonical jsonb.
                    if name == "jsonb_strip_nulls" {
                        Ok(crate::json::canonicalize_value(result))
                    } else {
                        Ok(result)
                    }
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "{name}() needs jsonb, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — jsonb_pretty(jsonb) returns
        // pretty-printed JSON text. Walks the parsed tree and
        // re-emits with 2-space indent + newlines between object /
        // array members.
        // "json_pretty" is MySQL's spelling.
        "jsonb_pretty" | "json_pretty" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("jsonb_pretty() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                // Text accepted alongside Json so string-literal
                // arguments (MySQL json_pretty spelling) reach it.
                Value::Json(s) | Value::Text(s) => {
                    let parsed = crate::json::parse(s).map_err(|e| EvalError::TypeMismatch {
                        detail: format!("jsonb_pretty(): JSON parse failed: {e}"),
                    })?;
                    fn pretty(v: &crate::json::JsonValue, indent: usize) -> alloc::string::String {
                        use alloc::string::String;
                        let pad = "  ".repeat(indent);
                        let pad_next = "  ".repeat(indent + 1);
                        match v {
                            crate::json::JsonValue::Null => "null".into(),
                            crate::json::JsonValue::Bool(b) => {
                                if *b { "true".into() } else { "false".into() }
                            }
                            crate::json::JsonValue::Number(n) => alloc::format!("{n}"),
                            crate::json::JsonValue::NumberText(s) => s.clone(),
                            crate::json::JsonValue::String(s) => {
                                let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
                                alloc::format!("\"{escaped}\"")
                            }
                            crate::json::JsonValue::Array(items) => {
                                if items.is_empty() {
                                    return "[]".into();
                                }
                                let mut out = String::from("[\n");
                                for (i, item) in items.iter().enumerate() {
                                    out.push_str(&pad_next);
                                    out.push_str(&pretty(item, indent + 1));
                                    if i + 1 < items.len() {
                                        out.push(',');
                                    }
                                    out.push('\n');
                                }
                                out.push_str(&pad);
                                out.push(']');
                                out
                            }
                            crate::json::JsonValue::Object(members) => {
                                if members.is_empty() {
                                    return "{}".into();
                                }
                                let mut out = String::from("{\n");
                                for (i, (k, v)) in members.iter().enumerate() {
                                    let key_escaped =
                                        k.replace('\\', "\\\\").replace('"', "\\\"");
                                    out.push_str(&pad_next);
                                    out.push_str(&alloc::format!("\"{key_escaped}\": "));
                                    out.push_str(&pretty(v, indent + 1));
                                    if i + 1 < members.len() {
                                        out.push(',');
                                    }
                                    out.push('\n');
                                }
                                out.push_str(&pad);
                                out.push('}');
                                out
                            }
                        }
                    }
                    Ok(Value::text(pretty(&parsed, 0)))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "jsonb_pretty() needs jsonb, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — jsonb_typeof / json_typeof
        // returns PG's canonical type-name text for a jsonb/json
        // value: 'object' / 'array' / 'string' / 'number' /
        // 'boolean' / 'null'. Return NULL for SQL NULL input
        // (not JSON null — that returns text 'null').
        "jsonb_typeof" | "json_typeof" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                // v7.37 D.49 — accept a TEXT arg (PG casts an unknown-type string
                // literal to jsonb).
                Value::Json(s) | Value::Text(s) => {
                    let trimmed = s.trim_start();
                    let type_name = if let Some(first) = trimmed.chars().next() {
                        match first {
                            '{' => "object",
                            '[' => "array",
                            '"' => "string",
                            't' | 'f' => "boolean",
                            'n' => "null",
                            '-' | '0'..='9' => "number",
                            _ => "null",
                        }
                    } else {
                        "null"
                    };
                    Ok(Value::text(alloc::string::String::from(type_name)))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "{name}() needs jsonb/json, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — jsonb_array_length /
        // json_array_length returns the element count of a JSON
        // array. Errors on non-array input (matches PG). NULL
        // passthrough.
        "jsonb_array_length" | "json_array_length" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                // v7.37 D.49 — accept a TEXT arg too: PG implicitly casts an
                // unknown-type string literal (`jsonb_array_length('[1,2]')`) to
                // jsonb. Other jsonb builtins here already accept `Json | Text`.
                Value::Json(s) | Value::Text(s) => {
                    let parsed = crate::json::parse(s).map_err(|e| {
                        EvalError::TypeMismatch {
                            detail: format!("{name}(): JSON parse failed: {e}"),
                        }
                    })?;
                    match parsed {
                        crate::json::JsonValue::Array(arr) => {
                            Ok(Value::Int(arr.len() as i32))
                        }
                        _ => Err(EvalError::TypeMismatch {
                            detail: format!(
                                "{name}(): expected JSON array, got other JSON type"
                            ),
                        }),
                    }
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "{name}() needs jsonb/json, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — jsonb_array_elements[_text] /
        // json_array_elements[_text] scalar surface: returns the
        // array elements as a TextArray (same precedent as
        // jsonb_object_keys). The parser rewrites the FROM-clause
        // SRF form into `unnest(<this>(arg))`, so `SELECT * FROM
        // jsonb_array_elements('[…]')` emits one row per element.
        // `_text`: JSON null → SQL NULL, scalars → lexeme; plain:
        // every element as compact JSON text.
        "jsonb_array_elements"
        | "json_array_elements"
        | "jsonb_array_elements_text"
        | "json_array_elements_text" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            let as_text = name.ends_with("_text");
            let items = crate::json::array_element_rows(&args[0], as_text, name)?;
            Ok(Value::TextArray(items))
        }
        // v7.37.17 (17.6 siblings) — pg_column_size(v) returns the
        // storage size of a value in bytes. Real implementation for
        // the value types SPG carries in-line (int / bigint / float /
        // text / bytea / bool / null); the value tree doesn't yet
        // carry TOAST length so composite/array types get their
        // ser bytes size approximation via alloc::format.
        "pg_column_size" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("pg_column_size() takes 1 arg, got {}", args.len()),
                });
            }
            let size = match &args[0] {
                Value::Null => 0i32,
                Value::Bool(_) => 1,
                Value::SmallInt(_) => 2,
                Value::Int(_) => 4,
                Value::BigInt(_) => 8,
                Value::Float(_) => 8,
                Value::Text(s) => {
                    // PG includes the 4-byte length header for varlena.
                    (s.len() as i32).saturating_add(4)
                }
                Value::Bytes(b) => (b.len() as i32).saturating_add(4),
                other => {
                    // Fallback: format the value to estimate byte size
                    // (composite / array / range).
                    let s = alloc::format!("{other:?}");
                    (s.len() as i32).saturating_add(4)
                }
            };
            Ok(Value::Int(size))
        }
        // v7.37.17 (17.6 siblings) — pg_column_compression(v) — was
        // added in PG 14. SPG doesn't yet run per-column compression;
        // pg_dump / monitoring queries commonly emit this alongside
        // pg_column_size. Returns 'p' (plain) for text-like values,
        // NULL for others.
        "pg_column_compression" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "pg_column_compression() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Text(_) | Value::Bytes(_) => Ok(Value::text::<String>("plain".into())),
                _ => Ok(Value::Null),
            }
        }
        // v7.37.17 (17.6 siblings) — pg_relation_filepath /
        // pg_relation_filenode probes used by monitoring exporters.
        // SPG uses a segment-id + tier scheme, not PG's on-disk
        // filepath — the returned value is a virtual "spg://<name>"
        // marker so exporters can display something (real filepath
        // would leak the segment store to callers, which is not
        // meaningful outside the engine).
        "pg_relation_filepath" => Ok(Value::text::<String>("spg://storage".into())),
        "pg_relation_filenode" => Ok(Value::BigInt(0)),
        // v7.37.17 (17.6 siblings) — pg_get_backend_memory_contexts()
        // / pg_backend_memory_contexts() — return NULL (no memory
        // context tree exposed today). Monitoring queries typically
        // fall back gracefully.
        "pg_get_backend_memory_contexts" | "pg_backend_memory_contexts" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — pg_ls_dir / pg_ls_waldir /
        // pg_read_file / pg_read_binary_file — filesystem probes.
        // SPG doesn't expose the underlying store as PG-shaped paths;
        // return NULL. Admin tools that call these get a NULL back
        // instead of an "unknown function" error.
        "pg_ls_dir"
        | "pg_ls_waldir"
        | "pg_ls_logdir"
        | "pg_ls_tmpdir"
        | "pg_ls_archive_statusdir"
        | "pg_read_file"
        | "pg_read_binary_file"
        | "pg_stat_file" => Ok(Value::Null),
        // pg_tablespace_location(oid) — the filesystem path of a
        // tablespace. PG returns '' for the built-in pg_default /
        // pg_global; SPG parse-accepts tablespaces without location
        // semantics, so '' is the honest PG-shaped answer for all.
        "pg_tablespace_location" => match args.first() {
            Some(Value::Null) => Ok(Value::Null),
            _ => Ok(Value::text::<String>("".into())),
        },
        // pg_tablespace_databases(oid) — SRF of database oids in a
        // tablespace; pg_filenode_relation — filenode → regclass.
        // NULL for the scalar surface.
        "pg_tablespace_databases" | "pg_filenode_relation" => Ok(Value::Null),
        // pageinspect extension surface — raw page readers used in
        // corruption forensics. SPG's storage is not page-organized
        // the way PG's heap is; NULL keeps forensic runbooks
        // parse-through (SPG's own equivalent is pg_amcheck's
        // verify_heapam / bt_index_check which ARE real here).
        "get_raw_page"
        | "page_header"
        | "page_checksum"
        | "fsm_page_contents"
        | "heap_page_items"
        | "heap_page_item_attrs"
        | "heap_tuple_infomask_flags"
        | "bt_metap"
        | "bt_page_stats"
        | "bt_page_items"
        | "bt_multi_page_stats"
        | "brin_page_type"
        | "brin_metapage_info"
        | "brin_revmap_data"
        | "brin_page_items"
        | "gin_metapage_info"
        | "gin_page_opaque_info"
        | "gin_leafpage_items"
        | "gist_page_opaque_info"
        | "gist_page_items"
        | "gist_page_items_bytea"
        | "hash_page_type"
        | "hash_page_stats"
        | "hash_page_items"
        | "hash_bitmap_info"
        | "hash_metapage_info" => Ok(Value::Null),
        // pgstattuple / pg_prewarm / pg_buffercache extension
        // probes. pgstattuple's record surface → NULL; pg_relpages
        // and pg_prewarm return counts → 0 (SPG has no PG-shaped
        // page cache to report or warm).
        "pgstattuple"
        | "pgstattuple_approx"
        | "pgstatindex"
        | "pgstatginindex"
        | "pgstathashindex"
        | "pg_buffercache_summary"
        | "pg_buffercache_usage_counts"
        | "pg_buffercache_evict" => Ok(Value::Null),
        "pg_prewarm" => Ok(Value::BigInt(0)),
        // pg_relpages — same 8 KiB-page meter pg_class.relpages
        // reports (hot_bytes / 8192).
        "pg_relpages" => {
            let name_arg = match args.first() {
                None | Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Text(s)) => s.as_ref(),
                Some(_) => return Ok(Value::BigInt(0)),
            };
            let Some(cat) = ctx.catalog else {
                return Ok(Value::BigInt(0));
            };
            let bare = name_arg
                .strip_prefix("public.")
                .unwrap_or(name_arg)
                .trim_matches('"');
            match cat.get(bare) {
                Some(t) => {
                    Ok(Value::BigInt(t.hot_bytes().div_ceil(8192) as i64))
                }
                None => Ok(Value::Null),
            }
        }
        // v7.37.17 (17.6 siblings) — factorial(smallint | int | bigint)
        // returns n! as BIGINT. Overflows at n=20 for i64 — errors
        // beyond that. Negative n = error (matches PG).
        "factorial" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("factorial() takes 1 arg, got {}", args.len()),
                });
            }
            let n = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::SmallInt(x) => i64::from(*x),
                Value::Int(x) => i64::from(*x),
                Value::BigInt(x) => *x,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "factorial() needs integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if n < 0 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("factorial(): {n} is negative"),
                });
            }
            if n > 20 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("factorial({n}): result overflows BIGINT"),
                });
            }
            let mut r: i64 = 1;
            for k in 2..=n {
                r = r.saturating_mul(k);
            }
            Ok(Value::BigInt(r))
        }
        // v7.37.17 (17.6 siblings) — width_bucket(operand, low,
        // high, count) returns the bucket number that a value would
        // fall in given a histogram of `count` equal-width buckets
        // over [low, high]. Values < low return 0; values >= high
        // return count+1 (matches PG semantics).
        "width_bucket" => {
            // v7.38 (read01) — the array form
            // `width_bucket(operand, thresholds[])` returns the number of
            // (ascending-sorted) thresholds ≤ operand, i.e. the bucket the
            // operand falls into. NULL element thresholds are skipped.
            if args.len() == 2 {
                if args.iter().any(|v| matches!(v, Value::Null)) {
                    return Ok(Value::Null);
                }
                let op = value_to_f64(&args[0]).ok_or_else(|| EvalError::TypeMismatch {
                    detail: "width_bucket(): operand must be numeric".into(),
                })?;
                let Some(n) = array_len(&args[1]) else {
                    return Err(EvalError::TypeMismatch {
                        detail: "width_bucket(): second argument must be a numeric array".into(),
                    });
                };
                // An inline `ARRAY[1.0, 3.0, …]` of bare decimals is
                // inferred as a TEXT[], so parse text thresholds too.
                let as_f64 = |v: &Value| -> Option<f64> {
                    value_to_f64(v).or_else(|| match v {
                        Value::Text(s) => s.trim().parse::<f64>().ok(),
                        _ => None,
                    })
                };
                let mut bucket = 0i32;
                for i in 0..n {
                    let elem = array_element_at(&args[1], i).unwrap_or(Value::Null);
                    if let Some(t) = as_f64(&elem)
                        && op >= t
                    {
                        bucket += 1;
                    }
                }
                return Ok(Value::Int(bucket));
            }
            if args.len() != 4 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "width_bucket() takes 4 args, got {}",
                        args.len()
                    ),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let op = value_to_f64(&args[0]).ok_or_else(|| EvalError::TypeMismatch {
                detail: "width_bucket(): operand must be numeric".into(),
            })?;
            let low = value_to_f64(&args[1]).ok_or_else(|| EvalError::TypeMismatch {
                detail: "width_bucket(): low must be numeric".into(),
            })?;
            let high = value_to_f64(&args[2]).ok_or_else(|| EvalError::TypeMismatch {
                detail: "width_bucket(): high must be numeric".into(),
            })?;
            let count = match &args[3] {
                Value::SmallInt(n) => i64::from(*n),
                Value::Int(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "width_bucket(): count must be integer".into(),
                    });
                }
            };
            if count <= 0 {
                return Err(EvalError::TypeMismatch {
                    detail: "width_bucket(): count must be > 0".into(),
                });
            }
            if low == high {
                return Err(EvalError::TypeMismatch {
                    detail: "width_bucket(): low must differ from high".into(),
                });
            }
            // PG allows low > high by inverting the direction; we
            // do the same via a simple compare-and-scale.
            let (lo, hi, ascending) = if low < high {
                (low, high, true)
            } else {
                (high, low, false)
            };
            if op < lo {
                return Ok(Value::Int(if ascending { 0 } else { count as i32 + 1 }));
            }
            if op >= hi {
                return Ok(Value::Int(if ascending { count as i32 + 1 } else { 0 }));
            }
            let frac = (op - lo) / (hi - lo);
            let bucket = (frac * (count as f64)) as i64 + 1;
            let bucket = if ascending {
                bucket
            } else {
                count - bucket + 1
            };
            Ok(Value::Int(bucket as i32))
        }
        // v7.37.17 (17.6 siblings) — chr(int) / ascii(text) /
        // initcap(text). PG-standard string builders.
        "chr" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("chr() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => {
                    let code = u32::try_from(*n).map_err(|_| EvalError::TypeMismatch {
                        detail: alloc::format!("chr(): {n} out of range"),
                    })?;
                    let ch = char::from_u32(code).ok_or_else(|| EvalError::TypeMismatch {
                        detail: alloc::format!("chr(): {code} is not a valid Unicode code point"),
                    })?;
                    let mut s = alloc::string::String::new();
                    s.push(ch);
                    Ok(Value::text(s))
                }
                Value::BigInt(n) => {
                    let code = u32::try_from(*n).map_err(|_| EvalError::TypeMismatch {
                        detail: alloc::format!("chr(): {n} out of range"),
                    })?;
                    let ch = char::from_u32(code).ok_or_else(|| EvalError::TypeMismatch {
                        detail: alloc::format!("chr(): {code} is not a valid Unicode code point"),
                    })?;
                    let mut s = alloc::string::String::new();
                    s.push(ch);
                    Ok(Value::text(s))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "chr() needs integer, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "ascii" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("ascii() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    // PG: ascii('') is 0 (no first character), not an error.
                    Ok(Value::Int(s.chars().next().map_or(0, |ch| ch as i32)))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "ascii() needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "initcap" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("initcap() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    // PG semantics: capitalize first char of every
                    // word, lowercase the rest. Word boundary = any
                    // non-alphanumeric transition.
                    let mut out = alloc::string::String::with_capacity(s.len());
                    let mut at_word_start = true;
                    for ch in s.chars() {
                        if ch.is_alphanumeric() {
                            if at_word_start {
                                for c in ch.to_uppercase() {
                                    out.push(c);
                                }
                                at_word_start = false;
                            } else {
                                for c in ch.to_lowercase() {
                                    out.push(c);
                                }
                            }
                        } else {
                            out.push(ch);
                            at_word_start = true;
                        }
                    }
                    Ok(Value::text(out))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "initcap() needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — PG math functions.
        //
        // ln(x)     — natural log
        // log(x)    — log base 10 (PG default)
        // log10(x)  — explicit log base 10
        // log(b, x) — log base b (two-arg form)
        // exp(x)    — e^x
        // cbrt(x)   — cube root
        // pi()      — π
        // gcd(a, b) — greatest common divisor (BIGINT)
        // lcm(a, b) — least common multiple (BIGINT)
        // radians(x) / degrees(x)
        "ln" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("ln() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                v => {
                    let x = value_to_f64(v).ok_or_else(|| EvalError::TypeMismatch {
                        detail: alloc::format!("ln() needs numeric, got {:?}", v.data_type()),
                    })?;
                    if x <= 0.0 {
                        return Err(EvalError::TypeMismatch {
                            detail: "ln(): input must be > 0".into(),
                        });
                    }
                    Ok(Value::Float(f64_ln(x)))
                }
            }
        }
        "log" | "log10" => {
            let arg_count = args.len();
            if arg_count == 1 {
                match &args[0] {
                    Value::Null => Ok(Value::Null),
                    v => {
                        let x = value_to_f64(v).ok_or_else(|| EvalError::TypeMismatch {
                            detail: alloc::format!("{name}() needs numeric, got {:?}", v.data_type()),
                        })?;
                        if x <= 0.0 {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!("{name}(): input must be > 0"),
                            });
                        }
                        // PG's log(x) / log10(x) is the dedicated
                        // base-10 log — use libm::log10 (matching PG's
                        // C libm) so exact powers of ten land on whole
                        // numbers (log10(1000) = 3, not the
                        // 2.9999999999999996 that ln(x)/ln(10) yields).
                        Ok(Value::Float(libm::log10(x)))
                    }
                }
            } else if arg_count == 2 {
                if args.iter().any(|v| matches!(v, Value::Null)) {
                    return Ok(Value::Null);
                }
                let b = value_to_f64(&args[0]).ok_or_else(|| EvalError::TypeMismatch {
                    detail: "log(base, x) needs numeric base".into(),
                })?;
                let x = value_to_f64(&args[1]).ok_or_else(|| EvalError::TypeMismatch {
                    detail: "log(base, x) needs numeric x".into(),
                })?;
                if b <= 0.0 || b == 1.0 || x <= 0.0 {
                    return Err(EvalError::TypeMismatch {
                        detail: "log(): base must be > 0 and != 1, x must be > 0".into(),
                    });
                }
                Ok(Value::Float(f64_ln(x) / f64_ln(b)))
            } else {
                Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 or 2 args, got {arg_count}"),
                })
            }
        }
        "exp" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("exp() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                v => {
                    let x = value_to_f64(v).ok_or_else(|| EvalError::TypeMismatch {
                        detail: alloc::format!("exp() needs numeric, got {:?}", v.data_type()),
                    })?;
                    Ok(Value::Float(f64_exp(x)))
                }
            }
        }
        "cbrt" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("cbrt() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                v => {
                    let x = value_to_f64(v).ok_or_else(|| EvalError::TypeMismatch {
                        detail: alloc::format!("cbrt() needs numeric, got {:?}", v.data_type()),
                    })?;
                    // libm::cbrt is the accurate C-libm cube root PG
                    // itself calls — it round-trips perfect cubes
                    // exactly (cbrt(27) = 3, not 3.0000000000000004 as
                    // the exp(ln(|x|)/3) approximation gave) and handles
                    // the sign of negative inputs natively.
                    Ok(Value::Float(libm::cbrt(x)))
                }
            }
        }
        "pi" => {
            if !args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: format!("pi() takes no args, got {}", args.len()),
                });
            }
            Ok(Value::Float(core::f64::consts::PI))
        }
        // point(x, y) — the geometric-point constructor.
        "point" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("point() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let x = value_to_f64(&args[0]).ok_or_else(|| EvalError::TypeMismatch {
                detail: "point() needs numeric x".into(),
            })?;
            let y = value_to_f64(&args[1]).ok_or_else(|| EvalError::TypeMismatch {
                detail: "point() needs numeric y".into(),
            })?;
            Ok(Value::Point(spg_storage::Point2D { x, y }))
        }
        // v7.38 (read01) — two-argument geometric constructors (PG). The
        // one-argument spellings (`circle('<...>')`) fall through to the
        // function-style typecast instead.
        "circle" if args.len() == 2 => {
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let Value::Point(center) = &args[0] else {
                return Err(EvalError::TypeMismatch {
                    detail: "circle(point, radius) needs a point centre".into(),
                });
            };
            let radius = value_to_f64(&args[1]).ok_or_else(|| EvalError::TypeMismatch {
                detail: "circle(point, radius) needs a numeric radius".into(),
            })?;
            Ok(Value::Circle {
                center: *center,
                radius,
            })
        }
        "box" if args.len() == 2 => {
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let (Value::Point(a), Value::Point(b)) = (&args[0], &args[1]) else {
                return Err(EvalError::TypeMismatch {
                    detail: "box(point, point) needs two points".into(),
                });
            };
            // PG stores a box as (upper-right, lower-left).
            let ur = spg_storage::Point2D {
                x: a.x.max(b.x),
                y: a.y.max(b.y),
            };
            let ll = spg_storage::Point2D {
                x: a.x.min(b.x),
                y: a.y.min(b.y),
            };
            Ok(Value::PgBox(ur, ll))
        }
        "lseg" if args.len() == 2 => {
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let (Value::Point(a), Value::Point(b)) = (&args[0], &args[1]) else {
                return Err(EvalError::TypeMismatch {
                    detail: "lseg(point, point) needs two points".into(),
                });
            };
            Ok(Value::Lseg(*a, *b))
        }
        // npoints(path | polygon) — the vertex count.
        "npoints" if args.len() == 1 => match &args[0] {
            Value::Null => Ok(Value::Null),
            Value::Path { points, .. } => Ok(Value::Int(
                i32::try_from(points.len()).unwrap_or(i32::MAX),
            )),
            Value::Polygon(points) => Ok(Value::Int(
                i32::try_from(points.len()).unwrap_or(i32::MAX),
            )),
            v => Err(EvalError::TypeMismatch {
                detail: alloc::format!("npoints() not defined for {:?}", v.data_type()),
            }),
        },
        // Geometric accessors over box / circle / lseg. (`length(lseg)` is
        // handled in the `length` arm above, which the text form shares.)
        "area" | "width" | "height" | "center" | "radius" | "diameter"
        | "isvertical" | "ishorizontal"
            if args.len() == 1
                && matches!(
                    &args[0],
                    Value::PgBox(..) | Value::Circle { .. } | Value::Lseg(..)
                ) =>
        {
            let pt = |x: f64, y: f64| Value::Point(spg_storage::Point2D { x, y });
            match (name, &args[0]) {
                ("area", Value::PgBox(a, b)) => {
                    Ok(Value::Float((a.x - b.x).abs() * (a.y - b.y).abs()))
                }
                ("area", Value::Circle { radius, .. }) => {
                    Ok(Value::Float(core::f64::consts::PI * radius * radius))
                }
                ("width", Value::PgBox(a, b)) => Ok(Value::Float((a.x - b.x).abs())),
                ("height", Value::PgBox(a, b)) => Ok(Value::Float((a.y - b.y).abs())),
                ("center", Value::PgBox(a, b)) => Ok(pt((a.x + b.x) / 2.0, (a.y + b.y) / 2.0)),
                ("center", Value::Circle { center, .. }) => Ok(pt(center.x, center.y)),
                ("radius", Value::Circle { radius, .. }) => Ok(Value::Float(*radius)),
                ("diameter", Value::Circle { radius, .. }) => Ok(Value::Float(2.0 * radius)),
                ("isvertical", Value::Lseg(a, b)) => Ok(Value::Bool(a.x == b.x)),
                ("ishorizontal", Value::Lseg(a, b)) => Ok(Value::Bool(a.y == b.y)),
                (n, v) => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("{n}() not defined for {:?}", v.data_type()),
                }),
            }
        }
        "gcd" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("gcd() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            fn to_i64(v: &Value<'_>) -> Result<i64, EvalError> {
                match v {
                    Value::Int(n) => Ok(*n as i64),
                    Value::BigInt(n) => Ok(*n),
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "gcd/lcm need integer inputs, got {:?}",
                            other.data_type()
                        ),
                    }),
                }
            }
            let mut a = to_i64(&args[0])?.unsigned_abs();
            let mut b = to_i64(&args[1])?.unsigned_abs();
            while b != 0 {
                let t = b;
                b = a % b;
                a = t;
            }
            Ok(Value::BigInt(a as i64))
        }
        "lcm" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("lcm() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            fn to_i64(v: &Value<'_>) -> Result<i64, EvalError> {
                match v {
                    Value::Int(n) => Ok(*n as i64),
                    Value::BigInt(n) => Ok(*n),
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "lcm() needs integer inputs, got {:?}",
                            other.data_type()
                        ),
                    }),
                }
            }
            let a = to_i64(&args[0])?.unsigned_abs();
            let b = to_i64(&args[1])?.unsigned_abs();
            if a == 0 || b == 0 {
                return Ok(Value::BigInt(0));
            }
            let mut x = a;
            let mut y = b;
            while y != 0 {
                let t = y;
                y = x % y;
                x = t;
            }
            let g = x;
            let lcm = (a / g).saturating_mul(b);
            Ok(Value::BigInt(lcm as i64))
        }
        "radians" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("radians() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                v => {
                    let x = value_to_f64(v).ok_or_else(|| EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "radians() needs numeric, got {:?}",
                            v.data_type()
                        ),
                    })?;
                    Ok(Value::Float(x * core::f64::consts::PI / 180.0))
                }
            }
        }
        "degrees" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("degrees() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                v => {
                    let x = value_to_f64(v).ok_or_else(|| EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "degrees() needs numeric, got {:?}",
                            v.data_type()
                        ),
                    })?;
                    Ok(Value::Float(x * 180.0 / core::f64::consts::PI))
                }
            }
        }
        // v7.37.17 (17.6 siblings) — to_hex(int|bigint) — PG's
        // integer-to-hex-string conversion. Returns TEXT.
        "to_hex" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("to_hex() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::text(alloc::format!("{:x}", *n as u32))),
                Value::BigInt(n) => Ok(Value::text(alloc::format!("{:x}", *n as u64))),
                other => Err(EvalError::TypeMismatch {
                    detail: format!("to_hex() needs int/bigint, got {:?}", other.data_type()),
                }),
            }
        }
        // v7.38 (read01 sweep) — PG 16 to_bin() / to_oct(): the binary /
        // octal string of an int4 / int8 (two's-complement width, matching
        // to_hex's `as u32` / `as u64`).
        "to_bin" | "to_oct" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            let oct = name == "to_oct";
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::text(if oct {
                    alloc::format!("{:o}", *n as u32)
                } else {
                    alloc::format!("{:b}", *n as u32)
                })),
                Value::BigInt(n) => Ok(Value::text(if oct {
                    alloc::format!("{:o}", *n as u64)
                } else {
                    alloc::format!("{:b}", *n as u64)
                })),
                other => Err(EvalError::TypeMismatch {
                    detail: format!("{name}() needs int/bigint, got {:?}", other.data_type()),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — pgcrypto gen_random_bytes(n)
        // returns n cryptographically-random bytes. SPG uses the
        // internal prng_next_u64() splitter (same underlying
        // pool used by random() and gen_random_uuid()) which is
        // seeded from wall-clock on Engine construction and is
        // fine for auth-token style usage. Real system-entropy
        // hardening queues with the crypto epic.
        // "random_bytes" is MySQL's spelling of gen_random_bytes.
        "gen_random_bytes" | "random_bytes" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "gen_random_bytes() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            let n = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::SmallInt(x) => i64::from(*x),
                Value::Int(x) => i64::from(*x),
                Value::BigInt(x) => *x,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "gen_random_bytes(): needs integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if n < 0 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "gen_random_bytes(): {n} is negative"
                    ),
                });
            }
            // PG's gen_random_bytes caps at 1024 bytes. Match that.
            if n > 1024 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "gen_random_bytes(): {n} exceeds cap of 1024 bytes"
                    ),
                });
            }
            let n = n as usize;
            let mut buf = alloc::vec::Vec::with_capacity(n);
            // v7.38 (read01 P5.24) — draw cryptographic bytes from the host
            // CSPRNG (server injects /dev/urandom). Only if none is attached
            // do we fall back to the non-cryptographic xorshift PRNG.
            if let Some(salt) = ctx.salt_fn {
                while buf.len() < n {
                    for b in salt() {
                        if buf.len() < n {
                            buf.push(b);
                        }
                    }
                }
            } else {
                while buf.len() < n {
                    let word = super::math::prng_next_u64();
                    for b in word.to_le_bytes() {
                        if buf.len() < n {
                            buf.push(b);
                        }
                    }
                }
            }
            Ok(Value::Bytes(buf.into()))
        }
        // pgcrypto gen_salt(algo [, iter]) returns a random salt
        // for the crypt() function. SPG doesn't yet ship crypt()
        // (bcrypt/blowfish support queues with the crypto epic);
        // return a stub-shape 22-char text so ORMs that call it
        // don't error at parse time.
        // gen_salt(type[, iter]) — random salt for crypt(). The
        // 'md5' scheme is real (matching the real md5crypt in
        // crypt() below); bf/des/xdes error honestly — their
        // ciphers (blowfish/DES) queue with the pgcrypto epic.
        "gen_salt" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "gen_salt() takes 1 or 2 args, got {}",
                        args.len()
                    ),
                });
            }
            let scheme = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_lowercase(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "gen_salt(): type must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            match scheme.as_str() {
                "md5" => {
                    const ITOA64: &[u8; 64] =
                        b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
                    let mut salt = alloc::string::String::from("$1$");
                    // v7.38 (read01 P5.24) — cryptographic salt from the host
                    // CSPRNG when available, PRNG only as a last resort.
                    let mut bits = if let Some(salt_fn) = ctx.salt_fn {
                        let s = salt_fn();
                        u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
                    } else {
                        super::math::prng_next_u64()
                    };
                    for _ in 0..8 {
                        salt.push(ITOA64[(bits & 63) as usize] as char);
                        bits >>= 6;
                    }
                    salt.push('$');
                    Ok(Value::text(salt))
                }
                "bf" | "des" | "xdes" => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "gen_salt('{scheme}'): scheme not yet supported — \
                         blowfish/DES ciphers queue with the pgcrypto epic; \
                         use gen_salt('md5')"
                    ),
                }),
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "gen_salt(): unknown salt type {other:?}"
                    ),
                }),
            }
        }
        // crypt(password, salt) — pgcrypto password hashing. The
        // '$1$' md5crypt scheme (FreeBSD algorithm) is real via
        // the md-5 crate; verifying is crypt(pw, stored) == stored,
        // exactly like PG. bcrypt/DES salts error honestly.
        "crypt" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("crypt() takes 2 args, got {}", args.len()),
                });
            }
            let (password, salt_full) = match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
                (Value::Text(p), Value::Text(s)) => (p.as_bytes(), s.as_ref()),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "crypt() takes 2 TEXT args".into(),
                    });
                }
            };
            let Some(rest) = salt_full.strip_prefix("$1$") else {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "crypt(): only the '$1$' md5crypt scheme is \
                         supported today (got {salt_full:?}); \
                         bcrypt/DES queue with the pgcrypto epic"
                    ),
                });
            };
            // Salt = up to 8 chars, terminated by '$' if present.
            let salt: &[u8] = rest
                .split('$')
                .next()
                .unwrap_or("")
                .as_bytes();
            let salt = &salt[..salt.len().min(8)];

            use md5::{Digest, Md5};
            // FreeBSD md5crypt.
            let mut ctx = Md5::new();
            ctx.update(password);
            ctx.update(b"$1$");
            ctx.update(salt);
            let mut alt = Md5::new();
            alt.update(password);
            alt.update(salt);
            alt.update(password);
            let alt_sum = alt.finalize();
            let mut plen = password.len();
            while plen > 0 {
                ctx.update(&alt_sum[..plen.min(16)]);
                plen = plen.saturating_sub(16);
            }
            let mut i = password.len();
            while i > 0 {
                if i & 1 != 0 {
                    ctx.update([0u8]);
                } else {
                    ctx.update(&password[..1]);
                }
                i >>= 1;
            }
            let mut current: [u8; 16] = ctx.finalize().into();
            for round in 0..1000 {
                let mut c = Md5::new();
                if round & 1 != 0 {
                    c.update(password);
                } else {
                    c.update(current);
                }
                if round % 3 != 0 {
                    c.update(salt);
                }
                if round % 7 != 0 {
                    c.update(password);
                }
                if round & 1 != 0 {
                    c.update(current);
                } else {
                    c.update(password);
                }
                current = c.finalize().into();
            }
            // Custom base64 of rearranged bytes.
            fn to64(out: &mut alloc::string::String, mut v: u32, n: usize) {
                const ITOA64: &[u8; 64] =
                    b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
                for _ in 0..n {
                    out.push(ITOA64[(v & 63) as usize] as char);
                    v >>= 6;
                }
            }
            let mut out = alloc::string::String::from("$1$");
            out.push_str(core::str::from_utf8(salt).unwrap_or(""));
            out.push('$');
            let c = &current;
            to64(
                &mut out,
                (u32::from(c[0]) << 16) | (u32::from(c[6]) << 8) | u32::from(c[12]),
                4,
            );
            to64(
                &mut out,
                (u32::from(c[1]) << 16) | (u32::from(c[7]) << 8) | u32::from(c[13]),
                4,
            );
            to64(
                &mut out,
                (u32::from(c[2]) << 16) | (u32::from(c[8]) << 8) | u32::from(c[14]),
                4,
            );
            to64(
                &mut out,
                (u32::from(c[3]) << 16) | (u32::from(c[9]) << 8) | u32::from(c[15]),
                4,
            );
            to64(
                &mut out,
                (u32::from(c[4]) << 16) | (u32::from(c[10]) << 8) | u32::from(c[5]),
                4,
            );
            to64(&mut out, u32::from(c[11]), 2);
            Ok(Value::text(out))
        }
        // v7.37.17 (17.6 siblings) — pgcrypto hmac(data, key, algo)
        // returns keyed-hash MAC. Uses RustCrypto's `hmac` crate
        // with the sha1/sha2 backends already in the dep graph.
        "hmac" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("hmac() takes 3 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let input: &[u8] = match &args[0] {
                Value::Text(s) => s.as_bytes(),
                Value::Bytes(b) => b.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "hmac(): data must be text or bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let key: &[u8] = match &args[1] {
                Value::Text(s) => s.as_bytes(),
                Value::Bytes(b) => b.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "hmac(): key must be text or bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let algo = match &args[2] {
                Value::Text(s) => s.to_ascii_lowercase(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "hmac(): type must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            use hmac::{Hmac, Mac};
            let out: alloc::vec::Vec<u8> = match algo.as_str() {
                "md5" => {
                    type H = Hmac<md5::Md5>;
                    let mut m = H::new_from_slice(key).map_err(|_| EvalError::TypeMismatch {
                        detail: "hmac(): invalid key length".into(),
                    })?;
                    m.update(input);
                    m.finalize().into_bytes().to_vec()
                }
                "sha1" => {
                    type H = Hmac<sha1::Sha1>;
                    let mut m = H::new_from_slice(key).map_err(|_| EvalError::TypeMismatch {
                        detail: "hmac(): invalid key length".into(),
                    })?;
                    m.update(input);
                    m.finalize().into_bytes().to_vec()
                }
                "sha224" => {
                    type H = Hmac<sha2::Sha224>;
                    let mut m = H::new_from_slice(key).map_err(|_| EvalError::TypeMismatch {
                        detail: "hmac(): invalid key length".into(),
                    })?;
                    m.update(input);
                    m.finalize().into_bytes().to_vec()
                }
                "sha256" => {
                    type H = Hmac<sha2::Sha256>;
                    let mut m = H::new_from_slice(key).map_err(|_| EvalError::TypeMismatch {
                        detail: "hmac(): invalid key length".into(),
                    })?;
                    m.update(input);
                    m.finalize().into_bytes().to_vec()
                }
                "sha384" => {
                    type H = Hmac<sha2::Sha384>;
                    let mut m = H::new_from_slice(key).map_err(|_| EvalError::TypeMismatch {
                        detail: "hmac(): invalid key length".into(),
                    })?;
                    m.update(input);
                    m.finalize().into_bytes().to_vec()
                }
                "sha512" => {
                    type H = Hmac<sha2::Sha512>;
                    let mut m = H::new_from_slice(key).map_err(|_| EvalError::TypeMismatch {
                        detail: "hmac(): invalid key length".into(),
                    })?;
                    m.update(input);
                    m.finalize().into_bytes().to_vec()
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "hmac(): unsupported algorithm {other:?}; use md5/sha1/sha224/sha256/sha384/sha512"
                        ),
                    });
                }
            };
            Ok(Value::Bytes(out.into()))
        }
        // v7.37.17 (17.6 siblings) — pgcrypto digest(data, type)
        // returns the hash of data using the named algorithm. This
        // is PG's pgcrypto extension surface but many apps + ORMs
        // use it via the built-in sha functions we already ship.
        // Recognize the standard pgcrypto names.
        "digest" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("digest() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let input: &[u8] = match &args[0] {
                Value::Text(s) => s.as_bytes(),
                Value::Bytes(b) => b.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "digest(): data must be text or bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let algo = match &args[1] {
                Value::Text(s) => s.to_ascii_lowercase(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "digest(): type must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let out: alloc::vec::Vec<u8> = match algo.as_str() {
                "md5" => {
                    use md5::{Digest, Md5};
                    let mut h = Md5::new();
                    h.update(input);
                    h.finalize().to_vec()
                }
                "sha1" => {
                    use sha1::{Digest, Sha1};
                    let mut h = Sha1::new();
                    h.update(input);
                    h.finalize().to_vec()
                }
                "sha224" => {
                    use sha2::{Digest, Sha224};
                    let mut h = Sha224::new();
                    h.update(input);
                    h.finalize().to_vec()
                }
                "sha256" => {
                    use sha2::{Digest, Sha256};
                    let mut h = Sha256::new();
                    h.update(input);
                    h.finalize().to_vec()
                }
                "sha384" => {
                    use sha2::{Digest, Sha384};
                    let mut h = Sha384::new();
                    h.update(input);
                    h.finalize().to_vec()
                }
                "sha512" => {
                    use sha2::{Digest, Sha512};
                    let mut h = Sha512::new();
                    h.update(input);
                    h.finalize().to_vec()
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "digest(): unsupported algorithm {other:?}; use md5/sha1/sha224/sha256/sha384/sha512"
                        ),
                    });
                }
            };
            Ok(Value::Bytes(out.into()))
        }
        // v7.37.17 (17.6 siblings) — PG's built-in md5(text|bytea)
        // returns the 32-char lowercase hex digest text (matches
        // PG default), NOT the raw bytes like sha256 does. This is
        // the historical PG spec: md5() is text-in / text-out.
        "md5" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("md5() takes 1 arg, got {}", args.len()),
                });
            }
            use md5::{Digest, Md5};
            let input: &[u8] = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.as_bytes(),
                Value::Bytes(b) => b.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "md5() needs text or bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let mut h = Md5::new();
            h.update(input);
            let digest = h.finalize();
            let mut hex = alloc::string::String::with_capacity(32);
            for b in digest.iter() {
                use core::fmt::Write;
                let _ = write!(hex, "{b:02x}");
            }
            Ok(Value::text(hex))
        }
        // v7.37.17 (17.6 siblings) — MySQL TO_BASE64 / FROM_BASE64.
        // MySQL wraps the encoded form at 76 chars per line; the
        // decoder tolerates whitespace. FROM_BASE64 returns binary
        // (Bytes); invalid input → NULL, not an error.
        "to_base64" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("to_base64() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(_) | Value::Bytes(_) => {
                    let encoded = super::encoding::encode_text(&[
                        args[0].clone().into_owned(),
                        Value::text(alloc::string::String::from("base64")),
                    ])?;
                    let Value::Text(flat) = encoded else {
                        unreachable!("encode_text returns Text");
                    };
                    let mut wrapped = alloc::string::String::with_capacity(flat.len() + 8);
                    for (i, c) in flat.chars().enumerate() {
                        if i > 0 && i % 76 == 0 {
                            wrapped.push('\n');
                        }
                        wrapped.push(c);
                    }
                    Ok(Value::text(wrapped))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "to_base64() needs text or bytea, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "from_base64" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("from_base64() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let compact: alloc::string::String =
                        s.chars().filter(|c| !c.is_whitespace()).collect();
                    match super::encoding::decode_text(&[
                        Value::text(compact),
                        Value::text(alloc::string::String::from("base64")),
                    ]) {
                        Ok(Value::Text(t)) => {
                            Ok(Value::Bytes(t.as_bytes().to_vec().into()))
                        }
                        Ok(other) => Ok(other),
                        // MySQL: invalid base64 → NULL.
                        Err(_) => Ok(Value::Null),
                    }
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "from_base64() needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — MySQL SHA(str) (hex text — MySQL
        // semantics, unlike PG's bytes-returning sha1) and
        // SHA2(str, bits) with bits 0|224|256|384|512 (0 = 256).
        "sha" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("sha() takes 1 arg, got {}", args.len()),
                });
            }
            use sha1::{Digest, Sha1};
            let input: &[u8] = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.as_bytes(),
                Value::Bytes(b) => b.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "sha() needs text or bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let mut h = Sha1::new();
            h.update(input);
            let mut hex = alloc::string::String::with_capacity(40);
            for b in h.finalize() {
                hex.push_str(&alloc::format!("{b:02x}"));
            }
            Ok(Value::text(hex))
        }
        "sha2" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("sha2() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|a| matches!(a, Value::Null)) {
                return Ok(Value::Null);
            }
            use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};
            let input: &[u8] = match &args[0] {
                Value::Text(s) => s.as_bytes(),
                Value::Bytes(b) => b.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "sha2() needs text or bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let bits = match &args[1] {
                Value::Int(n) => i64::from(*n),
                Value::SmallInt(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "sha2() bits must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let digest: alloc::vec::Vec<u8> = match bits {
                0 | 256 => Sha256::digest(input).to_vec(),
                224 => Sha224::digest(input).to_vec(),
                384 => Sha384::digest(input).to_vec(),
                512 => Sha512::digest(input).to_vec(),
                // MySQL: unsupported bit length → NULL.
                _ => return Ok(Value::Null),
            };
            let mut hex = alloc::string::String::with_capacity(digest.len() * 2);
            for b in digest {
                hex.push_str(&alloc::format!("{b:02x}"));
            }
            Ok(Value::text(hex))
        }
        // MySQL LOAD_FILE: NULL without the FILE privilege /
        // secure_file_priv access — the shape unprivileged clients
        // always see. SPG does not expose host-filesystem reads
        // through SQL.
        "load_file" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — PG cryptographic hash functions.
        // sha1 is already in the dep graph (users.rs MySQL auth);
        // sha2 provides sha224/sha256/sha384/sha512. Hex output
        // matches PG's `encode(digest(x, 'sha256'), 'hex')` shape
        // that PostgreSQL 15+ built-in `sha256(x)` uses.
        "sha1" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("sha1() takes 1 arg, got {}", args.len()),
                });
            }
            use sha1::{Digest, Sha1};
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let mut h = Sha1::new();
                    h.update(s.as_bytes());
                    Ok(Value::Bytes(h.finalize().to_vec().into()))
                }
                Value::Bytes(b) => {
                    let mut h = Sha1::new();
                    h.update(b.as_ref());
                    Ok(Value::Bytes(h.finalize().to_vec().into()))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!("sha1() needs text or bytea, got {:?}", other.data_type()),
                }),
            }
        }
        "sha224" | "sha256" | "sha384" | "sha512" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};
            let input: &[u8] = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.as_bytes(),
                Value::Bytes(b) => b.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "{name}() needs text or bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let out: alloc::vec::Vec<u8> = match name {
                "sha224" => {
                    let mut h = Sha224::new();
                    h.update(input);
                    h.finalize().to_vec()
                }
                "sha256" => {
                    let mut h = Sha256::new();
                    h.update(input);
                    h.finalize().to_vec()
                }
                "sha384" => {
                    let mut h = Sha384::new();
                    h.update(input);
                    h.finalize().to_vec()
                }
                "sha512" => {
                    let mut h = Sha512::new();
                    h.update(input);
                    h.finalize().to_vec()
                }
                _ => unreachable!(),
            };
            Ok(Value::Bytes(out.into()))
        }
        // v7.37.17 (17.6 siblings) — PG 14+ bit_count(x) counts
        // the 1-bits (popcount) in a bytea or integer input.
        "bit_count" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("bit_count() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::SmallInt(n) => Ok(Value::BigInt(
                    (*n as i16).count_ones() as i64
                )),
                Value::Int(n) => Ok(Value::BigInt(n.count_ones() as i64)),
                Value::BigInt(n) => Ok(Value::BigInt(n.count_ones() as i64)),
                Value::Bytes(b) => {
                    let mut c: i64 = 0;
                    for byte in b.iter() {
                        c += byte.count_ones() as i64;
                    }
                    Ok(Value::BigInt(c))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "bit_count() needs integer or bytea, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — SQL:2003 BIT_LENGTH(x) is
        // OCTET_LENGTH(x) * 8. Uses the same input-type accepting
        // rules — TEXT (UTF-8 bytes) or BYTEA.
        "bit_length" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("bit_length() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let bytes = s.len();
                    let bits = bytes.saturating_mul(8);
                    let n = i32::try_from(bits).unwrap_or(i32::MAX);
                    Ok(Value::Int(n))
                }
                Value::Bytes(b) => {
                    let bytes = b.len();
                    let bits = bytes.saturating_mul(8);
                    let n = i32::try_from(bits).unwrap_or(i32::MAX);
                    Ok(Value::Int(n))
                }
                // PG bit_length(bit) is the bit count itself, not ×8.
                Value::BitString { nbits, .. } => {
                    Ok(Value::Int(i32::try_from(*nbits).unwrap_or(i32::MAX)))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "bit_length() needs text or bytea, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.11.6 — `array_length(arr, dim)` returns the element
        // count of `arr` along dimension `dim`. v7.11 only models
        // single-dimension arrays so dim must be 1 (otherwise NULL,
        // matching PG semantics for unsupported dimensions). NULL
        // array → NULL. v7.11 TEXT[] only; non-array operand is
        // a type mismatch.
        // v7.37.7 C.1.6 — `cardinality(arr)` is PG-standard for total
        // element count across all dimensions. SPG v7.11 models only
        // single-dim arrays, so the answer equals array_length(arr, 1)
        // for non-NULL arrays; NULL array → NULL. Routed here as a
        // synonym so dashboards / regression tools written against PG
        // just work.
        "cardinality" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("cardinality() takes 1 arg, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            let Some(len) = array_len(&args[0]) else {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "cardinality() arg must be an array, got {:?}",
                        args[0].data_type()
                    ),
                });
            };
            let n = i32::try_from(len).unwrap_or(i32::MAX);
            Ok(Value::Int(n))
        }
        // PG `array_fill(value, dims [, lower_bounds])` — build an array of
        // `dims[0]` copies of `value`. SPG stores 1-D arrays; multi-dim fill
        // is an honest error. The optional lower-bounds arg is accepted and
        // ignored (SPG arrays are 1-based).
        "array_fill" => {
            if args.len() != 2 && args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_fill() takes 2 or 3 args, got {}", args.len()),
                });
            }
            let dims: &[Option<i32>] = match &args[1] {
                Value::IntArray(d) => d,
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "array_fill(): dimensions must be int[], got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if dims.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: "array_fill(): only 1-dimensional fill is supported".into(),
                });
            }
            let n = dims[0].unwrap_or(0);
            if n < 0 {
                return Err(EvalError::TypeMismatch {
                    detail: "array_fill(): dimension must be non-negative".into(),
                });
            }
            let n = n as usize;
            match &args[0] {
                Value::Int(v) => Ok(Value::IntArray(alloc::vec![Some(*v); n])),
                Value::SmallInt(v) => Ok(Value::IntArray(alloc::vec![Some(i32::from(*v)); n])),
                Value::BigInt(v) => Ok(Value::BigIntArray(alloc::vec![Some(*v); n])),
                Value::Text(s) => Ok(Value::TextArray(alloc::vec![Some(s.to_string()); n])),
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_fill(): unsupported element type {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "array_length" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_length() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            let Some(len) = array_len(&args[0]) else {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_length() first arg must be an array, got {:?}",
                        args[0].data_type()
                    ),
                });
            };
            let dim: i64 = match args[1] {
                Value::Int(n) => i64::from(n),
                Value::BigInt(n) => n,
                Value::SmallInt(n) => i64::from(n),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "array_length() second arg must be integer, got {:?}",
                            args[1].data_type()
                        ),
                    });
                }
            };
            // PG: an empty array has *no* dimensions, so
            // `array_length('{}'::int[], 1)` is NULL — not 0.
            if dim != 1 || len == 0 {
                return Ok(Value::Null);
            }
            let n = i32::try_from(len).unwrap_or(i32::MAX);
            Ok(Value::Int(n))
        }
        // v7.37.17 (17.6 siblings) — array_upper / array_lower /
        // array_ndims / array_dims. PG models multi-dim arrays with
        // per-dim lower/upper bounds; SPG v7.11 only models 1-D
        // arrays so:
        //   array_ndims(arr) → 1 if non-empty, NULL if empty
        //   array_lower(arr, 1) → 1 (PG default), NULL for other dims
        //   array_upper(arr, 1) → length, NULL for other dims
        //   array_dims(arr)  → '[1:N]' text or NULL for empty
        "array_upper" | "array_lower" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            let Some(len) = array_len(&args[0]) else {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "{name}() first arg must be an array, got {:?}",
                        args[0].data_type()
                    ),
                });
            };
            let dim: i64 = match args[1] {
                Value::Int(n) => i64::from(n),
                Value::BigInt(n) => n,
                Value::SmallInt(n) => i64::from(n),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "{name}() second arg must be integer, got {:?}",
                            args[1].data_type()
                        ),
                    });
                }
            };
            if dim != 1 || len == 0 {
                return Ok(Value::Null);
            }
            let result = if name == "array_lower" {
                1
            } else {
                i32::try_from(len).unwrap_or(i32::MAX)
            };
            Ok(Value::Int(result))
        }
        "array_ndims" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_ndims() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                other => match array_len(other) {
                    // PG: an empty array has no dimensions → NULL; a 1-D
                    // array → 1 (SPG only models 1-D arrays).
                    Some(0) => Ok(Value::Null),
                    Some(_) => Ok(Value::Int(1)),
                    None => Err(EvalError::TypeMismatch {
                        detail: format!(
                            "array_ndims() needs array, got {:?}",
                            other.data_type()
                        ),
                    }),
                },
            }
        }
        "array_dims" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_dims() takes 1 arg, got {}", args.len()),
                });
            }
            if matches!(&args[0], Value::Null) {
                return Ok(Value::Null);
            }
            let Some(len) = array_len(&args[0]) else {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_dims() needs array, got {:?}",
                        args[0].data_type()
                    ),
                });
            };
            if len == 0 {
                Ok(Value::Null)
            } else {
                Ok(Value::text(alloc::format!("[1:{}]", len)))
            }
        }
        // v7.11.6 — `array_position(arr, val)` returns 1-based
        // index of the first element of `arr` equal to `val`, or
        // NULL if not found. PG NULL semantics: NULL array → NULL;
        // NULL val never matches (returns NULL if absent).
        "array_position" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_position() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            let Some(len) = array_len(&args[0]) else {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_position() first arg must be an array, got {:?}",
                        args[0].data_type()
                    ),
                });
            };
            // PG compares with IS NOT DISTINCT FROM semantics across every
            // element type (a NULL search value matches a NULL element:
            // `array_position(ARRAY[1,NULL,2], NULL)` → 2), reusing the
            // scalar `=` dispatch so cross-width numerics / date / uuid /
            // bytea / interval / money / jsonb all match consistently.
            for i in 0..len {
                let elem = array_element_at(&args[0], i).unwrap_or(Value::Null);
                if array_search_match(&elem, &args[1])? {
                    return Ok(Value::Int(i32::try_from(i + 1).unwrap_or(i32::MAX)));
                }
            }
            Ok(Value::Null)
        }
        // v7.37.17 (17.6 siblings) — PG 16+ array_shuffle(arr)
        // returns a randomly-permuted copy. Fisher-Yates using the
        // internal prng_next_u64 splitter.
        "array_shuffle" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_shuffle() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::IntArray(items) => {
                    let mut out: alloc::vec::Vec<Option<i32>> = items.clone();
                    let n = out.len();
                    for i in (1..n).rev() {
                        let j = (super::math::prng_next_u64() as usize)
                            % (i + 1);
                        out.swap(i, j);
                    }
                    Ok(Value::IntArray(out))
                }
                Value::BigIntArray(items) => {
                    let mut out: alloc::vec::Vec<Option<i64>> = items.clone();
                    let n = out.len();
                    for i in (1..n).rev() {
                        let j = (super::math::prng_next_u64() as usize)
                            % (i + 1);
                        out.swap(i, j);
                    }
                    Ok(Value::BigIntArray(out))
                }
                Value::TextArray(items) => {
                    let mut out: alloc::vec::Vec<Option<alloc::string::String>> =
                        items.clone();
                    let n = out.len();
                    for i in (1..n).rev() {
                        let j = (super::math::prng_next_u64() as usize)
                            % (i + 1);
                        out.swap(i, j);
                    }
                    Ok(Value::TextArray(out))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "array_shuffle() needs array, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — PG 16+ array_sample(arr, n)
        // returns a random subset of n items from arr (partial
        // Fisher-Yates — pick n distinct indices).
        "array_sample" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_sample() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            let n = match &args[1] {
                Value::Null => return Ok(Value::Null),
                Value::SmallInt(x) => i64::from(*x),
                Value::Int(x) => i64::from(*x),
                Value::BigInt(x) => *x,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "array_sample(): n must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if n < 0 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "array_sample(): {n} is negative"
                    ),
                });
            }
            let n = n as usize;
            match &args[0] {
                Value::IntArray(items) => {
                    let take = n.min(items.len());
                    let mut src: alloc::vec::Vec<Option<i32>> = items.clone();
                    let src_n = src.len();
                    for i in 0..take {
                        let j = i + (super::math::prng_next_u64() as usize)
                            % (src_n - i);
                        src.swap(i, j);
                    }
                    Ok(Value::IntArray(src[..take].to_vec()))
                }
                Value::BigIntArray(items) => {
                    let take = n.min(items.len());
                    let mut src: alloc::vec::Vec<Option<i64>> = items.clone();
                    let src_n = src.len();
                    for i in 0..take {
                        let j = i + (super::math::prng_next_u64() as usize)
                            % (src_n - i);
                        src.swap(i, j);
                    }
                    Ok(Value::BigIntArray(src[..take].to_vec()))
                }
                Value::TextArray(items) => {
                    let take = n.min(items.len());
                    let mut src: alloc::vec::Vec<Option<alloc::string::String>> =
                        items.clone();
                    let src_n = src.len();
                    for i in 0..take {
                        let j = i + (super::math::prng_next_u64() as usize)
                            % (src_n - i);
                        src.swap(i, j);
                    }
                    Ok(Value::TextArray(src[..take].to_vec()))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "array_sample() needs array, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — array_remove(arr, val) returns
        // the array with every occurrence of val removed. NULL
        // passthrough on NULL array. NULL needle removes NULL items.
        "array_remove" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_remove() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            match (&args[0], &args[1]) {
                (Value::IntArray(items), needle_v) => {
                    if matches!(needle_v, Value::Null) {
                        let out: alloc::vec::Vec<Option<i32>> =
                            items.iter().filter(|o| o.is_some()).copied().collect();
                        return Ok(Value::IntArray(out));
                    }
                    let needle: i64 = match *needle_v {
                        Value::Int(n) => i64::from(n),
                        Value::SmallInt(n) => i64::from(n),
                        Value::BigInt(n) => n,
                        _ => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "array_remove(): needle type {:?} doesn't match IntArray",
                                    needle_v.data_type()
                                ),
                            });
                        }
                    };
                    let out: alloc::vec::Vec<Option<i32>> = items
                        .iter()
                        .filter(|o| o.map(i64::from) != Some(needle))
                        .copied()
                        .collect();
                    Ok(Value::IntArray(out))
                }
                (Value::BigIntArray(items), needle_v) => {
                    if matches!(needle_v, Value::Null) {
                        let out: alloc::vec::Vec<Option<i64>> =
                            items.iter().filter(|o| o.is_some()).copied().collect();
                        return Ok(Value::BigIntArray(out));
                    }
                    let needle: i64 = match *needle_v {
                        Value::Int(n) => i64::from(n),
                        Value::SmallInt(n) => i64::from(n),
                        Value::BigInt(n) => n,
                        _ => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "array_remove(): needle type {:?} doesn't match BigIntArray",
                                    needle_v.data_type()
                                ),
                            });
                        }
                    };
                    let out: alloc::vec::Vec<Option<i64>> = items
                        .iter()
                        .filter(|o| **o != Some(needle))
                        .copied()
                        .collect();
                    Ok(Value::BigIntArray(out))
                }
                (other, _) => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_remove() first arg must be array, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — array_replace(arr, from, to)
        // returns the array with every occurrence of `from` replaced
        // with `to`. NULL passthrough on NULL array. NULL from
        // replaces NULL items with to.
        "array_replace" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_replace() takes 3 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            match (&args[0], &args[1], &args[2]) {
                (Value::IntArray(items), from_v, to_v) => {
                    let from = match from_v {
                        Value::Null => None,
                        Value::Int(n) => Some(*n),
                        Value::SmallInt(n) => Some(i32::from(*n)),
                        Value::BigInt(n) => i32::try_from(*n).ok(),
                        _ => {
                            return Err(EvalError::TypeMismatch {
                                detail: "array_replace(): from must be integer".into(),
                            });
                        }
                    };
                    let to = match to_v {
                        Value::Null => None,
                        Value::Int(n) => Some(*n),
                        Value::SmallInt(n) => Some(i32::from(*n)),
                        Value::BigInt(n) => i32::try_from(*n).ok(),
                        _ => {
                            return Err(EvalError::TypeMismatch {
                                detail: "array_replace(): to must be integer".into(),
                            });
                        }
                    };
                    let out: alloc::vec::Vec<Option<i32>> = items
                        .iter()
                        .map(|o| if *o == from { to } else { *o })
                        .collect();
                    Ok(Value::IntArray(out))
                }
                (Value::BigIntArray(items), from_v, to_v) => {
                    let from = match from_v {
                        Value::Null => None,
                        Value::Int(n) => Some(i64::from(*n)),
                        Value::SmallInt(n) => Some(i64::from(*n)),
                        Value::BigInt(n) => Some(*n),
                        _ => {
                            return Err(EvalError::TypeMismatch {
                                detail: "array_replace(): from must be integer".into(),
                            });
                        }
                    };
                    let to = match to_v {
                        Value::Null => None,
                        Value::Int(n) => Some(i64::from(*n)),
                        Value::SmallInt(n) => Some(i64::from(*n)),
                        Value::BigInt(n) => Some(*n),
                        _ => {
                            return Err(EvalError::TypeMismatch {
                                detail: "array_replace(): to must be integer".into(),
                            });
                        }
                    };
                    let out: alloc::vec::Vec<Option<i64>> = items
                        .iter()
                        .map(|o| if *o == from { to } else { *o })
                        .collect();
                    Ok(Value::BigIntArray(out))
                }
                (other, _, _) => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_replace() first arg must be array, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — array_positions(arr, val)
        // returns an IntArray of all 1-based indices where val
        // occurs in arr. NULL if arr is NULL. Empty array if not
        // found. Matches PG semantics.
        "array_positions" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_positions() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            let Some(len) = array_len(&args[0]) else {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_positions() first arg must be an array, got {:?}",
                        args[0].data_type()
                    ),
                });
            };
            // Same IS NOT DISTINCT FROM element match as array_position,
            // across every element type — a NULL search value collects the
            // positions of NULL elements (PG allows it here, unlike some
            // array functions).
            let mut hits: alloc::vec::Vec<Option<i32>> = alloc::vec::Vec::new();
            for i in 0..len {
                let elem = array_element_at(&args[0], i).unwrap_or(Value::Null);
                if array_search_match(&elem, &args[1])? {
                    hits.push(Some(i32::try_from(i + 1).unwrap_or(i32::MAX)));
                }
            }
            Ok(Value::IntArray(hits))
        }
        // v7.37.17 (17.6 siblings) — array_append(arr, el) /
        // array_prepend(el, arr). PG semantics: a NULL array acts as
        // an empty array of the element's type (array_append(NULL, 3)
        // → {3}); a NULL element is appended as a NULL item. Both
        // NULL → NULL (PG can't resolve the polymorphic type either).
        // v7.37 D.53 — `UPDATE t SET arr[i] = v` desugars (in the parser) to
        // `arr = __array_assign(arr, i, v)`. PG assigns to the i-th (1-based)
        // element, NULL-padding the array when i exceeds its current length.
        "__array_assign" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("__array_assign() takes 3 args, got {}", args.len()),
                });
            }
            let idx: i64 = match &args[1] {
                Value::SmallInt(n) => i64::from(*n),
                Value::Int(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                Value::Null => {
                    return Err(EvalError::TypeMismatch {
                        detail: "array subscript in an UPDATE must not be null".into(),
                    });
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("array subscript must be integer, got {:?}", other.data_type()),
                    });
                }
            };
            if idx < 1 {
                return Err(EvalError::TypeMismatch {
                    detail: "array subscripts below 1 are not supported for assignment".into(),
                });
            }
            let pos = (idx - 1) as usize;
            let val = &args[2];
            let as_i32 = |v: &Value| -> Option<Option<i32>> {
                match v {
                    Value::Null => Some(None),
                    Value::Int(n) => Some(Some(*n)),
                    Value::SmallInt(n) => Some(Some(i32::from(*n))),
                    Value::BigInt(n) => i32::try_from(*n).ok().map(Some),
                    _ => None,
                }
            };
            let as_i64 = |v: &Value| -> Option<Option<i64>> {
                match v {
                    Value::Null => Some(None),
                    Value::Int(n) => Some(Some(i64::from(*n))),
                    Value::SmallInt(n) => Some(Some(i64::from(*n))),
                    Value::BigInt(n) => Some(Some(*n)),
                    _ => None,
                }
            };
            // A NULL array with an element assignment becomes a fresh array
            // padded up to the index (PG: `arr[3]=x` on NULL → `[NULL,NULL,x]`).
            match &args[0] {
                Value::TextArray(items) => {
                    let mut out = items.clone();
                    if pos >= out.len() {
                        out.resize(pos + 1, None);
                    }
                    out[pos] = match val {
                        Value::Null => None,
                        Value::Text(s) => Some(s.to_string()),
                        other => return Err(EvalError::TypeMismatch {
                            detail: format!("cannot assign {:?} to a text[] element", other.data_type()),
                        }),
                    };
                    Ok(Value::TextArray(out))
                }
                Value::IntArray(items) => {
                    let mut out = items.clone();
                    if pos >= out.len() {
                        out.resize(pos + 1, None);
                    }
                    out[pos] = as_i32(val).ok_or_else(|| EvalError::TypeMismatch {
                        detail: format!("cannot assign {:?} to an int[] element", val.data_type()),
                    })?;
                    Ok(Value::IntArray(out))
                }
                Value::BigIntArray(items) => {
                    let mut out = items.clone();
                    if pos >= out.len() {
                        out.resize(pos + 1, None);
                    }
                    out[pos] = as_i64(val).ok_or_else(|| EvalError::TypeMismatch {
                        detail: format!("cannot assign {:?} to a bigint[] element", val.data_type()),
                    })?;
                    Ok(Value::BigIntArray(out))
                }
                Value::Null => {
                    // Infer the new array's type from the assigned value.
                    match val {
                        Value::Text(s) => {
                            let mut out = alloc::vec![None; pos + 1];
                            out[pos] = Some(s.to_string());
                            Ok(Value::TextArray(out))
                        }
                        Value::Int(_) | Value::SmallInt(_) => {
                            let mut out = alloc::vec![None; pos + 1];
                            out[pos] = as_i32(val).unwrap();
                            Ok(Value::IntArray(out))
                        }
                        Value::BigInt(_) => {
                            let mut out = alloc::vec![None; pos + 1];
                            out[pos] = as_i64(val).unwrap();
                            Ok(Value::BigIntArray(out))
                        }
                        Value::Null => Ok(Value::Null),
                        other => Err(EvalError::TypeMismatch {
                            detail: format!("cannot infer array type for element {:?}", other.data_type()),
                        }),
                    }
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!("array element assignment target must be an array, got {:?}", other.data_type()),
                }),
            }
        }
        "array_append" | "array_prepend" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 2 args, got {}", args.len()),
                });
            }
            // Normalize to (array, element, prepend?).
            let (arr_v, el_v, prepend) = if name == "array_append" {
                (&args[0], &args[1], false)
            } else {
                (&args[1], &args[0], true)
            };
            let push_text = |items: &[Option<String>], el: Option<String>| {
                let mut out: alloc::vec::Vec<Option<String>> = items.to_vec();
                if prepend {
                    out.insert(0, el);
                } else {
                    out.push(el);
                }
                Value::TextArray(out)
            };
            let push_int = |items: &[Option<i32>], el: Option<i32>| {
                let mut out: alloc::vec::Vec<Option<i32>> = items.to_vec();
                if prepend {
                    out.insert(0, el);
                } else {
                    out.push(el);
                }
                Value::IntArray(out)
            };
            let push_bigint = |items: &[Option<i64>], el: Option<i64>| {
                let mut out: alloc::vec::Vec<Option<i64>> = items.to_vec();
                if prepend {
                    out.insert(0, el);
                } else {
                    out.push(el);
                }
                Value::BigIntArray(out)
            };
            let as_i32 = |v: &Value| -> Option<Option<i32>> {
                match v {
                    Value::Null => Some(None),
                    Value::Int(n) => Some(Some(*n)),
                    Value::SmallInt(n) => Some(Some(i32::from(*n))),
                    Value::BigInt(n) => i32::try_from(*n).ok().map(Some),
                    _ => None,
                }
            };
            let as_i64 = |v: &Value| -> Option<Option<i64>> {
                match v {
                    Value::Null => Some(None),
                    Value::Int(n) => Some(Some(i64::from(*n))),
                    Value::SmallInt(n) => Some(Some(i64::from(*n))),
                    Value::BigInt(n) => Some(Some(*n)),
                    _ => None,
                }
            };
            match arr_v {
                Value::Null => match el_v {
                    Value::Null => Ok(Value::Null),
                    Value::Text(s) => Ok(push_text(&[], Some(s.to_string()))),
                    Value::Int(_) | Value::SmallInt(_) => {
                        Ok(push_int(&[], as_i32(el_v).unwrap()))
                    }
                    Value::BigInt(n) => Ok(push_bigint(&[], Some(*n))),
                    other => Err(EvalError::TypeMismatch {
                        detail: format!(
                            "{name}(): unsupported element type {:?}",
                            other.data_type()
                        ),
                    }),
                },
                Value::TextArray(items) => match el_v {
                    Value::Null => Ok(push_text(items, None)),
                    Value::Text(s) => Ok(push_text(items, Some(s.to_string()))),
                    other => Err(EvalError::TypeMismatch {
                        detail: format!(
                            "{name}(): element type {:?} doesn't match TextArray",
                            other.data_type()
                        ),
                    }),
                },
                Value::IntArray(items) => match as_i32(el_v) {
                    Some(el) => Ok(push_int(items, el)),
                    None => Err(EvalError::TypeMismatch {
                        detail: format!(
                            "{name}(): element type {:?} doesn't match IntArray",
                            el_v.data_type()
                        ),
                    }),
                },
                Value::BigIntArray(items) => match as_i64(el_v) {
                    Some(el) => Ok(push_bigint(items, el)),
                    None => Err(EvalError::TypeMismatch {
                        detail: format!(
                            "{name}(): element type {:?} doesn't match BigIntArray",
                            el_v.data_type()
                        ),
                    }),
                },
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "{name}() array arg must be an array, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — array_cat(arr1, arr2)
        // concatenates two arrays of the same element type. A NULL
        // side yields the other side unchanged (PG semantics).
        "array_cat" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_cat() takes 2 args, got {}", args.len()),
                });
            }
            match (&args[0], &args[1]) {
                (Value::Null, other) | (other, Value::Null) => {
                    Ok(other.clone().into_owned())
                }
                (Value::TextArray(a), Value::TextArray(b)) => {
                    let mut out = a.clone();
                    out.extend(b.iter().cloned());
                    Ok(Value::TextArray(out))
                }
                (Value::IntArray(a), Value::IntArray(b)) => {
                    let mut out = a.clone();
                    out.extend(b.iter().copied());
                    Ok(Value::IntArray(out))
                }
                (Value::BigIntArray(a), Value::BigIntArray(b)) => {
                    let mut out = a.clone();
                    out.extend(b.iter().copied());
                    Ok(Value::BigIntArray(out))
                }
                (a, b) => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_cat() element types differ: {:?} vs {:?}",
                        a.data_type(),
                        b.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — range constructor functions.
        // Until now ranges only entered SPG through '::int4range'
        // text casts; these are the PG constructor forms
        // int4range(lo, hi [, bounds]) etc. NULL bounds are
        // unbounded; equal bounds without '[]' inclusivity collapse
        // to the empty range (PG canonical). Element coercion
        // matches parse_range_element's shapes.
        "int4range" | "int8range" | "numrange" | "daterange" | "tsrange" | "tstzrange" => {
            if !matches!(args.len(), 2 | 3) {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 2 or 3 args, got {}", args.len()),
                });
            }
            use spg_storage::RangeKind as K;
            let kind = match name {
                "int4range" => K::Int4,
                "int8range" => K::Int8,
                "numrange" => K::Num,
                "daterange" => K::Date,
                "tsrange" => K::Ts,
                _ => K::TsTz,
            };
            let (lower_inc, upper_inc) = match args.get(2) {
                None | Some(Value::Null) => (true, false),
                Some(Value::Text(b)) => match b.as_ref() {
                    "[)" => (true, false),
                    "[]" => (true, true),
                    "()" => (false, false),
                    "(]" => (false, true),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!("invalid range bound flags: {other:?}"),
                        });
                    }
                },
                Some(other) => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "{name}() bounds must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let coerce = |v: &Value<'_>| -> Result<Option<Value<'static>>, EvalError> {
                Ok(match (kind, v) {
                    (_, Value::Null) => None,
                    (K::Int4, Value::Int(n)) => Some(Value::Int(*n)),
                    (K::Int4, Value::SmallInt(n)) => Some(Value::Int(i32::from(*n))),
                    (K::Int4, Value::BigInt(n)) => Some(Value::Int(
                        i32::try_from(*n).map_err(|_| EvalError::TypeMismatch {
                            detail: format!("{name}(): bound out of int4 range: {n}"),
                        })?,
                    )),
                    (K::Int8, Value::Int(n)) => Some(Value::BigInt(i64::from(*n))),
                    (K::Int8, Value::SmallInt(n)) => Some(Value::BigInt(i64::from(*n))),
                    (K::Int8, Value::BigInt(n)) => Some(Value::BigInt(*n)),
                    (K::Num, Value::Numeric { scaled, scale }) => Some(Value::Numeric {
                        scaled: *scaled,
                        scale: *scale,
                    }),
                    (K::Num, Value::Int(n)) => Some(Value::Numeric {
                        scaled: i128::from(*n),
                        scale: 0,
                    }),
                    (K::Num, Value::SmallInt(n)) => Some(Value::Numeric {
                        scaled: i128::from(*n),
                        scale: 0,
                    }),
                    (K::Num, Value::BigInt(n)) => Some(Value::Numeric {
                        scaled: i128::from(*n),
                        scale: 0,
                    }),
                    // Float literals (numrange(1.1, 2.2)) route
                    // through the range-element text parse so the
                    // Numeric scale matches the lexeme.
                    (K::Num, Value::Float(f)) => Some(
                        crate::conversions::parse_range_element(
                            &alloc::format!("{f}"),
                            kind,
                        )
                        .ok_or_else(|| EvalError::TypeMismatch {
                            detail: format!("{name}(): cannot parse bound {f}"),
                        })?,
                    ),
                    (K::Date, Value::Date(d)) => Some(Value::Date(*d)),
                    (K::Ts | K::TsTz, Value::Timestamp(t)) => Some(Value::Timestamp(*t)),
                    (K::Ts | K::TsTz, Value::Date(d)) => {
                        Some(Value::Timestamp(i64::from(*d) * 86_400_000_000))
                    }
                    // Text bounds reuse the range-element parser.
                    (_, Value::Text(s)) => {
                        Some(crate::conversions::parse_range_element(s, kind).ok_or_else(
                            || EvalError::TypeMismatch {
                                detail: format!("{name}(): cannot parse bound {s:?}"),
                            },
                        )?)
                    }
                    (_, other) => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!(
                                "{name}(): bound type {:?} doesn't match the range",
                                other.data_type()
                            ),
                        });
                    }
                })
            };
            let lower = coerce(&args[0])?;
            let upper = coerce(&args[1])?;
            // Canonicalize (infinite→exclusive + discrete `[)` fold) via the
            // shared helper so the constructors and the `'…'::int4range`
            // text-input path agree — `int4range(1,3,'[]')` is `[1,4)`.
            let (lower, upper, lower_inc, upper_inc, empty) =
                crate::conversions::canonicalize_range_bounds(
                    kind, lower, upper, lower_inc, upper_inc,
                )
                .ok_or_else(|| EvalError::TypeMismatch {
                    detail: format!("{name}(): range bound overflow while canonicalizing"),
                })?;
            if empty {
                return Ok(Value::Range {
                    kind,
                    lower: None,
                    upper: None,
                    lower_inc: false,
                    upper_inc: false,
                    empty: true,
                });
            }
            Ok(Value::Range {
                kind,
                lower: lower.map(alloc::boxed::Box::new),
                upper: upper.map(alloc::boxed::Box::new),
                lower_inc,
                upper_inc,
                empty: false,
            })
        }
        // v7.37.17 (17.6 siblings) — multirange constructor
        // functions: variadic over ranges of the matching kind.
        // Empty ranges are dropped; zero arguments produce the
        // empty multirange {}. No overlap coalescing — matches the
        // existing Multirange contract (the engine trusts the
        // caller, mirroring PG's _construct_array pattern).
        "int4multirange" | "int8multirange" | "nummultirange" | "datemultirange"
        | "tsmultirange" | "tstzmultirange" => {
            use spg_storage::RangeKind as K;
            let kind = match name {
                "int4multirange" => K::Int4,
                "int8multirange" => K::Int8,
                "nummultirange" => K::Num,
                "datemultirange" => K::Date,
                "tsmultirange" => K::Ts,
                _ => K::TsTz,
            };
            let mut ranges: alloc::vec::Vec<spg_storage::RangeSpan> = alloc::vec::Vec::new();
            for arg in args {
                match arg {
                    Value::Null => return Ok(Value::Null),
                    Value::Range {
                        kind: rk,
                        lower,
                        upper,
                        lower_inc,
                        upper_inc,
                        empty,
                    } => {
                        if *rk != kind {
                            return Err(EvalError::TypeMismatch {
                                detail: format!(
                                    "{name}(): range kind {rk:?} doesn't match {kind:?}"
                                ),
                            });
                        }
                        if *empty {
                            continue;
                        }
                        ranges.push(spg_storage::RangeSpan {
                            lower: lower.clone(),
                            upper: upper.clone(),
                            lower_inc: *lower_inc,
                            upper_inc: *upper_inc,
                            empty: false,
                        });
                    }
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!(
                                "{name}() arguments must be ranges, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                }
            }
            // PG normalizes multirange members: sort + merge overlapping /
            // adjacent spans + drop empty, so overlapping inputs collapse
            // (`int4range(1,5),int4range(4,8)` → `{[1,8)}`).
            let ranges = super::binop::normalize_multirange_spans(kind, &ranges);
            Ok(Value::Multirange { kind, ranges })
        }
        // v7.37.17 (17.6 siblings) — generate_subscripts(arr, dim
        // [, reverse]) scalar surface: returns the valid subscripts
        // of the array's dim'th dimension as an IntArray (the parser
        // rewrites the FROM-clause SRF form into unnest over this).
        // SPG arrays are one-dimensional, so dim != 1 yields an
        // empty set — PG's behaviour for a missing dimension.
        "generate_subscripts" => {
            if !matches!(args.len(), 2 | 3) {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "generate_subscripts() takes 2 or 3 args, got {}",
                        args.len()
                    ),
                });
            }
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            let len = match &args[0] {
                Value::TextArray(items) => items.len(),
                Value::IntArray(items) => items.len(),
                Value::BigIntArray(items) => items.len(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "generate_subscripts() first arg must be an array, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let dim = match &args[1] {
                Value::Int(n) => i64::from(*n),
                Value::SmallInt(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "generate_subscripts() dim must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let reverse = match args.get(2) {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(other) => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "generate_subscripts() reverse must be boolean, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let mut subs: alloc::vec::Vec<Option<i32>> = if dim == 1 {
                (1..=len as i32).map(Some).collect()
            } else {
                alloc::vec::Vec::new()
            };
            if reverse {
                subs.reverse();
            }
            Ok(Value::IntArray(subs))
        }
        // v7.37.17 (17.6 siblings) — PG 14+ trim_array(arr, n) removes
        // the last n elements. Errors like PG when n is negative or
        // exceeds the array length.
        "trim_array" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("trim_array() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            let n = match &args[1] {
                Value::Int(n) => i64::from(*n),
                Value::SmallInt(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "trim_array() second arg must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let len = match &args[0] {
                Value::TextArray(items) => items.len(),
                Value::IntArray(items) => items.len(),
                Value::BigIntArray(items) => items.len(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "trim_array() first arg must be an array, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if n < 0 || n as usize > len {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "number of elements to trim must be between 0 and {len}"
                    ),
                });
            }
            let keep = len - n as usize;
            match &args[0] {
                Value::TextArray(items) => {
                    Ok(Value::TextArray(items[..keep].to_vec()))
                }
                Value::IntArray(items) => Ok(Value::IntArray(items[..keep].to_vec())),
                Value::BigIntArray(items) => {
                    Ok(Value::BigIntArray(items[..keep].to_vec()))
                }
                _ => unreachable!(),
            }
        }
        // v7.11.15 — `substring(s, start)` / `substring(s, start, length)`
        // for both TEXT and BYTEA. PG semantics: `start` is 1-based;
        // values ≤ 0 clamp into the string (i.e. effective start is
        // adjusted so the window still begins at index 1 — but
        // `length` is reduced by the clipped prefix). A NULL arg
        // makes the result NULL. Out-of-range windows return an
        // empty value, not NULL.
        // "mid" is the MySQL alias for 3-arg substring.
        "substring" | "substr" | "mid" => {
            if !matches!(args.len(), 2 | 3) {
                return Err(EvalError::TypeMismatch {
                    detail: format!("substring() takes 2 or 3 args, got {}", args.len()),
                });
            }
            if args.iter().any(|a| matches!(a, Value::Null)) {
                return Ok(Value::Null);
            }
            // v7.37 D.24 — PG `substring(string FROM pattern)` /
            // `substring(string, pattern)`: when the 2nd arg is a TEXT pattern
            // (not an integer position), do POSIX regex extraction. A pattern
            // without a parenthesized subexpression → the whole match (== the
            // first regexp_substr match). A pattern WITH a capturing group → PG
            // returns the first group, which needs regex capture-group extraction
            // (a regex-engine gap, per Epic Rx P3) — honest-error rather than
            // silently returning the whole match.
            if name != "mid" && args.len() == 2 && matches!(&args[1], Value::Text(_)) {
                if let Value::Text(pat) = &args[1]
                    && pattern_has_capturing_group(pat)
                {
                    return Err(EvalError::TypeMismatch {
                        detail: "substring(string FROM pattern) with a capturing \
                                 group needs regex capture-group extraction (not \
                                 yet supported); use a pattern with no parentheses"
                            .into(),
                    });
                }
                return super::regexp::regexp_substr(&args[..2]);
            }
            let start: i64 = match args[1] {
                Value::Int(n) => i64::from(n),
                Value::BigInt(n) => n,
                Value::SmallInt(n) => i64::from(n),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "substring() start must be integer, got {:?}",
                            args[1].data_type()
                        ),
                    });
                }
            };
            let length: Option<i64> = if args.len() == 3 {
                match args[2] {
                    Value::Int(n) => Some(i64::from(n)),
                    Value::BigInt(n) => Some(n),
                    Value::SmallInt(n) => Some(i64::from(n)),
                    _ => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!(
                                "substring() length must be integer, got {:?}",
                                args[2].data_type()
                            ),
                        });
                    }
                }
            } else {
                None
            };
            // PG: when length is given, end = start + length; if
            // end < start the result is empty. Clip start to 1.
            let (effective_start, effective_length): (i64, Option<i64>) = match length {
                Some(len) => {
                    // PG raises on a negative length; a non-negative length
                    // whose window ends at or before position 1 is empty.
                    if len < 0 {
                        return Err(EvalError::TypeMismatch {
                            detail: "negative substring length not allowed".into(),
                        });
                    }
                    let end = start.saturating_add(len);
                    if end <= 1 {
                        return Ok(match &args[0] {
                            Value::Text(_) => Value::text(String::new()),
                            Value::Bytes(_) => Value::bytes(Vec::new()),
                            Value::BitString { .. } => {
                                Value::BitString { nbits: 0, bytes: alloc::borrow::Cow::Owned(Vec::new()) }
                            }
                            other => {
                                return Err(EvalError::TypeMismatch {
                                    detail: format!(
                                        "substring() needs text or bytea, got {:?}",
                                        other.data_type()
                                    ),
                                });
                            }
                        });
                    }
                    let eff_start = start.max(1);
                    let eff_len = end - eff_start;
                    (eff_start, Some(eff_len.max(0)))
                }
                None => (start.max(1), None),
            };
            match &args[0] {
                Value::Text(s) => {
                    // PG counts in characters (codepoints) for TEXT.
                    let chars: Vec<char> = s.chars().collect();
                    let skip = (effective_start - 1) as usize;
                    if skip >= chars.len() {
                        return Ok(Value::text(String::new()));
                    }
                    let take = match effective_length {
                        Some(n) => (n as usize).min(chars.len() - skip),
                        None => chars.len() - skip,
                    };
                    Ok(Value::text(
                        chars[skip..skip + take].iter().collect::<String>(),
                    ))
                }
                Value::Bytes(b) => {
                    let skip = (effective_start - 1) as usize;
                    if skip >= b.len() {
                        return Ok(Value::bytes(Vec::new()));
                    }
                    let take = match effective_length {
                        Some(n) => (n as usize).min(b.len() - skip),
                        None => b.len() - skip,
                    };
                    Ok(Value::bytes(b[skip..skip + take].to_vec()))
                }
                // PG substring(bit/varbit FROM s FOR l) — bit-level slice
                // (MSB-first, 1-based), repacked into a new bit string.
                Value::BitString { nbits, bytes } => {
                    let skip = (effective_start - 1) as usize;
                    let total = *nbits as usize;
                    if skip >= total {
                        return Ok(Value::BitString {
                            nbits: 0,
                            bytes: alloc::borrow::Cow::Owned(Vec::new()),
                        });
                    }
                    let take = match effective_length {
                        Some(n) => (n as usize).min(total - skip),
                        None => total - skip,
                    };
                    let bit_at = |i: usize| -> u8 { (bytes[i / 8] >> (7 - i % 8)) & 1 };
                    let mut out = alloc::vec![0u8; take.div_ceil(8)];
                    for j in 0..take {
                        if bit_at(skip + j) == 1 {
                            out[j / 8] |= 1u8 << (7 - j % 8);
                        }
                    }
                    Ok(Value::BitString {
                        nbits: u32::try_from(take).unwrap_or(u32::MAX),
                        bytes: alloc::borrow::Cow::Owned(out),
                    })
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "substring() needs text or bytea, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.11.15 — `position(needle, haystack)`. PG semantics:
        // 1-based byte/char index of first occurrence, or 0 if
        // absent. NULL on either operand → NULL. Empty needle
        // returns 1 (PG convention). Works on TEXT (char positions)
        // and BYTEA (byte positions). (The PG-spec syntax `position(
        // needle IN haystack)` is not parsed in v7.11; clients must
        // call the function-call form.)
        "position" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("position() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            match (&args[0], &args[1]) {
                (Value::Text(needle), Value::Text(haystack)) => {
                    if needle.is_empty() {
                        return Ok(Value::Int(1));
                    }
                    // Char-based position (PG uses character count).
                    let h_chars: Vec<char> = haystack.chars().collect();
                    let n_chars: Vec<char> = needle.chars().collect();
                    if n_chars.len() > h_chars.len() {
                        return Ok(Value::Int(0));
                    }
                    for i in 0..=h_chars.len() - n_chars.len() {
                        if h_chars[i..i + n_chars.len()] == n_chars[..] {
                            return Ok(Value::Int(i32::try_from(i + 1).unwrap_or(i32::MAX)));
                        }
                    }
                    Ok(Value::Int(0))
                }
                (Value::Bytes(needle), Value::Bytes(haystack)) => {
                    if needle.is_empty() {
                        return Ok(Value::Int(1));
                    }
                    if needle.len() > haystack.len() {
                        return Ok(Value::Int(0));
                    }
                    for i in 0..=haystack.len() - needle.len() {
                        if &haystack[i..i + needle.len()] == needle.as_ref() {
                            return Ok(Value::Int(i32::try_from(i + 1).unwrap_or(i32::MAX)));
                        }
                    }
                    Ok(Value::Int(0))
                }
                (a, b) => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "position() operands must both be text or both bytea, got {:?} and {:?}",
                        a.data_type(),
                        b.data_type()
                    ),
                }),
            }
        }
        // MySQL hex(int|text) — uppercase hex of an integer's value
        // or of a string's bytes.
        "hex" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("hex() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => {
                    Ok(Value::text(alloc::format!("{:X}", *n as i64 as u64)))
                }
                Value::SmallInt(n) => {
                    Ok(Value::text(alloc::format!("{:X}", *n as i64 as u64)))
                }
                Value::BigInt(n) => {
                    Ok(Value::text(alloc::format!("{:X}", *n as u64)))
                }
                Value::Text(s) => {
                    let mut out = alloc::string::String::with_capacity(s.len() * 2);
                    for b in s.as_bytes() {
                        out.push_str(&alloc::format!("{b:02X}"));
                    }
                    Ok(Value::text(out))
                }
                Value::Bytes(b) => {
                    let mut out = alloc::string::String::with_capacity(b.len() * 2);
                    for byte in b.iter() {
                        out.push_str(&alloc::format!("{byte:02X}"));
                    }
                    Ok(Value::text(out))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "hex() needs int/text/bytea, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // MySQL unhex(hex_text) — hex string back to bytes; NULL on
        // invalid input (MySQL semantics, not an error).
        "unhex" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("unhex() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let t = s.as_ref();
                    if t.len() % 2 != 0 {
                        return Ok(Value::Null);
                    }
                    let mut out = alloc::vec::Vec::with_capacity(t.len() / 2);
                    let bytes = t.as_bytes();
                    for pair in bytes.chunks(2) {
                        let hi = (pair[0] as char).to_digit(16);
                        let lo = (pair[1] as char).to_digit(16);
                        match (hi, lo) {
                            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
                            _ => return Ok(Value::Null),
                        }
                    }
                    Ok(Value::Bytes(alloc::borrow::Cow::Owned(out)))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "unhex() needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // MySQL conv(n, from_base, to_base) — base conversion
        // (2-36). Negative to_base renders as signed.
        "conv" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("conv() takes 3 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let digits = match &args[0] {
                Value::Text(s) => s.as_ref().to_string(),
                Value::Int(n) => n.to_string(),
                Value::BigInt(n) => n.to_string(),
                Value::SmallInt(n) => n.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "conv() needs text/int, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let base_of = |v: &Value<'_>| -> Option<i64> {
                match v {
                    Value::Int(n) => Some(i64::from(*n)),
                    Value::BigInt(n) => Some(*n),
                    Value::SmallInt(n) => Some(i64::from(*n)),
                    _ => None,
                }
            };
            let (Some(from_base), Some(to_base)) =
                (base_of(&args[1]), base_of(&args[2]))
            else {
                return Err(EvalError::TypeMismatch {
                    detail: "conv() bases must be integers".into(),
                });
            };
            if !(2..=36).contains(&from_base.abs())
                || !(2..=36).contains(&to_base.abs())
            {
                return Ok(Value::Null);
            }
            let trimmed = digits.trim();
            let (neg, body) = match trimmed.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, trimmed),
            };
            let Ok(mag) = u64::from_str_radix(body, from_base.unsigned_abs() as u32)
            else {
                return Ok(Value::text::<String>("0".into()));
            };
            // MySQL treats values as unsigned unless to_base < 0.
            let value: u64 = if neg { (mag as i64).wrapping_neg() as u64 } else { mag };
            fn to_base_str(mut v: u64, base: u64) -> alloc::string::String {
                const DIGITS: &[u8; 36] =
                    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
                if v == 0 {
                    return "0".into();
                }
                let mut out = alloc::vec::Vec::new();
                while v > 0 {
                    out.push(DIGITS[(v % base) as usize]);
                    v /= base;
                }
                out.reverse();
                alloc::string::String::from_utf8(out).unwrap_or_default()
            }
            let rendered = if to_base < 0 {
                let signed = value as i64;
                if signed < 0 {
                    alloc::format!(
                        "-{}",
                        to_base_str(signed.unsigned_abs(), to_base.unsigned_abs())
                    )
                } else {
                    to_base_str(signed as u64, to_base.unsigned_abs())
                }
            } else {
                to_base_str(value, to_base as u64)
            };
            Ok(Value::text(rendered))
        }
        // MySQL bin(n) / oct(n) — binary / octal digit strings.
        "bin" | "oct" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            let n = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                Value::SmallInt(n) => i64::from(*n),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() needs integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let rendered = if name == "bin" {
                alloc::format!("{:b}", n as u64)
            } else {
                alloc::format!("{:o}", n as u64)
            };
            Ok(Value::text(rendered))
        }
        // MySQL ord(s) — the code of the first character: for
        // multi-byte chars, its UTF-8 bytes read as a big-endian
        // number (matches MySQL's utf8mb4 behavior).
        "ord" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("ord() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let mut value: i64 = 0;
                    match s.chars().next() {
                        None => Ok(Value::BigInt(0)),
                        Some(c) => {
                            let mut buf = [0u8; 4];
                            for b in c.encode_utf8(&mut buf).as_bytes() {
                                value = (value << 8) | i64::from(*b);
                            }
                            Ok(Value::BigInt(value))
                        }
                    }
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "ord() needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // MySQL lcase / ucase — aliases of lower / upper.
        "upper" | "ucase" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("upper() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::text(s.to_uppercase())),
                // PG `upper(anyrange)` returns the upper bound (NULL for an
                // empty or upper-unbounded range).
                Value::Range { upper, empty, .. } => Ok(if *empty {
                    Value::Null
                } else {
                    upper.as_ref().map_or(Value::Null, |v| (**v).clone())
                }),
                other => Err(EvalError::TypeMismatch {
                    detail: format!("upper() needs text, got {:?}", other.data_type()),
                }),
            }
        }
        "lower" | "lcase" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("lower() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::text(s.to_lowercase())),
                // PG `lower(anyrange)` returns the lower bound (NULL for an
                // empty or lower-unbounded range).
                Value::Range { lower, empty, .. } => Ok(if *empty {
                    Value::Null
                } else {
                    lower.as_ref().map_or(Value::Null, |v| (**v).clone())
                }),
                other => Err(EvalError::TypeMismatch {
                    detail: format!("lower() needs text, got {:?}", other.data_type()),
                }),
            }
        }
        "abs" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("abs() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                // v7.38 (read01 P4.18) — abs(INT_MIN) has no representable
                // result; error like PG ("out of range") instead of wrapping
                // back to the negative INT_MIN.
                Value::Int(n) => n.checked_abs().map(Value::Int).ok_or_else(|| {
                    EvalError::TypeMismatch {
                        detail: "integer out of range".into(),
                    }
                }),
                Value::BigInt(n) => n.checked_abs().map(Value::BigInt).ok_or_else(|| {
                    EvalError::TypeMismatch {
                        detail: "bigint out of range".into(),
                    }
                }),
                Value::Float(x) => Ok(Value::Float(x.abs())),
                // PG `abs(numeric)` returns numeric — preserve the type
                // and scale, negating only the sign of the mantissa.
                Value::Numeric { scaled, scale } => Ok(Value::Numeric {
                    scaled: scaled.abs(),
                    scale: *scale,
                }),
                other => Err(EvalError::TypeMismatch {
                    detail: format!("abs() needs numeric, got {:?}", other.data_type()),
                }),
            }
        }
        "coalesce" => {
            for a in args {
                if !matches!(a, Value::Null) {
                    return Ok(a.clone().into_owned());
                }
            }
            Ok(Value::Null)
        }
        "date_trunc" => date_trunc(args),
        // v7.37.17 (17.6 siblings) — PG 14+ date_bin(stride, ts, origin)
        // "bins" ts to the nearest lower multiple of stride from
        // origin. Real implementation: compute integer micros
        // between ts and origin, divide by stride, multiply back,
        // add to origin.
        "date_bin" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("date_bin() takes 3 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            // stride: Interval (months=0 or reject — PG rejects
            // month-strides because months aren't a fixed length).
            // PG coerces an unadorned interval string ('15 minutes') to
            // interval for the stride argument, so accept both a real
            // Interval value and a Text literal.
            let (stride_months, stride_days, stride_micros) = match &args[0] {
                Value::Interval { months, days, micros } => (*months, *days, *micros),
                Value::Text(s) => match spg_sql::parser::parse_interval_text(s) {
                    Some(parts) => parts,
                    None => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "date_bin(): cannot parse stride interval {s:?}"
                            ),
                        });
                    }
                },
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "date_bin(): stride must be interval, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if stride_months != 0 {
                return Err(EvalError::TypeMismatch {
                    detail: "date_bin(): stride with months not supported".into(),
                });
            }
            const DAY_US: i64 = 24 * 60 * 60 * 1_000_000;
            let stride_us: i64 = i64::from(stride_days) * DAY_US + stride_micros;
            if stride_us <= 0 {
                return Err(EvalError::TypeMismatch {
                    detail: "date_bin(): stride must be positive".into(),
                });
            }
            let ts_us = match &args[1] {
                Value::Timestamp(t) => *t,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "date_bin(): ts must be timestamp, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let origin_us = match &args[2] {
                Value::Timestamp(t) => *t,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "date_bin(): origin must be timestamp, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let delta = ts_us - origin_us;
            let bins = delta.div_euclid(stride_us);
            let binned = origin_us + bins * stride_us;
            Ok(Value::Timestamp(binned))
        }
        "date_part" => date_part(args),
        // v7.37.17 (17.6 siblings) — isfinite(date|timestamp|interval)
        // returns whether the value is finite (not ±infinity). SPG
        // doesn't store infinite date/timestamp sentinels, so
        // everything stored is finite → true. Float args check
        // f64::is_finite for the numeric overload some drivers emit.
        // MySQL date accessors — dayname / monthname / dayofweek /
        // dayofyear / weekofyear / last_day / datediff. All real,
        // computed on days-since-epoch via civil_from_days.
        "dayname" | "monthname" | "dayofweek" | "dayofyear" | "weekofyear"
        | "last_day" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            let days: i32 = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Date(d) => *d,
                Value::Timestamp(us) => {
                    (us.div_euclid(86_400_000_000)) as i32
                }
                Value::Text(s) => {
                    match super::format::parse_date_literal(s) {
                        Some(d) => d,
                        None => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "{name}(): invalid date {s:?}"
                                ),
                            });
                        }
                    }
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() needs date, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let (y, m, d) = super::civil_from_days(days);
            match name {
                "dayname" => {
                    const NAMES: [&str; 7] = [
                        "Monday", "Tuesday", "Wednesday", "Thursday",
                        "Friday", "Saturday", "Sunday",
                    ];
                    // 1970-01-01 was a Thursday (index 3).
                    let idx = (i64::from(days) + 3).rem_euclid(7) as usize;
                    Ok(Value::text::<String>(NAMES[idx].into()))
                }
                "monthname" => {
                    const NAMES: [&str; 12] = [
                        "January", "February", "March", "April", "May",
                        "June", "July", "August", "September", "October",
                        "November", "December",
                    ];
                    Ok(Value::text::<String>(NAMES[m as usize - 1].into()))
                }
                "dayofweek" => {
                    // MySQL: 1 = Sunday .. 7 = Saturday.
                    let idx = (i64::from(days) + 4).rem_euclid(7);
                    Ok(Value::Int(idx as i32 + 1))
                }
                "dayofyear" => {
                    let jan1 = super::days_from_civil(y, 1, 1);
                    Ok(Value::Int(days - jan1 + 1))
                }
                "weekofyear" => {
                    // ISO 8601 week number: the week containing the
                    // year's first Thursday is week 1.
                    let weekday = (i64::from(days) + 3).rem_euclid(7) as i32; // 0=Mon
                    let thursday = days + (3 - weekday);
                    let (ty, ..) = super::civil_from_days(thursday);
                    let jan1 = super::days_from_civil(ty, 1, 1);
                    Ok(Value::Int((thursday - jan1) / 7 + 1))
                }
                "last_day" => {
                    let next_month = if m == 12 {
                        super::days_from_civil(y + 1, 1, 1)
                    } else {
                        super::days_from_civil(y, m + 1, 1)
                    };
                    let _ = d;
                    Ok(Value::Date(next_month - 1))
                }
                _ => unreachable!(),
            }
        }
        // MySQL quote(str) — single-quoted literal with backslash
        // escaping; SQL NULL renders as the word NULL (no quotes).
        "quote" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("quote() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::text::<String>("NULL".into())),
                Value::Text(s) => {
                    let mut out = alloc::string::String::with_capacity(s.len() + 2);
                    out.push('\'');
                    for c in s.chars() {
                        match c {
                            '\'' => out.push_str("\\'"),
                            '\\' => out.push_str("\\\\"),
                            '\0' => out.push_str("\\0"),
                            '\u{1a}' => out.push_str("\\Z"),
                            other => out.push(other),
                        }
                    }
                    out.push('\'');
                    Ok(Value::text(out))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "quote() needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // MySQL export_set(bits, on, off[, separator[, n_bits]]) —
        // one on/off string per bit, LSB first.
        "export_set" => {
            if !(3..=5).contains(&args.len()) {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "export_set() takes 3-5 args, got {}",
                        args.len()
                    ),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let bits = match &args[0] {
                Value::Int(n) => *n as i64 as u64,
                Value::BigInt(n) => *n as u64,
                Value::SmallInt(n) => *n as i64 as u64,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "export_set() bits must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let text_of = |v: &Value<'_>, what: &str| -> Result<alloc::string::String, EvalError> {
                match v {
                    Value::Text(s) => Ok(s.as_ref().into()),
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "export_set() {what} must be text, got {:?}",
                            other.data_type()
                        ),
                    }),
                }
            };
            let on = text_of(&args[1], "on")?;
            let off = text_of(&args[2], "off")?;
            let sep = match args.get(3) {
                Some(v) => text_of(v, "separator")?,
                None => ",".into(),
            };
            let n_bits = match args.get(4) {
                Some(Value::Int(n)) => (*n).clamp(0, 64) as u32,
                Some(Value::BigInt(n)) => (*n).clamp(0, 64) as u32,
                Some(Value::SmallInt(n)) => i32::from(*n).clamp(0, 64) as u32,
                None => 64,
                Some(other) => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "export_set() n_bits must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let mut parts = alloc::vec::Vec::with_capacity(n_bits as usize);
            for i in 0..n_bits {
                parts.push(if bits & (1u64 << i) != 0 {
                    on.clone()
                } else {
                    off.clone()
                });
            }
            Ok(Value::text(parts.join(&sep)))
        }
        // MySQL make_set(bits, str1, str2, ...) — the strings whose
        // bit is set, comma-joined; NULL members skipped.
        "make_set" => {
            if args.len() < 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "make_set() takes 2+ args, got {}",
                        args.len()
                    ),
                });
            }
            let bits = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => *n as i64 as u64,
                Value::BigInt(n) => *n as u64,
                Value::SmallInt(n) => *n as i64 as u64,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "make_set() bits must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let mut parts: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
            for (i, member) in args[1..].iter().enumerate() {
                if bits & (1u64 << i) != 0 {
                    if let Value::Text(s) = member {
                        parts.push(s.as_ref());
                    }
                }
            }
            Ok(Value::text(parts.join(",")))
        }
        // MySQL bare date-component accessors — day / dayofmonth /
        // month / year / weekday / week. The single most common
        // MySQL report shape (`SELECT MONTH(created_at) ...`).
        "day" | "dayofmonth" | "month" | "year" | "weekday" | "week" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            let days: i32 = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Date(d) => *d,
                Value::Timestamp(us) => (us.div_euclid(86_400_000_000)) as i32,
                Value::Text(s) => match super::format::parse_date_literal(s) {
                    Some(d) => d,
                    None => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "{name}(): invalid date {s:?}"
                            ),
                        });
                    }
                },
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() needs date, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let (y, m, d) = super::civil_from_days(days);
            match name {
                "day" | "dayofmonth" => Ok(Value::Int(d as i32)),
                "month" => Ok(Value::Int(m as i32)),
                "year" => Ok(Value::Int(y)),
                // MySQL WEEKDAY: 0 = Monday .. 6 = Sunday.
                "weekday" => {
                    Ok(Value::Int((i64::from(days) + 3).rem_euclid(7) as i32))
                }
                // WEEK mode 0: weeks start Sunday; days before the
                // year's first Sunday are week 0.
                "week" => {
                    let jan1 = super::days_from_civil(y, 1, 1);
                    let wd_sun = (i64::from(jan1) + 4).rem_euclid(7) as i32;
                    let first_sunday = jan1 + ((7 - wd_sun) % 7);
                    if days >= first_sunday {
                        Ok(Value::Int((days - first_sunday) / 7 + 1))
                    } else {
                        Ok(Value::Int(0))
                    }
                }
                _ => unreachable!(),
            }
        }
        // MySQL bare time-component accessors — hour / minute /
        // second on time text or timestamps.
        "hour" | "minute" | "second" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            let us: i64 = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Timestamp(t) => t.rem_euclid(86_400_000_000),
                Value::Text(s) => {
                    // '[-]H:MM:SS[.ffffff]' — reuse the simple split;
                    // hour may exceed 24 for MySQL TIME values.
                    let body = s.trim().trim_start_matches('-');
                    let mut parts = body.split(':');
                    let h: i64 = parts
                        .next()
                        .and_then(|p| p.parse().ok())
                        .ok_or_else(|| EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "{name}(): invalid time {s:?}"
                            ),
                        })?;
                    let m: i64 =
                        parts.next().unwrap_or("0").parse().unwrap_or(0);
                    let sec_part = parts.next().unwrap_or("0");
                    let sec: i64 = sec_part
                        .split_once('.')
                        .map_or(sec_part, |(w, _)| w)
                        .parse()
                        .unwrap_or(0);
                    h * 3_600_000_000 + m * 60_000_000 + sec * 1_000_000
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() needs time, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            match name {
                "hour" => Ok(Value::Int((us / 3_600_000_000) as i32)),
                "minute" => Ok(Value::Int(((us / 60_000_000) % 60) as i32)),
                "second" => Ok(Value::Int(((us / 1_000_000) % 60) as i32)),
                _ => unreachable!(),
            }
        }
        // MySQL period_add(P, N) / period_diff(P1, P2) — YYYYMM
        // period arithmetic.
        "period_add" | "period_diff" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let period_of = |v: &Value<'_>| -> Option<i64> {
                let p = match v {
                    Value::Int(n) => i64::from(*n),
                    Value::BigInt(n) => *n,
                    Value::SmallInt(n) => i64::from(*n),
                    _ => return None,
                };
                // YYMM shorthand → 20YY/19YY like MySQL.
                let p = if p < 10_000 {
                    let yy = p / 100;
                    let mm = p % 100;
                    if yy < 70 { (2000 + yy) * 100 + mm } else { (1900 + yy) * 100 + mm }
                } else {
                    p
                };
                Some(p)
            };
            let months_of = |p: i64| -> i64 { (p / 100) * 12 + (p % 100) - 1 };
            match name {
                "period_add" => {
                    let (Some(p), n) = (
                        period_of(&args[0]),
                        match &args[1] {
                            Value::Int(n) => i64::from(*n),
                            Value::BigInt(n) => *n,
                            Value::SmallInt(n) => i64::from(*n),
                            _ => {
                                return Err(EvalError::TypeMismatch {
                                    detail: "period_add() takes integers".into(),
                                });
                            }
                        },
                    ) else {
                        return Err(EvalError::TypeMismatch {
                            detail: "period_add() takes integers".into(),
                        });
                    };
                    let total = months_of(p) + n;
                    Ok(Value::BigInt(
                        (total.div_euclid(12)) * 100 + total.rem_euclid(12) + 1,
                    ))
                }
                "period_diff" => {
                    let (Some(a), Some(b)) =
                        (period_of(&args[0]), period_of(&args[1]))
                    else {
                        return Err(EvalError::TypeMismatch {
                            detail: "period_diff() takes integers".into(),
                        });
                    };
                    Ok(Value::BigInt(months_of(a) - months_of(b)))
                }
                _ => unreachable!(),
            }
        }
        // MySQL time-of-day arithmetic — time_to_sec / sec_to_time /
        // maketime / addtime / subtime / timediff / microsecond.
        // SPG carries TIME as text; these parse '[-]H:MM:SS[.ffffff]'
        // into signed microseconds and format back (hours may exceed
        // 24, like MySQL's TIME type).
        "time_to_sec" | "sec_to_time" | "maketime" | "addtime" | "subtime"
        | "timediff" | "microsecond" => {
            fn parse_time_us(s: &str) -> Option<i64> {
                let s = s.trim();
                let (neg, body) = match s.strip_prefix('-') {
                    Some(rest) => (true, rest),
                    None => (false, s),
                };
                let mut parts = body.split(':');
                let h: i64 = parts.next()?.parse().ok()?;
                let m: i64 = parts.next().unwrap_or("0").parse().ok()?;
                let sec_part = parts.next().unwrap_or("0");
                let (sec_s, frac_s) =
                    sec_part.split_once('.').unwrap_or((sec_part, ""));
                let sec: i64 = sec_s.parse().ok()?;
                let mut frac_padded = alloc::string::String::from(frac_s);
                while frac_padded.len() < 6 {
                    frac_padded.push('0');
                }
                frac_padded.truncate(6);
                let micros: i64 = if frac_padded.is_empty() {
                    0
                } else {
                    frac_padded.parse().ok()?
                };
                let total =
                    h * 3_600_000_000 + m * 60_000_000 + sec * 1_000_000 + micros;
                Some(if neg { -total } else { total })
            }
            fn format_time_us(us: i64) -> alloc::string::String {
                let neg = us < 0;
                let abs = us.unsigned_abs();
                let h = abs / 3_600_000_000;
                let m = (abs / 60_000_000) % 60;
                let s = (abs / 1_000_000) % 60;
                let frac = abs % 1_000_000;
                let sign = if neg { "-" } else { "" };
                if frac == 0 {
                    alloc::format!("{sign}{h:02}:{m:02}:{s:02}")
                } else {
                    alloc::format!("{sign}{h:02}:{m:02}:{s:02}.{frac:06}")
                }
            }
            let time_arg = |v: &Value<'_>,
                            what: &str|
             -> Result<Option<i64>, EvalError> {
                match v {
                    Value::Null => Ok(None),
                    Value::Text(s) => {
                        parse_time_us(s).map(Some).ok_or_else(|| {
                            EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "{what}: invalid time {s:?}"
                                ),
                            }
                        })
                    }
                    Value::Timestamp(us) => {
                        Ok(Some(us.rem_euclid(86_400_000_000)))
                    }
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{what}: needs time, got {:?}",
                            other.data_type()
                        ),
                    }),
                }
            };
            match name {
                "time_to_sec" => {
                    let Some(us) = time_arg(&args[0], "time_to_sec()")? else {
                        return Ok(Value::Null);
                    };
                    Ok(Value::BigInt(us.div_euclid(1_000_000)))
                }
                "sec_to_time" => {
                    let secs = match &args[0] {
                        Value::Null => return Ok(Value::Null),
                        Value::Int(n) => i64::from(*n),
                        Value::BigInt(n) => *n,
                        Value::SmallInt(n) => i64::from(*n),
                        other => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "sec_to_time() needs integer, got {:?}",
                                    other.data_type()
                                ),
                            });
                        }
                    };
                    Ok(Value::text(format_time_us(secs * 1_000_000)))
                }
                "maketime" => {
                    if args.len() != 3 {
                        return Err(EvalError::TypeMismatch {
                            detail: format!(
                                "maketime() takes 3 args, got {}",
                                args.len()
                            ),
                        });
                    }
                    if args.iter().any(|v| matches!(v, Value::Null)) {
                        return Ok(Value::Null);
                    }
                    let int_of = |v: &Value<'_>| -> Option<i64> {
                        match v {
                            Value::Int(n) => Some(i64::from(*n)),
                            Value::BigInt(n) => Some(*n),
                            Value::SmallInt(n) => Some(i64::from(*n)),
                            _ => None,
                        }
                    };
                    let (Some(h), Some(m), Some(s)) = (
                        int_of(&args[0]),
                        int_of(&args[1]),
                        int_of(&args[2]),
                    ) else {
                        return Err(EvalError::TypeMismatch {
                            detail: "maketime() takes 3 integer args".into(),
                        });
                    };
                    if !(0..=59).contains(&m) || !(0..=59).contains(&s) {
                        return Ok(Value::Null);
                    }
                    let sign = if h < 0 { -1 } else { 1 };
                    let us = sign
                        * (h.abs() * 3_600_000_000
                            + m * 60_000_000
                            + s * 1_000_000);
                    Ok(Value::text(format_time_us(us)))
                }
                "addtime" | "subtime" | "timediff" => {
                    if args.len() != 2 {
                        return Err(EvalError::TypeMismatch {
                            detail: format!(
                                "{name}() takes 2 args, got {}",
                                args.len()
                            ),
                        });
                    }
                    let (Some(a), Some(b)) = (
                        time_arg(&args[0], name)?,
                        time_arg(&args[1], name)?,
                    ) else {
                        return Ok(Value::Null);
                    };
                    let result = match name {
                        "addtime" => a + b,
                        "subtime" | "timediff" => a - b,
                        _ => unreachable!(),
                    };
                    Ok(Value::text(format_time_us(result)))
                }
                "microsecond" => {
                    let Some(us) = time_arg(&args[0], "microsecond()")? else {
                        return Ok(Value::Null);
                    };
                    Ok(Value::Int((us.rem_euclid(1_000_000)) as i32))
                }
                _ => unreachable!(),
            }
        }
        // MySQL quarter / to_days / yearweek — same date-input
        // handling as the accessor batch above.
        "quarter" | "to_days" | "yearweek" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            let days: i32 = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Date(d) => *d,
                Value::Timestamp(us) => (us.div_euclid(86_400_000_000)) as i32,
                Value::Text(s) => match super::format::parse_date_literal(s) {
                    Some(d) => d,
                    None => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "{name}(): invalid date {s:?}"
                            ),
                        });
                    }
                },
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() needs date, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let (y, m, _d) = super::civil_from_days(days);
            match name {
                "quarter" => Ok(Value::Int(((m - 1) / 3 + 1) as i32)),
                // MySQL TO_DAYS counts from year 0:
                // to_days('1970-01-01') = 719528.
                "to_days" => Ok(Value::BigInt(i64::from(days) + 719_528)),
                "yearweek" => {
                    // MySQL default mode 0: weeks start Sunday;
                    // days before the year's first Sunday belong to
                    // the previous year's last week.
                    fn week_of(days: i32, year: i32) -> Option<i32> {
                        let jan1 = crate::eval::format::days_from_civil(year, 1, 1);
                        // Weekday with Sunday=0: 1970-01-01 was
                        // Thursday (Sunday-based index 4).
                        let wd_sun = (i64::from(jan1) + 4).rem_euclid(7) as i32;
                        let first_sunday = jan1 + ((7 - wd_sun) % 7);
                        if days >= first_sunday {
                            Some((days - first_sunday) / 7 + 1)
                        } else {
                            None
                        }
                    }
                    match week_of(days, y) {
                        Some(w) => Ok(Value::Int(y * 100 + w)),
                        None => {
                            // Belongs to the previous year's count.
                            let w = week_of(days, y - 1).unwrap_or(52);
                            Ok(Value::Int((y - 1) * 100 + w))
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
        // MySQL from_days(n) — inverse of to_days.
        "from_days" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "from_days() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Date(*n - 719_528)),
                Value::BigInt(n) => Ok(Value::Date((*n - 719_528) as i32)),
                Value::SmallInt(n) => {
                    Ok(Value::Date(i32::from(*n) - 719_528))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "from_days() needs integer, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // MySQL makedate(year, dayofyear) — dayofyear must be ≥ 1;
        // 0 or negative → NULL (MySQL semantics).
        "makedate" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "makedate() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            let int_of = |v: &Value<'_>| -> Option<i64> {
                match v {
                    Value::Int(n) => Some(i64::from(*n)),
                    Value::BigInt(n) => Some(*n),
                    Value::SmallInt(n) => Some(i64::from(*n)),
                    _ => None,
                }
            };
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let (Some(year), Some(doy)) = (int_of(&args[0]), int_of(&args[1]))
            else {
                return Err(EvalError::TypeMismatch {
                    detail: "makedate() takes 2 integer args".into(),
                });
            };
            if doy < 1 {
                return Ok(Value::Null);
            }
            let jan1 = super::days_from_civil(year as i32, 1, 1);
            Ok(Value::Date(jan1 + doy as i32 - 1))
        }
        // MySQL datediff(a, b) — a - b in days (date parts only).
        "datediff" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "datediff() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            let to_days = |v: &Value<'_>| -> Result<Option<i32>, EvalError> {
                match v {
                    Value::Null => Ok(None),
                    Value::Date(d) => Ok(Some(*d)),
                    Value::Timestamp(us) => {
                        Ok(Some(us.div_euclid(86_400_000_000) as i32))
                    }
                    Value::Text(s) => super::format::parse_date_literal(s)
                        .map(Some)
                        .ok_or_else(|| EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "datediff(): invalid date {s:?}"
                            ),
                        }),
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "datediff() needs dates, got {:?}",
                            other.data_type()
                        ),
                    }),
                }
            };
            match (to_days(&args[0])?, to_days(&args[1])?) {
                (Some(a), Some(b)) => Ok(Value::Int(a - b)),
                _ => Ok(Value::Null),
            }
        }
        // MySQL strcmp(a, b) — -1 / 0 / 1 string comparison.
        "strcmp" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("strcmp() takes 2 args, got {}", args.len()),
                });
            }
            match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(a), Value::Text(b)) => {
                    Ok(Value::Int(match a.as_ref().cmp(b.as_ref()) {
                        core::cmp::Ordering::Less => -1,
                        core::cmp::Ordering::Equal => 0,
                        core::cmp::Ordering::Greater => 1,
                    }))
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: "strcmp() takes 2 TEXT args".into(),
                }),
            }
        }
        "isfinite" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("isfinite() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Float(f) => Ok(Value::Bool(f.is_finite())),
                Value::Date(_)
                | Value::Timestamp(_)
                | Value::Interval { .. } => Ok(Value::Bool(true)),
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "isfinite() needs date/timestamp/interval, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — PG 9.4+ make_date / make_time /
        // make_timestamp / make_interval constructors.
        "make_date" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("make_date() takes 3 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            fn int_of(v: &Value<'_>) -> Result<i64, EvalError> {
                match v {
                    Value::Int(n) => Ok(i64::from(*n)),
                    Value::SmallInt(n) => Ok(i64::from(*n)),
                    Value::BigInt(n) => Ok(*n),
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "make_date(): needs int, got {:?}",
                            other.data_type()
                        ),
                    }),
                }
            }
            let y = int_of(&args[0])?;
            let m = int_of(&args[1])?;
            let d = int_of(&args[2])?;
            if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "make_date(): invalid date ({y}, {m}, {d})"
                    ),
                });
            }
            let days = super::days_from_civil(y as i32, m as u32, d as u32);
            Ok(Value::Date(days))
        }
        "make_time" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("make_time() takes 3 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let h = match &args[0] {
                Value::Int(n) => i64::from(*n),
                Value::SmallInt(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "make_time(): hour must be int".into(),
                    });
                }
            };
            let m = match &args[1] {
                Value::Int(n) => i64::from(*n),
                Value::SmallInt(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "make_time(): min must be int".into(),
                    });
                }
            };
            let s = match &args[2] {
                Value::Float(f) => *f,
                Value::Int(n) => f64::from(*n),
                Value::SmallInt(n) => f64::from(*n),
                Value::BigInt(n) => *n as f64,
                Value::Numeric { scaled, scale } => {
                    (*scaled as f64) / f64_powi(10.0, i32::from(*scale))
                }
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "make_time(): sec must be numeric".into(),
                    });
                }
            };
            if !(0..=23).contains(&h) || !(0..=59).contains(&m) || !(0.0..60.0).contains(&s) {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "make_time(): invalid time ({h}, {m}, {s})"
                    ),
                });
            }
            let us = h * 3_600_000_000 + m * 60_000_000 + (s * 1_000_000.0) as i64;
            // v7.38 (read01 sweep) — make_time returns TIME (micros-since-
            // midnight), not TIMESTAMP. `Value::Time` is the standalone type;
            // the old Timestamp shape rendered as "1970-01-01 12:30:00" and
            // reported the wrong pg_typeof.
            Ok(Value::Time(us))
        }
        "make_timestamp" => {
            if args.len() != 6 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "make_timestamp() takes 6 args, got {}",
                        args.len()
                    ),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            fn int_of(v: &Value<'_>) -> Result<i64, EvalError> {
                match v {
                    Value::Int(n) => Ok(i64::from(*n)),
                    Value::SmallInt(n) => Ok(i64::from(*n)),
                    Value::BigInt(n) => Ok(*n),
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "make_timestamp(): needs int, got {:?}",
                            other.data_type()
                        ),
                    }),
                }
            }
            let y = int_of(&args[0])?;
            let mo = int_of(&args[1])?;
            let d = int_of(&args[2])?;
            let h = int_of(&args[3])?;
            let mi = int_of(&args[4])?;
            let s = match &args[5] {
                Value::Float(f) => *f,
                Value::Int(n) => f64::from(*n),
                Value::SmallInt(n) => f64::from(*n),
                Value::BigInt(n) => *n as f64,
                Value::Numeric { scaled, scale } => {
                    (*scaled as f64) / f64_powi(10.0, i32::from(*scale))
                }
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "make_timestamp(): sec must be numeric".into(),
                    });
                }
            };
            if !(1..=12).contains(&mo)
                || !(1..=31).contains(&d)
                || !(0..=23).contains(&h)
                || !(0..=59).contains(&mi)
                || !(0.0..60.0).contains(&s)
            {
                return Err(EvalError::TypeMismatch {
                    detail: "make_timestamp(): out-of-range component".into(),
                });
            }
            let days = super::days_from_civil(y as i32, mo as u32, d as u32);
            let us = i64::from(days) * 86_400_000_000
                + h * 3_600_000_000
                + mi * 60_000_000
                + (s * 1_000_000.0) as i64;
            Ok(Value::Timestamp(us))
        }
        "make_interval" => {
            // make_interval(years, months, weeks, days, hours,
            // mins, secs) — all optional positional (PG uses named
            // args; positional zero-padding accepted here).
            if args.len() > 7 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "make_interval() takes 0-7 args, got {}",
                        args.len()
                    ),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            fn num_of(v: Option<&Value<'_>>) -> Result<f64, EvalError> {
                match v {
                    None => Ok(0.0),
                    Some(Value::Int(n)) => Ok(f64::from(*n)),
                    Some(Value::SmallInt(n)) => Ok(f64::from(*n)),
                    Some(Value::BigInt(n)) => Ok(*n as f64),
                    Some(Value::Float(f)) => Ok(*f),
                    Some(Value::Numeric { scaled, scale }) => {
                        Ok((*scaled as f64) / f64_powi(10.0, i32::from(*scale)))
                    }
                    Some(other) => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "make_interval(): needs numeric, got {:?}",
                            other.data_type()
                        ),
                    }),
                }
            }
            let years = num_of(args.first())?;
            let months = num_of(args.get(1))?;
            let weeks = num_of(args.get(2))?;
            let days = num_of(args.get(3))?;
            let hours = num_of(args.get(4))?;
            let mins = num_of(args.get(5))?;
            let secs = num_of(args.get(6))?;
            let total_months = (years * 12.0 + months) as i32;
            let total_days = (weeks * 7.0 + days) as i32;
            let micros = (hours * 3_600_000_000.0
                + mins * 60_000_000.0
                + secs * 1_000_000.0) as i64;
            Ok(Value::Interval {
                months: total_months,
                days: total_days,
                micros,
            })
        }
        // v7.37.17 (17.6 siblings) — PG 16+ `date_add(ts, interval)`
        // and `date_subtract(ts, interval)`. Same as `ts + interval`
        // and `ts - interval` but as explicit function form (some
        // ORM query builders emit these).
        "date_add" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("date_add() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            match (&args[0], &args[1]) {
                (Value::Timestamp(t), Value::Interval { months, days, micros }) => {
                    let m = i64::from(*months);
                    let d = i64::from(*days);
                    let extra = m
                        .saturating_mul(30 * 86_400_000_000)
                        + d.saturating_mul(86_400_000_000)
                        + micros;
                    Ok(Value::Timestamp(t.saturating_add(extra)))
                }
                (Value::Date(d), Value::Interval { months, days, micros }) => {
                    let base_us = i64::from(*d).saturating_mul(86_400_000_000);
                    let m = i64::from(*months);
                    let dd = i64::from(*days);
                    let extra = m
                        .saturating_mul(30 * 86_400_000_000)
                        + dd.saturating_mul(86_400_000_000)
                        + micros;
                    Ok(Value::Timestamp(base_us.saturating_add(extra)))
                }
                (a, b) => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "date_add() needs (timestamp|date, interval), got ({:?}, {:?})",
                        a.data_type(),
                        b.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — MySQL ADDDATE / SUBDATE /
        // DATE_SUB. The bare-integer second argument means days
        // (MySQL's ADDDATE(d, 31) shorthand); intervals shift like
        // date_add. Results follow MySQL: DATE + day-granular shift
        // stays a DATE, anything else is a TIMESTAMP.
        "adddate" | "subdate" | "date_sub" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let sign: i64 = if name == "adddate" { 1 } else { -1 };
            let base_is_date = matches!(&args[0], Value::Date(_))
                || matches!(&args[0], Value::Text(s) if !s.contains(':'));
            let base = super::datetime::text_or_temporal_micros(&args[0], name)?;
            let (shift_micros, day_granular) = match &args[1] {
                Value::Int(n) => (i64::from(*n) * 86_400_000_000, true),
                Value::SmallInt(n) => (i64::from(*n) * 86_400_000_000, true),
                Value::BigInt(n) => (n.saturating_mul(86_400_000_000), true),
                Value::Interval {
                    months,
                    days,
                    micros,
                } => (
                    i64::from(*months).saturating_mul(30 * 86_400_000_000)
                        + i64::from(*days).saturating_mul(86_400_000_000)
                        + micros,
                    *micros == 0,
                ),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "{name}() second arg must be days or interval, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let out = base.saturating_add(sign.saturating_mul(shift_micros));
            if base_is_date && day_granular {
                Ok(Value::Date(
                    i32::try_from(out.div_euclid(86_400_000_000)).unwrap_or(i32::MAX),
                ))
            } else {
                Ok(Value::Timestamp(out))
            }
        }
        "date_subtract" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("date_subtract() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            match (&args[0], &args[1]) {
                (Value::Timestamp(t), Value::Interval { months, days, micros }) => {
                    let m = i64::from(*months);
                    let d = i64::from(*days);
                    let extra = m
                        .saturating_mul(30 * 86_400_000_000)
                        + d.saturating_mul(86_400_000_000)
                        + micros;
                    Ok(Value::Timestamp(t.saturating_sub(extra)))
                }
                (Value::Date(d), Value::Interval { months, days, micros }) => {
                    let base_us = i64::from(*d).saturating_mul(86_400_000_000);
                    let m = i64::from(*months);
                    let dd = i64::from(*days);
                    let extra = m
                        .saturating_mul(30 * 86_400_000_000)
                        + dd.saturating_mul(86_400_000_000)
                        + micros;
                    Ok(Value::Timestamp(base_us.saturating_sub(extra)))
                }
                (a, b) => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "date_subtract() needs (timestamp|date, interval), got ({:?}, {:?})",
                        a.data_type(),
                        b.data_type()
                    ),
                }),
            }
        }
        "age" => age(args),
        // mxid_age(xid) — multixact wraparound distance. SPG has no
        // multixact machinery (single-writer model) → honestly 0,
        // same rationale as the age(xid) overload.
        "mxid_age" => match args.first() {
            Some(Value::Null) | None => Ok(Value::Null),
            _ => Ok(Value::Int(0)),
        },
        "to_char" => to_char(args),
        // v7.37.17 (17.6 siblings) — PG's `to_number(text, fmt)`
        // parses a formatted numeric string. Real implementation
        // strips the format-marker characters ($, ',', ' ', 'D',
        // 'G') and parses the residue as a float. The `fmt` arg
        // is used for validation-shape but the parse is
        // permissive since many callers pass '9G999D99' style
        // masks that we can safely ignore.
        "to_number" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("to_number() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let raw = match &args[0] {
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "to_number(): needs text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let _fmt = match &args[1] {
                Value::Text(s) => s.to_string(),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "to_number(): fmt must be text".into(),
                    });
                }
            };
            // Strip locale + presentation chars.
            let cleaned: alloc::string::String = raw
                .chars()
                .filter(|c| {
                    c.is_ascii_digit()
                        || matches!(*c, '.' | '-' | '+' | 'e' | 'E')
                })
                .collect();
            if cleaned.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "to_number(): could not parse {raw:?}"
                    ),
                });
            }
            // v7.38 (read01 P6.02) — PG's to_number returns `numeric`, not a
            // float; parse into the exact (scaled, scale) representation so
            // long / high-precision inputs don't lose digits to f64.
            match crate::numeric::parse_numeric_text(&cleaned) {
                Some((scaled, scale)) => Ok(Value::Numeric { scaled, scale }),
                None => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("to_number(): could not parse {cleaned:?}"),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — fuzzystrmatch soundex(text)
        // returns the 4-char Soundex code (Russell / Odell 1918
        // classic). PG's fuzzystrmatch extension emits this. Used
        // by name-search / duplicate-detection ORM queries and by
        // pg_trgm's alt-search paths.
        "soundex" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("soundex() takes 1 arg, got {}", args.len()),
                });
            }
            let s = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "soundex(): needs text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            fn code(c: char) -> Option<char> {
                match c.to_ascii_uppercase() {
                    'B' | 'F' | 'P' | 'V' => Some('1'),
                    'C' | 'G' | 'J' | 'K' | 'Q' | 'S' | 'X' | 'Z' => Some('2'),
                    'D' | 'T' => Some('3'),
                    'L' => Some('4'),
                    'M' | 'N' => Some('5'),
                    'R' => Some('6'),
                    _ => None,
                }
            }
            let mut chars = s.chars().filter(|c| c.is_ascii_alphabetic()).peekable();
            let mut out = alloc::string::String::new();
            let Some(first) = chars.next() else {
                return Ok(Value::text::<String>("".into()));
            };
            out.push(first.to_ascii_uppercase());
            let mut last_code = code(first);
            for c in chars {
                let cur = code(c);
                if cur.is_some() && cur != last_code {
                    out.push(cur.unwrap());
                    if out.len() >= 4 {
                        break;
                    }
                }
                if cur.is_some() {
                    last_code = cur;
                } else {
                    // Vowels + h + w — do NOT reset last_code so
                    // classic PG matches ('HERMAN' → 'H655', not
                    // 'H6555').
                    if !matches!(c.to_ascii_uppercase(), 'H' | 'W') {
                        last_code = None;
                    }
                }
            }
            while out.len() < 4 {
                out.push('0');
            }
            Ok(Value::text(out))
        }
        // v7.37.17 (17.6 siblings) — fuzzystrmatch difference(a, b)
        // computes Soundex codes of both inputs and returns how
        // many characters (0-4) of the two codes match. Common
        // ORM idiom for name-similarity thresholds.
        "difference" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("difference() takes 2 args, got {}", args.len()),
                });
            }
            fn soundex_str(v: &Value<'_>) -> Result<Option<alloc::string::String>, EvalError> {
                let s = match v {
                    Value::Null => return Ok(None),
                    Value::Text(s) => s.to_string(),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "difference(): needs text, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                };
                // Reuse the soundex encoder inline (same as the
                // arm below).
                fn code(c: char) -> Option<char> {
                    match c.to_ascii_uppercase() {
                        'B' | 'F' | 'P' | 'V' => Some('1'),
                        'C' | 'G' | 'J' | 'K' | 'Q' | 'S' | 'X' | 'Z' => Some('2'),
                        'D' | 'T' => Some('3'),
                        'L' => Some('4'),
                        'M' | 'N' => Some('5'),
                        'R' => Some('6'),
                        _ => None,
                    }
                }
                let mut chars = s.chars().filter(|c| c.is_ascii_alphabetic());
                let mut out = alloc::string::String::new();
                let Some(first) = chars.next() else {
                    return Ok(Some(alloc::string::String::new()));
                };
                out.push(first.to_ascii_uppercase());
                let mut last_code = code(first);
                for c in chars {
                    let cur = code(c);
                    if cur.is_some() && cur != last_code {
                        out.push(cur.unwrap());
                        if out.len() >= 4 {
                            break;
                        }
                    }
                    if cur.is_some() {
                        last_code = cur;
                    } else if !matches!(c.to_ascii_uppercase(), 'H' | 'W') {
                        last_code = None;
                    }
                }
                while out.len() < 4 {
                    out.push('0');
                }
                Ok(Some(out))
            }
            let a_code = match soundex_str(&args[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let b_code = match soundex_str(&args[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let matched = a_code
                .chars()
                .zip(b_code.chars())
                .take(4)
                .filter(|(x, y)| x == y)
                .count() as i32;
            Ok(Value::Int(matched))
        }
        // v7.37.17 (17.6 siblings) — fuzzystrmatch extension:
        // levenshtein(a, b [, ins_cost, del_cost, sub_cost])
        // returns edit distance between two texts. Common ORM
        // fuzzy-match idiom + PG regression suite uses this.
        "levenshtein" | "levenshtein_less_equal" => {
            if args.len() < 2 || args.len() > 5 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "levenshtein() takes 2-5 args, got {}",
                        args.len()
                    ),
                });
            }
            let a = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "levenshtein(): needs text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let b = match &args[1] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "levenshtein(): needs text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            // Costs default to 1 each (PG's implicit values).
            let a_chars: alloc::vec::Vec<char> = a.chars().collect();
            let b_chars: alloc::vec::Vec<char> = b.chars().collect();
            let m = a_chars.len();
            let n = b_chars.len();
            // 1-D rolling DP row — O(min(m, n)) memory.
            let (short, long_) = if m <= n {
                (&a_chars, &b_chars)
            } else {
                (&b_chars, &a_chars)
            };
            let mut prev: alloc::vec::Vec<i32> =
                (0..=short.len() as i32).collect();
            for j in 1..=long_.len() {
                let mut curr: alloc::vec::Vec<i32> = alloc::vec![0; short.len() + 1];
                curr[0] = j as i32;
                for i in 1..=short.len() {
                    let cost = if short[i - 1] == long_[j - 1] { 0 } else { 1 };
                    curr[i] = (curr[i - 1] + 1)
                        .min(prev[i] + 1)
                        .min(prev[i - 1] + cost);
                }
                prev = curr;
            }
            Ok(Value::Int(prev[short.len()]))
        }
        // v7.37.17 (17.6 siblings) — pg_backup_start / pg_backup_stop
        // (PG 15+) and pg_start_backup / pg_stop_backup (PG < 15
        // legacy names). Used by wal-g / pgbackrest / native
        // basebackup workflows. Return text '0/0' (LSN form) so
        // callers get valid text data — real backup surface will
        // return actual WAL positions once replication ships.
        "pg_backup_start"
        | "pg_start_backup" => Ok(Value::text::<String>("0/0".into())),
        "pg_backup_stop"
        | "pg_stop_backup" => Ok(Value::text::<String>("0/0".into())),
        // pg_is_in_backup — returns bool whether a backup is in
        // progress. SPG has no in-progress backup surface, so
        // always false.
        "pg_is_in_backup" => Ok(Value::Bool(false)),
        // pg_create_restore_point returns the LSN at which a
        // restore-point label was recorded. Return '0/0'.
        "pg_create_restore_point" => Ok(Value::text::<String>("0/0".into())),
        // pg_backup_labels probes basebackup-metadata; return NULL.
        "pg_backup_label" | "pg_backup_labels" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — `pg_bytes_pretty` alias for
        // pg_size_pretty (some monitoring tools emit either name).
        "pg_bytes_pretty" => {
            // Delegate to pg_size_pretty by re-dispatching the same
            // arm inline.
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "pg_bytes_pretty() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            let n: i64 = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::SmallInt(x) => i64::from(*x),
                Value::Int(x) => i64::from(*x),
                Value::BigInt(x) => *x,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_bytes_pretty(): needs numeric, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let (mut val, mut unit) = (n as f64, "bytes");
            const KB: f64 = 1024.0;
            const CROSSOVER: f64 = 10.0 * KB;
            if val.abs() >= CROSSOVER {
                val /= KB;
                unit = "kB";
                if val.abs() >= CROSSOVER {
                    val /= KB;
                    unit = "MB";
                    if val.abs() >= CROSSOVER {
                        val /= KB;
                        unit = "GB";
                        if val.abs() >= CROSSOVER {
                            val /= KB;
                            unit = "TB";
                            if val.abs() >= CROSSOVER {
                                val /= KB;
                                unit = "PB";
                            }
                        }
                    }
                }
            }
            let s = if unit == "bytes" {
                alloc::format!("{n} bytes")
            } else {
                alloc::format!("{} {unit}", val.round() as i64)
            };
            Ok(Value::text(s))
        }
        // v7.37.17 (17.6 siblings) — `pg_object_size(regclass)` /
        // `pg_object_size(oid)` — table + index size sum. Same
        // shape as pg_total_relation_size; ORM introspection
        // often emits this alias.
        "pg_object_size" | "pg_relation_size_pretty" => Ok(Value::BigInt(0)),
        // v7.37.17 (17.6 siblings) — PG's internal hash operator
        // support functions. These map a value to a 32-bit hash
        // used by hash indexes + hash join. Use Rust's DefaultHasher
        // for a deterministic-per-run answer.
        "hashint4" | "hashint2" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("hashint4() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => {
                    // Inline stable u32 avalanche mix (splitmix-style).
                    let mut state = *n as u32;
                    state = state.wrapping_add(0x9e37_79b1);
                    state = state.wrapping_mul(0x517c_c1b7);
                    state ^= state >> 16;
                    Ok(Value::Int(state as i32))
                }
                Value::SmallInt(n) => Ok(Value::Int((*n as i32).wrapping_mul(0x9e37_79b1u32 as i32))),
                Value::BigInt(n) => Ok(Value::Int(*n as i32)),
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "hashint4() needs integer, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "hashint8" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("hashint8() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::BigInt(n) => {
                    // XOR-fold 64-bit into 32-bit for PG hash-index compat.
                    let hi = (*n >> 32) as i32;
                    let lo = *n as i32;
                    Ok(Value::Int(hi ^ lo))
                }
                Value::Int(n) => Ok(Value::Int(*n)),
                Value::SmallInt(n) => Ok(Value::Int(*n as i32)),
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "hashint8() needs integer, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "hashtext" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("hashtext() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    // FNV-1a 32-bit — stable, no dep on stdlib RNG.
                    let mut h: u32 = 0x811c_9dc5;
                    for b in s.as_bytes() {
                        h ^= *b as u32;
                        h = h.wrapping_mul(0x0100_0193);
                    }
                    Ok(Value::Int(h as i32))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "hashtext() needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "hashbytea" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("hashbytea() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Bytes(b) => {
                    let mut h: u32 = 0x811c_9dc5;
                    for byte in b.iter() {
                        h ^= *byte as u32;
                        h = h.wrapping_mul(0x0100_0193);
                    }
                    Ok(Value::Int(h as i32))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "hashbytea() needs bytea, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — enum introspection stubs.
        // Real semantics thread with the enum type system in
        // v7.40. Callers get parse-through NULL so ORM
        // introspection queries don't crash.
        "enum_first" | "enum_last" | "enum_range" | "enum_range_between" => {
            Ok(Value::Null)
        }
        // v7.37.17 (17.6 siblings) — object identifier text
        // conversion. `to_regclass_text` and `regclass_to_text`
        // both accept an OID or regclass and return the text form.
        // SPG uses table names as regclass keys, so the round-trip
        // is trivial when input is TEXT.
        "regclass_to_text" | "regnamespace_to_text" | "regrole_to_text"
        | "regtype_to_text" | "regoper_to_text" | "regoperator_to_text"
        | "regproc_to_text" | "regprocedure_to_text" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "regclass_to_text() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::text(s.to_string())),
                Value::Int(n) => Ok(Value::text(alloc::format!("{n}"))),
                Value::BigInt(n) => Ok(Value::text(alloc::format!("{n}"))),
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "regclass_to_text(): needs text or int, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — XML surface stubs. Real XML
        // support (xmlparse/xmlelement/xmlserialize/xpath) threads
        // with the xml2/xml crate epic; scalar surface accept-then-
        // NULL preserves parse-through.
        "xmlcomment" => {
            // `xmlcomment(text)` wraps text in <!-- --> and returns
            // it as an xml value. Trivial to implement without a
            // real xml type — return a text-shaped result.
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("xmlcomment() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    if s.contains("--") {
                        return Err(EvalError::TypeMismatch {
                            detail: "xmlcomment(): argument must not contain '--'".into(),
                        });
                    }
                    Ok(Value::text(alloc::format!("<!--{s}-->")))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "xmlcomment(): needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // xml_is_well_formed / _document / _content check syntax
        // of a text-form XML fragment. SPG doesn't ship a real
        // XML parser yet — use a defensive heuristic that returns
        // true only when the input is empty or looks like XML
        // (starts with '<'). Real xml crate integration is queued
        // with the XML epic.
        "xml_is_well_formed"
        | "xml_is_well_formed_document"
        | "xml_is_well_formed_content" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "xml_is_well_formed() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let t = s.trim();
                    if t.is_empty() {
                        return Ok(Value::Bool(true));
                    }
                    if !t.starts_with('<') || !t.ends_with('>') {
                        return Ok(Value::Bool(false));
                    }
                    // Rough count-tag balance check. Not a real
                    // parser, but rejects obvious garbage.
                    let opens = t.matches('<').count();
                    let closes = t.matches('>').count();
                    Ok(Value::Bool(opens == closes))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "xml_is_well_formed(): needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // xpath / xpath_exists / xmlexists — SETOF or bool result.
        // Return NULL / false until real parser lands.
        "xpath" | "xmlexists" => Ok(Value::Null),
        "xpath_exists" => Ok(Value::Bool(false)),
        // xmltext(text) — PG 16+: escape a string into an XML text
        // node (&, <, > get entity-escaped). Real implementation.
        "xmltext" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "xmltext() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let mut out = alloc::string::String::with_capacity(s.len());
                    for c in s.chars() {
                        match c {
                            '&' => out.push_str("&amp;"),
                            '<' => out.push_str("&lt;"),
                            '>' => out.push_str("&gt;"),
                            other => out.push(other),
                        }
                    }
                    Ok(Value::text(out))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "xmltext(): needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // xmlelement(name_text, content …) — the parser lowers
        // `XMLELEMENT(NAME ident, …)` to this call with the element name as
        // the first (text) argument. Content args concatenate as the element
        // body: xml-typed content is inserted verbatim, everything else is
        // text-escaped (& < >). No content → a self-closing `<name/>`.
        "xmlelement" => {
            if args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: "xmlelement() needs an element name".into(),
                });
            }
            let name = match &args[0] {
                Value::Null => {
                    return Err(EvalError::TypeMismatch {
                        detail: "xmlelement() name must not be NULL".into(),
                    });
                }
                Value::Text(s) => s.to_string(),
                other => value_to_format_text(other),
            };
            let mut body = alloc::string::String::new();
            for arg in &args[1..] {
                match arg {
                    Value::Null => {}
                    Value::Xml(s) => body.push_str(s),
                    other => {
                        for c in value_to_format_text(other).chars() {
                            match c {
                                '&' => body.push_str("&amp;"),
                                '<' => body.push_str("&lt;"),
                                '>' => body.push_str("&gt;"),
                                o => body.push(o),
                            }
                        }
                    }
                }
            }
            let xml = if body.is_empty() {
                alloc::format!("<{name}/>")
            } else {
                alloc::format!("<{name}>{body}</{name}>")
            };
            Ok(Value::Xml(alloc::borrow::Cow::Owned(xml)))
        }
        // xmlforest(name1, val1, name2, val2, …) — the parser lowers
        // `XMLFOREST(value AS name, …)` to alternating name/value args. Each
        // pair yields a `<name>value</name>` element (value text-escaped);
        // a NULL value omits its element. Result is the concatenation.
        "xmlforest" => {
            if args.len() % 2 != 0 {
                return Err(EvalError::TypeMismatch {
                    detail: "xmlforest() expects name/value pairs".into(),
                });
            }
            let mut out = alloc::string::String::new();
            for pair in args.chunks_exact(2) {
                let name = match &pair[0] {
                    Value::Text(s) => s.to_string(),
                    other => value_to_format_text(other),
                };
                match &pair[1] {
                    Value::Null => {}
                    Value::Xml(s) => {
                        out.push_str(&alloc::format!("<{name}>{s}</{name}>"));
                    }
                    other => {
                        let mut body = alloc::string::String::new();
                        for c in value_to_format_text(other).chars() {
                            match c {
                                '&' => body.push_str("&amp;"),
                                '<' => body.push_str("&lt;"),
                                '>' => body.push_str("&gt;"),
                                o => body.push(o),
                            }
                        }
                        out.push_str(&alloc::format!("<{name}>{body}</{name}>"));
                    }
                }
            }
            Ok(Value::Xml(alloc::borrow::Cow::Owned(out)))
        }
        // xmlconcat(xml, ...) — variadic fragment concatenation.
        // NULL args are skipped; all-NULL → NULL (PG semantics).
        "xmlconcat" => {
            let mut out = alloc::string::String::new();
            let mut any = false;
            for arg in args {
                match arg {
                    Value::Null => {}
                    Value::Text(s) => {
                        out.push_str(s);
                        any = true;
                    }
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "xmlconcat(): needs xml text, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                }
            }
            if any {
                Ok(Value::text(out))
            } else {
                Ok(Value::Null)
            }
        }
        // XML export family — table_to_xml / query_to_xml /
        // cursor_to_xml / schema_to_xml / database_to_xml + the
        // _and_xmlschema / _to_xmlschema variants. Mapping catalog
        // state into PG's XML Schema output queues with the XML
        // epic; NULL keeps exporter scripts parse-through.
        "table_to_xml"
        | "query_to_xml"
        | "cursor_to_xml"
        | "schema_to_xml"
        | "database_to_xml"
        | "table_to_xmlschema"
        | "query_to_xmlschema"
        | "cursor_to_xmlschema"
        | "schema_to_xmlschema"
        | "database_to_xmlschema"
        | "table_to_xml_and_xmlschema"
        | "query_to_xml_and_xmlschema"
        | "schema_to_xml_and_xmlschema"
        | "database_to_xml_and_xmlschema" => Ok(Value::Null),
        // xmlagg is aggregate; xmlparse / xmlserialize / xmlelement
        // are parser-level constructs (special-case in AST). This
        // arm covers only their scalar synonyms if any driver
        // emits them. (xmlconcat has a real implementation above.)
        "xml" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — replication-slot + subscription
        // + progress-info stat probes emitted by postgres_exporter
        // and Prometheus replication-lag scrapes.
        "pg_stat_get_replication_slot"
        | "pg_stat_get_subscription"
        | "pg_stat_get_subscription_stats"
        | "pg_stat_get_slru"
        | "pg_stat_get_progress_info" => Ok(Value::Null),
        // pg_stat_get_wal_senders / _receivers — return NULL for
        // scalar surface; row surface via pg_stat_replication view.
        "pg_stat_get_wal_senders" | "pg_stat_get_wal_receivers" => Ok(Value::Null),
        // pg_stat_get_backend_client_addr fallback (v7.37.17 slice
        // handled parent; keep alt name too).
        "pg_stat_get_client_addr" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — PG 16+ pg_input_is_valid(
        // text, type_name) probes whether a text value can be
        // parsed as the given type. Real implementation attempts
        // the cast and reports success.
        "pg_input_is_valid" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "pg_input_is_valid() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            let (input, ty) = match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
                (Value::Text(s), Value::Text(t)) => (s.to_string(), t.to_ascii_lowercase()),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "pg_input_is_valid(): both args must be text".into(),
                    });
                }
            };
            // Cover the common builtin types the ORM crowd probes.
            let ok = match ty.as_str() {
                "integer" | "int" | "int4" => input.trim().parse::<i32>().is_ok(),
                "smallint" | "int2" => input.trim().parse::<i16>().is_ok(),
                "bigint" | "int8" => input.trim().parse::<i64>().is_ok(),
                "real" | "float4" | "double precision" | "float8" => {
                    input.trim().parse::<f64>().is_ok()
                }
                "boolean" | "bool" => {
                    matches!(
                        input.trim().to_ascii_lowercase().as_str(),
                        "t" | "true" | "yes" | "on" | "1" |
                        "f" | "false" | "no" | "off" | "0"
                    )
                }
                "text" | "varchar" | "character varying" | "char" | "bpchar" => true,
                "numeric" | "decimal" => {
                    let t = input.trim();
                    !t.is_empty()
                        && t.chars()
                            .all(|c| c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E')
                }
                _ => true, // Unknown type — best-effort.
            };
            Ok(Value::Bool(ok))
        }
        // pg_input_error_info(text, type_name) returns error text
        // if invalid; NULL if valid. SPG returns NULL always for
        // now (real error surfaces after v7.38 type-cast overhaul).
        "pg_input_error_info" | "pg_input_error_message" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — PG 15+ unicode_version() and
        // icu_unicode_version(). Return the Unicode version Rust's
        // std char tables track — matches what unistr / normalize
        // / lower / upper end up computing against.
        "unicode_version" | "icu_unicode_version" => {
            // Rust's std::char is at Unicode 15.0 as of Rust 1.85+;
            // pin to that so callers get a real answer. Real
            // introspection thread with libicu when ICU support
            // lands.
            Ok(Value::text::<String>("15.0".into()))
        }
        // pg_char_to_encoding / pg_encoding_max_length — the
        // encoding introspection pair. pg_encoding_max_length
        // returns the max bytes for a codepoint under the encoding
        // (4 for UTF-8).
        "pg_encoding_max_length" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "pg_encoding_max_length() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                // UTF8 is encoding id 6; SPG only speaks UTF8.
                Value::Int(6) | Value::BigInt(6) => Ok(Value::Int(4)),
                Value::Int(_) | Value::BigInt(_) => Ok(Value::Int(4)),
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "pg_encoding_max_length() needs int encoding id, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // pg_get_multixact_members(oid) — returns a set; scalar
        // surface NULL. pg_xact_commit_timestamp already handled.
        "pg_get_multixact_members" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — PG 16+ unistr(text) processes
        // Unicode escape sequences in the input. Recognizes:
        //   \\        → literal backslash
        //   \XXXX     → hex codepoint (4 hex digits)
        //   \+XXXXXX  → hex codepoint (6 hex digits)
        //   \uXXXX    → hex codepoint (4 hex digits, alt syntax)
        //   \UXXXXXXXX → hex codepoint (8 hex digits, alt syntax)
        "unistr" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("unistr() takes 1 arg, got {}", args.len()),
                });
            }
            let s: alloc::string::String = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "unistr(): needs text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let chars: alloc::vec::Vec<char> = s.chars().collect();
            let mut out = alloc::string::String::new();
            let mut i = 0usize;
            while i < chars.len() {
                let c = chars[i];
                if c != '\\' {
                    out.push(c);
                    i += 1;
                    continue;
                }
                // Backslash escape.
                if i + 1 >= chars.len() {
                    // Trailing backslash — error.
                    return Err(EvalError::TypeMismatch {
                        detail: "unistr(): trailing backslash".into(),
                    });
                }
                let next = chars[i + 1];
                if next == '\\' {
                    out.push('\\');
                    i += 2;
                    continue;
                }
                let (hex_len, start): (usize, usize) = if next == '+' {
                    (6, i + 2)
                } else if next == 'u' {
                    (4, i + 2)
                } else if next == 'U' {
                    (8, i + 2)
                } else if next.is_ascii_hexdigit() {
                    (4, i + 1)
                } else {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "unistr(): unrecognized escape \\{next}"
                        ),
                    });
                };
                if start + hex_len > chars.len() {
                    return Err(EvalError::TypeMismatch {
                        detail: "unistr(): short hex escape".into(),
                    });
                }
                let hex: alloc::string::String =
                    chars[start..start + hex_len].iter().collect();
                let code = u32::from_str_radix(&hex, 16).map_err(|_| {
                    EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "unistr(): invalid hex {hex:?}"
                        ),
                    }
                })?;
                let ch = char::from_u32(code).ok_or(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "unistr(): {code:#x} is not a valid Unicode scalar"
                    ),
                })?;
                out.push(ch);
                i = start + hex_len;
            }
            Ok(Value::text(out))
        }
        // v7.37.17 (17.6 siblings) — PG 11+ starts_with(str, prefix)
        // — the `^@` operator's function form. Also ends_with which
        // PG doesn't ship but many drivers still emit against
        // Amazon RDS / CockroachDB compat.
        "starts_with" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("starts_with() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            match (&args[0], &args[1]) {
                (Value::Text(s), Value::Text(p)) => Ok(Value::Bool(s.starts_with(p.as_ref()))),
                (a, b) => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "starts_with() needs (text, text), got ({:?}, {:?})",
                        a.data_type(),
                        b.data_type()
                    ),
                }),
            }
        }
        "ends_with" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("ends_with() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            match (&args[0], &args[1]) {
                (Value::Text(s), Value::Text(p)) => Ok(Value::Bool(s.ends_with(p.as_ref()))),
                (a, b) => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "ends_with() needs (text, text), got ({:?}, {:?})",
                        a.data_type(),
                        b.data_type()
                    ),
                }),
            }
        }
        // string_agg is aggregate, handled elsewhere. text_starts_with
        // is a PG internal alias for starts_with.
        "text_starts_with" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("text_starts_with() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            match (&args[0], &args[1]) {
                (Value::Text(s), Value::Text(p)) => Ok(Value::Bool(s.starts_with(p.as_ref()))),
                _ => Err(EvalError::TypeMismatch {
                    detail: "text_starts_with(): both args must be text".into(),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — PG 9.6+ `parse_ident(qualname
        // [, strict_mode])` splits a qualified identifier
        // 'schema.table' into a text array ['schema', 'table'].
        // Handles double-quoted parts (preserving embedded dots +
        // case) and PG's rule that a trailing garbage tail is
        // rejected in strict mode (default true).
        "parse_ident" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("parse_ident() takes 1 or 2 args, got {}", args.len()),
                });
            }
            let s: alloc::string::String = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "parse_ident(): needs text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let strict = if args.len() == 2 {
                match &args[1] {
                    Value::Null => true,
                    Value::Bool(b) => *b,
                    _ => {
                        return Err(EvalError::TypeMismatch {
                            detail: "parse_ident(): strict flag must be bool".into(),
                        });
                    }
                }
            } else {
                true
            };
            let mut out: alloc::vec::Vec<Option<alloc::string::String>> =
                alloc::vec::Vec::new();
            let bytes: alloc::vec::Vec<char> = s.chars().collect();
            let mut i = 0usize;
            while i < bytes.len() {
                while i < bytes.len() && bytes[i].is_whitespace() {
                    i += 1;
                }
                if i >= bytes.len() {
                    break;
                }
                if bytes[i] == '"' {
                    // Quoted identifier: preserve as-is between
                    // matching quotes; "" is an escaped quote.
                    i += 1;
                    let mut piece = alloc::string::String::new();
                    while i < bytes.len() {
                        if bytes[i] == '"' {
                            if i + 1 < bytes.len() && bytes[i + 1] == '"' {
                                piece.push('"');
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        } else {
                            piece.push(bytes[i]);
                            i += 1;
                        }
                    }
                    out.push(Some(piece));
                } else if bytes[i].is_alphabetic() || bytes[i] == '_' {
                    let mut piece = alloc::string::String::new();
                    while i < bytes.len()
                        && (bytes[i].is_alphanumeric() || bytes[i] == '_' || bytes[i] == '$')
                    {
                        // Downcase per PG's default lower-fold.
                        for c in bytes[i].to_lowercase() {
                            piece.push(c);
                        }
                        i += 1;
                    }
                    out.push(Some(piece));
                } else {
                    // Garbage char.
                    if strict {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "parse_ident(): unexpected character {:?} in {s:?}",
                                bytes[i]
                            ),
                        });
                    } else {
                        break;
                    }
                }
                // Look for a separator.
                while i < bytes.len() && bytes[i].is_whitespace() {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == '.' {
                    i += 1;
                } else if strict && i < bytes.len() {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "parse_ident(): trailing garbage {:?}",
                            &s[i..]
                        ),
                    });
                }
            }
            Ok(Value::TextArray(out))
        }
        // v7.37.17 (17.6 siblings) — PG's `pg_size_bytes(text)`
        // parses a human-readable size string ("2MB", "1.5 GB",
        // "512 kB") into a BigInt byte count. Inverse of
        // pg_size_pretty. Matches PG's unit table:
        //   bytes / B         → 1
        //   kB                → 1024
        //   MB                → 1024^2
        //   GB                → 1024^3
        //   TB                → 1024^4
        //   PB                → 1024^5
        // No unit → bytes.
        "pg_size_bytes" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("pg_size_bytes() takes 1 arg, got {}", args.len()),
                });
            }
            let raw: alloc::string::String = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_size_bytes() needs text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: "pg_size_bytes(): empty input".into(),
                });
            }
            // Split into numeric prefix + unit suffix. The unit is
            // whatever trailing alphabetic run is at the end.
            let split_at = trimmed.trim_end().rfind(|c: char| {
                c.is_ascii_digit() || c == '.' || c == '-' || c == '+'
            });
            let (num_str, unit_raw) = match split_at {
                Some(i) => trimmed.split_at(i + 1),
                None => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_size_bytes(): could not parse {trimmed:?}"
                        ),
                    });
                }
            };
            let num: f64 = num_str.trim().parse().map_err(|_| {
                EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "pg_size_bytes(): could not parse numeric part {num_str:?}"
                    ),
                }
            })?;
            let unit = unit_raw.trim().to_ascii_lowercase();
            let mul: i64 = match unit.as_str() {
                "" | "bytes" | "byte" | "b" => 1,
                "kb" | "kib" => 1024,
                "mb" | "mib" => 1024_i64.pow(2),
                "gb" | "gib" => 1024_i64.pow(3),
                "tb" | "tib" => 1024_i64.pow(4),
                "pb" | "pib" => 1024_i64.pow(5),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_size_bytes(): unknown unit {other:?}"
                        ),
                    });
                }
            };
            let bytes = (num * mul as f64).round() as i64;
            Ok(Value::BigInt(bytes))
        }
        // v7.37.17 (17.6 siblings) — PG's `to_timestamp(double)`
        // converts a Unix epoch (seconds since 1970-01-01 UTC) to a
        // `timestamp with time zone`. SPG uses micros-since-epoch
        // internally, so this is just a scale-and-round.
        // to_date(text, fmt) — format-template date parsing.
        // Tokens: YYYY YY MM DD MON MONTH + literal separators.
        "to_date" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "to_date() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            let (input, fmt) = match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
                (Value::Text(i), Value::Text(f)) => (i.as_ref(), f.as_ref()),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "to_date() takes 2 TEXT args".into(),
                    });
                }
            };
            let (y, m, d, ..) = parse_by_format(input, fmt).map_err(|e| {
                EvalError::TypeMismatch {
                    detail: alloc::format!("to_date(): {e}"),
                }
            })?;
            Ok(Value::Date(super::days_from_civil(y, m, d)))
        }
        "to_timestamp" => {
            // 2-arg form: to_timestamp(text, fmt) — format parsing.
            if args.len() == 2 {
                let (input, fmt) = match (&args[0], &args[1]) {
                    (Value::Null, _) | (_, Value::Null) => {
                        return Ok(Value::Null);
                    }
                    (Value::Text(i), Value::Text(f)) => {
                        (i.as_ref(), f.as_ref())
                    }
                    _ => {
                        return Err(EvalError::TypeMismatch {
                            detail:
                                "to_timestamp(text, fmt) takes 2 TEXT args"
                                    .into(),
                        });
                    }
                };
                let (y, m, d, h, mi, s, us) = parse_by_format(input, fmt)
                    .map_err(|e| EvalError::TypeMismatch {
                        detail: alloc::format!("to_timestamp(): {e}"),
                    })?;
                let days = i64::from(super::days_from_civil(y, m, d));
                let micros = days * 86_400_000_000
                    + i64::from(h) * 3_600_000_000
                    + i64::from(mi) * 60_000_000
                    + i64::from(s) * 1_000_000
                    + i64::from(us);
                return Ok(Value::Timestamp(micros));
            }
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "to_timestamp() takes 1 arg (numeric epoch) or 2 args (text, fmt), got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Timestamp(
                    i64::from(*n).saturating_mul(1_000_000),
                )),
                Value::BigInt(n) => Ok(Value::Timestamp(
                    n.saturating_mul(1_000_000),
                )),
                Value::SmallInt(n) => Ok(Value::Timestamp(
                    i64::from(*n).saturating_mul(1_000_000),
                )),
                Value::Float(f) => {
                    let us = (*f * 1_000_000.0).round() as i64;
                    Ok(Value::Timestamp(us))
                }
                Value::Numeric { scaled, scale } => {
                    // scaled / 10^scale = seconds. Multiply by 1e6.
                    let ten_pow = 10i128.pow(*scale as u32);
                    let us_i128 = *scaled as i128 * 1_000_000 / ten_pow;
                    let us = i64::try_from(us_i128).unwrap_or(i64::MAX);
                    Ok(Value::Timestamp(us))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "to_timestamp(): needs numeric epoch, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.17.0 Phase 3.P0-29 — MySQL time aliases. WordPress,
        // Laravel, mysql-connector-python emit these constantly.
        // `unix_timestamp()` (bare) is folded by clock_replacement_for
        // into a BigInt literal — this arm only handles the 1-arg
        // form (TIMESTAMP / DATE → epoch seconds).
        "date_format" => date_format_mysql(args),
        // v7.37.17 (17.6 siblings) — inverse/companion of
        // date_format.
        "str_to_date" => super::datetime::str_to_date_mysql(args),
        "time_format" => super::datetime::time_format_mysql(args),
        "timestampadd" => super::datetime::timestampadd_mysql(args),
        "timestampdiff" => super::datetime::timestampdiff_mysql(args),
        "get_format" => super::datetime::get_format_mysql(args),
        "convert_tz" => super::datetime::convert_tz_mysql(args),
        // PG timezone(zone, ts) — the function form of AT TIME ZONE.
        "timezone" => super::datetime::timezone_pg(args),
        "unix_timestamp" => unix_timestamp_of(args),
        "from_unixtime" => from_unixtime(args),
        // v7.17.0 Phase 3.8 — PG `format(fmt, args…)` sprintf-style.
        // Conversion specifiers: `%s` (literal string from arg),
        // `%I` (quoted identifier), `%L` (quoted SQL literal),
        // `%%` (literal `%`). `%n$X` argument-position prefix
        // (1-based). NULL arg → empty string for %s; NULL for %I
        // is an error in PG; NULL for %L renders as the SQL
        // literal `NULL`. Args missing for a position → error.
        // format() serves two dialects: PG's printf-style form takes
        // a text format string first; MySQL's FORMAT(X, D) takes a
        // number first (round to D decimals + comma thousands
        // separators). Disambiguate on the first arg's type — PG's
        // form can never start with a number.
        "format"
            if args.len() == 2
                && matches!(
                    args[0],
                    Value::Int(_)
                        | Value::SmallInt(_)
                        | Value::BigInt(_)
                        | Value::Float(_)
                        | Value::Numeric { .. }
                ) =>
        {
            let x = match &args[0] {
                Value::Int(n) => f64::from(*n),
                Value::SmallInt(n) => f64::from(*n),
                Value::BigInt(n) => *n as f64,
                Value::Float(f) => *f,
                Value::Numeric { scaled, scale } => {
                    (*scaled as f64) / libm::pow(10.0, f64::from(*scale))
                }
                _ => unreachable!(),
            };
            let d = match &args[1] {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => (*n).clamp(0, 30) as usize,
                Value::SmallInt(n) => i32::from(*n).clamp(0, 30) as usize,
                Value::BigInt(n) => (*n).clamp(0, 30) as usize,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "format() decimals must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let rendered = alloc::format!("{x:.d$}");
            let (int_part, frac_part) = match rendered.split_once('.') {
                Some((i, f)) => (i, Some(f)),
                None => (rendered.as_str(), None),
            };
            let (sign, digits) = match int_part.strip_prefix('-') {
                Some(rest) => ("-", rest),
                None => ("", int_part),
            };
            let mut grouped = alloc::string::String::new();
            for (i, c) in digits.chars().enumerate() {
                if i > 0 && (digits.len() - i).is_multiple_of(3) {
                    grouped.push(',');
                }
                grouped.push(c);
            }
            let out = match frac_part {
                Some(f) => alloc::format!("{sign}{grouped}.{f}"),
                None => alloc::format!("{sign}{grouped}"),
            };
            Ok(Value::text(out))
        }
        "format" => format_string(args),
        // v7.37.17 (17.6 siblings) — MySQL NAME_CONST(name, value)
        // returns the value; the name only labels the output column
        // in MySQL (mysqlbinlog emits these).
        "name_const" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("name_const() takes 2 args, got {}", args.len()),
                });
            }
            Ok(args[1].clone().into_owned())
        }
        // PG `concat(args...)` — variadic; coerces every arg to
        // its text representation; NULL arguments are silently
        // skipped (the canonical PG semantic — `concat()` is the
        // NULL-tolerant counterpart to the `||` operator which
        // propagates NULL).
        //
        // Reference:
        //   https://www.postgresql.org/docs/current/functions-string.html
        //   "Concatenates the text representations of all the
        //   arguments. NULL arguments are ignored."
        //
        // Edge cases:
        //   * `concat()` (no args) → ''
        //   * Every arg NULL → '' (NEVER returns NULL — distinct
        //     from `||` and from `array_agg`)
        //   * Bool → PG single-char form 't' / 'f'
        //   * SmallInt / Int / BigInt / Float / Numeric / Date /
        //     Timestamp / Json / Bytes → their canonical text
        //     rendering (shared with `format()`'s %s specifier
        //     via `value_to_format_text`).
        "concat" => {
            let mut out = String::new();
            for v in args {
                if matches!(v, Value::Null) {
                    continue;
                }
                out.push_str(&value_to_format_text(v));
            }
            Ok(Value::text(out))
        }
        // Bare ROW(a, b, …) constructor rendered as PG record text:
        // fields comma-joined inside parens, NULL as empty, fields
        // containing special characters double-quoted with `"`→`""`
        // and `\`→`\\` escapes. (Comparison forms never reach here —
        // the parser expands them fieldwise at parse time.)
        "row" => {
            let mut out = String::from("(");
            for (i, v) in args.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if matches!(v, Value::Null) {
                    continue;
                }
                let field = value_to_format_text(v);
                let needs_quote = field.is_empty()
                    || field.chars().any(|c| {
                        matches!(c, ',' | '(' | ')' | '"' | '\\') || c.is_whitespace()
                    });
                if needs_quote {
                    out.push('"');
                    for c in field.chars() {
                        match c {
                            '"' => out.push_str("\"\""),
                            '\\' => out.push_str("\\\\"),
                            other => out.push(other),
                        }
                    }
                    out.push('"');
                } else {
                    out.push_str(&field);
                }
            }
            out.push(')');
            Ok(Value::text(out))
        }
        // PG `concat_ws(sep, val1 [, val2 ...])` — like concat but
        // with a separator inserted between each pair of NON-NULL
        // arguments. Critical semantic subtleties:
        //   * NULL separator → NULL result (the sep position is
        //     mandatory and poison-prone; this is the ONLY way
        //     concat_ws can return NULL).
        //   * NULL data args silently SKIPPED — the separator is
        //     NOT inserted around them. `concat_ws(',', 'a', NULL,
        //     'b')` → `'a,b'`, not `'a,,b'`.
        //   * Empty-string data args are KEPT (separator placed
        //     around them). `concat_ws(',', 'a', '', 'b')` →
        //     `'a,,b'`. Distinction with NULL matters for code
        //     like `concat_ws(', ', first_name, middle_name,
        //     last_name)`.
        //   * 0 args → arity error (sep is mandatory).
        //   * Only sep (no data) → '' (NOT NULL — distinct from
        //     the all-NULL data case which also returns '').
        //
        // Reference:
        //   https://www.postgresql.org/docs/current/functions-string.html
        // PG `trim` / `ltrim` / `rtrim` / `btrim`.
        //
        // Semantic anchors (PG-canonical):
        //   * Default chars set is the ASCII SPACE only (NOT the
        //     POSIX whitespace class — tab / newline / form-feed
        //     stay put unless explicitly listed in `chars`).
        //   * `chars` arg is a UTF-8 codepoint SET — any char in
        //     the set is stripped, not the substring.
        //   * `trim(s)` == `btrim(s)` == strip both ends.
        //   * `ltrim(s, c)` / `rtrim(s, c)` strip only the named
        //     side; inner occurrences are preserved.
        //   * NULL on EITHER arg → NULL result.
        //   * Non-text input is coerced via `value_to_format_text`
        //     so trim(42) returns '42'.
        //
        // Reference:
        //   https://www.postgresql.org/docs/current/functions-string.html
        // PG `replace(string, from, to)` — substring substitution
        // for every (non-overlapping, greedy left-to-right)
        // occurrence. Empty `from` passes input through unchanged
        // (PG behavior — avoids infinite loop). Inserted text is
        // NOT re-scanned for new matches (so `replace('a', 'a',
        // 'aa')` terminates at `'aa'`, not blows up). NULL on any
        // arg poisons.
        // PG `split_part(string, delimiter, n)` — split on delim,
        // return the n-th field (1-indexed). Negative n counts
        // from the end (PG 14+). Out-of-range n → '' (NOT NULL).
        // n = 0 → error. Empty delimiter → error. NULL on any
        // arg → NULL.
        // PG `repeat(string, n)` — duplicate the input N times.
        // n=0 → ''; n<0 → '' (PG does NOT error on negative);
        // NULL on any arg → NULL.
        // PG `lpad(string, length [, fill])` / `rpad(...)`.
        // length is the target CODEPOINT count. Truncation when
        // input longer (lpad keeps the LEFT side, rpad keeps
        // LEFT too — both wait truncate from the right side per
        // PG-verified behavior). Padding when shorter, using
        // `fill` (default SPACE) cycling for multi-char fills.
        // length<=0 → ''. Empty fill + needs padding → returns
        // input verbatim (potentially truncated). NULL on any
        // arg → NULL.
        // PG `strpos(string, substring)` — same as position()
        // but with reversed arg order. PG convention is
        // strpos(haystack, needle); position(needle, haystack).
        // Both are 1-indexed; 0 = not found; codepoint-counted.
        // PG `left(string, n)` / `right(string, n)` — head/tail
        // substring helpers. Negative n means "all but last/first
        // |n| chars" — slice from the OPPOSITE side. n=0 → ''.
        // Codepoint-counted. NULL on any arg → NULL.
        // PG `floor(x)` — largest integer <= x.
        //   * Negative floats floor TOWARD -infinity, NOT toward 0.
        //   * Integer types passthrough unchanged.
        //   * NULL → NULL.
        // PG `ceil(x)` / `ceiling(x)` — smallest integer >= x.
        //   * Negative floats round TOWARD zero (toward +inf):
        //     ceil(-1.5) → -1, NOT -2.
        //   * Integer types passthrough unchanged.
        //   * NULL → NULL.
        // PG `round(x)` / `round(x, scale)` — half-away-from-zero
        // rounding (NUMERIC semantic).
        //   * round(0.5) → 1; round(-0.5) → -1; round(2.5) → 3.
        //   * Two-arg form rounds to N decimal places (n>0) or to
        //     nearest 10^|n| (n<0).
        //   * Integer types passthrough unchanged.
        //   * NULL on any arg → NULL.
        // PG `trunc(x)` / `trunc(x, scale)` — truncate TOWARD zero.
        //   * Distinct from floor() which rounds toward -inf:
        //     trunc(-1.7)→-1; floor(-1.7)→-2.
        //   * Distinct from round() which rounds half-away:
        //     trunc(1.5)→1; round(1.5)→2.
        //   * Two-arg form truncates to N decimal places (or 10^|n|
        //     for negative n).
        //   * Integer types passthrough unchanged.
        //   * NULL on any arg → NULL.
        // PG `nullif(a, b)` — returns NULL if a = b, else a.
        // Canonical use cases:
        //   * Divide-by-zero protection: `x / nullif(y, 0)`
        //   * Empty-string normalisation: `nullif(field, '')`
        // Edge: nullif(NULL, NULL) returns NULL. nullif(NULL, x)
        // returns NULL. nullif(x, NULL) returns x (since NULL is
        // not == to anything per IS DISTINCT FROM semantic, x ≠ NULL).
        // PG `greatest(...)` / `least(...)` — variadic max/min.
        // NULL args silently skipped (PG-canonical). All-NULL → NULL.
        // Cross-type widening for numeric comparisons.
        // PG `mod(y, x)` — modulo. Result sign follows dividend.
        //   * mod(7, 3) = 1
        //   * mod(-7, 3) = -1
        //   * mod(7, -3) = 1
        //   * mod(-7, -3) = -1
        // Division by zero → error. NULL on any arg → NULL.
        // PG `power(x, y)` / `pow(x, y)` — x^y.
        // Integer exponent → exact via repeated multiplication
        // (no precision loss). Fractional exponent → exp(y*ln(x))
        // via the no_std exp/ln series helpers.
        // x=0 with negative y → error (1/0). NULL → NULL.
        // PG `sqrt(x)` — square root. Negative input → error.
        // PG `sign(x)` — -1 / 0 / 1.
        // PG `random()` — uniform float in [0, 1). Per-row /
        // per-call: each evaluation returns a different value
        // even within the same statement. Backed by a xorshift64*
        // PRNG with a process-static seed; not cryptographically
        // secure (use a cryptographic source for security tokens).
        "random" => {
            if !args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("random() takes 0 args, got {}", args.len()),
                });
            }
            Ok(Value::Float(prng_next_f64()))
        }
        // v7.38 (read01 U15) — internal per-row draw for `TABLESAMPLE …
        // REPEATABLE(seed)`. Uses the scan-local sampler cell in the eval
        // context (seeded lazily from the literal seed on the first draw),
        // so the sequence is deterministic and isolated from the global
        // `random()` PRNG. Never surfaced to SQL directly — the parser
        // synthesises `__tsm_fract(seed) < prob/100` for a REPEATABLE
        // sample.
        "__tsm_fract" => {
            let Some(cell) = ctx.sample_rng else {
                // Reached only when a REPEATABLE sample sits on a FROM item
                // that isn't a direct single-table scan (e.g. a JOIN or
                // derived-table primary), whose filter runs without a
                // sampler cell. Honest, clear message rather than a silent
                // wrong sample.
                return Err(EvalError::TypeMismatch {
                    detail: "TABLESAMPLE REPEATABLE is supported on a direct table scan; \
                             a joined / derived-table sample is not deterministic yet"
                        .into(),
                });
            };
            #[allow(clippy::cast_precision_loss)]
            let mut state = match cell.get() {
                Some(s) => s,
                None => {
                    // Seed lazily from the REPEATABLE literal; splitmix64 the
                    // float bits to a well-diffused nonzero xorshift state.
                    let seed_f = match args.first() {
                        Some(Value::Float(x)) => *x,
                        Some(Value::Int(n)) => f64::from(*n),
                        Some(Value::BigInt(n)) => *n as f64,
                        Some(Value::SmallInt(n)) => f64::from(*n),
                        _ => 0.0,
                    };
                    let mut z = seed_f.to_bits().wrapping_add(0x9E37_79B9_7F4A_7C15);
                    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                    z ^= z >> 31;
                    if z == 0 { 0x2545_F491_4F6C_DD1D } else { z }
                }
            };
            // xorshift64* step.
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            cell.set(Some(state));
            #[allow(clippy::cast_precision_loss)]
            let fract = ((state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64)
                / ((1u64 << 53) as f64);
            Ok(Value::Float(fract))
        }
        // MySQL rand([seed]) — same [0,1) uniform; the optional
        // seed reseeds the PRNG first (repeatable series).
        "rand" => {
            match args.first() {
                None => Ok(Value::Float(prng_next_f64())),
                Some(Value::Null) => Ok(Value::Float(prng_next_f64())),
                Some(Value::Int(n)) => {
                    super::math::prng_seed(f64::from(*n));
                    Ok(Value::Float(prng_next_f64()))
                }
                Some(Value::BigInt(n)) => {
                    super::math::prng_seed(*n as f64);
                    Ok(Value::Float(prng_next_f64()))
                }
                Some(Value::SmallInt(n)) => {
                    super::math::prng_seed(f64::from(*n));
                    Ok(Value::Float(prng_next_f64()))
                }
                Some(other) => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "rand() seed must be integer, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // MySQL session/utility probes. connection_id — SPG
        // embedded has exactly one logical connection; sleep —
        // no-op returning 0 like pg_sleep's shape; benchmark —
        // returns 0 without looping (the timing side-channel is
        // meaningless in-process); found_rows / row_count /
        // last_insert_id — session counters queue with the MySQL
        // wire epic, 0/-1 keep clients moving.
        "connection_id" => Ok(Value::BigInt(1)),
        "sleep" | "benchmark" => Ok(Value::Int(0)),
        "found_rows" | "last_insert_id" => Ok(Value::BigInt(0)),
        "row_count" => Ok(Value::BigInt(-1)),
        // MySQL uuid_short() — 64-bit sequential-ish id off the
        // PRNG (uniqueness within a session, like MySQL's within-
        // server promise).
        "uuid_short" => Ok(Value::BigInt(
            (super::math::prng_next_u64() >> 1) as i64,
        )),
        // MySQL is_uuid(text) — validates the 36-char dashed or
        // 32-char bare hex form.
        // v7.37.17 (17.6 siblings) — MySQL INSERT(str, pos, len,
        // newstr): replaces the len-char window starting at 1-based
        // pos with newstr. pos out of range → original string; len
        // past the end → replace through the end. Char-based, so
        // multi-byte text is safe.
        "insert" => {
            if args.len() != 4 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("insert() takes 4 args, got {}", args.len()),
                });
            }
            if args.iter().any(|a| matches!(a, Value::Null)) {
                return Ok(Value::Null);
            }
            let (Value::Text(s), Value::Text(new)) = (&args[0], &args[3]) else {
                return Err(EvalError::TypeMismatch {
                    detail: "insert() str and newstr must be text".into(),
                });
            };
            let as_i64 = |v: &Value| -> Option<i64> {
                match v {
                    Value::Int(n) => Some(i64::from(*n)),
                    Value::SmallInt(n) => Some(i64::from(*n)),
                    Value::BigInt(n) => Some(*n),
                    _ => None,
                }
            };
            let (Some(pos), Some(len)) = (as_i64(&args[1]), as_i64(&args[2])) else {
                return Err(EvalError::TypeMismatch {
                    detail: "insert() pos and len must be integers".into(),
                });
            };
            let chars: alloc::vec::Vec<char> = s.chars().collect();
            let n = chars.len() as i64;
            if pos < 1 || pos > n {
                return Ok(Value::text(s.to_string()));
            }
            let start = (pos - 1) as usize;
            let end = if len < 0 {
                chars.len()
            } else {
                ((pos - 1).saturating_add(len).min(n)) as usize
            };
            let mut out = alloc::string::String::new();
            out.extend(&chars[..start]);
            out.push_str(new);
            out.extend(&chars[end..]);
            Ok(Value::text(out))
        }
        "is_uuid" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("is_uuid() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Uuid(_) => Ok(Value::Bool(true)),
                Value::Text(s) => {
                    let t = s.trim();
                    let hex_only: alloc::string::String =
                        t.chars().filter(|c| *c != '-').collect();
                    let valid = hex_only.len() == 32
                        && hex_only.chars().all(|c| c.is_ascii_hexdigit())
                        && (t.len() == 32
                            || (t.len() == 36
                                && t.as_bytes()[8] == b'-'
                                && t.as_bytes()[13] == b'-'
                                && t.as_bytes()[18] == b'-'
                                && t.as_bytes()[23] == b'-'));
                    Ok(Value::Bool(valid))
                }
                _ => Ok(Value::Bool(false)),
            }
        }
        // v7.37.17 (17.6 siblings) — trig family via libm. Real
        // sin/cos/tan/asin/acos/atan/atan2/sinh/cosh/tanh + arch
        // hyperbolic variants. All accept f64; NULL passthrough.
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan"
        | "sinh" | "cosh" | "tanh" | "asinh" | "acosh" | "atanh" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            let x: f64 = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Float(f) => *f,
                Value::Int(n) => f64::from(*n),
                Value::SmallInt(n) => f64::from(*n),
                Value::BigInt(n) => *n as f64,
                // PG accepts numeric via the implicit numeric→float8 cast
                // (all these take double precision).
                Value::Numeric { scaled, scale } => {
                    (*scaled as f64) / f64_powi(10.0, i32::from(*scale))
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() needs numeric, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let r = match name {
                "sin" => libm::sin(x),
                "cos" => libm::cos(x),
                "tan" => libm::tan(x),
                "asin" => libm::asin(x),
                "acos" => libm::acos(x),
                "atan" => libm::atan(x),
                "sinh" => libm::sinh(x),
                "cosh" => libm::cosh(x),
                "tanh" => libm::tanh(x),
                "asinh" => libm::asinh(x),
                "acosh" => libm::acosh(x),
                "atanh" => libm::atanh(x),
                _ => unreachable!(),
            };
            Ok(Value::Float(r))
        }
        // cot(x) — cotangent. PG builtin missing from the trig
        // batch above.
        "cot" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("cot() takes 1 arg, got {}", args.len()),
                });
            }
            let x = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Float(f) => *f,
                Value::Int(n) => f64::from(*n),
                Value::SmallInt(n) => f64::from(*n),
                Value::BigInt(n) => *n as f64,
                Value::Numeric { scaled, scale } => {
                    (*scaled as f64) / f64_powi(10.0, i32::from(*scale))
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "cot() needs numeric, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            Ok(Value::Float(1.0 / libm::tan(x)))
        }
        // log2(x) — base-2 logarithm (not PG builtin but MySQL-compat
        // + common in analytics SQL).
        "log2" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("log2() takes 1 arg, got {}", args.len()),
                });
            }
            let x = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Float(f) => *f,
                Value::Int(n) => f64::from(*n),
                Value::SmallInt(n) => f64::from(*n),
                Value::BigInt(n) => *n as f64,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "log2() needs numeric, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if x <= 0.0 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "log2(): argument must be positive, got {x}"
                    ),
                });
            }
            Ok(Value::Float(libm::log2(x)))
        }
        // atan2(y, x) takes 2 args.
        "atan2" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("atan2() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            fn as_f64(v: &Value<'_>) -> Result<f64, EvalError> {
                match v {
                    Value::Float(f) => Ok(*f),
                    Value::Int(n) => Ok(f64::from(*n)),
                    Value::SmallInt(n) => Ok(f64::from(*n)),
                    Value::BigInt(n) => Ok(*n as f64),
                    Value::Numeric { scaled, scale } => {
                        Ok((*scaled as f64) / f64_powi(10.0, i32::from(*scale)))
                    }
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "atan2(): needs numeric, got {:?}",
                            other.data_type()
                        ),
                    }),
                }
            }
            let y = as_f64(&args[0])?;
            let x = as_f64(&args[1])?;
            Ok(Value::Float(libm::atan2(y, x)))
        }
        // sind/cosd/tand/asind/acosd/atand — PG's degree-input trig
        // variants (multiply/divide by π/180).
        "sind" | "cosd" | "tand" | "cotd" | "asind" | "acosd" | "atand" | "atan2d" => {
            const D2R: f64 = core::f64::consts::PI / 180.0;
            const R2D: f64 = 180.0 / core::f64::consts::PI;
            if name == "atan2d" {
                if args.len() != 2 {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("atan2d() takes 2 args, got {}", args.len()),
                    });
                }
                if args.iter().any(|v| matches!(v, Value::Null)) {
                    return Ok(Value::Null);
                }
                fn as_f64(v: &Value<'_>) -> Result<f64, EvalError> {
                    match v {
                        Value::Float(f) => Ok(*f),
                        Value::Int(n) => Ok(f64::from(*n)),
                        Value::SmallInt(n) => Ok(f64::from(*n)),
                        Value::BigInt(n) => Ok(*n as f64),
                        Value::Numeric { scaled, scale } => {
                            Ok((*scaled as f64) / f64_powi(10.0, i32::from(*scale)))
                        }
                        other => Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "atan2d(): needs numeric, got {:?}",
                                other.data_type()
                            ),
                        }),
                    }
                }
                let y = as_f64(&args[0])?;
                let x = as_f64(&args[1])?;
                return Ok(Value::Float(libm::atan2(y, x) * R2D));
            }
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            let x: f64 = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Float(f) => *f,
                Value::Int(n) => f64::from(*n),
                Value::SmallInt(n) => f64::from(*n),
                Value::BigInt(n) => *n as f64,
                Value::Numeric { scaled, scale } => {
                    (*scaled as f64) / f64_powi(10.0, i32::from(*scale))
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() needs numeric, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let r = match name {
                "sind" => libm::sin(x * D2R),
                "cosd" => libm::cos(x * D2R),
                "tand" => libm::tan(x * D2R),
                "cotd" => 1.0 / libm::tan(x * D2R),
                "asind" => libm::asin(x) * R2D,
                "acosd" => libm::acos(x) * R2D,
                "atand" => libm::atan(x) * R2D,
                _ => unreachable!(),
            };
            Ok(Value::Float(r))
        }
        // v7.37.17 (17.6 siblings) — PG 14+ pg_wait_for_backend_termination
        // waits (up to timeout ms) for a specific backend to
        // terminate. SPG has no separate backends yet — the
        // function returns immediately with true. pgpool-II and
        // patroni emit this during failover coordination.
        "pg_wait_for_backend_termination" => Ok(Value::Bool(true)),
        // pg_isolation_test_session_is_blocked / pg_safe_snapshot_blocking_pids
        // — regress-test helpers used by PG's isolation tests. SPG
        // has no session-blocking model yet; return NULL / false.
        "pg_isolation_test_session_is_blocked" => Ok(Value::Bool(false)),
        "pg_safe_snapshot_blocking_pids" => Ok(Value::Null),
        // pg_stat_get_backend_activity_started_at etc — variants of
        // the activity_start probe (alt spellings by tooling).
        "pg_stat_get_backend_activity_started_at" => Ok(Value::Null),
        // pg_terminate_backend can take (int, int) too (PG 14+
        // with timeout arg). Original arm handles the 1-arg form.
        "pg_terminate_backend_with_timeout" => Ok(Value::Bool(true)),
        // v7.37.17 (17.6 siblings) — PG 17+ overload `random(min, max)`
        // returns a random value in [min, max]. Both int and numeric
        // widths are common. Also supports random_int(min, max) alias.
        "random_int" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("random_int() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let (lo, hi): (i64, i64) = match (&args[0], &args[1]) {
                (Value::Int(a), Value::Int(b)) => (i64::from(*a), i64::from(*b)),
                (Value::BigInt(a), Value::BigInt(b)) => (*a, *b),
                (Value::SmallInt(a), Value::SmallInt(b)) => {
                    (i64::from(*a), i64::from(*b))
                }
                (a, b)
                    if matches!(
                        a,
                        Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_)
                    ) && matches!(
                        b,
                        Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_)
                    ) =>
                {
                    // Mixed-width — widen both to i64.
                    let l = match a {
                        Value::Int(n) => i64::from(*n),
                        Value::BigInt(n) => *n,
                        Value::SmallInt(n) => i64::from(*n),
                        _ => unreachable!(),
                    };
                    let h = match b {
                        Value::Int(n) => i64::from(*n),
                        Value::BigInt(n) => *n,
                        Value::SmallInt(n) => i64::from(*n),
                        _ => unreachable!(),
                    };
                    (l, h)
                }
                (a, b) => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "random_int() needs integer args, got ({:?}, {:?})",
                            a.data_type(),
                            b.data_type()
                        ),
                    });
                }
            };
            if lo > hi {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "random_int(): min ({lo}) > max ({hi})"
                    ),
                });
            }
            let range = (hi - lo + 1) as u64;
            let r = super::math::prng_next_u64() % range;
            Ok(Value::BigInt(lo + r as i64))
        }
        // v7.37.17 (17.6 siblings) — setseed(f) reseeds the PRNG.
        // PG accepts f ∈ [-1, 1]; SPG allows the full f64 range
        // for simplicity. Returns void (NULL). Deterministic
        // repro tests rely on this.
        "setseed" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("setseed() takes 1 arg, got {}", args.len()),
                });
            }
            let seed = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Float(f) => *f,
                Value::Int(n) => f64::from(*n),
                Value::SmallInt(n) => f64::from(*n),
                Value::BigInt(n) => *n as f64,
                Value::Numeric { scaled, scale } => {
                    (*scaled as f64) / f64_powi(10.0, i32::from(*scale))
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "setseed() needs numeric, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if seed < -1.0 || seed > 1.0 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "setseed(): seed {seed} out of range [-1, 1]"
                    ),
                });
            }
            super::math::prng_seed(seed);
            Ok(Value::Null)
        }
        // v7.17.0 — PG `gen_random_uuid()` (built-in, no extension)
        // and the historical uuid-ossp `uuid_generate_v4()` alias.
        // Both produce a RFC 4122 v4 (random) UUID. This is the
        // function Django / Rails / Hibernate emit in `id UUID
        // PRIMARY KEY DEFAULT gen_random_uuid()`, the modern
        // default PK pattern.
        // v7.37.17 (17.6 siblings) — PG 16+ random_normal(mean,
        // stddev) uses Box-Muller to produce a normally-
        // distributed random number. Real implementation using
        // the internal prng.
        "random_normal" => {
            if args.len() > 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "random_normal() takes 0-2 args, got {}",
                        args.len()
                    ),
                });
            }
            let mean = if args.is_empty() {
                0.0
            } else {
                match &args[0] {
                    Value::Null => return Ok(Value::Null),
                    Value::Float(f) => *f,
                    Value::Int(n) => f64::from(*n),
                    Value::SmallInt(n) => f64::from(*n),
                    Value::BigInt(n) => *n as f64,
                    Value::Numeric { scaled, scale } => {
                        (*scaled as f64) / f64_powi(10.0, i32::from(*scale))
                    }
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "random_normal(): mean must be numeric, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                }
            };
            let stddev = if args.len() < 2 {
                1.0
            } else {
                match &args[1] {
                    Value::Null => return Ok(Value::Null),
                    Value::Float(f) => *f,
                    Value::Int(n) => f64::from(*n),
                    Value::SmallInt(n) => f64::from(*n),
                    Value::BigInt(n) => *n as f64,
                    Value::Numeric { scaled, scale } => {
                        (*scaled as f64) / f64_powi(10.0, i32::from(*scale))
                    }
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "random_normal(): stddev must be numeric, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                }
            };
            // Marsaglia polar method (avoids no_std cos/sin):
            // Pick u, v uniform in [-1, 1] until s = u²+v² ∈ (0, 1);
            // then z = u * sqrt(-2 ln s / s).
            let mut z = 0.0f64;
            for _ in 0..16 {
                let u = 2.0 * super::math::prng_next_f64() - 1.0;
                let v = 2.0 * super::math::prng_next_f64() - 1.0;
                let s = u * u + v * v;
                if s > 0.0 && s < 1.0 {
                    let factor = super::math::f64_sqrt(
                        -2.0 * super::math::f64_ln(s) / s,
                    );
                    z = u * factor;
                    break;
                }
            }
            Ok(Value::Float(mean + stddev * z))
        }
        "gen_random_uuid" | "uuid_generate_v4" => {
            if !args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("{name}() takes 0 args, got {}", args.len()),
                });
            }
            Ok(Value::Uuid(gen_random_uuid_bytes()))
        }
        // v7.37.17 (17.6 siblings) — uuid-ossp namespace constants
        // + uuid_generate_v5 (SHA-1 name-based) + v3 (MD5).
        "uuid_nil" => {
            if !args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: format!("uuid_nil() takes 0 args, got {}", args.len()),
                });
            }
            Ok(Value::Uuid([0u8; 16]))
        }
        "uuid_ns_dns" => Ok(Value::Uuid([
            0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1,
            0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
        ])),
        "uuid_ns_url" => Ok(Value::Uuid([
            0x6b, 0xa7, 0xb8, 0x11, 0x9d, 0xad, 0x11, 0xd1,
            0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
        ])),
        "uuid_ns_oid" => Ok(Value::Uuid([
            0x6b, 0xa7, 0xb8, 0x12, 0x9d, 0xad, 0x11, 0xd1,
            0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
        ])),
        "uuid_ns_x500" => Ok(Value::Uuid([
            0x6b, 0xa7, 0xb8, 0x14, 0x9d, 0xad, 0x11, 0xd1,
            0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
        ])),
        // uuid_generate_v5(namespace, name) — SHA-1 name-based UUID.
        // Deterministic: same (ns, name) always yields the same UUID.
        "uuid_generate_v5" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "uuid_generate_v5() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            let ns = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Uuid(b) => *b,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "uuid_generate_v5(): namespace must be uuid, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let name = match &args[1] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "uuid_generate_v5(): name must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            use sha1::{Digest, Sha1};
            let mut h = Sha1::new();
            h.update(ns);
            h.update(name.as_bytes());
            let digest = h.finalize();
            let mut b = [0u8; 16];
            b.copy_from_slice(&digest[..16]);
            b[6] = (b[6] & 0x0F) | 0x50; // version 5
            b[8] = (b[8] & 0x3F) | 0x80; // variant 10xx
            Ok(Value::Uuid(b))
        }
        // uuid_generate_v3(namespace, name) — MD5 name-based UUID.
        "uuid_generate_v3" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "uuid_generate_v3() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            let ns = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Uuid(b) => *b,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "uuid_generate_v3(): namespace must be uuid, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let name = match &args[1] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "uuid_generate_v3(): name must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            use md5::{Digest, Md5};
            let mut h = Md5::new();
            h.update(ns);
            h.update(name.as_bytes());
            let digest = h.finalize();
            let mut b = [0u8; 16];
            b.copy_from_slice(&digest);
            b[6] = (b[6] & 0x0F) | 0x30; // version 3
            b[8] = (b[8] & 0x3F) | 0x80; // variant 10xx
            Ok(Value::Uuid(b))
        }
        // v7.37.17 (17.6 siblings) — PG 18 uuidv7() — time-ordered
        // UUID v7 (RFC 9562). 48-bit millis-since-epoch prefix + a
        // 12-bit intra-millisecond monotonic counter + random tail;
        // sorts by generation time, making it the preferred PK for
        // write-heavy indexes.
        //
        // v7.38 (read01 P6.08) — the prefix is now the host wall clock
        // (µs → ms) when one is attached, giving a real time-ordered
        // value; with no host clock it falls back to the deterministic
        // 2020-01-01 anchor. Either way the process-wide monotonic guard
        // (`uuidv7_monotonic`) keeps successive UUIDs strictly ordered
        // within a millisecond and across a backward clock step.
        "uuidv7" | "uuid_generate_v7" | "gen_uuid_v7" => {
            if !args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("{name}() takes 0 args, got {}", args.len()),
                });
            }
            const ANCHOR_MS: u64 = 1_577_836_800_000;
            let rand_b = super::math::prng_next_u64();
            let base_ms = match ctx.clock {
                Some(f) => (f().max(0) as u64) / 1000,
                None => ANCHOR_MS,
            };
            let (ts, counter) = super::math::uuidv7_monotonic(base_ms);
            let mut b = [0u8; 16];
            // 48-bit big-endian timestamp.
            b[0] = (ts >> 40) as u8;
            b[1] = (ts >> 32) as u8;
            b[2] = (ts >> 24) as u8;
            b[3] = (ts >> 16) as u8;
            b[4] = (ts >> 8) as u8;
            b[5] = ts as u8;
            // ver (7) in high nibble of byte 6 + the 12-bit monotonic
            // counter in the `rand_a` field (RFC 9562 method 2).
            b[6] = 0x70 | ((counter >> 8) & 0x0F) as u8;
            b[7] = counter as u8;
            // variant (10xx) in byte 8 + rand_b tail.
            b[8] = 0x80 | ((rand_b >> 56) & 0x3F) as u8;
            b[9] = (rand_b >> 48) as u8;
            b[10] = (rand_b >> 40) as u8;
            b[11] = (rand_b >> 32) as u8;
            b[12] = (rand_b >> 24) as u8;
            b[13] = (rand_b >> 16) as u8;
            b[14] = (rand_b >> 8) as u8;
            b[15] = rand_b as u8;
            Ok(Value::Uuid(b))
        }
        // PG 18 uuid_extract_version(uuid) — the version nibble.
        "uuid_extract_version" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "uuid_extract_version() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Uuid(b) => Ok(Value::Int(i32::from(b[6] >> 4))),
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "uuid_extract_version() needs uuid, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // PG 18 uuid_extract_timestamp(uuid) — the 48-bit millis
        // prefix of a v7 UUID as a timestamp. NULL for non-v7.
        "uuid_extract_timestamp" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "uuid_extract_timestamp() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Uuid(b) => {
                    let version = b[6] >> 4;
                    if version != 7 {
                        return Ok(Value::Null);
                    }
                    let ms: u64 = (u64::from(b[0]) << 40)
                        | (u64::from(b[1]) << 32)
                        | (u64::from(b[2]) << 24)
                        | (u64::from(b[3]) << 16)
                        | (u64::from(b[4]) << 8)
                        | u64::from(b[5]);
                    Ok(Value::Timestamp((ms as i64).saturating_mul(1000)))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "uuid_extract_timestamp() needs uuid, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "sign" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("sign() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::SmallInt(n) => Ok(Value::SmallInt(n.signum())),
                Value::Int(n) => Ok(Value::Int(n.signum())),
                Value::BigInt(n) => Ok(Value::BigInt(n.signum())),
                Value::Float(x) => {
                    let s = if *x > 0.0 {
                        1.0
                    } else if *x < 0.0 {
                        -1.0
                    } else {
                        0.0
                    };
                    Ok(Value::Float(s))
                }
                Value::Numeric { scaled, scale } => {
                    let s = scaled.signum();
                    Ok(Value::Numeric {
                        scaled: s * pow10_i128(*scale),
                        scale: *scale,
                    })
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("sign() needs numeric, got {:?}", other.data_type()),
                }),
            }
        }
        "sqrt" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("sqrt() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                v => {
                    let x = value_to_f64(v).ok_or_else(|| EvalError::TypeMismatch {
                        detail: alloc::format!("sqrt() needs numeric, got {:?}", v.data_type()),
                    })?;
                    if x < 0.0 {
                        return Err(EvalError::TypeMismatch {
                            detail: "sqrt(): negative input outside real domain".into(),
                        });
                    }
                    if x == 0.0 {
                        return Ok(Value::Float(0.0));
                    }
                    Ok(Value::Float(f64_sqrt(x)))
                }
            }
        }
        "power" | "pow" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("power() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let x = value_to_f64(&args[0]).ok_or_else(|| EvalError::TypeMismatch {
                detail: "power() needs numeric x".into(),
            })?;
            let y = value_to_f64(&args[1]).ok_or_else(|| EvalError::TypeMismatch {
                detail: "power() needs numeric y".into(),
            })?;
            // Integer-exponent fast path.
            let y_int = y as i32;
            if (y_int as f64) == y && y.abs() < 1024.0 {
                let result = f64_powi(x, y_int);
                return Ok(Value::Float(result));
            }
            // Fractional exponent — only defined for x >= 0 in real
            // arithmetic. Negative x raised to fractional power is
            // complex; reject cleanly.
            if x < 0.0 {
                return Err(EvalError::TypeMismatch {
                    detail: "power(): negative base with fractional exponent yields complex result"
                        .into(),
                });
            }
            if x == 0.0 && y < 0.0 {
                return Err(EvalError::TypeMismatch {
                    detail: "power(): 0 raised to negative power is undefined".into(),
                });
            }
            if x == 0.0 {
                return Ok(Value::Float(0.0));
            }
            Ok(Value::Float(f64_exp(y * f64_ln(x))))
        }
        // v7.37.17 (17.6 siblings) — div(y, x) — integer quotient
        // (truncated division). PG's div works on numeric; SPG
        // dispatches int/bigint/float.
        "div" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("div() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            match (&args[0], &args[1]) {
                (Value::Int(a), Value::Int(b)) => {
                    if *b == 0 {
                        return Err(EvalError::TypeMismatch {
                            detail: "div(): division by zero".into(),
                        });
                    }
                    Ok(Value::Int(a.wrapping_div(*b)))
                }
                (Value::BigInt(a), Value::BigInt(b)) => {
                    if *b == 0 {
                        return Err(EvalError::TypeMismatch {
                            detail: "div(): division by zero".into(),
                        });
                    }
                    Ok(Value::BigInt(a.wrapping_div(*b)))
                }
                (a, b) => {
                    // Widen mixed numeric to f64, truncate.
                    let x = value_to_f64(a).ok_or_else(|| EvalError::TypeMismatch {
                        detail: "div() needs numeric args".into(),
                    })?;
                    let y = value_to_f64(b).ok_or_else(|| EvalError::TypeMismatch {
                        detail: "div() needs numeric args".into(),
                    })?;
                    if y == 0.0 {
                        return Err(EvalError::TypeMismatch {
                            detail: "div(): division by zero".into(),
                        });
                    }
                    Ok(Value::BigInt((x / y) as i64))
                }
            }
        }
        // v7.37.17 (17.6 siblings) — PG 17+ erf(x) / erfc(x) — the
        // Gauss error function + complement, via libm.
        "erf" | "erfc" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            let x = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Float(f) => *f,
                Value::Int(n) => f64::from(*n),
                Value::SmallInt(n) => f64::from(*n),
                Value::BigInt(n) => *n as f64,
                Value::Numeric { scaled, scale } => {
                    (*scaled as f64) / f64_powi(10.0, i32::from(*scale))
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() needs numeric, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let r = if name == "erf" { libm::erf(x) } else { libm::erfc(x) };
            Ok(Value::Float(r))
        }
        "mod" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("mod() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            // NUMERIC operands: PG returns an exact numeric remainder
            // (`mod(7.5, 2.0) = 1.5`). Align the two scales, then take the
            // truncated i128 remainder (sign of the dividend, like `%`).
            if matches!(args[0], Value::Numeric { .. }) || matches!(args[1], Value::Numeric { .. })
            {
                let as_num = |v: &Value| -> Option<(i128, u8)> {
                    match v {
                        Value::SmallInt(n) => Some((i128::from(*n), 0)),
                        Value::Int(n) => Some((i128::from(*n), 0)),
                        Value::BigInt(n) => Some((i128::from(*n), 0)),
                        Value::Numeric { scaled, scale } => Some((*scaled, *scale)),
                        _ => None,
                    }
                };
                if let (Some((ys, ysc)), Some((xs, xsc))) = (as_num(&args[0]), as_num(&args[1])) {
                    let common = ysc.max(xsc);
                    let ya = ys.checked_mul(10i128.pow(u32::from(common - ysc)));
                    let xa = xs.checked_mul(10i128.pow(u32::from(common - xsc)));
                    if let (Some(ya), Some(xa)) = (ya, xa) {
                        if xa == 0 {
                            return Err(EvalError::TypeMismatch {
                                detail: "mod(): division by zero".into(),
                            });
                        }
                        return Ok(Value::Numeric {
                            scaled: ya.wrapping_rem(xa),
                            scale: common,
                        });
                    }
                }
            }
            // FLOAT operands: C-style `fmod` (sign of the dividend).
            if matches!(args[0], Value::Float(_)) || matches!(args[1], Value::Float(_)) {
                #[allow(clippy::cast_precision_loss)]
                let as_f = |v: &Value| -> Option<f64> {
                    match v {
                        Value::SmallInt(n) => Some(f64::from(*n)),
                        Value::Int(n) => Some(f64::from(*n)),
                        Value::BigInt(n) => Some(*n as f64),
                        Value::Float(x) => Some(*x),
                        Value::Numeric { scaled, scale } => {
                            Some((*scaled as f64) / (10i128.pow(u32::from(*scale)) as f64))
                        }
                        _ => None,
                    }
                };
                if let (Some(a), Some(b)) = (as_f(&args[0]), as_f(&args[1])) {
                    if b == 0.0 {
                        return Err(EvalError::TypeMismatch {
                            detail: "mod(): division by zero".into(),
                        });
                    }
                    return Ok(Value::Float(a % b));
                }
            }
            let to_i64 = |v: &Value| -> Result<i64, EvalError> {
                match v {
                    Value::SmallInt(x) => Ok(i64::from(*x)),
                    Value::Int(x) => Ok(i64::from(*x)),
                    Value::BigInt(x) => Ok(*x),
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!("mod() needs integer, got {:?}", other.data_type()),
                    }),
                }
            };
            let y = to_i64(&args[0])?;
            let x = to_i64(&args[1])?;
            if x == 0 {
                return Err(EvalError::TypeMismatch {
                    detail: "mod(): division by zero".into(),
                });
            }
            // Rust's `%` operator on signed integers follows the
            // dividend's sign — same as PG.
            let result = y % x;
            // Pick the narrowest type that holds the result.
            if let Ok(small) = i16::try_from(result) {
                if matches!(args[0], Value::SmallInt(_)) && matches!(args[1], Value::SmallInt(_)) {
                    return Ok(Value::SmallInt(small));
                }
            }
            if let Ok(int_) = i32::try_from(result) {
                if !matches!(args[0], Value::BigInt(_)) && !matches!(args[1], Value::BigInt(_)) {
                    return Ok(Value::Int(int_));
                }
            }
            Ok(Value::BigInt(result))
        }
        // textcat / byteacat — the catalog names behind the ||
        // concatenation operator (strict: NULL in → NULL out,
        // unlike the variadic concat() which skips NULLs).
        "textcat" | "byteacat" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 2 args, got {}", args.len()),
                });
            }
            match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(a), Value::Text(b)) => {
                    Ok(Value::text(alloc::format!("{a}{b}")))
                }
                (Value::Bytes(a), Value::Bytes(b)) => {
                    let mut out =
                        alloc::vec::Vec::with_capacity(a.len() + b.len());
                    out.extend_from_slice(a);
                    out.extend_from_slice(b);
                    Ok(Value::Bytes(alloc::borrow::Cow::Owned(out)))
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "{name}() needs matching text/bytea args"
                    ),
                }),
            }
        }
        // The *_larger / *_smaller catalog names are the pairwise
        // internals PG's MAX/MIN aggregates reference in pg_proc —
        // introspecting drivers occasionally call them by name.
        // Same comparison machinery as greatest/least.
        "greatest" | "least" | "text_larger" | "text_smaller"
        | "bytea_larger" | "bytea_smaller" | "int2larger" | "int2smaller"
        | "int4larger" | "int4smaller" | "int8larger" | "int8smaller"
        | "float4larger" | "float4smaller" | "float8larger"
        | "float8smaller" | "numeric_larger" | "numeric_smaller"
        | "date_larger" | "date_smaller" | "timestamp_larger"
        | "timestamp_smaller" | "interval_larger" | "interval_smaller" => {
            if args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("{name}() takes at least 1 arg"),
                });
            }
            let non_null_refs: alloc::vec::Vec<&Value> =
                args.iter().filter(|v| !matches!(v, Value::Null)).collect();
            if non_null_refs.is_empty() {
                return Ok(Value::Null);
            }
            // PG coerces every GREATEST/LEAST arg to a common type; an
            // unknown-type string literal takes a sibling typed arg's type.
            // Without this, `greatest(time, '14:00')` compared Time vs Text,
            // fell to the value_cmp fallback, and kept the first arg.
            let target = non_null_refs.iter().find_map(|v| match v.data_type() {
                Some(spg_storage::DataType::Text) | None => None,
                dt => dt,
            });
            let non_null: alloc::vec::Vec<Value<'static>> = non_null_refs
                .iter()
                .map(|v| {
                    if let (Value::Text(s), Some(dt)) = (v, target) {
                        crate::conversions::coerce_value(Value::text(s.as_ref()), dt, "", 0)
                            .unwrap_or_else(|_| (*v).clone().into_owned())
                    } else {
                        (*v).clone().into_owned()
                    }
                })
                .collect();
            let is_greatest = name.eq_ignore_ascii_case("greatest")
                || name.ends_with("larger");
            let mut best: Value<'static> = non_null[0].clone();
            for v in &non_null[1..] {
                let ord = value_cmp_for_min_max(&best, v);
                let take = if is_greatest {
                    ord == core::cmp::Ordering::Less
                } else {
                    ord == core::cmp::Ordering::Greater
                };
                if take {
                    best = v.clone();
                }
            }
            Ok(best)
        }
        // MySQL `ifnull(a, b)` — alias for coalesce(a, b).
        // Used by every ORM with a MySQL target (Hibernate /
        // Laravel / Sequelize).
        // MySQL locate(substr, str[, pos]) — 1-based char position,
        // 0 when absent. NOTE the MySQL argument order (needle
        // first), the reverse of PG's strpos.
        "locate" => {
            if !(2..=3).contains(&args.len()) {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "locate() takes 2 or 3 args, got {}",
                        args.len()
                    ),
                });
            }
            let (needle, hay) = match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
                (Value::Text(n), Value::Text(h)) => (n.as_ref(), h.as_ref()),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "locate() takes TEXT args".into(),
                    });
                }
            };
            let start = match args.get(2) {
                None => 1i64,
                Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Int(n)) => i64::from(*n),
                Some(Value::BigInt(n)) => *n,
                Some(Value::SmallInt(n)) => i64::from(*n),
                Some(other) => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "locate() pos must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if start < 1 {
                return Ok(Value::Int(0));
            }
            // Skip (start-1) chars, search, report 1-based char pos.
            let skip = (start - 1) as usize;
            let char_offset: usize = hay.chars().take(skip).map(char::len_utf8).sum();
            if char_offset > hay.len() {
                return Ok(Value::Int(0));
            }
            match hay[char_offset..].find(needle) {
                Some(byte_pos) => {
                    let chars_before =
                        hay[..char_offset + byte_pos].chars().count();
                    Ok(Value::Int(chars_before as i32 + 1))
                }
                None => Ok(Value::Int(0)),
            }
        }
        // MySQL instr(str, substr) — same as locate with swapped args.
        "instr" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("instr() takes 2 args, got {}", args.len()),
                });
            }
            match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(hay), Value::Text(needle)) => {
                    match hay.find(needle.as_ref()) {
                        Some(byte_pos) => Ok(Value::Int(
                            hay[..byte_pos].chars().count() as i32 + 1,
                        )),
                        None => Ok(Value::Int(0)),
                    }
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: "instr() takes 2 TEXT args".into(),
                }),
            }
        }
        // MySQL substring_index(str, delim, count) — everything
        // before the count-th delimiter; negative count = from the
        // right.
        "substring_index" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "substring_index() takes 3 args, got {}",
                        args.len()
                    ),
                });
            }
            let (s, delim) = match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
                (Value::Text(s), Value::Text(d)) => (s.as_ref(), d.as_ref()),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "substring_index() takes TEXT args".into(),
                    });
                }
            };
            let count = match &args[2] {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                Value::SmallInt(n) => i64::from(*n),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "substring_index() count must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if count == 0 || delim.is_empty() {
                return Ok(Value::text::<String>("".into()));
            }
            if count > 0 {
                let mut pos = 0usize;
                let mut found = 0i64;
                while let Some(p) = s[pos..].find(delim) {
                    found += 1;
                    if found == count {
                        return Ok(Value::text(s[..pos + p].to_string()));
                    }
                    pos += p + delim.len();
                }
                Ok(Value::text(s.to_string()))
            } else {
                let want = -count;
                let positions: alloc::vec::Vec<usize> = {
                    let mut v = alloc::vec::Vec::new();
                    let mut pos = 0usize;
                    while let Some(p) = s[pos..].find(delim) {
                        v.push(pos + p);
                        pos += p + delim.len();
                    }
                    v
                };
                if (positions.len() as i64) < want {
                    Ok(Value::text(s.to_string()))
                } else {
                    let idx = positions[positions.len() - want as usize];
                    Ok(Value::text(s[idx + delim.len()..].to_string()))
                }
            }
        }
        // MySQL find_in_set(str, strlist) — 1-based index of str in
        // a comma-separated list, 0 when absent.
        "find_in_set" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "find_in_set() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(needle), Value::Text(list)) => {
                    if list.is_empty() {
                        return Ok(Value::Int(0));
                    }
                    for (i, item) in list.split(',').enumerate() {
                        if item == needle.as_ref() {
                            return Ok(Value::Int(i as i32 + 1));
                        }
                    }
                    Ok(Value::Int(0))
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: "find_in_set() takes 2 TEXT args".into(),
                }),
            }
        }
        // MySQL elt(n, a, b, ...) — the n-th string argument
        // (1-based); NULL when out of range.
        "elt" => {
            if args.len() < 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "elt() takes 2+ args, got {}",
                        args.len()
                    ),
                });
            }
            let n = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                Value::SmallInt(n) => i64::from(*n),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "elt() index must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if n < 1 || (n as usize) >= args.len() {
                return Ok(Value::Null);
            }
            Ok(args[n as usize].clone().into_owned())
        }
        // MySQL field(str, a, b, ...) — 1-based index of str among
        // the rest; 0 when absent or str is NULL.
        "field" => {
            if args.len() < 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "field() takes 2+ args, got {}",
                        args.len()
                    ),
                });
            }
            let needle = match &args[0] {
                Value::Null => return Ok(Value::Int(0)),
                Value::Text(s) => s.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "field() needs text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            for (i, candidate) in args[1..].iter().enumerate() {
                if let Value::Text(c) = candidate {
                    if c.as_ref() == needle {
                        return Ok(Value::Int(i as i32 + 1));
                    }
                }
            }
            Ok(Value::Int(0))
        }
        // MySQL space(n) — a string of n spaces.
        "space" => {
            let n = match args.first() {
                Some(Value::Null) | None => return Ok(Value::Null),
                Some(Value::Int(n)) => i64::from(*n),
                Some(Value::BigInt(n)) => *n,
                Some(Value::SmallInt(n)) => i64::from(*n),
                Some(other) => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "space() needs integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if n <= 0 {
                return Ok(Value::text::<String>("".into()));
            }
            if n > 1_000_000 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "space(): {n} exceeds the 1MB result cap"
                    ),
                });
            }
            Ok(Value::text(" ".repeat(n as usize)))
        }
        "ifnull" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("ifnull() takes 2 args, got {}", args.len()),
                });
            }
            for v in args {
                if !matches!(v, Value::Null) {
                    return Ok(v.clone().into_owned());
                }
            }
            Ok(Value::Null)
        }
        // MySQL `if(cond, then, else)` — alias for CASE WHEN.
        // NULL condition → else branch (MySQL semantic).
        // Integer condition: nonzero is true.
        "if" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "if() takes 3 args (cond, then, else), got {}",
                        args.len()
                    ),
                });
            }
            let truthy = match &args[0] {
                Value::Null => false,
                Value::Bool(b) => *b,
                Value::SmallInt(n) => *n != 0,
                Value::Int(n) => *n != 0,
                Value::BigInt(n) => *n != 0,
                Value::Float(x) => *x != 0.0,
                Value::Text(s) => !s.is_empty() && s != "0",
                _ => true,
            };
            if truthy {
                Ok(args[1].clone().into_owned())
            } else {
                Ok(args[2].clone().into_owned())
            }
        }
        "nullif" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("nullif() takes 2 args, got {}", args.len()),
                });
            }
            match (&args[0], &args[1]) {
                (Value::Null, _) => Ok(Value::Null),
                (a, Value::Null) => Ok(a.clone().into_owned()),
                (a, b) => {
                    // Use value_cmp (already defined as Ord-like
                    // function in lib.rs) — but it's not accessible
                    // here. Fall back to direct equality.
                    if values_equal_for_nullif(a, b) {
                        Ok(Value::Null)
                    } else {
                        Ok(a.clone().into_owned())
                    }
                }
            }
        }
        // "truncate" is the MySQL name for 2-arg trunc.
        "trunc" | "truncate" => {
            match args.len() {
                1 => match &args[0] {
                    Value::Null => Ok(Value::Null),
                    Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) => {
                        Ok(args[0].clone().into_owned())
                    }
                    Value::Float(x) => Ok(Value::Float(f64_trunc(*x))),
                    Value::Numeric { scaled, scale } => {
                        let factor = pow10_i128(*scale);
                        // Truncate toward zero — sign-preserving division.
                        // PG `trunc(numeric)` (1-arg) yields scale 0.
                        let q = scaled / factor;
                        Ok(Value::Numeric {
                            scaled: q,
                            scale: 0,
                        })
                    }
                    // PG `trunc(macaddr)` zeros the last 3 bytes (clears the
                    // device-specific part, keeping the manufacturer prefix).
                    Value::Macaddr(m) => {
                        let mut out = *m;
                        out[3] = 0;
                        out[4] = 0;
                        out[5] = 0;
                        Ok(Value::Macaddr(out))
                    }
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "trunc() needs numeric, got {:?}",
                            other.data_type()
                        ),
                    }),
                },
                2 => {
                    if args.iter().any(|v| matches!(v, Value::Null)) {
                        return Ok(Value::Null);
                    }
                    let n = match &args[1] {
                        Value::SmallInt(x) => i32::from(*x),
                        Value::Int(x) => *x,
                        Value::BigInt(x) => {
                            i32::try_from(*x).map_err(|_| EvalError::TypeMismatch {
                                detail: "trunc(): scale must fit in i32".into(),
                            })?
                        }
                        other => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "trunc(): scale must be integer, got {:?}",
                                    other.data_type()
                                ),
                            });
                        }
                    };
                    let x = match &args[0] {
                        Value::SmallInt(v) => f64::from(*v),
                        Value::Int(v) => f64::from(*v),
                        Value::BigInt(v) => *v as f64,
                        Value::Float(v) => *v,
                        Value::Numeric { scaled, scale } => {
                            (*scaled as f64) / f64_powi(10.0, i32::from(*scale))
                        }
                        other => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "trunc() needs numeric x, got {:?}",
                                    other.data_type()
                                ),
                            });
                        }
                    };
                    let result = if n >= 0 {
                        let factor = f64_powi(10.0, n);
                        f64_trunc(x * factor) / factor
                    } else {
                        let factor = f64_powi(10.0, -n);
                        f64_trunc(x / factor) * factor
                    };
                    Ok(Value::Float(result))
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("trunc() takes 1 or 2 args, got {}", args.len()),
                }),
            }
        }
        "round" => {
            match args.len() {
                1 => match &args[0] {
                    Value::Null => Ok(Value::Null),
                    Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) => {
                        Ok(args[0].clone().into_owned())
                    }
                    // v7.38 (read01) — PG rounds `double precision` half-to-even
                    // (round(2.5::float8)=2). Now that a bare `2.5` is a NUMERIC
                    // literal (routed through the half-away numeric arm below), a
                    // genuine Float here is an explicit double, so banker's rounding
                    // is correct and no longer clashes with `round(2.5)=3`.
                    Value::Float(x) => Ok(Value::Float(super::math::f64_round_half_even(*x))),
                    Value::Numeric { scaled, scale } => {
                        let factor = pow10_i128(*scale);
                        // Half-away-from-zero on the magnitude, then
                        // restore the sign — div_euclid alone rounds
                        // toward -inf and mishandles negatives
                        // (round(-2.5) must be -3, not -2).
                        let neg = *scaled < 0;
                        let abs = scaled.unsigned_abs() as i128;
                        let q = abs / factor;
                        let r = abs % factor;
                        let mag = if 2 * r >= factor { q + 1 } else { q };
                        // PG `round(numeric)` (1-arg) yields scale 0.
                        Ok(Value::Numeric {
                            scaled: if neg { -mag } else { mag },
                            scale: 0,
                        })
                    }
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "round() needs numeric, got {:?}",
                            other.data_type()
                        ),
                    }),
                },
                2 => {
                    if args.iter().any(|v| matches!(v, Value::Null)) {
                        return Ok(Value::Null);
                    }
                    let n = match &args[1] {
                        Value::SmallInt(x) => i32::from(*x),
                        Value::Int(x) => *x,
                        Value::BigInt(x) => {
                            i32::try_from(*x).map_err(|_| EvalError::TypeMismatch {
                                detail: "round(): scale must fit in i32".into(),
                            })?
                        }
                        other => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "round(): scale must be integer, got {:?}",
                                    other.data_type()
                                ),
                            });
                        }
                    };
                    // Exact NUMERIC rounding for a non-negative target scale:
                    // going through f64 corrupts values like
                    // `round(1.255::numeric, 2)` (1.255 has no exact f64, so it
                    // lands at 1.25 instead of PG's 1.26). Do half-away-from-zero
                    // on the integer mantissa. Negative target scales fall
                    // through to the f64 path below (existing behaviour).
                    if let Value::Numeric { scaled, scale } = &args[0] {
                        let cur = i32::from(*scale);
                        if (0..=38).contains(&n) {
                            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                            let out = if n >= cur {
                                scaled.checked_mul(pow10_i128((n - cur) as u8)).map(|m| {
                                    Value::Numeric { scaled: m, scale: n as u8 }
                                })
                            } else {
                                let factor = pow10_i128((cur - n) as u8);
                                let neg = *scaled < 0;
                                let abs = scaled.unsigned_abs() as i128;
                                let q = abs / factor;
                                let r = abs % factor;
                                let mag = if 2 * r >= factor { q + 1 } else { q };
                                Some(Value::Numeric {
                                    scaled: if neg { -mag } else { mag },
                                    scale: n as u8,
                                })
                            };
                            if let Some(v) = out {
                                return Ok(v);
                            }
                        }
                    }
                    // Convert input to f64 for arithmetic
                    // simplicity (PG does NUMERIC math here but
                    // SPG's f64 path matches the dominant
                    // customer expectation for round(N, scale)
                    // patterns).
                    let x = match &args[0] {
                        Value::SmallInt(v) => f64::from(*v),
                        Value::Int(v) => f64::from(*v),
                        Value::BigInt(v) => *v as f64,
                        Value::Float(v) => *v,
                        Value::Numeric { scaled, scale } => {
                            (*scaled as f64) / f64_powi(10.0, i32::from(*scale))
                        }
                        other => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "round() needs numeric x, got {:?}",
                                    other.data_type()
                                ),
                            });
                        }
                    };
                    // Avoid float precision drift from the
                    // 10^(-k) reciprocal — for n<0 work with the
                    // positive-exponent form: round(x / 10^|n|) *
                    // 10^|n|.
                    let result = if n >= 0 {
                        let factor = f64_powi(10.0, n);
                        f64_round_half_away(x * factor) / factor
                    } else {
                        let factor = f64_powi(10.0, -n);
                        f64_round_half_away(x / factor) * factor
                    };
                    Ok(Value::Float(result))
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("round() takes 1 or 2 args, got {}", args.len()),
                }),
            }
        }
        "ceil" | "ceiling" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("ceil() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) => {
                    Ok(args[0].clone().into_owned())
                }
                Value::Float(x) => Ok(Value::Float(f64_ceil(*x))),
                Value::Numeric { scaled, scale } => {
                    let factor = pow10_i128(*scale);
                    let q = scaled.div_euclid(factor);
                    let r = scaled.rem_euclid(factor);
                    let result = if r == 0 { q } else { q + 1 };
                    // PG `ceil(numeric)` yields scale 0.
                    Ok(Value::Numeric {
                        scaled: result,
                        scale: 0,
                    })
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("ceil() needs numeric, got {:?}", other.data_type()),
                }),
            }
        }
        "floor" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("floor() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) => {
                    Ok(args[0].clone().into_owned())
                }
                Value::Float(x) => Ok(Value::Float(f64_floor(*x))),
                Value::Numeric { scaled, scale } => {
                    let factor = pow10_i128(*scale);
                    let q = scaled.div_euclid(factor);
                    // div_euclid rounds toward -infinity which is
                    // exactly the floor semantic — perfect for
                    // negative values. PG `floor(numeric)` yields scale 0.
                    Ok(Value::Numeric {
                        scaled: q,
                        scale: 0,
                    })
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("floor() needs numeric, got {:?}", other.data_type()),
                }),
            }
        }
        "left" => string_left_right(args, true, "left"),
        "right" => string_left_right(args, false, "right"),
        "strpos" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "strpos() takes 2 args (haystack, needle), got {}",
                        args.len()
                    ),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            // PG `POSITION(bit IN bit)` — bit-level search (MSB-first), 1-based.
            // Must precede any text/byte fallback: the MSB-packed byte forms
            // would otherwise mis-match (B'11'=[0xC0] not found in B'0110'=[0x60]).
            if let (
                Value::BitString { nbits: h_nbits, bytes: h_bytes },
                Value::BitString { nbits: n_nbits, bytes: n_bytes },
            ) = (&args[0], &args[1])
            {
                if *n_nbits == 0 {
                    return Ok(Value::Int(1));
                }
                if n_nbits > h_nbits {
                    return Ok(Value::Int(0));
                }
                let bit_at = |bytes: &[u8], i: usize| -> u8 { (bytes[i / 8] >> (7 - i % 8)) & 1 };
                let (hn, nn) = (*h_nbits as usize, *n_nbits as usize);
                for start in 0..=(hn - nn) {
                    if (0..nn).all(|j| bit_at(h_bytes, start + j) == bit_at(n_bytes, j)) {
                        return Ok(Value::Int(i32::try_from(start + 1).unwrap_or(i32::MAX)));
                    }
                }
                return Ok(Value::Int(0));
            }
            // PG `POSITION(bytea IN bytea)` lowers here as strpos(str, sub);
            // it is a byte-level search, not a rendered-text search.
            if let (Value::Bytes(haystack), Value::Bytes(needle)) = (&args[0], &args[1]) {
                if needle.is_empty() {
                    return Ok(Value::Int(1));
                }
                if needle.len() > haystack.len() {
                    return Ok(Value::Int(0));
                }
                for i in 0..=haystack.len() - needle.len() {
                    if &haystack[i..i + needle.len()] == needle.as_ref() {
                        return Ok(Value::Int(i32::try_from(i + 1).unwrap_or(i32::MAX)));
                    }
                }
                return Ok(Value::Int(0));
            }
            let haystack = value_to_format_text(&args[0]);
            let needle = value_to_format_text(&args[1]);
            if needle.is_empty() {
                return Ok(Value::Int(1));
            }
            let h_chars: Vec<char> = haystack.chars().collect();
            let n_chars: Vec<char> = needle.chars().collect();
            if n_chars.len() > h_chars.len() {
                return Ok(Value::Int(0));
            }
            for i in 0..=h_chars.len() - n_chars.len() {
                if h_chars[i..i + n_chars.len()] == n_chars[..] {
                    return Ok(Value::Int(i32::try_from(i + 1).unwrap_or(i32::MAX)));
                }
            }
            Ok(Value::Int(0))
        }
        // v7.37.17 (17.6 siblings) — PG 18 casefold(text): Unicode
        // case folding for caseless matching. Rust's char-level
        // to_lowercase applies the full Unicode lowercase mapping
        // (ß → ss requires the special foldings which str::
        // to_lowercase also handles).
        "casefold" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("casefold() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::text(s.to_lowercase())),
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "casefold() needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "lpad" => string_pad(args, true, "lpad"),
        "rpad" => string_pad(args, false, "rpad"),
        // v7.37.17 (17.6 siblings) — PG reverse(text) inverts the
        // char sequence. Multi-byte-safe via chars() iterator.
        "reverse" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("reverse() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let reversed: alloc::string::String = s.chars().rev().collect();
                    Ok(Value::text(reversed))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "reverse() needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "repeat" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("repeat() takes 2 args, got {}", args.len()),
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
                            "repeat(): n must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if n <= 0 {
                return Ok(Value::text(String::new()));
            }
            // Safety cap so a runaway argument doesn't allocate
            // terabytes. PG itself enforces a similar cap via
            // work_mem; SPG inherits a defensive 64MiB cap.
            const MAX_REPEAT_BYTES: usize = 64 * 1024 * 1024;
            let needed =
                s.len()
                    .checked_mul(n as usize)
                    .ok_or_else(|| EvalError::TypeMismatch {
                        detail: "repeat(): result size overflows usize".into(),
                    })?;
            if needed > MAX_REPEAT_BYTES {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "repeat(): result would exceed {MAX_REPEAT_BYTES} bytes"
                    ),
                });
            }
            Ok(Value::text(s.repeat(n as usize)))
        }
        "split_part" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "split_part() takes 3 args (string, delim, n), got {}",
                        args.len()
                    ),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let s = value_to_format_text(&args[0]);
            let delim = value_to_format_text(&args[1]);
            if delim.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: "split_part(): delimiter cannot be empty".into(),
                });
            }
            let n = match &args[2] {
                Value::SmallInt(x) => i64::from(*x),
                Value::Int(x) => i64::from(*x),
                Value::BigInt(x) => *x,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "split_part(): n must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if n == 0 {
                return Err(EvalError::TypeMismatch {
                    detail: "split_part(): n must be nonzero (PG: 1-indexed)".into(),
                });
            }
            let parts: alloc::vec::Vec<&str> = s.split(&delim[..]).collect();
            let total = parts.len() as i64;
            let idx = if n > 0 {
                n - 1
            } else {
                // n=-1 → last (idx = total - 1)
                total + n
            };
            if idx < 0 || idx >= total {
                return Ok(Value::text(String::new()));
            }
            Ok(Value::text(parts[idx as usize].to_string()))
        }
        // PG `translate(s, from, to)` — char-by-char positional
        // mapping. Each codepoint in `from` is replaced by the
        // codepoint at the same index in `to`. When `from` is
        // longer than `to`, the extra `from` codepoints are
        // DELETED (not replaced). When `from` has duplicates,
        // the FIRST occurrence's mapping wins. NULL → NULL.
        "translate" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("translate() takes 3 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let s = value_to_format_text(&args[0]);
            let from = value_to_format_text(&args[1]);
            let to = value_to_format_text(&args[2]);
            let from_chars: Vec<char> = from.chars().collect();
            let to_chars: Vec<char> = to.chars().collect();
            // Build the codepoint map. First occurrence wins.
            let mut map: alloc::collections::BTreeMap<char, Option<char>> =
                alloc::collections::BTreeMap::new();
            for (i, &fc) in from_chars.iter().enumerate() {
                if map.contains_key(&fc) {
                    continue;
                }
                let replacement = to_chars.get(i).copied();
                map.insert(fc, replacement);
            }
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                match map.get(&c) {
                    Some(Some(r)) => out.push(*r),
                    Some(None) => {} // mapped to "deleted"
                    None => out.push(c),
                }
            }
            Ok(Value::text(out))
        }
        "replace" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "replace() takes 3 args (string, from, to), got {}",
                        args.len()
                    ),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let s = value_to_format_text(&args[0]);
            let from = value_to_format_text(&args[1]);
            let to = value_to_format_text(&args[2]);
            if from.is_empty() {
                return Ok(Value::text(s));
            }
            // std `String::replace` matches PG semantics exactly:
            // non-overlapping, left-to-right, no re-scan of
            // inserted text. Sealed test surface verifies the
            // edge cases independently.
            Ok(Value::text(s.replace(&from[..], &to)))
        }
        "trim" | "btrim" => string_trim(args, TrimSide::Both, "trim"),
        "ltrim" => string_trim(args, TrimSide::Left, "ltrim"),
        "rtrim" => string_trim(args, TrimSide::Right, "rtrim"),
        "concat_ws" => {
            if args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: "concat_ws() requires at least 1 arg (the separator)".into(),
                });
            }
            // NULL separator poisons the result.
            let sep = match &args[0] {
                Value::Null => return Ok(Value::Null),
                v => value_to_format_text(v),
            };
            let mut out = String::new();
            let mut first = true;
            for v in &args[1..] {
                if matches!(v, Value::Null) {
                    continue;
                }
                if first {
                    first = false;
                } else {
                    out.push_str(&sep);
                }
                out.push_str(&value_to_format_text(v));
            }
            Ok(Value::text(out))
        }
        // v7.17.0 Phase 3.7 — PG regex function family.
        "regexp_matches" => regexp_matches(args),
        // PG 10+ singular form: first match as text[], NULL when
        // no match.
        "regexp_match" => super::regexp::regexp_match(args),
        "regexp_replace" => regexp_replace(args),
        "regexp_split_to_array" => regexp_split_to_array(args),
        // v7.37.17 (17.6 siblings) — PG 15+ regexp family.
        "regexp_count" => super::regexp::regexp_count(args),
        "regexp_instr" => super::regexp::regexp_instr(args),
        "regexp_substr" => super::regexp::regexp_substr(args),
        "regexp_like" => super::regexp::regexp_like(args),
        // v7.17.0 Phase 3.P0-28 — PG JSON builder family.
        // to_json / to_jsonb coerce any value to JSON text (NULL
        // becomes the JSON literal 'null', not SQL NULL).
        "to_json" | "to_jsonb" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("to_json() takes 1 arg, got {}", args.len()),
                });
            }
            // Json input passes through verbatim — PG identity.
            let out = if let Value::Json(s) = &args[0] {
                Value::json(s.clone())
            } else {
                Value::json(crate::json::value_to_json_text(&args[0]))
            };
            // to_jsonb yields canonical jsonb; to_json stays verbatim.
            if name == "to_jsonb" {
                Ok(crate::json::canonicalize_value(out))
            } else {
                Ok(out)
            }
        }
        "jsonb_build_object" => {
            crate::json::build_object(args).map(crate::json::canonicalize_value)
        }
        "json_build_object" => crate::json::build_object(args),
        // Catalog function forms of the ? / ?| / ?& / @> / <@
        // operators — same helpers the operators use.
        "jsonb_exists" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "jsonb_exists() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            crate::json::key_exists(&args[0], &args[1])
        }
        "jsonb_exists_any" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "jsonb_exists_any() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            crate::json::keys_any(&args[0], &args[1])
        }
        "jsonb_exists_all" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "jsonb_exists_all() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            crate::json::keys_all(&args[0], &args[1])
        }
        "jsonb_contains" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "jsonb_contains() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            crate::json::contains(&args[0], &args[1])
        }
        "jsonb_contained" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "jsonb_contained() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            // a <@ b  ==  b @> a.
            crate::json::contains(&args[1], &args[0])
        }
        // Catalog function forms of the -> / ->> operators —
        // json_object_field(doc, key), json_array_element(doc, i)
        // + _text variants + jsonb_ twins. Some ORMs/drivers call
        // these by name instead of emitting the operator. Delegates
        // to the operators' path_walk with a single-step path.
        "json_object_field"
        | "jsonb_object_field"
        | "json_object_field_text"
        | "jsonb_object_field_text"
        | "json_array_element"
        | "jsonb_array_element"
        | "json_array_element_text"
        | "jsonb_array_element_text" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let as_text = name.ends_with("_text");
            let step = if name.contains("array_element") {
                match &args[1] {
                    Value::Int(n) => n.to_string(),
                    Value::BigInt(n) => n.to_string(),
                    Value::SmallInt(n) => n.to_string(),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "{name}() index must be integer, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                }
            } else {
                match &args[1] {
                    Value::Text(s) => s.as_ref().to_string(),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "{name}() key must be text, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                }
            };
            let path = alloc::format!("{{{step}}}");
            crate::json::path_walk(&args[0], &Value::text(path), as_text)
        }
        // v7.37.17 (17.6 siblings) — json_extract_path(json,
        // VARIADIC path...) + _text variants. Function form of the
        // #> / #>> operators. Delegates to the same path_walk used
        // by the operators, converting the variadic tail into the
        // '{a,b}' text-array form path_walk expects.
        "json_extract_path"
        | "jsonb_extract_path"
        | "json_extract_path_text"
        | "jsonb_extract_path_text" => {
            if args.len() < 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "{name}() takes at least 2 args, got {}",
                        args.len()
                    ),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            let mut path = alloc::string::String::from("{");
            for (i, step) in args[1..].iter().enumerate() {
                if i > 0 {
                    path.push(',');
                }
                match step {
                    Value::Null => return Ok(Value::Null),
                    Value::Text(s) => path.push_str(s),
                    Value::Int(n) => path.push_str(&alloc::format!("{n}")),
                    Value::BigInt(n) => path.push_str(&alloc::format!("{n}")),
                    Value::SmallInt(n) => path.push_str(&alloc::format!("{n}")),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "{name}(): path elements must be text, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                }
            }
            path.push('}');
            let as_text = name.ends_with("_text");
            crate::json::path_walk(&args[0], &Value::text(path), as_text)
        }
        // v7.37.17 (17.6 siblings) — json_object(text[]) /
        // jsonb_object(text[]) build an object from a flat array
        // of alternating key/value pairs. 2-array form
        // json_object(keys, values) also supported.
        "json_object" | "jsonb_object" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("json_object() takes 1 or 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            // v7.38 (read01 sweep) — PG coerces an untyped `'{a,b}'` literal to
            // text[] for the json_object(text[] [, text[]]) signature. SPG keeps
            // it as text, so accept a Text arg that parses as an array literal
            // (the ARRAY[...] and ::text[] spellings already arrive as TextArray).
            let args: alloc::vec::Vec<Value> = args
                .iter()
                .map(|a| match a {
                    Value::Text(s) => match crate::conversions::decode_text_array_literal(s) {
                        Ok(arr) => Value::TextArray(arr),
                        Err(_) => a.clone(),
                    },
                    other => other.clone(),
                })
                .collect();
            fn escape_into(s: &str, out: &mut alloc::string::String) {
                out.push('"');
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if (c as u32) < 0x20 => {
                            out.push_str(&alloc::format!("\\u{:04x}", c as u32));
                        }
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            let mut out = alloc::string::String::from("{");
            if args.len() == 1 {
                // Flat array: [k1, v1, k2, v2, ...]
                let items = match &args[0] {
                    Value::TextArray(items) => items,
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "json_object(): needs text[], got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                };
                if items.len() % 2 != 0 {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "json_object(): array must have even length, got {}",
                            items.len()
                        ),
                    });
                }
                let mut first = true;
                for pair in items.chunks_exact(2) {
                    let Some(key) = &pair[0] else {
                        return Err(EvalError::TypeMismatch {
                            detail: "json_object(): key cannot be NULL".into(),
                        });
                    };
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    escape_into(key, &mut out);
                    out.push(':');
                    match &pair[1] {
                        Some(v) => escape_into(v, &mut out),
                        None => out.push_str("null"),
                    }
                }
            } else {
                // 2-array form: keys[], values[].
                let (keys, values) = match (&args[0], &args[1]) {
                    (Value::TextArray(k), Value::TextArray(v)) => (k, v),
                    (a, b) => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "json_object(): needs (text[], text[]), got ({:?}, {:?})",
                                a.data_type(),
                                b.data_type()
                            ),
                        });
                    }
                };
                if keys.len() != values.len() {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "json_object(): key/value arrays differ in length ({} vs {})",
                            keys.len(),
                            values.len()
                        ),
                    });
                }
                let mut first = true;
                for (k, v) in keys.iter().zip(values.iter()) {
                    let Some(key) = k else {
                        return Err(EvalError::TypeMismatch {
                            detail: "json_object(): key cannot be NULL".into(),
                        });
                    };
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    escape_into(key, &mut out);
                    out.push(':');
                    match v {
                        Some(val) => escape_into(val, &mut out),
                        None => out.push_str("null"),
                    }
                }
            }
            out.push('}');
            Ok(Value::json(out))
        }
        // "json_array" is MySQL's spelling of json_build_array.
        "jsonb_build_array" => {
            crate::json::build_array(args).map(crate::json::canonicalize_value)
        }
        "json_build_array" | "json_array" => crate::json::build_array(args),
        // v7.37.17 (17.6 siblings) — MySQL path-based JSON functions
        // (json.rs mysql_path_steps parser: $, .key, ."quoted", [N];
        // wildcards error honestly).
        "json_extract" => crate::json::mysql_json_extract(args),
        "json_contains_path" => crate::json::mysql_json_contains_path(args),
        "json_array_append" => crate::json::mysql_json_array_append(args),
        "json_array_insert" => crate::json::mysql_json_array_insert(args),
        "json_contains" => crate::json::mysql_json_contains(args),
        "json_merge_patch" => crate::json::mysql_json_merge_patch(args),
        // "json_merge" is the deprecated MySQL alias for _preserve.
        "json_merge_preserve" | "json_merge" => {
            crate::json::mysql_json_merge_preserve(args)
        }
        "json_overlaps" => crate::json::mysql_json_overlaps(args),
        "json_search" => crate::json::mysql_json_search(args),
        "json_value" => crate::json::mysql_json_value(args),
        // v7.37.17 (17.6 siblings) — MySQL non-path JSON functions.
        // v7.37.17 (17.6 siblings) — the SQL:2016 / PG 16
        // `IS [NOT] JSON [kind]` predicate lowers onto this. Never
        // errors: invalid JSON is simply false.
        "pg_is_json" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("pg_is_json() takes 2 args, got {}", args.len()),
                });
            }
            let Value::Text(kind) = &args[1] else {
                return Err(EvalError::TypeMismatch {
                    detail: "pg_is_json() kind must be text".into(),
                });
            };
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Json(s) | Value::Text(s) => {
                    let Ok(parsed) = crate::json::parse(s) else {
                        return Ok(Value::Bool(false));
                    };
                    let ok = match kind.as_ref() {
                        "object" => matches!(parsed, crate::json::JsonValue::Object(_)),
                        "array" => matches!(parsed, crate::json::JsonValue::Array(_)),
                        "scalar" => !matches!(
                            parsed,
                            crate::json::JsonValue::Object(_)
                                | crate::json::JsonValue::Array(_)
                        ),
                        _ => true, // "value" — any valid JSON
                    };
                    Ok(Value::Bool(ok))
                }
                // Non-text values are not JSON text.
                _ => Ok(Value::Bool(false)),
            }
        }
        "json_valid" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("json_valid() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Json(s) | Value::Text(s) => {
                    Ok(Value::Bool(crate::json::parse(s).is_ok()))
                }
                _ => Ok(Value::Bool(false)),
            }
        }
        // MySQL JSON_TYPE: uppercase names with the INTEGER/DOUBLE
        // distinction (unlike PG's lowercase jsonb_typeof).
        "json_type" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("json_type() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Json(s) | Value::Text(s) => {
                    let parsed = crate::json::parse(s).map_err(|e| {
                        EvalError::TypeMismatch {
                            detail: format!("json_type(): invalid JSON: {e}"),
                        }
                    })?;
                    let name = match &parsed {
                        crate::json::JsonValue::Object(_) => "OBJECT",
                        crate::json::JsonValue::Array(_) => "ARRAY",
                        crate::json::JsonValue::String(_) => "STRING",
                        crate::json::JsonValue::Bool(_) => "BOOLEAN",
                        crate::json::JsonValue::Null => "NULL",
                        crate::json::JsonValue::Number(x) => {
                            if *x == libm::trunc(*x) {
                                "INTEGER"
                            } else {
                                "DOUBLE"
                            }
                        }
                        crate::json::JsonValue::NumberText(t) => {
                            if t.contains('.') || t.contains('e') || t.contains('E') {
                                "DOUBLE"
                            } else {
                                "INTEGER"
                            }
                        }
                    };
                    Ok(Value::text(alloc::string::String::from(name)))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "json_type() needs json, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // MySQL JSON_LENGTH: object → member count, array → element
        // count, scalar → 1.
        "json_length" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("json_length() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Json(s) | Value::Text(s) => {
                    let parsed = crate::json::parse(s).map_err(|e| {
                        EvalError::TypeMismatch {
                            detail: format!("json_length(): invalid JSON: {e}"),
                        }
                    })?;
                    let n = match &parsed {
                        crate::json::JsonValue::Object(members) => members.len(),
                        crate::json::JsonValue::Array(items) => items.len(),
                        _ => 1,
                    };
                    Ok(Value::Int(n as i32))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "json_length() needs json, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // MySQL JSON_KEYS: the object's top-level keys as a JSON
        // array (["a","b"]), not a SQL array.
        "json_keys" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("json_keys() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Json(s) | Value::Text(s) => {
                    let parsed = crate::json::parse(s).map_err(|e| {
                        EvalError::TypeMismatch {
                            detail: format!("json_keys(): invalid JSON: {e}"),
                        }
                    })?;
                    match parsed {
                        crate::json::JsonValue::Object(members) => {
                            let mut out = alloc::string::String::from("[");
                            for (i, (k, _)) in members.iter().enumerate() {
                                if i > 0 {
                                    out.push_str(", ");
                                }
                                out.push('"');
                                for c in k.chars() {
                                    match c {
                                        '"' => out.push_str("\\\""),
                                        '\\' => out.push_str("\\\\"),
                                        c => out.push(c),
                                    }
                                }
                                out.push('"');
                            }
                            out.push(']');
                            Ok(Value::Json(alloc::borrow::Cow::Owned(out)))
                        }
                        // Non-object → NULL (MySQL semantics).
                        _ => Ok(Value::Null),
                    }
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "json_keys() needs json, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // MySQL JSON_DEPTH: scalar / empty object / empty array = 1;
        // each level of nesting adds 1.
        "json_depth" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("json_depth() takes 1 arg, got {}", args.len()),
                });
            }
            fn depth(v: &crate::json::JsonValue) -> i32 {
                match v {
                    crate::json::JsonValue::Object(members) => {
                        1 + members.iter().map(|(_, x)| depth(x)).max().unwrap_or(0)
                    }
                    crate::json::JsonValue::Array(items) => {
                        1 + items.iter().map(depth).max().unwrap_or(0)
                    }
                    _ => 1,
                }
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Json(s) | Value::Text(s) => {
                    let parsed = crate::json::parse(s).map_err(|e| {
                        EvalError::TypeMismatch {
                            detail: format!("json_depth(): invalid JSON: {e}"),
                        }
                    })?;
                    Ok(Value::Int(depth(&parsed)))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "json_depth() needs json, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // MySQL JSON_QUOTE: wrap a string as a JSON string literal.
        "json_quote" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("json_quote() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let mut out = alloc::string::String::from("\"");
                    for c in s.chars() {
                        match c {
                            '"' => out.push_str("\\\""),
                            '\\' => out.push_str("\\\\"),
                            '\n' => out.push_str("\\n"),
                            '\r' => out.push_str("\\r"),
                            '\t' => out.push_str("\\t"),
                            c if (c as u32) < 0x20 => {
                                out.push_str(&alloc::format!("\\u{:04x}", c as u32));
                            }
                            c => out.push(c),
                        }
                    }
                    out.push('"');
                    Ok(Value::Json(alloc::borrow::Cow::Owned(out)))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "json_quote() needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // MySQL JSON_UNQUOTE: a JSON string loses its quotes; other
        // JSON values render as their compact text.
        "json_unquote" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("json_unquote() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Json(s) | Value::Text(s) => {
                    let trimmed = s.trim();
                    if trimmed.starts_with('"') {
                        let parsed = crate::json::parse(trimmed).map_err(|e| {
                            EvalError::TypeMismatch {
                                detail: format!("json_unquote(): invalid JSON: {e}"),
                            }
                        })?;
                        match parsed {
                            crate::json::JsonValue::String(inner) => Ok(Value::text(inner)),
                            _ => Ok(Value::text(trimmed.to_string())),
                        }
                    } else {
                        Ok(Value::text(trimmed.to_string()))
                    }
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "json_unquote() needs json, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // MySQL JSON_STORAGE_SIZE: bytes used to store the value —
        // SPG stores JSON as text, so this is the compact text's
        // byte length (real for SPG's storage model).
        "json_storage_size" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "json_storage_size() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Json(s) | Value::Text(s) => Ok(Value::Int(s.len() as i32)),
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "json_storage_size() needs json, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // json_set serves two dialects: MySQL's '$.x' string paths
        // route to the MySQL mutator; PG's text-array path spelling
        // ('{a,b}') stays on crate::json::set.
        "jsonb_set" | "json_set"
            if args.len() >= 3
                && matches!(&args[1], Value::Text(p) if p.trim_start().starts_with('$')) =>
        {
            crate::json::mysql_json_set(args)
        }
        "jsonb_set" => crate::json::set(args).map(crate::json::canonicalize_value),
        "json_set" => crate::json::set(args),
        // v7.37.17 (17.6 siblings) — MySQL JSON mutation family on
        // the '$.x' path machinery.
        "json_replace" => crate::json::mysql_json_replace(args),
        "json_remove" => crate::json::mysql_json_remove(args),
        // jsonb_set_lax — jsonb_set with configurable SQL-NULL
        // new_value handling (PG 13+). Treatments:
        //   use_json_null (default) / raise_exception /
        //   return_target / delete_key
        "jsonb_set_lax" => {
            if !(3..=5).contains(&args.len()) {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "jsonb_set_lax() takes 3-5 args, got {}",
                        args.len()
                    ),
                });
            }
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null)
            {
                return Ok(Value::Null);
            }
            if !matches!(args[2], Value::Null) {
                // Non-NULL new_value — plain jsonb_set.
                return crate::json::set(&args[..args.len().min(4)]);
            }
            let treatment = match args.get(4) {
                None | Some(Value::Null) => "use_json_null",
                Some(Value::Text(s)) => s.as_ref(),
                Some(other) => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "jsonb_set_lax() treatment must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            match treatment {
                "use_json_null" => {
                    let mut adjusted: alloc::vec::Vec<Value<'_>> =
                        args[..2].to_vec();
                    adjusted.push(Value::json("null"));
                    if let Some(create) = args.get(3) {
                        adjusted.push(create.clone());
                    }
                    crate::json::set(&adjusted)
                }
                "raise_exception" => Err(EvalError::TypeMismatch {
                    detail: "jsonb_set_lax(): JSON value must not be null"
                        .into(),
                }),
                "return_target" => Ok(args[0].clone().into_owned()),
                "delete_key" => crate::json::delete_path(&args[..2]),
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "jsonb_set_lax(): invalid null_value_treatment {other:?}"
                    ),
                }),
            }
        }
        // json_to_tsvector / jsonb_to_tsvector — reduce a JSON doc
        // to a tsvector using a filter: '"all"' or an array of
        // "string" / "numeric" / "boolean" / "key". Optional
        // leading config arg (3-arg form) is accepted and ignored
        // (SPG's FTS pipeline is single-config).
        "json_to_tsvector" | "jsonb_to_tsvector" => {
            let (doc_arg, filter_arg) = match args.len() {
                2 => (&args[0], &args[1]),
                3 => (&args[1], &args[2]),
                n => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() takes 2 or 3 args, got {n}"
                        ),
                    });
                }
            };
            let doc_text = match doc_arg {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.as_ref(),
                Value::Json(s) => s.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() needs json, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let filter_text = match filter_arg {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.as_ref(),
                Value::Json(s) => s.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() filter must be json, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            // Parse the filter into flags.
            let filter = crate::json::parse(filter_text).map_err(|e| {
                EvalError::TypeMismatch {
                    detail: alloc::format!("{name}(): invalid filter — {e}"),
                }
            })?;
            let mut want_string = false;
            let mut want_numeric = false;
            let mut want_boolean = false;
            let mut want_key = false;
            let mut apply = |flag: &str| -> Result<(), EvalError> {
                match flag {
                    "all" => {
                        want_string = true;
                        want_numeric = true;
                        want_boolean = true;
                        want_key = true;
                    }
                    "string" => want_string = true,
                    "numeric" => want_numeric = true,
                    "boolean" => want_boolean = true,
                    "key" => want_key = true,
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "{name}(): unknown filter flag {other:?}"
                            ),
                        });
                    }
                }
                Ok(())
            };
            match &filter {
                crate::json::JsonValue::String(s) => apply(s)?,
                crate::json::JsonValue::Array(items) => {
                    for item in items {
                        match item {
                            crate::json::JsonValue::String(s) => apply(s)?,
                            _ => {
                                return Err(EvalError::TypeMismatch {
                                    detail: alloc::format!(
                                        "{name}(): filter array must hold strings"
                                    ),
                                });
                            }
                        }
                    }
                }
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}(): filter must be a string or string array"
                        ),
                    });
                }
            }
            let doc = crate::json::parse(doc_text).map_err(|e| {
                EvalError::TypeMismatch {
                    detail: alloc::format!("{name}(): invalid JSON — {e}"),
                }
            })?;
            fn collect(
                node: &crate::json::JsonValue,
                want: (bool, bool, bool, bool),
                out: &mut alloc::vec::Vec<alloc::string::String>,
            ) {
                let (s, n, b, k) = want;
                match node {
                    crate::json::JsonValue::String(v) if s => {
                        for word in v.split_whitespace() {
                            out.push(word.to_lowercase());
                        }
                    }
                    crate::json::JsonValue::Number(v) if n => {
                        out.push(alloc::format!("{v}"));
                    }
                    crate::json::JsonValue::NumberText(v) if n => {
                        out.push(v.clone());
                    }
                    crate::json::JsonValue::Bool(v) if b => {
                        out.push(alloc::format!("{v}"));
                    }
                    crate::json::JsonValue::Array(items) => {
                        for item in items {
                            collect(item, want, out);
                        }
                    }
                    crate::json::JsonValue::Object(entries) => {
                        for (key, val) in entries {
                            if k {
                                for word in key.split_whitespace() {
                                    out.push(word.to_lowercase());
                                }
                            }
                            collect(val, want, out);
                        }
                    }
                    _ => {}
                }
            }
            let mut lexemes = alloc::vec::Vec::new();
            collect(
                &doc,
                (want_string, want_numeric, want_boolean, want_key),
                &mut lexemes,
            );
            lexemes.sort();
            lexemes.dedup();
            Ok(Value::text(lexemes.join(" ")))
        }
        // pg_collation_for(any) — the collation of the argument's
        // type. Text is collatable → PG's '"default"'; everything
        // else errors like PG ("collations are not supported").
        "pg_collation_for" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "pg_collation_for() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(_) => {
                    Ok(Value::text::<String>("\"default\"".into()))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "collations are not supported by type {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — jsonb_delete_path: function
        // form of the #- operator.
        "jsonb_delete_path" => {
            crate::json::delete_path(args).map(crate::json::canonicalize_value)
        }
        "json_delete_path" => crate::json::delete_path(args),
        // Same two-dialect routing as json_set above.
        "jsonb_insert" | "json_insert"
            if args.len() >= 3
                && matches!(&args[1], Value::Text(p) if p.trim_start().starts_with('$')) =>
        {
            crate::json::mysql_json_insert(args)
        }
        "jsonb_insert" => crate::json::insert(args).map(crate::json::canonicalize_value),
        "json_insert" => crate::json::insert(args),
        // v7.17.0 Phase 3.9 — PG `jsonb_path_query` family.
        // v7.37.17 (17.6 siblings) — jsonb_concat / jsonb_delete —
        // function forms of || and - operators.
        "jsonb_concat" | "json_concat" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("jsonb_concat() takes 2 args, got {}", args.len()),
                });
            }
            crate::json::concat(&args[0], &args[1])
        }
        "jsonb_delete" | "json_delete" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("jsonb_delete() takes 2 args, got {}", args.len()),
                });
            }
            crate::json::delete_key(&args[0], &args[1])
        }
        // v7.37.17 (17.6 siblings) — jsonb_path_exists(doc, path)
        // returns whether the JSONPath matches at least one item.
        // jsonb_path_match(doc, path) evaluates a boolean-predicate
        // path (we approximate with exists — full predicate
        // evaluation queues with the jsonpath engine widening).
        "jsonb_path_exists" | "json_path_exists" => {
            if args.len() < 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "jsonb_path_exists() takes 2+ args, got {}",
                        args.len()
                    ),
                });
            }
            if args[..2].iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let q = crate::json::path_query(&args[0], &args[1])?;
            match q {
                Value::TextArray(items) => Ok(Value::Bool(!items.is_empty())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Bool(true)),
            }
        }
        "jsonb_path_match" | "json_path_match" => {
            if args.len() < 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "jsonb_path_match() takes 2+ args, got {}",
                        args.len()
                    ),
                });
            }
            if args[..2].iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let q = crate::json::path_query(&args[0], &args[1])?;
            match q {
                Value::TextArray(items) => {
                    // If the first match is a boolean literal, use it;
                    // otherwise treat any-match as true.
                    match items.first() {
                        Some(Some(s)) if s == "true" => Ok(Value::Bool(true)),
                        Some(Some(s)) if s == "false" => Ok(Value::Bool(false)),
                        Some(_) => Ok(Value::Bool(true)),
                        None => Ok(Value::Bool(false)),
                    }
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Bool(true)),
            }
        }
        "jsonb_path_query" | "json_path_query" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("jsonb_path_query() takes 2 args, got {}", args.len()),
                });
            }
            crate::json::path_query(&args[0], &args[1])
        }
        "jsonb_path_query_first" | "json_path_query_first" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "jsonb_path_query_first() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            crate::json::path_query_first(&args[0], &args[1])
        }
        "jsonb_path_query_array" | "json_path_query_array" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "jsonb_path_query_array() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            crate::json::path_query_array(&args[0], &args[1])
        }
        // v7.17.0 Phase 7 — INET / CIDR network helpers.
        "host" => inet_host(args),
        "network" => inet_network(args),
        "masklen" => inet_masklen(args),
        "set_masklen" => super::inet::inet_set_masklen(args),
        "abbrev" => super::inet::inet_abbrev(args),
        // v7.37.17 (17.6 siblings) — completing the INET family.
        "family" => super::inet::inet_family(args),
        "netmask" => super::inet::inet_netmask(args),
        "hostmask" => super::inet::inet_hostmask(args),
        "broadcast" => super::inet::inet_broadcast(args),
        "inet_same_family" => super::inet::inet_same_family(args),
        // inet_merge(a, b) — the smallest network including both.
        "inet_merge" => super::inet::inet_merge(args),
        // v7.37.17 (17.6 siblings) — MySQL network address helpers.
        "inet_aton" => super::inet::mysql_inet_aton(args),
        "inet_ntoa" => super::inet::mysql_inet_ntoa(args),
        "inet6_aton" => super::inet::mysql_inet6_aton(args),
        "inet6_ntoa" => super::inet::mysql_inet6_ntoa(args),
        "is_ipv4" => super::inet::mysql_is_ipv4(args),
        "is_ipv6" => super::inet::mysql_is_ipv6(args),
        // macaddr8_set7bit — EUI-64 → modified EUI-64.
        "macaddr8_set7bit" => super::inet::macaddr8_set7bit(args),
        // Connection-address probes. PG returns NULL for these on
        // Unix-socket connections; SPG embedded has no TCP
        // connection at all, so NULL is the honest PG-shaped
        // answer (monitoring dashboards render it as "local").
        "inet_client_addr" | "inet_server_addr" | "inet_client_port"
        | "inet_server_port" => Ok(Value::Null),
        // timeofday() — PG legacy wall-clock as formatted text
        // ('Dow Mon DD HH24:MI:SS.US YYYY TZ'). Uses the same
        // deterministic 2020-01-01 UTC anchor as the rest of the
        // wall-clock stubs until v7.38 wall-clock plumbing.
        "timeofday" => {
            if !args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "timeofday() takes no args, got {}",
                        args.len()
                    ),
                });
            }
            // 2020-01-01 00:00:00 UTC was a Wednesday.
            Ok(Value::text::<String>(
                "Wed Jan 01 00:00:00.000000 2020 UTC".into(),
            ))
        }
        // v6.4.3 — encode/decode + error_on_null SQL function bundle.
        "encode" => encode_text(args),
        "decode" => decode_text(args),
        // pgcrypto ASCII armor — real RFC 4880 §6 implementation
        // (base64 body + CRC-24 trailer).
        "armor" => super::encoding::pgp_armor(args),
        "dearmor" => super::encoding::pgp_dearmor(args),
        // PGP encryption surface — needs AES/blowfish ciphers not
        // in the dep graph; errors honestly (queued with the
        // pgcrypto epic) instead of producing fake ciphertext.
        "pgp_sym_encrypt" | "pgp_sym_decrypt" | "pgp_sym_encrypt_bytea"
        | "pgp_sym_decrypt_bytea" | "pgp_pub_encrypt" | "pgp_pub_decrypt"
        | "pgp_pub_encrypt_bytea" | "pgp_pub_decrypt_bytea" => {
            Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "{name}(): PGP encryption not yet supported — \
                     AES/blowfish ciphers queue with the pgcrypto epic"
                ),
            })
        }
        // pgp_key_id(bytea) — key-ID extraction from a PGP packet.
        "pgp_key_id" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — convert_from / convert_to
        // handle text ↔ bytea encoding conversion. SPG stores text as
        // UTF-8; UTF8 / SQL_ASCII therefore just reinterpret the bytes,
        // while LATIN1 (ISO-8859-1) really transcodes — every byte maps
        // 1:1 to U+0000..=U+00FF, so the conversion is exact.
        //
        //   convert_from(bytea, 'UTF8')   → text
        //   convert_to(text, 'UTF8')      → bytea
        //   convert_from(bytea, 'LATIN1') → text (byte b → codepoint b)
        //   convert_to(text, 'LATIN1')    → bytea (codepoint ≤ 0xFF)
        //
        // Any other encoding name errors; the wider transcoding matrix
        // queues with the collation/encoding epic.
        "convert_from" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "convert_from() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let b = match &args[0] {
                Value::Bytes(b) => b,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "convert_from(): needs bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let enc = match &args[1] {
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "convert_from(): encoding must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let enc_up = enc.to_ascii_uppercase();
            if enc_up != "UTF8"
                && enc_up != "UTF-8"
                && enc_up != "SQL_ASCII"
                && enc_up != "LATIN1"
                && !is_win1252(&enc_up)
            {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "convert_from(): unsupported encoding {enc:?} — SPG stores UTF-8 only; use UTF8 / SQL_ASCII / LATIN1 / WIN1252"
                    ),
                });
            }
            let s = if enc_up == "LATIN1" {
                // ISO-8859-1: each byte is exactly its own codepoint.
                b.iter().map(|&byte| byte as char).collect::<alloc::string::String>()
            } else if is_win1252(&enc_up) {
                // Windows-1252: identity except the remapped 0x80–0x9F range.
                let mut out = alloc::string::String::with_capacity(b.len());
                for &byte in b.iter() {
                    match win1252_byte_to_char(byte) {
                        Some(c) => out.push(c),
                        None => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "convert_from(): byte {byte:#04x} is not defined in encoding WIN1252"
                                ),
                            });
                        }
                    }
                }
                out
            } else {
                // UTF8 / SQL_ASCII: the bytes are already UTF-8.
                match core::str::from_utf8(b) {
                    Ok(v) => alloc::string::String::from(v),
                    Err(e) => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "convert_from(): input is not valid UTF-8: {e}"
                            ),
                        });
                    }
                }
            };
            Ok(Value::text(s))
        }
        "convert_to" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("convert_to() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let s = match &args[0] {
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "convert_to(): needs text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let enc = match &args[1] {
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "convert_to(): encoding must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let enc_up = enc.to_ascii_uppercase();
            if enc_up != "UTF8"
                && enc_up != "UTF-8"
                && enc_up != "SQL_ASCII"
                && enc_up != "LATIN1"
                && !is_win1252(&enc_up)
            {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "convert_to(): unsupported encoding {enc:?} — SPG stores UTF-8 only; use UTF8 / SQL_ASCII / LATIN1 / WIN1252"
                    ),
                });
            }
            let bytes = if is_win1252(&enc_up) {
                // UTF-8 text → Windows-1252 bytes.
                let mut out = alloc::vec::Vec::with_capacity(s.len());
                for ch in s.chars() {
                    match win1252_char_to_byte(ch) {
                        Some(byte) => out.push(byte),
                        None => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "convert_to(): character {ch:?} has no equivalent in encoding WIN1252"
                                ),
                            });
                        }
                    }
                }
                out
            } else if enc_up == "LATIN1" {
                // UTF-8 text → ISO-8859-1 bytes. A codepoint above U+00FF
                // has no LATIN1 equivalent, which PG reports as an error.
                let mut out = alloc::vec::Vec::with_capacity(s.len());
                for ch in s.chars() {
                    let cp = ch as u32;
                    if cp <= 0xFF {
                        out.push(cp as u8);
                    } else {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "convert_to(): character {ch:?} has no equivalent in encoding LATIN1"
                            ),
                        });
                    }
                }
                out
            } else {
                s.into_bytes()
            };
            Ok(Value::Bytes(bytes.into()))
        }
        "error_on_null" => error_on_null(args),
        // v7.12.1 — PG full-text search lexer / tsquery builders.
        // mailrs G-CRIT-3 acceptance path: `to_tsvector('english',
        // … || ' ' || … || …)` runs end-to-end against a tsvector
        // column with Porter stemming + standard english stopwords.
        "to_tsvector" => fts_to_tsvector(args, ctx),
        // v7.24 (round-16 C) — setweight(tsvector, 'A'..'D'): label
        // every lexeme. mailrs's migrate-016 search trigger builds
        // its vector as setweight(to_tsvector(…),'A') || ….
        "setweight" => fts_setweight(args),
        // v7.37.17 (17.6 siblings) — ts_headline([config,] doc,
        // query [, options]): wrap matched words in StartSel/StopSel.
        "ts_headline" => fts_ts_headline(args, ctx),
        // v7.37.17 (17.6 siblings) — ts_rewrite(query, target,
        // substitute): synonym-expansion subtree rewrite.
        "ts_rewrite" => fts_ts_rewrite(args, ctx),
        // v7.37.17 (17.6 siblings) — the boolean catalog forms of
        // the && / || / !! tsquery operators.
        "tsquery_and" => fts_tsquery_bool(args, ctx, "and"),
        "tsquery_or" => fts_tsquery_bool(args, ctx, "or"),
        "tsquery_not" => fts_tsquery_bool(args, ctx, "not"),
        // v7.24 (round-15) — string_to_array(text, delim): inverse
        // of array_to_string. PG semantics: NULL text → NULL,
        // '' → empty array, NULL delim → one element per char.
        "string_to_array" => fn_string_to_array(args),
        // v7.37.17 (17.6 siblings) — PG's built-in
        // `array_to_string(arr, delimiter [, null_string])` joins
        // array elements with delimiter; NULL elements are dropped
        // unless null_string is given (then they're replaced).
        "array_to_string" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_to_string() takes 2 or 3 args, got {}",
                        args.len()
                    ),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            let delim = match &args[1] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "array_to_string(): delimiter must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let null_replacement = match args.get(2) {
                None | Some(Value::Null) => None,
                Some(Value::Text(s)) => Some(s.to_string()),
                Some(other) => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "array_to_string(): null_string must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let pieces: alloc::vec::Vec<alloc::string::String> = match &args[0] {
                Value::IntArray(items) => items
                    .iter()
                    .filter_map(|o| match o {
                        Some(n) => Some(alloc::format!("{n}")),
                        None => null_replacement.clone(),
                    })
                    .collect(),
                Value::BigIntArray(items) => items
                    .iter()
                    .filter_map(|o| match o {
                        Some(n) => Some(alloc::format!("{n}")),
                        None => null_replacement.clone(),
                    })
                    .collect(),
                Value::TextArray(items) => items
                    .iter()
                    .filter_map(|o| match o {
                        Some(s) => Some(s.clone()),
                        None => null_replacement.clone(),
                    })
                    .collect(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "array_to_string(): first arg must be array, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            Ok(Value::text(pieces.join(&delim)))
        }
        "plainto_tsquery" => fts_plainto_tsquery(args, ctx),
        "phraseto_tsquery" => fts_phraseto_tsquery(args, ctx),
        "websearch_to_tsquery" => fts_websearch_to_tsquery(args, ctx),
        "to_tsquery" => fts_to_tsquery(args, ctx),
        // v7.12.2 — ranking functions. mailrs's fallback search
        // query ORDERs BY ts_rank(search_vector, q) DESC.
        "ts_rank" => fts_ts_rank(args),
        // v7.37.17 (17.6 siblings) — index-maintenance probes.
        // GIN / BRIN maintenance functions emitted by autovacuum
        // scripts + cron cleanups. Return 0 (count of pages/rows
        // processed) so scripts complete cleanly.
        "gin_clean_pending_list"
        | "brin_summarize_new_values" => Ok(Value::BigInt(0)),
        // brin_summarize_range / brin_desummarize_range are void.
        "brin_summarize_range" | "brin_desummarize_range" => Ok(Value::Null),
        // amvalidate(oid) — validates an access-method operator
        // class. Always true for SPG's builtin BTree.
        "amvalidate" => Ok(Value::Bool(true)),
        // gin/gist support probes.
        "gin_cmp_tslexeme" | "gin_compare_jsonb" => Ok(Value::Int(0)),
        // v7.37.17 (17.6 siblings) — FTS introspection helpers.
        // strip(tsvector) removes position/weight info; SPG's
        // tsvector text form is 'word:pos' pairs — strip drops
        // the :pos suffixes.
        "strip" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("strip() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let stripped: alloc::vec::Vec<alloc::string::String> = s
                        .split_whitespace()
                        .map(|lexeme| {
                            // Keep only up to the first ':'.
                            match lexeme.split_once(':') {
                                Some((word, _)) => word.into(),
                                None => lexeme.into(),
                            }
                        })
                        .collect();
                    Ok(Value::text(stripped.join(" ")))
                }
                // v7.38 (read01 P6.30) — strip a real tsvector value: drop
                // every lexeme's positions and weight, keeping just the words.
                Value::TsVector(lexemes) => {
                    let stripped: alloc::vec::Vec<spg_storage::TsLexeme> = lexemes
                        .iter()
                        .map(|l| spg_storage::TsLexeme {
                            word: l.word.clone(),
                            positions: alloc::vec::Vec::new(),
                            weight: 0,
                        })
                        .collect();
                    Ok(Value::TsVector(stripped))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "strip() needs tsvector text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // tsvector_length / length(tsvector) — count distinct
        // lexemes. SPG's length() handles text; this named form
        // counts whitespace-separated lexemes.
        "tsvector_length" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "tsvector_length() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::TsVector(lexemes) => {
                    Ok(Value::Int(i32::try_from(lexemes.len()).unwrap_or(i32::MAX)))
                }
                Value::Text(s) => {
                    Ok(Value::Int(s.split_whitespace().count() as i32))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "tsvector_length() needs tsvector text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // numnode(tsquery) — count lexeme + operator nodes.
        // Approximation: count words + explicit operators.
        "numnode" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("numnode() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                // v7.38 (read01 P6.31) — a real tsquery value: count each
                // lexeme and each operator node exactly (PG's numnode).
                Value::TsQuery(ast) => Ok(Value::Int(count_tsquery_nodes(ast))),
                Value::Text(s) => {
                    let words = s
                        .split(|c: char| c.is_whitespace() || c == '&' || c == '|' || c == '!' || c == '(' || c == ')')
                        .filter(|w| !w.is_empty())
                        .count();
                    let ops = s.chars().filter(|c| matches!(c, '&' | '|' | '!')).count();
                    Ok(Value::Int((words + ops) as i32))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "numnode() needs tsquery text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // querytree(tsquery) — the indexable part of the query.
        // SPG's simple form returns the input minus negated terms.
        "querytree" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("querytree() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                // v7.38 (read01 P6.31) — a real tsquery value: return its
                // GIN-indexable part (PG's querytree), formatted as text.
                Value::TsQuery(ast) => match querytree_indexable(ast) {
                    Some(q) => Ok(Value::text(super::textsearch::format_tsquery(&q))),
                    None => Ok(Value::text::<String>("T".into())),
                },
                Value::Text(s) => {
                    // Drop '!term' segments.
                    let kept: alloc::vec::Vec<&str> = s
                        .split_whitespace()
                        .filter(|w| !w.starts_with('!'))
                        .collect();
                    if kept.is_empty() {
                        Ok(Value::text::<String>("T".into()))
                    } else {
                        Ok(Value::text(kept.join(" ")))
                    }
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "querytree() needs tsquery text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "ts_rank_cd" => fts_ts_rank_cd(args),
        // ts_delete(tsvector, lexeme) / ts_delete(tsvector, text[])
        // — remove the given lexeme(s). SPG tsvector text form is
        // 'word:pos' pairs; match on the word part.
        "ts_delete" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "ts_delete() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            let targets: alloc::vec::Vec<alloc::string::String> = match &args[1]
            {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => alloc::vec![s.as_ref().into()],
                Value::TextArray(items) => {
                    items.iter().flatten().cloned().collect()
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "ts_delete() needs lexeme text or text[], got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            // v7.38 (read01 P6.30) — accept a real tsvector value (filter its
            // lexemes) as well as the text form.
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::TsVector(lexemes) => {
                    let kept: alloc::vec::Vec<spg_storage::TsLexeme> = lexemes
                        .iter()
                        .filter(|l| !targets.iter().any(|t| t.as_str() == l.word.as_str()))
                        .cloned()
                        .collect();
                    Ok(Value::TsVector(kept))
                }
                Value::Text(s) => {
                    let kept: alloc::vec::Vec<&str> = s
                        .split_whitespace()
                        .filter(|lexeme| {
                            let word = match lexeme.split_once(':') {
                                Some((w, _)) => w,
                                None => lexeme,
                            };
                            !targets.iter().any(|t| t == word)
                        })
                        .collect();
                    Ok(Value::text(kept.join(" ")))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "ts_delete() needs tsvector text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // ts_filter(tsvector, weights) — keep only lexemes that have
        // at least one position tagged with one of the given weight
        // letters (A/B/C/D). Weights arrive as text like '{a,b}' or
        // a TextArray.
        "ts_filter" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "ts_filter() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            let vec_text = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "ts_filter() needs tsvector text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let weights: alloc::vec::Vec<char> = match &args[1] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s
                    .chars()
                    .filter(|c| c.is_ascii_alphabetic())
                    .map(|c| c.to_ascii_uppercase())
                    .collect(),
                Value::TextArray(items) => items
                    .iter()
                    .flatten()
                    .filter_map(|s| s.chars().next())
                    .map(|c| c.to_ascii_uppercase())
                    .collect(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "ts_filter() needs weight char[], got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if weights.iter().any(|c| !matches!(c, 'A'..='D')) {
                return Err(EvalError::TypeMismatch {
                    detail: "ts_filter(): weights must be A, B, C or D".into(),
                });
            }
            let kept: alloc::vec::Vec<&str> = vec_text
                .split_whitespace()
                .filter(|lexeme| match lexeme.split_once(':') {
                    Some((_, positions)) => {
                        positions.split(',').any(|pos| {
                            match pos.chars().last() {
                                Some(c) if c.is_ascii_alphabetic() => weights
                                    .contains(&c.to_ascii_uppercase()),
                                // Unweighted position = weight D.
                                _ => weights.contains(&'D'),
                            }
                        })
                    }
                    // No positions at all = weight D.
                    None => weights.contains(&'D'),
                })
                .collect();
            Ok(Value::text(kept.join(" ")))
        }
        // tsquery_phrase(q1, q2[, distance]) — combine two tsqueries
        // into a phrase query: q1 <-> q2, or q1 <N> q2.
        // tsvector_to_array(tsvector) — lexemes as text[], positions
        // dropped. PG returns them sorted; SPG's tsvector text form
        // is already sorted at construction, but sort defensively so
        // hand-written literals round-trip like PG.
        "tsvector_to_array" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "tsvector_to_array() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let mut words: alloc::vec::Vec<alloc::string::String> = s
                        .split_whitespace()
                        .map(|lexeme| match lexeme.split_once(':') {
                            Some((word, _)) => word.into(),
                            None => lexeme.into(),
                        })
                        .collect();
                    words.sort();
                    words.dedup();
                    Ok(Value::TextArray(
                        words.into_iter().map(Some).collect(),
                    ))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "tsvector_to_array() needs tsvector text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // array_to_tsvector(text[]) — lexemes joined into a
        // position-less tsvector. PG sorts + dedups and rejects
        // NULL elements.
        "array_to_tsvector" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_to_tsvector() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::TextArray(items) => {
                    let mut words: alloc::vec::Vec<alloc::string::String> =
                        alloc::vec::Vec::with_capacity(items.len());
                    for item in items {
                        match item {
                            Some(w) => words.push(w.clone()),
                            None => {
                                return Err(EvalError::TypeMismatch {
                                    detail:
                                        "array_to_tsvector(): lexeme array may not contain nulls"
                                            .into(),
                                });
                            }
                        }
                    }
                    words.sort();
                    words.dedup();
                    Ok(Value::text(words.join(" ")))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "array_to_tsvector() needs text[], got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // get_current_ts_config() — the default_text_search_config
        // GUC. SPG's FTS pipeline is the PG 'english' simple path.
        "get_current_ts_config" => {
            Ok(Value::text::<String>("english".into()))
        }
        // ts_lexize(dict, token) — run one token through a
        // dictionary. SPG ships the 'simple' dictionary semantics:
        // lowercase the token, return it as a 1-element array.
        "ts_lexize" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "ts_lexize() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(_dict), Value::Text(token)) => {
                    Ok(Value::TextArray(alloc::vec![Some(
                        token.to_lowercase()
                    )]))
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: "ts_lexize() takes 2 TEXT args".into(),
                }),
            }
        }
        "tsquery_phrase" => {
            if args.len() != 2 && args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "tsquery_phrase() takes 2 or 3 args, got {}",
                        args.len()
                    ),
                });
            }
            let q1 = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "tsquery_phrase() needs tsquery text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let q2 = match &args[1] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "tsquery_phrase() needs tsquery text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let distance = match args.get(2) {
                None => 1i64,
                Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Int(n)) => i64::from(*n),
                Some(Value::SmallInt(n)) => i64::from(*n),
                Some(Value::BigInt(n)) => *n,
                Some(other) => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "tsquery_phrase() distance must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let joined = if distance == 1 {
                alloc::format!("{q1} <-> {q2}")
            } else {
                alloc::format!("{q1} <{distance}> {q2}")
            };
            Ok(Value::text(joined))
        }
        // v7.14.0 — PG dump preamble emits
        // `SELECT pg_catalog.set_config('search_path', '', false);`
        // and friends. SPG is single-schema; accept-as-no-op
        // returning either the new value or NULL.
        "set_config" => Ok(args
            .get(1)
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null)),
        // v7.37.17 (17.6 siblings) — current_setting(name [, missing_ok])
        // returns the current value of a GUC. Real widening: for
        // the driver-probed defaults callers rely on, return
        // sensible PG defaults instead of empty text. missing_ok
        // is honored — when true, unknown params return NULL
        // (matches PG); when false / omitted, error but we're
        // permissive and return empty text so probes complete.
        "current_setting" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "current_setting() takes 1 or 2 args, got {}",
                        args.len()
                    ),
                });
            }
            let name = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "current_setting(): needs text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let missing_ok = if args.len() == 2 {
                matches!(&args[1], Value::Bool(true))
            } else {
                false
            };
            let lname = name.to_ascii_lowercase();
            // A value written with SET / set_config wins over the static
            // default (PG: `SET application_name = 'x'` then
            // current_setting('application_name') → 'x'). Custom
            // namespaced GUCs (`app.user_id`) only live here.
            if let Some(gucs) = ctx.session_gucs
                && let Some(v) = gucs.get(lname.as_str())
            {
                return Ok(Value::text(v.clone()));
            }
            let val = match lname.as_str() {
                "server_version" => "18.4 (SPG-compat)",
                "server_version_num" => "180004",
                "server_encoding" => "UTF8",
                "client_encoding" => "UTF8",
                "lc_collate" => "C.UTF-8",
                "lc_ctype" => "C.UTF-8",
                "lc_messages" => "C",
                "lc_monetary" => "C",
                "lc_numeric" => "C",
                "lc_time" => "C",
                "timezone" | "time zone" => "UTC",
                "datestyle" | "date_style" => "ISO, MDY",
                "intervalstyle" | "interval_style" => "postgres",
                "search_path" => "\"$user\", public",
                "default_transaction_isolation" => "read committed",
                "transaction_isolation" => "read committed",
                "standard_conforming_strings" => "on",
                "check_function_bodies" => "on",
                "row_security" => "on",
                "bytea_output" => "hex",
                "xmloption" => "content",
                "application_name" => "",
                "backslash_quote" => "safe_encoding",
                _ => {
                    if missing_ok {
                        return Ok(Value::Null);
                    }
                    ""
                }
            };
            Ok(Value::text::<String>(val.into()))
        }
        // PG `pg_catalog.*` discovery / cast helpers commonly
        // emitted by ORMs probing the server. Accept-as-no-op
        // with sensible defaults so the dump preamble doesn't
        // fail. `pg_get_serial_sequence` returns NULL (no
        // sequence — SPG has AUTO_INCREMENT instead).
        // pg_get_indexdef(index [, col, pretty]) — REAL: rebuilt
        // from live catalog state, same construction the pg_indexes
        // view uses. Name input (regclass text) resolves; numeric
        // oids can't map (synthetic) → NULL. The 3-arg column form
        // returns the indexed column's name when col > 0.
        "pg_get_indexdef" => {
            let Some(cat) = ctx.catalog else {
                return Ok(Value::Null);
            };
            // Resolve a pg_class/pg_index OID to the index name by
            // replaying synth_pg_index_raw's assignment (100000 + the
            // running index count over table_names()), so the reverse
            // tracks the forward exactly. A name arg is used directly.
            let resolved: String;
            let name_arg: &str = match args.first() {
                None | Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Text(s)) => s.as_ref(),
                Some(Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_)) => {
                    let oid = match &args[0] {
                        Value::Int(n) => i64::from(*n),
                        Value::BigInt(n) => *n,
                        Value::SmallInt(n) => i64::from(*n),
                        _ => unreachable!(),
                    };
                    let mut counter = 100_000i64;
                    let mut found = None;
                    'find: for tname in cat.table_names() {
                        let Some(t) = cat.get(&tname) else { continue };
                        for idx in t.indices() {
                            counter += 1;
                            if counter == oid {
                                found = Some(idx.name.clone());
                                break 'find;
                            }
                        }
                    }
                    let Some(nm) = found else {
                        return Ok(Value::Null);
                    };
                    resolved = nm;
                    resolved.as_str()
                }
                Some(_) => return Ok(Value::Null),
            };
            let bare = name_arg
                .strip_prefix("public.")
                .unwrap_or(name_arg)
                .trim_matches('"');
            let col_no = match args.get(1) {
                Some(Value::Int(n)) => i64::from(*n),
                Some(Value::BigInt(n)) => *n,
                Some(Value::SmallInt(n)) => i64::from(*n),
                _ => 0,
            };
            for tname in cat.table_names() {
                let Some(t) = cat.get(&tname) else { continue };
                for idx in t.indices() {
                    if idx.name != bare {
                        continue;
                    }
                    let col_at = |pos: usize| -> String {
                        t.schema()
                            .columns
                            .get(pos)
                            .map_or_else(|| String::from("?"), |c| c.name.clone())
                    };
                    // Full ordered column list (leading + trailing keys).
                    let mut positions = alloc::vec![idx.column_position];
                    positions.extend(idx.extra_column_positions.iter().copied());
                    let col_names: Vec<String> = positions.iter().map(|&p| col_at(p)).collect();
                    if col_no > 0 {
                        // Column form: the N-th (1-based) key column.
                        return Ok(col_names.get((col_no - 1) as usize).map_or(
                            Value::Null,
                            |c| Value::text(c.clone()),
                        ));
                    }
                    let unique_kw = if idx.is_unique { "UNIQUE " } else { "" };
                    return Ok(Value::text(alloc::format!(
                        "CREATE {unique_kw}INDEX {} ON public.{} USING btree ({})",
                        idx.name,
                        tname,
                        col_names.join(", ")
                    )));
                }
            }
            Ok(Value::Null)
        }
        // pg_get_constraintdef(conname [, pretty]) — REAL for
        // PK / UNIQUE / FK: rebuilt from live catalog state using
        // the same naming convention synth_pg_constraint emits
        // ({t}_pkey / {t}_uniq{i} / fk.name-or-{t}_fk{i}).
        // Numeric-oid input can't map (synthetic) → NULL.
        "pg_get_constraintdef" => {
            let Some(cat) = ctx.catalog else {
                return Ok(Value::Null);
            };
            // The name is either given directly (Text) or reached through
            // a pg_constraint OID. SPG has no OID space, so resolve the
            // OID against the pg_constraint synth view itself — its OID
            // assignment is the single source of truth, so the reverse
            // can never drift from the forward.
            let resolved: String;
            let name_arg: &str = match args.first() {
                None | Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Text(s)) => s.as_ref(),
                Some(Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_)) => {
                    let oid = match &args[0] {
                        Value::Int(n) => i64::from(*n),
                        Value::BigInt(n) => *n,
                        Value::SmallInt(n) => i64::from(*n),
                        _ => unreachable!(),
                    };
                    let (_, rows) = crate::system_catalog::synth_pg_constraint(cat);
                    let Some(row) = rows
                        .iter()
                        .find(|r| matches!(r.values.first(), Some(Value::BigInt(n)) if *n == oid))
                    else {
                        return Ok(Value::Null);
                    };
                    match row.values.get(1) {
                        Some(Value::Text(s)) => resolved = s.to_string(),
                        _ => return Ok(Value::Null),
                    }
                    resolved.as_str()
                }
                Some(_) => return Ok(Value::Null),
            };
            let bare = name_arg.trim_matches('"');
            for tname in cat.table_names() {
                let Some(t) = cat.get(&tname) else { continue };
                let cols = &t.schema().columns;
                let col_name_at = |pos: usize| -> String {
                    cols.get(pos).map_or_else(
                        || alloc::format!("col{pos}"),
                        |c| c.name.clone(),
                    )
                };
                for uc in t.schema().uniqueness_constraints.iter() {
                    let names: alloc::vec::Vec<String> =
                        uc.columns.iter().map(|&p| col_name_at(p)).collect();
                    // Match synth_pg_constraint's PG-style auto-names:
                    // `{t}_pkey` / `{t}_{col…}_key`.
                    let conname = if uc.is_primary_key {
                        alloc::format!("{tname}_pkey")
                    } else {
                        alloc::format!("{tname}_{}_key", names.join("_"))
                    };
                    if conname != bare {
                        continue;
                    }
                    let kw = if uc.is_primary_key {
                        "PRIMARY KEY"
                    } else {
                        "UNIQUE"
                    };
                    return Ok(Value::text(alloc::format!(
                        "{kw} ({})",
                        names.join(", ")
                    )));
                }
                for (fi, fk) in t.schema().foreign_keys.iter().enumerate() {
                    let conname = fk
                        .name
                        .clone()
                        .unwrap_or_else(|| alloc::format!("{tname}_fk{fi}"));
                    if conname != bare {
                        continue;
                    }
                    let local: alloc::vec::Vec<String> = fk
                        .local_columns
                        .iter()
                        .map(|&p| col_name_at(p))
                        .collect();
                    let parent_names: alloc::vec::Vec<String> =
                        match ctx.catalog.and_then(|c| c.get(&fk.parent_table))
                        {
                            Some(parent) => fk
                                .parent_columns
                                .iter()
                                .map(|&p| {
                                    parent.schema().columns.get(p).map_or_else(
                                        || alloc::format!("col{p}"),
                                        |c| c.name.clone(),
                                    )
                                })
                                .collect(),
                            None => fk
                                .parent_columns
                                .iter()
                                .map(|p| alloc::format!("col{p}"))
                                .collect(),
                        };
                    let mut def = alloc::format!(
                        "FOREIGN KEY ({}) REFERENCES {}({})",
                        local.join(", "),
                        fk.parent_table,
                        parent_names.join(", ")
                    );
                    use spg_storage::FkAction;
                    // SPG stores Restrict as the unspecified-action
                    // default (PG's default is NO ACTION, omitted in
                    // its deparse). SPG can't distinguish a declared
                    // RESTRICT from the default → omit both, so the
                    // common case round-trips like PG.
                    let action_word = |a: FkAction| match a {
                        FkAction::Cascade => Some("CASCADE"),
                        FkAction::SetNull => Some("SET NULL"),
                        FkAction::SetDefault => Some("SET DEFAULT"),
                        FkAction::Restrict | FkAction::NoAction => None,
                    };
                    if let Some(w) = action_word(fk.on_delete) {
                        def.push_str(&alloc::format!(" ON DELETE {w}"));
                    }
                    if let Some(w) = action_word(fk.on_update) {
                        def.push_str(&alloc::format!(" ON UPDATE {w}"));
                    }
                    return Ok(Value::text(def));
                }
                // CHECK constraints — mirror synth_pg_constraint's PG-style
                // naming (`{t}_{col}_check` / `{t}_check`). PG wraps the
                // predicate in an outer `CHECK (…)`.
                let check_names =
                    crate::system_catalog::pg_check_connames(t, &tname, &t.schema().checks);
                for (ci, pred) in t.schema().checks.iter().enumerate() {
                    if check_names.get(ci).map(String::as_str) != Some(bare) {
                        continue;
                    }
                    let inner = pred.trim();
                    let body = if inner.starts_with('(') && inner.ends_with(')') {
                        inner.to_string()
                    } else {
                        alloc::format!("({inner})")
                    };
                    return Ok(Value::text(alloc::format!("CHECK ({body})")));
                }
                // NOT NULL constraints (PG 18) — `{t}_{col}_not_null` for
                // every NOT NULL column (incl. implicit-from-PK).
                let pk_cols: alloc::collections::BTreeSet<usize> = t
                    .schema()
                    .uniqueness_constraints
                    .iter()
                    .filter(|uc| uc.is_primary_key)
                    .flat_map(|uc| uc.columns.iter().copied())
                    .collect();
                for (i, col) in cols.iter().enumerate() {
                    if col.nullable && !pk_cols.contains(&i) {
                        continue;
                    }
                    if alloc::format!("{tname}_{}_not_null", col.name) == bare {
                        return Ok(Value::text(alloc::format!("NOT NULL {}", col.name)));
                    }
                }
            }
            Ok(Value::Null)
        }
        // v7.37.17 (17.6 siblings) — pg_get_serial_sequence returns
        // the OID name of the underlying sequence for an implicit
        // SERIAL / BIGSERIAL column. ORMs (SQLAlchemy, Django,
        // ActiveRecord) call it to detect auto-increment columns.
        // Real impl: parse `(schema.)?table` + column string, and
        // synthesize the PG-conventional sequence name form
        // `public.table_column_seq`. SPG uses AUTO_INCREMENT rather
        // than named sequences, so this returns a synthetic name
        // that at least never NULLs out.
        "pg_get_serial_sequence" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "pg_get_serial_sequence() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            let table = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_get_serial_sequence(): table must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let col = match &args[1] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_get_serial_sequence(): column must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            // Strip any leading "schema." from table part.
            let table_short = table
                .rsplit_once('.')
                .map(|(_schema, t)| t)
                .unwrap_or(&table);
            Ok(Value::text(alloc::format!(
                "public.{table_short}_{col}_seq"
            )))
        }
        // v7.37.17 (17.6 siblings) — additional pg_catalog probe
        // helpers that ORMs / migration tools emit. All return
        // NULL / empty text where PG would return real DDL text.
        // Full DDL reconstruction queues with the get_ddl surface
        // in v7.40 that reuses spgctl's DDL round-tripper.
        // pg_get_viewdef(view [, pretty]) — REAL: SPG's catalog
        // keeps every view's SQL body; look it up by name (regclass
        // text form; 'public.' qualification stripped). The pretty
        // flag is accepted and ignored — SPG returns the body as
        // written. Numeric-oid input can't map (synthetic oids) →
        // NULL.
        "pg_get_viewdef" => {
            let name_arg = match args.first() {
                None | Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Text(s)) => s.as_ref(),
                Some(_) => return Ok(Value::Null),
            };
            let Some(cat) = ctx.catalog else {
                return Ok(Value::Null);
            };
            let bare = name_arg
                .strip_prefix("public.")
                .unwrap_or(name_arg)
                .trim_matches('"');
            match cat.views().get(bare) {
                Some(def) => Ok(Value::text(def.body.clone())),
                None => Ok(Value::Null),
            }
        }
        "pg_get_functiondef"
        | "pg_get_triggerdef"
        | "pg_get_ruledef"
        | "pg_get_expr"
        | "pg_get_partkeydef"
        | "pg_get_statisticsobjdef" => Ok(Value::Null),
        // pg_get_userbyid always returns "admin" — SPG's single-user
        // model; matches CURRENT_USER default.
        "pg_get_userbyid" => Ok(Value::text::<String>("admin".into())),
        // pg_size_pretty(bigint) — commonly used by monitoring
        // queries; format a byte count as a human-readable string.
        // For now return empty text so the SELECT succeeds; real
        // formatting queues with a size-utils bump.
        // v7.37.17 (17.6 siblings) — real pg_size_pretty(bigint).
        // Convert byte count into a human-readable string using
        // 1024-boundaries. Matches PG's decision-point table:
        //   [0, 10 KB)     → "N bytes"
        //   [10 KB, 10 MB) → "N kB"
        //   [10 MB, 10 GB) → "N MB"
        //   [10 GB, 10 TB) → "N GB"
        //   [10 TB, ∞)     → "N TB"
        // Rounds to nearest integer at each unit boundary.
        "pg_size_pretty" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "pg_size_pretty() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            let n: i64 = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::SmallInt(x) => i64::from(*x),
                Value::Int(x) => i64::from(*x),
                Value::BigInt(x) => *x,
                Value::Numeric { scaled, scale } => {
                    let ten_pow = 10i128.pow(*scale as u32);
                    (*scaled as i128 / ten_pow) as i64
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_size_pretty(): needs numeric, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let (mut val, mut unit) = (n as f64, "bytes");
            const KB: f64 = 1024.0;
            const CROSSOVER: f64 = 10.0 * KB; // 10 kB threshold
            if val.abs() >= CROSSOVER {
                val /= KB;
                unit = "kB";
                if val.abs() >= CROSSOVER {
                    val /= KB;
                    unit = "MB";
                    if val.abs() >= CROSSOVER {
                        val /= KB;
                        unit = "GB";
                        if val.abs() >= CROSSOVER {
                            val /= KB;
                            unit = "TB";
                            if val.abs() >= CROSSOVER {
                                val /= KB;
                                unit = "PB";
                            }
                        }
                    }
                }
            }
            let s = if unit == "bytes" {
                alloc::format!("{n} bytes")
            } else {
                alloc::format!("{} {unit}", val.round() as i64)
            };
            Ok(Value::text(s))
        }
        // pg_database_size / pg_relation_size / pg_total_relation_size:
        // monitoring dashboards + Postgres exporter emit these.
        // pg_relation_size family — REAL from the storage layer's
        // maintained hot-tier byte meter (Table::hot_bytes, the
        // same counter the freezer budgets against) + the index
        // resident-byte walkers. Cold-tier on-disk accounting joins
        // in v7.38; hot bytes are the live footprint and strictly
        // better than the old constant 0.
        "pg_relation_size"
        | "pg_table_size"
        | "pg_total_relation_size"
        | "pg_indexes_size" => {
            let name_arg = match args.first() {
                None | Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Text(s)) => s.as_ref(),
                // Numeric oid — synthetic, no reverse map.
                Some(_) => return Ok(Value::BigInt(0)),
            };
            let Some(cat) = ctx.catalog else {
                return Ok(Value::BigInt(0));
            };
            let bare = name_arg
                .strip_prefix("public.")
                .unwrap_or(name_arg)
                .trim_matches('"');
            let Some(t) = cat.get(bare) else {
                return Ok(Value::Null);
            };
            let heap = t.hot_bytes() as i64;
            let idx: i64 = t
                .indices()
                .iter()
                .map(|i| i.kind.approx_resident_bytes() as i64)
                .sum();
            Ok(Value::BigInt(match name {
                "pg_relation_size" | "pg_table_size" => heap,
                "pg_indexes_size" => idx,
                _ => heap + idx, // pg_total_relation_size
            }))
        }
        // pg_database_size — the whole hot tier (same meter the
        // SPG_HOT_TIER_BYTES freezer budget reads).
        "pg_database_size" => {
            let Some(cat) = ctx.catalog else {
                return Ok(Value::BigInt(0));
            };
            Ok(Value::BigInt(cat.hot_tier_bytes() as i64))
        }
        // pg_encoding_to_char / pg_char_to_encoding — the encoding
        // lookup pair. SPG always speaks UTF8 (encoding id 6).
        "pg_encoding_to_char" => Ok(Value::text::<String>("UTF8".into())),
        "pg_char_to_encoding" => Ok(Value::Int(6)),
        // has_table_privilege / has_column_privilege / has_schema_privilege:
        // permission probes ORMs emit before generating DDL. SPG
        // is single-user, so grant everything.
        "has_table_privilege"
        | "has_column_privilege"
        | "has_schema_privilege"
        | "has_function_privilege"
        | "has_sequence_privilege"
        | "has_database_privilege"
        | "has_language_privilege"
        | "has_tablespace_privilege"
        | "has_type_privilege"
        // v7.37.17 (17.6 siblings) — round out the privilege
        // family. `has_any_column_privilege(obj, priv)` is a
        // sibling of has_column_privilege used by ORMs to detect
        // "can the current role touch this table at all?".
        // `has_server_privilege` / `has_foreign_data_wrapper_privilege`
        // are for foreign-data callers; SPG has no FDW yet so
        // return true. `has_parameter_privilege` (PG 15+) probes
        // ALTER SYSTEM permission on a GUC. `pg_has_role` is the
        // role-membership check used by RBAC-aware tooling.
        | "has_any_column_privilege"
        | "has_server_privilege"
        | "has_foreign_data_wrapper_privilege"
        | "has_parameter_privilege"
        | "pg_has_role" => Ok(Value::Bool(true)),
        // pg_backend_pid — session identifier. SPG uses u64 slot
        // ids; return a low deterministic value for embedded runs.
        "pg_backend_pid" => Ok(Value::Int(1)),
        // v7.37.17 (17.6 siblings) — PG 16+ system_user() returns
        // the authenticated identity in "auth_method:user_name"
        // form (e.g. "cert:alice", "scram-sha-256:bob"). SPG has
        // no wire-level auth surface yet; return a stable
        // "trust:admin" placeholder so callers get a non-NULL
        // formatted value.
        "system_user" => Ok(Value::text::<String>("trust:admin".into())),
        // current_query() / pg_stat_get_backend_activity — the SQL
        // string of the currently-executing query. SPG doesn't
        // expose the executing text back to itself; return empty
        // text. pg_dump/pg_dumpall probe this for lockcheck info.
        "current_query" | "pg_current_query" => {
            Ok(Value::text::<String>(String::new()))
        }
        // pg_column_summary — pg_stats-family helper. Return NULL
        // until real per-column stats land with the statistics epic.
        "pg_column_summary" => Ok(Value::Null),
        // pg_conf_load_time / pg_postmaster_start_time — return
        // process start time in the embedded case; wire-layer has
        // real timestamps.
        // v7.37.17 (17.6 siblings) — return a fixed 2020-01-01 UTC
        // timestamp so monitoring exporters that emit these get a
        // valid timestamp (real per-session/per-cluster start time
        // threads with v7.38 wall-clock plumbing).
        "pg_conf_load_time"
        | "pg_postmaster_start_time"
        | "pg_backend_start_time"
        | "pg_stat_get_backend_start" => {
            const ANCHOR_2020_UTC: i64 = 1_577_836_800_000_000;
            Ok(Value::Timestamp(ANCHOR_2020_UTC))
        }
        // pg_stat_get_backend_activity/state/wait_event etc. —
        // per-backend probes. Return NULL until v7.38 backend
        // tracking.
        "pg_stat_get_backend_activity_start"
        | "pg_stat_get_backend_client_addr"
        | "pg_stat_get_backend_client_port"
        | "pg_stat_get_backend_dbid"
        | "pg_stat_get_backend_pid"
        | "pg_stat_get_backend_userid"
        | "pg_stat_get_backend_wait_event"
        | "pg_stat_get_backend_wait_event_type"
        | "pg_stat_get_backend_xact_start"
        | "pg_stat_get_backend_subxact"
        | "pg_stat_get_backend_idset" => Ok(Value::Null),
        // pg_notify(channel, payload) — LISTEN/NOTIFY delivery.
        // SPG has no async notification channel yet; accept + return
        // void (NULL).
        "pg_notify" => Ok(Value::Null),
        // information_schema._pg_* internal helpers — SQLAlchemy,
        // asyncpg and JDBC's DatabaseMetaData introspection queries
        // call these. The typmod math is real (PG's atttypmod
        // encoding: varchar/bpchar typmod = len + 4; numeric
        // typmod = ((precision << 16) | scale) + 4).
        "_pg_char_max_length" | "_pg_char_octet_length" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 2 args, got {}", args.len()),
                });
            }
            let int_of = |v: &Value<'_>| -> Option<i64> {
                match v {
                    Value::Int(n) => Some(i64::from(*n)),
                    Value::BigInt(n) => Some(*n),
                    Value::SmallInt(n) => Some(i64::from(*n)),
                    _ => None,
                }
            };
            match (int_of(&args[0]), int_of(&args[1])) {
                (Some(typid), Some(typmod)) => {
                    // 1043 varchar, 1042 bpchar.
                    if (typid == 1043 || typid == 1042) && typmod >= 4 {
                        let len = typmod - 4;
                        if name == "_pg_char_octet_length" {
                            // Worst-case UTF-8 expansion, like PG.
                            Ok(Value::Int((len * 4) as i32))
                        } else {
                            Ok(Value::Int(len as i32))
                        }
                    } else {
                        Ok(Value::Null)
                    }
                }
                _ => Ok(Value::Null),
            }
        }
        "_pg_numeric_precision" | "_pg_numeric_scale" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 2 args, got {}", args.len()),
                });
            }
            let int_of = |v: &Value<'_>| -> Option<i64> {
                match v {
                    Value::Int(n) => Some(i64::from(*n)),
                    Value::BigInt(n) => Some(*n),
                    Value::SmallInt(n) => Some(i64::from(*n)),
                    _ => None,
                }
            };
            match (int_of(&args[0]), int_of(&args[1])) {
                (Some(typid), Some(typmod)) => match typid {
                    // Fixed-precision integer types.
                    21 if name == "_pg_numeric_precision" => Ok(Value::Int(16)),
                    23 if name == "_pg_numeric_precision" => Ok(Value::Int(32)),
                    20 if name == "_pg_numeric_precision" => Ok(Value::Int(64)),
                    21 | 23 | 20 => Ok(Value::Int(0)),
                    // 1700 numeric: typmod packs (precision, scale).
                    1700 if typmod >= 4 => {
                        let packed = typmod - 4;
                        if name == "_pg_numeric_precision" {
                            Ok(Value::Int(((packed >> 16) & 0xFFFF) as i32))
                        } else {
                            Ok(Value::Int((packed & 0xFFFF) as i32))
                        }
                    }
                    _ => Ok(Value::Null),
                },
                _ => Ok(Value::Null),
            }
        }
        "_pg_datetime_precision" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 2 args, got {}", args.len()),
                });
            }
            let int_of = |v: &Value<'_>| -> Option<i64> {
                match v {
                    Value::Int(n) => Some(i64::from(*n)),
                    Value::BigInt(n) => Some(*n),
                    Value::SmallInt(n) => Some(i64::from(*n)),
                    _ => None,
                }
            };
            match (int_of(&args[0]), int_of(&args[1])) {
                (Some(typid), Some(typmod)) => match typid {
                    // 1082 date has no fractional seconds.
                    1082 => Ok(Value::Int(0)),
                    // time/timestamp family: default precision 6,
                    // explicit typmod overrides.
                    1083 | 1114 | 1184 | 1266 => {
                        if typmod < 0 {
                            Ok(Value::Int(6))
                        } else {
                            Ok(Value::Int(typmod as i32))
                        }
                    }
                    _ => Ok(Value::Null),
                },
                _ => Ok(Value::Null),
            }
        }
        // Record-consuming internals — _pg_expandarray is an SRF
        // (SQLAlchemy walks index columns through it); truetypid /
        // truetypmod take pg_attribute+pg_type records. NULL keeps
        // the introspection queries parseable.
        "_pg_expandarray"
        | "_pg_index_position"
        | "_pg_truetypid"
        | "_pg_truetypmod"
        | "_pg_interval_type" => Ok(Value::Null),
        // Updatability probes — information_schema.views'
        // is_updatable/is_insertable_into and psql \d+ both test
        // pg_relation_is_updatable's event bitmask (8=INSERT,
        // 4=UPDATE, 16=DELETE → 28 = fully updatable). SPG tables
        // are always updatable; views are too (the v7.37.19 auto-
        // updatable redirect handles INSERT/UPDATE/DELETE);
        // missing relation → 0.
        "pg_relation_is_updatable" => {
            let bare_owned;
            let bare: &str = match args.first() {
                None | Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Text(s)) => {
                    bare_owned = s
                        .strip_prefix("public.")
                        .unwrap_or(s)
                        .trim_matches('"')
                        .to_string();
                    &bare_owned
                }
                // Numeric oid input — assume a known relation
                // (synthetic oids have no reverse map).
                Some(_) => return Ok(Value::Int(28)),
            };
            let Some(cat) = ctx.catalog else {
                return Ok(Value::Int(0));
            };
            let known = cat.get(bare).is_some()
                || cat.views().contains_key(bare);
            Ok(Value::Int(if known { 28 } else { 0 }))
        }
        // pg_column_is_updatable(rel, attnum, include_triggers) —
        // per-column form; SPG has no generated/identity columns
        // blocking updates, so it mirrors the relation answer.
        "pg_column_is_updatable" => {
            let bare_owned;
            let bare: &str = match args.first() {
                None | Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Text(s)) => {
                    bare_owned = s
                        .strip_prefix("public.")
                        .unwrap_or(s)
                        .trim_matches('"')
                        .to_string();
                    &bare_owned
                }
                Some(_) => return Ok(Value::Bool(true)),
            };
            let Some(cat) = ctx.catalog else {
                return Ok(Value::Bool(false));
            };
            let known = cat.get(bare).is_some()
                || cat.views().contains_key(bare);
            Ok(Value::Bool(known))
        }
        // row_security_active(rel) — SPG has no row-level security.
        "row_security_active" => match args.first() {
            None | Some(Value::Null) => Ok(Value::Null),
            _ => Ok(Value::Bool(false)),
        },
        // getdatabaseencoding() — SPG is UTF-8-only, like
        // pg_client_encoding / pg_encoding_to_char.
        "getdatabaseencoding" => Ok(Value::text::<String>("UTF8".into())),
        // current_schemas(include_implicit) — the effective search
        // path. SPG is single-schema 'public'; with the implicit
        // flag PG prepends pg_catalog.
        "current_schemas" => {
            let include_implicit = match args.first() {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(other) => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "current_schemas() takes a BOOL, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let mut schemas: alloc::vec::Vec<Option<alloc::string::String>> =
                alloc::vec::Vec::new();
            if include_implicit {
                schemas.push(Some("pg_catalog".into()));
            }
            schemas.push(Some("public".into()));
            Ok(Value::TextArray(schemas))
        }
        // pg_trigger_depth() — nesting level of trigger execution.
        // SPG's trigger surface doesn't re-enter; top level = 0.
        "pg_trigger_depth" => Ok(Value::Int(0)),
        // pg_jit_available() — SPG has no LLVM JIT.
        "pg_jit_available" => Ok(Value::Bool(false)),
        // pg_listening_channels() — LISTEN registrations (SRF).
        // Scalar surface: NULL (no channels).
        "pg_listening_channels" => Ok(Value::Null),
        // Event-trigger context readers — only meaningful inside a
        // running event trigger; PG errors outside that context,
        // SPG returns NULL to stay parse-through for tooling.
        "pg_event_trigger_ddl_commands"
        | "pg_event_trigger_dropped_objects"
        | "pg_event_trigger_table_rewrite_oid"
        | "pg_event_trigger_table_rewrite_reason" => Ok(Value::Null),
        // pg_cancel_backend / pg_terminate_backend — admin-level
        // signal helpers. Return true (as if the cancel took effect).
        "pg_cancel_backend" | "pg_terminate_backend" => Ok(Value::Bool(true)),
        // v7.37.17 (17.6 siblings) — string / catalog helpers.
        // quote_ident wraps a bare identifier in "…" if it needs
        // quoting; quote_literal / quote_nullable wrap string values
        // in '…' (or NULL for null). Basic implementations that
        // don't check the identifier-safe character class — always
        // quote to be safe.
        "quote_ident" => match args.first() {
            Some(Value::Text(s)) => Ok(Value::text(pg_quote_ident(s))),
            Some(Value::Null) | None => Ok(Value::Null),
            Some(other) => Ok(Value::text(pg_quote_ident(&value_to_format_text(other)))),
        },
        // quote_literal / quote_nullable are the anyelement→text cast
        // followed by literal-quoting, so non-text values render as PG's
        // `::text` cast would (bool → true/false, not the t/f wire form;
        // numbers/date/timestamp via the canonical renderer) rather than
        // leaking a Rust debug dump. pg_quote_literal handles the E'…'
        // backslash-escape form for both.
        "quote_literal" => match args.first() {
            Some(Value::Null) | None => Ok(Value::Null),
            Some(Value::Text(s)) => Ok(Value::text(pg_quote_literal(s))),
            Some(Value::Bool(b)) => {
                Ok(Value::text(pg_quote_literal(if *b { "true" } else { "false" })))
            }
            Some(other) => Ok(Value::text(pg_quote_literal(&value_to_format_text(other)))),
        },
        "quote_nullable" => match args.first() {
            None | Some(Value::Null) => Ok(Value::text::<String>("NULL".into())),
            Some(Value::Text(s)) => Ok(Value::text(pg_quote_literal(s))),
            Some(Value::Bool(b)) => {
                Ok(Value::text(pg_quote_literal(if *b { "true" } else { "false" })))
            }
            Some(other) => Ok(Value::text(pg_quote_literal(&value_to_format_text(other)))),
        },
        // format_type(type_oid [, typmod]) — REAL: canonical SQL
        // display names for the pg_type oid map SPG ships, with
        // PG's typmod rendering (varchar(n), numeric(p,s),
        // timestamp(n)). Unknown oid → PG returns "???"; SPG
        // matches. NULL oid → NULL.
        "format_type" => {
            // An `_`-prefixed internal name (`_int4`, `_text`) is an array
            // type: render the element spelling followed by `[]` (PG:
            // `integer[]`, `text[]`).
            let mut is_array = false;
            let oid = match args.first() {
                None | Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Int(n)) => i64::from(*n),
                Some(Value::BigInt(n)) => *n,
                Some(Value::SmallInt(n)) => i64::from(*n),
                // SPG's `::regtype` yields the type NAME (no OID space),
                // so format_type('int4'::regtype, …) arrives as Text.
                // Map the internal / SQL spelling back to an OID and reuse
                // the OID path (which also renders the typmod).
                Some(Value::Text(s)) => {
                    let raw = s.trim().trim_start_matches("pg_catalog.").to_ascii_lowercase();
                    let name = match raw.strip_prefix('_') {
                        Some(elem) => {
                            is_array = true;
                            alloc::string::String::from(elem)
                        }
                        None => raw,
                    };
                    match name.as_str() {
                        "bool" | "boolean" => 16,
                        "bytea" => 17,
                        "\"char\"" | "char" => 18,
                        "name" => 19,
                        "int8" | "bigint" => 20,
                        "int2" | "smallint" => 21,
                        "int4" | "integer" | "int" => 23,
                        "text" => 25,
                        "oid" => 26,
                        "json" => 114,
                        "xml" => 142,
                        "float4" | "real" => 700,
                        "float8" | "double precision" => 701,
                        "cidr" => 650,
                        "inet" => 869,
                        "macaddr" => 829,
                        "macaddr8" => 774,
                        "money" => 790,
                        "bpchar" | "character" => 1042,
                        "varchar" | "character varying" => 1043,
                        "date" => 1082,
                        "time" | "time without time zone" => 1083,
                        "timestamp" | "timestamp without time zone" => 1114,
                        "timestamptz" | "timestamp with time zone" => 1184,
                        "interval" => 1186,
                        "timetz" | "time with time zone" => 1266,
                        "numeric" | "decimal" => 1700,
                        "uuid" => 2950,
                        "jsonb" => 3802,
                        "tsvector" => 3614,
                        "tsquery" => 3615,
                        "int4range" => 3904,
                        "numrange" => 3906,
                        "tstzrange" => 3908,
                        "tsrange" => 3910,
                        "daterange" => 3912,
                        "int8range" => 3926,
                        _ => {
                            return Ok(Value::text(if is_array {
                                alloc::format!("{name}[]")
                            } else {
                                name
                            }));
                        }
                    }
                }
                Some(_) => return Ok(Value::Null),
            };
            let typmod = match args.get(1) {
                Some(Value::Int(n)) => i64::from(*n),
                Some(Value::BigInt(n)) => *n,
                Some(Value::SmallInt(n)) => i64::from(*n),
                _ => -1,
            };
            // PG's deparse names (SQL-standard spellings, not the
            // internal typname) — shared with the `::regtype` cast.
            let Some(base) = crate::conversions::regtype_oid_to_name(oid) else {
                return Ok(Value::text::<String>("???".into()));
            };
            let rendered = if typmod >= 4 {
                match oid {
                    1042 | 1043 => {
                        alloc::format!("{base}({})", typmod - 4)
                    }
                    1700 => {
                        let packed = typmod - 4;
                        alloc::format!(
                            "{base}({},{})",
                            (packed >> 16) & 0xFFFF,
                            packed & 0xFFFF
                        )
                    }
                    _ => base.into(),
                }
            } else if typmod >= 0
                && matches!(oid, 1083 | 1114 | 1184 | 1266)
            {
                // time/timestamp precision: 'timestamp(3) without
                // time zone' — the precision goes before the tz
                // qualifier.
                match base.split_once(' ') {
                    Some((head, tail)) => {
                        alloc::format!("{head}({typmod}) {tail}")
                    }
                    None => alloc::format!("{base}({typmod})"),
                }
            } else {
                base.into()
            };
            Ok(Value::text(if is_array {
                alloc::format!("{rendered}[]")
            } else {
                rendered
            }))
        }
        // obj_description / col_description / shobj_description —
        // COMMENT ON reader helpers. SPG doesn't yet retain
        // comments in the catalog; return NULL.
        "obj_description" | "col_description" | "shobj_description" => Ok(Value::Null),
        // acldefault(objtype, owner_oid) — the default ACL for an
        // object type, PG text form '{owner=privs/owner}'. SPG's
        // single-role model maps every oid to 'admin'
        // (pg_get_userbyid parity). Privilege strings per PG:
        //   r relation=arwdDxt  s sequence=rwU  f function=X
        //   d database=CTc     n schema=UC     L language=U
        //   t tablespace=C     T type=U        F FDW=U
        //   S server=U
        "acldefault" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "acldefault() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            let objtype = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.as_ref().to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "acldefault() objtype must be \"char\", got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            let privs = match objtype.as_str() {
                "r" => "arwdDxt",
                "s" => "rwU",
                "f" => "X",
                "d" => "CTc",
                "n" => "UC",
                "L" => "U",
                "t" => "C",
                "T" => "U",
                "F" => "U",
                "S" => "U",
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "acldefault(): unrecognized object type {other:?}"
                        ),
                    });
                }
            };
            Ok(Value::TextArray(alloc::vec![Some(alloc::format!(
                "admin={privs}/admin"
            ))]))
        }
        // makeaclitem(grantee_oid, grantor_oid, privileges, grantable)
        // — construct one aclitem in PG's text form. Grantee oid 0 =
        // PUBLIC (empty name before '='); '*' suffix per privilege
        // when grantable. SPG's single-role model names every
        // non-zero oid 'admin'.
        "makeaclitem" => {
            if args.len() != 4 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "makeaclitem() takes 4 args, got {}",
                        args.len()
                    ),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let grantee_is_public = match &args[0] {
                Value::Int(n) => *n == 0,
                Value::BigInt(n) => *n == 0,
                Value::SmallInt(n) => *n == 0,
                _ => false,
            };
            let privileges = match &args[2] {
                Value::Text(s) => s.as_ref().to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "makeaclitem() privileges must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let grantable = matches!(&args[3], Value::Bool(true));
            let mut letters = alloc::string::String::new();
            for piece in privileges.split(',') {
                let letter = match piece.trim().to_ascii_uppercase().as_str() {
                    "SELECT" => 'r',
                    "INSERT" => 'a',
                    "UPDATE" => 'w',
                    "DELETE" => 'd',
                    "TRUNCATE" => 'D',
                    "REFERENCES" => 'x',
                    "TRIGGER" => 't',
                    "EXECUTE" => 'X',
                    "USAGE" => 'U',
                    "CREATE" => 'C',
                    "CONNECT" => 'c',
                    "TEMPORARY" | "TEMP" => 'T',
                    "MAINTAIN" => 'm',
                    "SET" => 's',
                    "ALTER SYSTEM" => 'A',
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "makeaclitem(): unrecognized privilege {other:?}"
                            ),
                        });
                    }
                };
                letters.push(letter);
                if grantable {
                    letters.push('*');
                }
            }
            let grantee = if grantee_is_public { "" } else { "admin" };
            Ok(Value::text(alloc::format!("{grantee}={letters}/admin")))
        }
        // Object-addressing probes — record-returning identity
        // resolvers used by dependency tooling (pg_depend joins).
        // SPG's catalog doesn't retain PG's classid/objid address
        // space; NULL keeps the queries parse-through.
        "pg_describe_object"
        | "pg_identify_object"
        | "pg_identify_object_as_address"
        | "pg_get_object_address" => Ok(Value::Null),
        // to_regclass(name) — REAL: resolves a table/view name to
        // its oid using the same synthetic map pg_class synthesizes
        // (16384 + position in table_names order), NULL when the
        // relation doesn't exist. The dominant caller shape is the
        // existence check Django/Alembic emit:
        //   SELECT to_regclass('tbl') IS NOT NULL
        "to_regclass" => {
            let name_arg = match args.first() {
                None | Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Text(s)) => s.as_ref(),
                Some(_) => return Ok(Value::Null),
            };
            let Some(cat) = ctx.catalog else {
                return Ok(Value::Null);
            };
            let bare = name_arg
                .strip_prefix("public.")
                .unwrap_or(name_arg)
                .trim_matches('"');
            for (pos, tname) in cat.table_names().iter().enumerate() {
                if tname == bare {
                    return Ok(Value::BigInt(16384 + pos as i64));
                }
            }
            // Views resolve too (regclass covers all relations);
            // their oid band sits above the table band.
            for (pos, (vname, _)) in cat.views().iter().enumerate() {
                if vname == bare {
                    return Ok(Value::BigInt(32768 + pos as i64));
                }
            }
            Ok(Value::Null)
        }
        // to_regtype(name) — REAL for the builtin scalar map (the
        // same 38 names format_type renders); NULL for unknown.
        "to_regtype" => {
            let name_arg = match args.first() {
                None | Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Text(s)) => s.to_lowercase(),
                Some(_) => return Ok(Value::Null),
            };
            let oid: Option<i64> = match name_arg.trim() {
                "bool" | "boolean" => Some(16),
                "bytea" => Some(17),
                "name" => Some(19),
                "int8" | "bigint" => Some(20),
                "int2" | "smallint" => Some(21),
                "int4" | "int" | "integer" => Some(23),
                "text" => Some(25),
                "oid" => Some(26),
                "json" => Some(114),
                "xml" => Some(142),
                "float4" | "real" => Some(700),
                "float8" | "double precision" => Some(701),
                "cidr" => Some(650),
                "inet" => Some(869),
                "macaddr" => Some(829),
                "macaddr8" => Some(774),
                "money" => Some(790),
                "bpchar" | "char" | "character" => Some(1042),
                "varchar" | "character varying" => Some(1043),
                "date" => Some(1082),
                "time" | "time without time zone" => Some(1083),
                "timestamp" | "timestamp without time zone" => Some(1114),
                "timestamptz" | "timestamp with time zone" => Some(1184),
                "interval" => Some(1186),
                "timetz" | "time with time zone" => Some(1266),
                "numeric" | "decimal" => Some(1700),
                "uuid" => Some(2950),
                "jsonb" => Some(3802),
                "tsvector" => Some(3614),
                "tsquery" => Some(3615),
                _ => None,
            };
            Ok(oid.map_or(Value::Null, Value::BigInt))
        }
        // to_regnamespace — 'public' and 'pg_catalog' exist.
        "to_regnamespace" => match args.first() {
            None | Some(Value::Null) => Ok(Value::Null),
            Some(Value::Text(s)) => match s.as_ref() {
                "public" => Ok(Value::BigInt(2200)),
                "pg_catalog" => Ok(Value::BigInt(11)),
                "information_schema" => Ok(Value::BigInt(13000)),
                _ => Ok(Value::Null),
            },
            Some(_) => Ok(Value::Null),
        },
        // Function/operator/role resolvers — the oid spaces queue
        // with system_catalog v7.40 widening.
        "to_regproc"
        | "to_regprocedure"
        | "to_regoperator"
        | "to_regrole" => Ok(Value::Null),
        // pg_client_encoding — SPG always speaks UTF8.
        "pg_client_encoding" => Ok(Value::text::<String>("UTF8".into())),
        // pg_is_in_recovery / pg_is_wal_replay_paused — replication /
        // recovery status probes. SPG is primary-only in the drop-in
        // model.
        "pg_is_in_recovery" | "pg_is_wal_replay_paused" => Ok(Value::Bool(false)),
        // v7.37.17 (17.6 siblings) — PG 13+ normalize(text [, form])
        // + is_normalized(text [, form]). Real Unicode normalization
        // via the unicode-normalization crate. Forms: NFC (default),
        // NFD, NFKC, NFKD.
        "normalize" | "is_normalized" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 or 2 args, got {}", args.len()),
                });
            }
            let s = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() needs text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let form = if args.len() == 2 {
                match &args[1] {
                    Value::Null => return Ok(Value::Null),
                    Value::Text(f) => f.to_ascii_uppercase(),
                    _ => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "{name}(): form must be text"
                            ),
                        });
                    }
                }
            } else {
                alloc::string::String::from("NFC")
            };
            use unicode_normalization::UnicodeNormalization;
            let normalized: alloc::string::String = match form.as_str() {
                "NFC" => s.nfc().collect(),
                "NFD" => s.nfd().collect(),
                "NFKC" => s.nfkc().collect(),
                "NFKD" => s.nfkd().collect(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}(): unknown form {other:?}; use NFC/NFD/NFKC/NFKD"
                        ),
                    });
                }
            };
            if name == "normalize" {
                Ok(Value::text(normalized))
            } else {
                Ok(Value::Bool(normalized == s))
            }
        }
        // to_ascii(text) — strip accents, keeping the base letters.
        // PG restricts this to LATIN1/LATIN2/LATIN9/WIN1250 source
        // encodings; SPG operates on the already-decoded string:
        // NFD-decompose, drop combining marks, keep the rest.
        "to_ascii" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "to_ascii() takes 1 or 2 args, got {}",
                        args.len()
                    ),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    use unicode_normalization::UnicodeNormalization;
                    let stripped: alloc::string::String = s
                        .nfd()
                        .filter(|c| {
                            // Combining marks occupy U+0300..=U+036F
                            // (plus extended blocks).
                            !matches!(*c, '\u{0300}'..='\u{036F}'
                                | '\u{1AB0}'..='\u{1AFF}'
                                | '\u{1DC0}'..='\u{1DFF}'
                                | '\u{20D0}'..='\u{20FF}')
                        })
                        .collect();
                    Ok(Value::text(stripped))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "to_ascii() needs text, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // cash_words(money) — spell out an amount in English words,
        // PG shape: 'One hundred fourteen dollars and six cents'.
        // Accepts numeric-ish input (SPG stores money as numeric).
        "cash_words" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "cash_words() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            // Normalize the input to (dollars: i64, cents: i64).
            let (dollars, cents, negative) = match &args[0] {
                Value::Null => return Ok(Value::Null),
                // v7.38 (read01 P6.33) — the money type is i64 cents; split
                // into dollars + cents so `cash_words(x::money)` works (was
                // rejected as an unknown type).
                Value::Money(c) => {
                    let neg = *c < 0;
                    let abs = c.unsigned_abs();
                    ((abs / 100) as i64, (abs % 100) as i64, neg)
                }
                Value::Int(n) => (i64::from(n.unsigned_abs()), 0, *n < 0),
                Value::SmallInt(n) => (i64::from(n.unsigned_abs()), 0, *n < 0),
                Value::BigInt(n) => (n.wrapping_abs(), 0, *n < 0),
                Value::Float(f) => {
                    let neg = *f < 0.0;
                    let abs = if neg { -*f } else { *f };
                    let total_cents = (abs * 100.0 + 0.5) as i64;
                    (total_cents / 100, total_cents % 100, neg)
                }
                Value::Numeric { scaled, scale } => {
                    let neg = *scaled < 0;
                    let abs = scaled.unsigned_abs();
                    let pow = 10u128.pow(u32::from(*scale));
                    let whole = (abs / pow) as i64;
                    let frac = abs % pow;
                    // Rescale the fraction to cents.
                    let cents = if *scale >= 2 {
                        (frac / 10u128.pow(u32::from(*scale) - 2)) as i64
                    } else {
                        (frac * 10u128.pow(2 - u32::from(*scale))) as i64
                    };
                    (whole, cents, neg)
                }
                Value::Text(s) => {
                    // Money text form like '$1,234.56' or '1.23'.
                    let cleaned: alloc::string::String = s
                        .chars()
                        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
                        .collect();
                    let neg = cleaned.starts_with('-');
                    let cleaned = cleaned.trim_start_matches('-');
                    let (whole_s, frac_s) =
                        cleaned.split_once('.').unwrap_or((cleaned, ""));
                    let whole: i64 = whole_s.parse().unwrap_or(0);
                    let mut frac_2 = alloc::string::String::from(frac_s);
                    frac_2.truncate(2);
                    while frac_2.len() < 2 {
                        frac_2.push('0');
                    }
                    let cents: i64 = frac_2.parse().unwrap_or(0);
                    (whole, cents, neg)
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "cash_words() needs money/numeric, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            fn three_digits(n: i64, out: &mut alloc::string::String) {
                const ONES: [&str; 20] = [
                    "zero", "one", "two", "three", "four", "five", "six",
                    "seven", "eight", "nine", "ten", "eleven", "twelve",
                    "thirteen", "fourteen", "fifteen", "sixteen",
                    "seventeen", "eighteen", "nineteen",
                ];
                const TENS: [&str; 10] = [
                    "", "", "twenty", "thirty", "forty", "fifty", "sixty",
                    "seventy", "eighty", "ninety",
                ];
                let mut n = n;
                if n >= 100 {
                    out.push_str(ONES[(n / 100) as usize]);
                    out.push_str(" hundred");
                    n %= 100;
                    if n > 0 {
                        out.push(' ');
                    }
                }
                if n >= 20 {
                    out.push_str(TENS[(n / 10) as usize]);
                    n %= 10;
                    if n > 0 {
                        out.push(' ');
                        out.push_str(ONES[n as usize]);
                    }
                } else if n > 0 {
                    out.push_str(ONES[n as usize]);
                }
            }
            fn spell(n: i64) -> alloc::string::String {
                if n == 0 {
                    return "zero".into();
                }
                const GROUPS: [&str; 7] = [
                    "", " thousand", " million", " billion", " trillion",
                    " quadrillion", " quintillion",
                ];
                // Split into groups of three digits, most significant
                // first.
                let mut parts: alloc::vec::Vec<(i64, usize)> =
                    alloc::vec::Vec::new();
                let mut rem = n;
                let mut group = 0usize;
                while rem > 0 {
                    let chunk = rem % 1000;
                    if chunk > 0 {
                        parts.push((chunk, group));
                    }
                    rem /= 1000;
                    group += 1;
                }
                let mut out = alloc::string::String::new();
                for (i, (chunk, group)) in parts.iter().rev().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    three_digits(*chunk, &mut out);
                    out.push_str(GROUPS[*group]);
                }
                out
            }
            let mut words = alloc::string::String::new();
            if negative {
                words.push_str("minus ");
            }
            words.push_str(&spell(dollars));
            words.push_str(if dollars == 1 { " dollar" } else { " dollars" });
            words.push_str(" and ");
            words.push_str(&spell(cents));
            words.push_str(if cents == 1 { " cent" } else { " cents" });
            // PG capitalizes the first letter.
            let mut chars = words.chars();
            let capitalized: alloc::string::String = match chars.next() {
                Some(c) => c.to_uppercase().chain(chars).collect(),
                None => words,
            };
            Ok(Value::text(capitalized))
        }
        // v7.37.17 (17.6 siblings) — PG 13+ numeric scale helpers.
        //   scale(numeric)      — declared scale of the value
        //   min_scale(numeric)  — minimum scale to represent exactly
        //   trim_scale(numeric) — value with trailing zeroes removed
        "scale" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("scale() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Numeric { scale, .. } => Ok(Value::Int(i32::from(*scale))),
                Value::Int(_) | Value::SmallInt(_) | Value::BigInt(_) => {
                    Ok(Value::Int(0))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "scale() needs numeric, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "min_scale" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("min_scale() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Numeric { scaled, scale } => {
                    // Strip trailing zeroes off the scaled integer to
                    // find the minimal scale.
                    let mut s = *scale as i32;
                    let mut v = *scaled;
                    while s > 0 && v % 10 == 0 {
                        v /= 10;
                        s -= 1;
                    }
                    Ok(Value::Int(s))
                }
                Value::Int(_) | Value::SmallInt(_) | Value::BigInt(_) => {
                    Ok(Value::Int(0))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "min_scale() needs numeric, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "trim_scale" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("trim_scale() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Numeric { scaled, scale } => {
                    let mut s = *scale;
                    let mut v = *scaled;
                    while s > 0 && v % 10 == 0 {
                        v /= 10;
                        s -= 1;
                    }
                    Ok(Value::Numeric { scaled: v, scale: s })
                }
                v @ (Value::Int(_) | Value::SmallInt(_) | Value::BigInt(_)) => {
                    Ok(v.clone().into_owned())
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "trim_scale() needs numeric, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — PG 9.6+ num_nulls / num_nonnulls
        // variadic helpers. Count NULL / non-NULL args. Common in
        // CHECK constraints validating "exactly one of these
        // columns is set".
        "num_nulls" => {
            let n = args.iter().filter(|v| matches!(v, Value::Null)).count();
            Ok(Value::Int(n as i32))
        }
        "num_nonnulls" => {
            let n = args.iter().filter(|v| !matches!(v, Value::Null)).count();
            Ok(Value::Int(n as i32))
        }
        // v7.37.17 (17.6 siblings) — pg_lsn operator support fns.
        // These accept text-form LSN strings (SPG doesn't have a
        // pg_lsn type yet; text is the wire format PG uses too).
        "pg_lsn_larger" | "pg_lsn_smaller" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 2 args, got {}", args.len()),
                });
            }
            fn as_lsn(v: &Value<'_>) -> Result<Option<i64>, EvalError> {
                let s = match v {
                    Value::Null => return Ok(None),
                    Value::Text(s) => s.to_string(),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "pg_lsn_*(): args must be text, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                };
                let (hi, lo) = s.split_once('/').ok_or_else(|| {
                    EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_lsn_*(): bad LSN {s:?}"
                        ),
                    }
                })?;
                let hi_u = u32::from_str_radix(hi, 16).map_err(|_| {
                    EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_lsn_*(): bad LSN high {hi:?}"
                        ),
                    }
                })?;
                let lo_u = u32::from_str_radix(lo, 16).map_err(|_| {
                    EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_lsn_*(): bad LSN low {lo:?}"
                        ),
                    }
                })?;
                Ok(Some(((hi_u as i64) << 32) | lo_u as i64))
            }
            let a = match as_lsn(&args[0])? {
                None => return Ok(Value::Null),
                Some(v) => v,
            };
            let b = match as_lsn(&args[1])? {
                None => return Ok(Value::Null),
                Some(v) => v,
            };
            let picked = if name == "pg_lsn_larger" {
                a.max(b)
            } else {
                a.min(b)
            };
            let hi = ((picked >> 32) & 0xFFFF_FFFF) as u32;
            let lo = (picked & 0xFFFF_FFFF) as u32;
            Ok(Value::text::<String>(alloc::format!("{hi:X}/{lo:X}")))
        }
        // pg_lsn_hash returns a hash for the LSN (for hash indexes).
        "pg_lsn_hash" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("pg_lsn_hash() takes 1 arg, got {}", args.len()),
                });
            }
            let s = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_lsn_hash(): needs text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            // Same FNV-1a hasher as hashtext.
            let mut h: u32 = 0x811c_9dc5;
            for b in s.as_bytes() {
                h ^= *b as u32;
                h = h.wrapping_mul(0x0100_0193);
            }
            Ok(Value::Int(h as i32))
        }
        // v7.37.17 (17.6 siblings) — SPG-specific introspection.
        // These aren't PG functions but SPG operators emit them
        // from spgctl / monitoring dashboards.
        "spg_version" => Ok(Value::text::<String>(
            alloc::format!("SPG {}", env!("CARGO_PKG_VERSION")),
        )),
        "spg_build_time" => {
            // Static build-time embedding.
            Ok(Value::text::<String>("2026-07".into()))
        }
        "spg_edition" => {
            Ok(Value::text::<String>("embedded".into()))
        }
        "spg_uptime_seconds" => {
            // Wall-clock uptime queues with v7.38 wall-clock
            // plumbing; return 0 for now.
            Ok(Value::BigInt(0))
        }
        // Alias set for PG-name compat.
        "pg_current_edition" => {
            Ok(Value::text::<String>("SPG-compat".into()))
        }
        // v7.37.17 (17.6 siblings) — pg_stat_statements support
        // functions. Callers emit these to reset stats + probe
        // the module's status. SETOF probes → NULL, reset ops
        // → NULL (void).
        "pg_stat_statements_info"
        | "pg_stat_statements_reset"
        | "pg_stat_statements_reset_shared_memory_stats" => Ok(Value::Null),
        // pg_stat_statements returns a large row per statement.
        // Scalar-surface NULL.
        "pg_stat_statements" => Ok(Value::Null),
        // pg_get_shmem_allocations — shared-memory allocation view
        // helper.
        "pg_get_shmem_allocations" => Ok(Value::Null),
        // pg_config — returns the compile-time config settings.
        // Already handled above via NULL; keep alt name.
        "pg_config_env" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — extension introspection.
        // These SETOF functions are used by psql \dx + ORMs to
        // discover available extensions. Scalar-surface NULL so
        // scalar-context callers get parse-through.
        "pg_available_extensions"
        | "pg_available_extension_versions"
        | "pg_extension_update_paths"
        | "pg_extension_config_dump"
        | "pg_visible_in_snapshot_txid" => Ok(Value::Null),
        // pg_load_extension is used by CREATE EXTENSION machinery.
        // Return void.
        "pg_load_extension" => Ok(Value::Null),
        // pg_extension_check_version returns the installed version
        // of an extension by name. SPG has no extensions yet.
        "pg_extension_check_version" | "extension_version" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — bytea bitwise ops. Not built
        // into PG (bit strings have varbit ops) but common in
        // custom crypto/token pipelines that emit bytea XOR for
        // simple stream-cipher / mask ops.
        "bytea_xor" | "byteaxor" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "bytea_xor() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Bytes(a), Value::Bytes(b)) => {
                    if a.len() != b.len() {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "bytea_xor(): length mismatch {} vs {}",
                                a.len(),
                                b.len()
                            ),
                        });
                    }
                    let out: alloc::vec::Vec<u8> = a
                        .iter()
                        .zip(b.iter())
                        .map(|(x, y)| x ^ y)
                        .collect();
                    Ok(Value::Bytes(out.into()))
                }
                (a, b) => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "bytea_xor(): needs (bytea, bytea), got ({:?}, {:?})",
                        a.data_type(),
                        b.data_type()
                    ),
                }),
            }
        }
        // bytea_and / bytea_or — similar shape.
        "bytea_and" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("bytea_and() takes 2 args, got {}", args.len()),
                });
            }
            match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Bytes(a), Value::Bytes(b)) => {
                    if a.len() != b.len() {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "bytea_and(): length mismatch {} vs {}",
                                a.len(),
                                b.len()
                            ),
                        });
                    }
                    let out: alloc::vec::Vec<u8> = a
                        .iter()
                        .zip(b.iter())
                        .map(|(x, y)| x & y)
                        .collect();
                    Ok(Value::Bytes(out.into()))
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: "bytea_and(): needs (bytea, bytea)".into(),
                }),
            }
        }
        "bytea_or" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("bytea_or() takes 2 args, got {}", args.len()),
                });
            }
            match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Bytes(a), Value::Bytes(b)) => {
                    if a.len() != b.len() {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "bytea_or(): length mismatch {} vs {}",
                                a.len(),
                                b.len()
                            ),
                        });
                    }
                    let out: alloc::vec::Vec<u8> = a
                        .iter()
                        .zip(b.iter())
                        .map(|(x, y)| x | y)
                        .collect();
                    Ok(Value::Bytes(out.into()))
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: "bytea_or(): needs (bytea, bytea)".into(),
                }),
            }
        }
        // v7.37.17 (17.6 siblings) — CRC32 / CRC32C (Castagnoli).
        // Not built into PG but common in dblink/foreign-data
        // migration scripts and audit-log query helpers. Real
        // implementation via table-free polynomial.
        "crc32" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("crc32() takes 1 arg, got {}", args.len()),
                });
            }
            let bytes: &[u8] = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.as_bytes(),
                Value::Bytes(b) => b.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "crc32() needs text or bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            // IEEE 802.3 polynomial 0xEDB88320.
            let mut crc: u32 = 0xFFFF_FFFF;
            for byte in bytes.iter() {
                crc ^= *byte as u32;
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xEDB8_8320 & (0u32.wrapping_sub(crc & 1)));
                }
            }
            Ok(Value::BigInt(!crc as i64))
        }
        "crc32c" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("crc32c() takes 1 arg, got {}", args.len()),
                });
            }
            let bytes: &[u8] = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.as_bytes(),
                Value::Bytes(b) => b.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "crc32c() needs text or bytea, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            // Castagnoli polynomial 0x82F63B78.
            let mut crc: u32 = 0xFFFF_FFFF;
            for byte in bytes.iter() {
                crc ^= *byte as u32;
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0x82F6_3B78 & (0u32.wrapping_sub(crc & 1)));
                }
            }
            Ok(Value::BigInt(!crc as i64))
        }
        // v7.37.17 (17.6 siblings) — pg_stat_get_all_indexes /
        // _all_sequences family. Aggregate per-relation stats used
        // by monitoring exporters — return 0 counter.
        "pg_stat_get_last_scan"
        | "pg_stat_get_last_idx_scan"
        | "pg_stat_get_lastscan"
        | "pg_stat_get_lastidxscan" => Ok(Value::Null),
        // pg_stat_get_backend_userid variant (with real userid arg).
        // Already covered above; keep alt spelling.
        "pg_stat_get_backend_role" => Ok(Value::Null),
        // pg_stat_get_seq_scan_pos / _tid_scan — pg 17+ new counters
        // for scan-position tracking.
        "pg_stat_get_seq_scan_pos" | "pg_stat_get_tid_scan_pos" => {
            Ok(Value::BigInt(0))
        }
        // pg_stat_get_seq_tup_read / _idx_tup_fetch — tuple accessors
        // for per-table stat views.
        "pg_stat_get_seq_tup_read" | "pg_stat_get_idx_tup_fetch" | "pg_stat_get_idx_tup_read"
        | "pg_stat_get_idx_scan" | "pg_stat_get_seq_scan" => Ok(Value::BigInt(0)),
        // v7.37.17 (17.6 siblings) — pg_stat_get_wal / pg_stat_get_io
        // aggregate probes (PG 14+ / 16+). Both are SETOF probes;
        // scalar-surface NULL.
        "pg_stat_get_wal"
        | "pg_stat_get_io"
        | "pg_stat_get_activity_start_time"
        | "pg_stat_get_backend_query_start"
        | "pg_stat_get_backend_leader_pid"
        | "pg_stat_get_backend_pid_by_activity_start" => Ok(Value::Null),
        // pg_get_backend_memory_contexts / pg_backend_memory_contexts
        // covered separately as _memory_contexts.
        //
        // v7.37.17 (17.6 siblings) — pg_stat_get_recovery_prefetch:
        // recovery-prefetch tuning stats (PG 15+).
        "pg_stat_get_recovery_prefetch"
        | "pg_stat_get_recovery_prefetch_reset_time" => Ok(Value::BigInt(0)),
        // v7.37.17 (17.6 siblings) — pg_stat_get_checkpointer_*:
        // separate checkpointer stats view (PG 17+).
        "pg_stat_get_checkpointer_num_timed"
        | "pg_stat_get_checkpointer_num_requested"
        | "pg_stat_get_checkpointer_restartpoints_timed"
        | "pg_stat_get_checkpointer_restartpoints_requested"
        | "pg_stat_get_checkpointer_restartpoints_performed"
        | "pg_stat_get_checkpointer_write_time"
        | "pg_stat_get_checkpointer_sync_time"
        | "pg_stat_get_checkpointer_buffers_written"
        | "pg_stat_get_checkpointer_stat_reset_time" => Ok(Value::BigInt(0)),
        // v7.37.17 (17.6 siblings) — pg_stat_get_function_* family.
        // Per-function invocation stats for track_function_calls
        // observability. Real per-function tracking threads with
        // v7.38 observability epic.
        "pg_stat_get_function_calls"
        | "pg_stat_get_function_total_time"
        | "pg_stat_get_function_self_time"
        | "pg_stat_get_xact_function_calls"
        | "pg_stat_get_xact_function_total_time"
        | "pg_stat_get_xact_function_self_time" => Ok(Value::BigInt(0)),
        // v7.37.17 (17.6 siblings) — pg_stat_get_slru_blks_zeroed /
        // _blks_hit / _blks_read family. Buffer stats. All 0.
        "pg_stat_get_slru_blks_zeroed"
        | "pg_stat_get_slru_blks_hit"
        | "pg_stat_get_slru_blks_read"
        | "pg_stat_get_slru_blks_written"
        | "pg_stat_get_slru_blks_exists"
        | "pg_stat_get_slru_flushes"
        | "pg_stat_get_slru_truncates"
        | "pg_stat_get_slru_stat_reset_time" => Ok(Value::BigInt(0)),
        // v7.37.17 (17.6 siblings) — pg_lock_status / pg_locks
        // sibling probes. Return NULL until real lock tracking
        // threads with v7.38 MVCC.
        "pg_lock_status" => Ok(Value::Null),
        // pg_stat_get_progress_command emits the command name for
        // an in-progress background operation. NULL when idle.
        "pg_stat_get_progress_command"
        | "pg_stat_get_progress_relid"
        | "pg_stat_get_progress_datid"
        | "pg_stat_get_progress_pid" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — recovery-control probes.
        // Streaming-replica orchestration tools (patroni, repmgr)
        // emit these.
        "pg_wal_replay_pause"
        | "pg_wal_replay_resume"
        | "pg_xlog_replay_pause"   // pre-PG 10 name
        | "pg_xlog_replay_resume" => Ok(Value::Null),
        "pg_get_wal_replay_pause_state" => {
            Ok(Value::text::<String>("not paused".into()))
        }
        // pg_stat_get_progress_info takes a text arg naming the
        // progress view (VACUUM/CLUSTER/CREATE_INDEX/ANALYZE/
        // BASEBACKUP/COPY). Already handled above via NULL; keep
        // the alt spelling for pg_stat_get_progress_info_start.
        "pg_stat_get_progress_info_start" => Ok(Value::Null),
        // pg_get_wait_event_type / _name — resolve wait event to
        // its category / description. Return the text.
        "pg_get_wait_event_type" => Ok(Value::text::<String>("Client".into())),
        "pg_get_wait_event_name" => Ok(Value::text::<String>("Idle".into())),
        // pg_wal_lsn_diff — WAL byte-position arithmetic. Return 0
        // until real LSN types land.
        // v7.37.17 (17.6 siblings) — real pg_wal_lsn_diff(a, b)
        // parses PG-format LSN strings ("hex/hex") and returns the
        // byte-count difference (a - b). SPG's WAL uses seq_no
        // internally, but ORM callers pass literal LSN text.
        "pg_wal_lsn_diff" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "pg_wal_lsn_diff() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            fn lsn_bytes(v: &Value<'_>) -> Result<Option<i64>, EvalError> {
                let s = match v {
                    Value::Null => return Ok(None),
                    Value::Text(s) => s.to_string(),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "pg_wal_lsn_diff(): args must be text, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                };
                let (hi, lo) = s.split_once('/').ok_or_else(|| {
                    EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_wal_lsn_diff(): bad LSN {s:?}, expected 'hex/hex'"
                        ),
                    }
                })?;
                let hi_u32 = u32::from_str_radix(hi, 16).map_err(|_| {
                    EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_wal_lsn_diff(): bad LSN high {hi:?}"
                        ),
                    }
                })?;
                let lo_u32 = u32::from_str_radix(lo, 16).map_err(|_| {
                    EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_wal_lsn_diff(): bad LSN low {lo:?}"
                        ),
                    }
                })?;
                Ok(Some(((hi_u32 as i64) << 32) | lo_u32 as i64))
            }
            let a = match lsn_bytes(&args[0])? {
                None => return Ok(Value::Null),
                Some(v) => v,
            };
            let b = match lsn_bytes(&args[1])? {
                None => return Ok(Value::Null),
                Some(v) => v,
            };
            Ok(Value::BigInt(a - b))
        }
        "version" => Ok(Value::text("PostgreSQL 18.4 (SPG-compat)")),
        // v7.37.17 (17.6 siblings) — PG's `similar_to_escape(pattern
        // [, escape])` converts a SQL-standard SIMILAR TO pattern
        // to a POSIX regex string. The conversion table:
        //   %       → .*
        //   _       → .
        //   [       → [
        //   ]       → ]
        //   |       → |
        //   +*?{}() → same (already regex metachar)
        //   any other char → escaped literal
        // Result is wrapped in ^...$ so it matches the whole
        // string, matching PG's SIMILAR TO whole-string semantics.
        "similar_to_escape" | "similar_escape" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "similar_to_escape() takes 1 or 2 args, got {}",
                        args.len()
                    ),
                });
            }
            let pat = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.to_string(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "similar_to_escape(): pattern must be text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let esc: Option<char> = if args.len() == 2 {
                match &args[1] {
                    Value::Null => None,
                    Value::Text(s) => {
                        let mut chars = s.chars();
                        chars.next()
                    }
                    _ => {
                        return Err(EvalError::TypeMismatch {
                            detail: "similar_to_escape(): escape must be text".into(),
                        });
                    }
                }
            } else {
                Some('\\')
            };
            let mut out = alloc::string::String::from("^");
            let mut iter = pat.chars();
            while let Some(c) = iter.next() {
                if Some(c) == esc {
                    match iter.next() {
                        Some(next) => {
                            out.push('\\');
                            out.push(next);
                        }
                        None => out.push('\\'),
                    }
                    continue;
                }
                match c {
                    '%' => out.push_str(".*"),
                    '_' => out.push('.'),
                    '[' | ']' | '(' | ')' | '|' | '+' | '*' | '?' | '{' | '}' => {
                        out.push(c);
                    }
                    // POSIX regex metachars that SIMILAR TO doesn't
                    // treat as special — escape them.
                    '.' | '^' | '$' | '\\' => {
                        out.push('\\');
                        out.push(c);
                    }
                    _ => out.push(c),
                }
            }
            out.push('$');
            Ok(Value::text(out))
        }
        // v7.17.0 Phase 3.P0-30 — session / introspection functions.
        // Engine-level dispatch so these compose inside expressions
        // (`WHERE schemaname = current_schema()`, `SELECT *,
        // database() AS db FROM t`) — the pgwire layer's canned
        // shortcuts only catch the bare top-level SELECT shape.
        // SPG is single-database + single-schema; the values
        // mirror the wire-layer canned defaults.
        "current_database" | "database" => Ok(Value::text("spg")),
        "current_schema" => Ok(Value::text::<String>("public".into())),
        "current_user" | "session_user" | "user" => Ok(Value::text::<String>("admin".into())),
        // v7.37.17 (17.6 siblings) — SQL:2003 spelling variants.
        // CURRENT_CATALOG is the SQL-standard synonym for
        // CURRENT_DATABASE; CURRENT_ROLE is the SQL-standard synonym
        // for CURRENT_USER. pg_dump uses them in a few places
        // (SECURITY LABEL FOR / event-trigger owner assignment)
        // and PG psql accepts both. Mirror CURRENT_DATABASE /
        // CURRENT_USER so drivers that emit the SQL-standard names
        // don't get "unknown function" errors.
        "current_catalog" => Ok(Value::text("spg")),
        "current_role" => Ok(Value::text::<String>("admin".into())),
        // v7.37.43-T4 — PG advisory locks. SPG is single-writer +
        // single-process; the engine holds its own exclusive RwLock
        // on the write path, so there's no concurrent-writer race
        // for advisory locks to mediate. `sqlx::migrate!()` issues
        // `pg_advisory_lock($1)` / `pg_advisory_unlock($1)` around
        // its migration set so two parallel migrators don't double-
        // apply; under SPG semantics those calls become no-ops that
        // return `void` / `bool true` per PG's signatures, so the
        // sqlx migration pipeline runs end-to-end. Same applies to
        // every drop-in customer (mailrs, sentori, any sqlx-shaped
        // app) — they get advisory-lock acceptance for free without
        // a customer-side code change. Reservation-level functions
        // (`pg_try_advisory_lock` / `_unlock_all`) match the same
        // contract — true on lock attempt, true on unlock, void on
        // unlock_all — because under single-writer there's nothing
        // a real lock would block.
        "pg_advisory_lock"
        | "pg_advisory_xact_lock"
        | "pg_advisory_lock_shared"
        | "pg_advisory_xact_lock_shared" => Ok(Value::Null),
        "pg_try_advisory_lock"
        | "pg_try_advisory_xact_lock"
        | "pg_try_advisory_lock_shared"
        | "pg_try_advisory_xact_lock_shared"
        | "pg_advisory_unlock"
        | "pg_advisory_unlock_shared" => Ok(Value::Bool(true)),
        "pg_advisory_unlock_all" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — pg_sleep / pg_sleep_for /
        // pg_sleep_until. Return void (NULL) without actually
        // sleeping. Tests that use pg_sleep to trigger cache
        // eviction / stat rollup are typically doing it as a
        // shape marker; SPG's stats are synchronous so a real
        // sleep isn't useful. Preserves parse-through for
        // migration scripts + regression tests.
        "pg_sleep" | "pg_sleep_for" | "pg_sleep_until" => Ok(Value::Null),
        // pg_xact_commit_timestamp(xid) — commit-timestamp
        // extension probe (typically off by default). Return NULL.
        "pg_xact_commit_timestamp" | "pg_last_committed_xact" => Ok(Value::Null),
        // pg_current_wal_lsn / pg_current_wal_flush_lsn /
        // pg_current_wal_insert_lsn — return NULL (SPG's WAL
        // uses seq_no instead of PG-style LSN bytes; the real
        // mapping queues with the replication-protocol RFC).
        // v7.37.17 (17.6 siblings) — return the PG text-form LSN
        // "0/0" instead of NULL. postgres_exporter / pgpool-II /
        // orchestrators expect a text value even from a fresh
        // instance, so NULL causes false-positive alerts. Real
        // seq_no ↔ LSN mapping threads with the replication-
        // protocol RFC.
        "pg_current_wal_lsn"
        | "pg_current_wal_flush_lsn"
        | "pg_current_wal_insert_lsn"
        | "pg_last_wal_receive_lsn"
        | "pg_last_wal_replay_lsn" => Ok(Value::text::<String>("0/0".into())),
        // pg_last_xact_replay_timestamp — replica lag probe.
        "pg_last_xact_replay_timestamp" => Ok(Value::Null),
        // Range bound predicates — real text-form parsing. SPG
        // stores ranges in PG's canonical text form '[a,b)' /
        // '(a,b]' / 'empty'; the predicates read the brackets and
        // bound emptiness directly. (These returned constant false
        // before, which was wrong for the canonical '[a,b)' form
        // where lower_inc is true.)
        "lower_inc" | "upper_inc" | "lower_inf" | "upper_inf" | "isempty" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{name}() takes 1 arg, got {}", args.len()),
                });
            }
            let s = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s.trim(),
                // Real Range values (from the constructor functions)
                // answer directly off the bounds.
                Value::Range {
                    lower,
                    upper,
                    lower_inc,
                    upper_inc,
                    empty,
                    ..
                } => {
                    let result = match name {
                        "isempty" => *empty,
                        "lower_inc" => !empty && *lower_inc && lower.is_some(),
                        "upper_inc" => !empty && *upper_inc && upper.is_some(),
                        "lower_inf" => !empty && lower.is_none(),
                        "upper_inf" => !empty && upper.is_none(),
                        _ => unreachable!(),
                    };
                    return Ok(Value::Bool(result));
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() needs range text, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if s.eq_ignore_ascii_case("empty") {
                // PG: all four bound predicates are false on the
                // empty range; isempty is true.
                return Ok(Value::Bool(name == "isempty"));
            }
            if name == "isempty" {
                return Ok(Value::Bool(false));
            }
            // Split '[lo,hi)' into (bracket, lo, hi, closer).
            let opener = s.chars().next();
            let closer = s.chars().last();
            let inner = &s[1..s.len().saturating_sub(1)];
            let (lo, hi) = inner.split_once(',').unwrap_or((inner, ""));
            let result = match name {
                "lower_inc" => opener == Some('[') && !lo.trim().is_empty(),
                "upper_inc" => closer == Some(']') && !hi.trim().is_empty(),
                "lower_inf" => lo.trim().is_empty(),
                "upper_inf" => hi.trim().is_empty(),
                _ => unreachable!(),
            };
            Ok(Value::Bool(result))
        }
        // range_adjacent(a, b) — the `-|-` operator: the ranges touch at
        // exactly one bound with no gap and no overlap.
        "range_adjacent" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("range_adjacent() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            match super::binop::range_adjacent_pair(&args[0], &args[1]) {
                Some(b) => Ok(Value::Bool(b)),
                None => Err(EvalError::TypeMismatch {
                    detail: "range_adjacent() takes 2 range args".into(),
                }),
            }
        }
        // range_merge(a, b) — the smallest range containing both.
        // Numeric bounds compare numerically, others lexically.
        "range_merge" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "range_merge() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            // Typed range values (the common `range_merge(int4range(...), ...)`
            // shape) merge via the range machinery in `binop`.
            if matches!(args[0], Value::Range { .. }) || matches!(args[1], Value::Range { .. }) {
                if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                    return Ok(Value::Null);
                }
                if let Some(v) = super::binop::range_merge_pair(&args[0], &args[1]) {
                    return Ok(v);
                }
            }
            let (a, b) = match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
                (Value::Text(a), Value::Text(b)) => (a.trim(), b.trim()),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "range_merge() takes 2 range TEXT args".into(),
                    });
                }
            };
            if a.eq_ignore_ascii_case("empty") {
                return Ok(Value::text(b.to_string()));
            }
            if b.eq_ignore_ascii_case("empty") {
                return Ok(Value::text(a.to_string()));
            }
            // (opener, lo, hi, closer) from '[lo,hi)'.
            fn split_range(s: &str) -> (char, &str, &str, char) {
                let opener = s.chars().next().unwrap_or('[');
                let closer = s.chars().last().unwrap_or(')');
                let inner = &s[1..s.len().saturating_sub(1)];
                let (lo, hi) = inner.split_once(',').unwrap_or((inner, ""));
                (opener, lo.trim(), hi.trim(), closer)
            }
            // less-than for bounds: numeric when both parse, else
            // lexicographic.
            fn bound_lt(x: &str, y: &str) -> bool {
                match (x.parse::<f64>(), y.parse::<f64>()) {
                    (Ok(fx), Ok(fy)) => fx < fy,
                    _ => x < y,
                }
            }
            let (op_a, lo_a, hi_a, cl_a) = split_range(a);
            let (op_b, lo_b, hi_b, cl_b) = split_range(b);
            // Lower bound: -inf (empty) wins; then the smaller
            // value; on tie the inclusive bracket wins.
            let (lo, op) = if lo_a.is_empty() || lo_b.is_empty() {
                ("", '(')
            } else if bound_lt(lo_a, lo_b) {
                (lo_a, op_a)
            } else if bound_lt(lo_b, lo_a) {
                (lo_b, op_b)
            } else {
                (lo_a, if op_a == '[' || op_b == '[' { '[' } else { '(' })
            };
            let (hi, cl) = if hi_a.is_empty() || hi_b.is_empty() {
                ("", ')')
            } else if bound_lt(hi_b, hi_a) {
                (hi_a, cl_a)
            } else if bound_lt(hi_a, hi_b) {
                (hi_b, cl_b)
            } else {
                (hi_a, if cl_a == ']' || cl_b == ']' { ']' } else { ')' })
            };
            Ok(Value::text(alloc::format!("{op}{lo},{hi}{cl}")))
        }
        // v7.37.14 (B6.5) — PG `pg_blocking_pids(pid)` returns the
        // array of pids currently blocking `pid`. SPG's single-
        // writer + Arc-snapshot model means there is no per-tuple
        // lock chain to walk (write-lock contention is at most
        // 1-deep, fully observable via spg_stat_activity.wait_event).
        // Until v7.37.15 lands per-row tuple locks, this function
        // always returns NULL — but its presence keeps PG-shaped
        // monitoring queries (`SELECT pg_blocking_pids(pid)`)
        // syntactically valid against SPG. Tests / dashboards
        // written today against this surface keep working when
        // v7.37.15 starts populating real chains.
        "pg_blocking_pids" => Ok(Value::Null),
        // v7.37.16 (16.12) — PG partition catalog scalar functions.
        // PG `pg_partition_root(regclass)` returns the top-most
        // ancestor of a partition. SPG's catalog only knows table
        // names (not OIDs), so we take TEXT; a non-existent name
        // or non-partition table returns NULL (matches PG).
        "pg_partition_root" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "pg_partition_root() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            let name = match &args[0] {
                Value::Text(s) => s.to_string(),
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_partition_root() arg must be TEXT, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let Some(cat) = ctx.catalog else {
                return Ok(Value::Null);
            };
            Ok(match crate::partition_walks::root_of(cat, &name) {
                Some(root) => Value::text::<String>(root),
                None => Value::Null,
            })
        }
        // PG `pg_partition_ancestors(regclass)` is set-returning;
        // SPG's scalar form returns a comma-separated TEXT (no SRF
        // surface yet). The ordered chain from leaf → root mirrors
        // PG's row order; same semantics for a non-partition table
        // (single-row containing the input name).
        "pg_partition_ancestors" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "pg_partition_ancestors() takes 1 arg, got {}",
                        args.len()
                    ),
                });
            }
            let name = match &args[0] {
                Value::Text(s) => s.to_string(),
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "pg_partition_ancestors() arg must be TEXT, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let Some(cat) = ctx.catalog else {
                return Ok(Value::Null);
            };
            let chain = crate::partition_walks::ancestors_of(cat, &name);
            if chain.is_empty() {
                return Ok(Value::Null);
            }
            Ok(Value::text::<String>(chain.join(",")))
        }
        // v7.37.22 (22.4) — PG amcheck extension scalar surface.
        // PG ships `bt_index_check(regclass)` (validate BTree
        // structural invariants — sibling links, key ordering, leaf
        // page consistency) and `verify_heapam(regclass)` (validate
        // heap tuple visibility + dead-row consistency).
        //
        // SPG's storage model differs (PersistentVec rows + parallel
        // RowHeader vec), so the checks are different — but the
        // PG-compatible function names + return-NULL-on-success
        // contract let monitoring queries against PG move over
        // without changes.
        //
        // Each function takes a table name (TEXT) and returns NULL
        // on a clean check or a TEXT message describing the first
        // issue found.
        "bt_index_check" | "spg_bt_index_check" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "{}() takes 1 arg, got {}",
                        name,
                        args.len()
                    ),
                });
            }
            let table = match &args[0] {
                Value::Text(s) => s.to_string(),
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() arg must be TEXT, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let Some(cat) = ctx.catalog else {
                return Ok(Value::Null);
            };
            Ok(match crate::amcheck::check_btree_indices(cat, &table) {
                Ok(()) => Value::Null,
                Err(msg) => Value::text::<String>(msg),
            })
        }
        "verify_heapam" | "spg_verify_heapam" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "{}() takes 1 arg, got {}",
                        name,
                        args.len()
                    ),
                });
            }
            let table = match &args[0] {
                Value::Text(s) => s.to_string(),
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "{name}() arg must be TEXT, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let Some(cat) = ctx.catalog else {
                return Ok(Value::Null);
            };
            Ok(match crate::amcheck::check_heap_invariants(cat, &table) {
                Ok(()) => Value::Null,
                Err(msg) => Value::text::<String>(msg),
            })
        }
        // v7.17.0 Phase 3.P0-31 — `pg_typeof(any)` returns the
        // canonical PG lowercase type name. sqlx / SQLAlchemy /
        // Diesel emit this during describe; generic ORMs may
        // branch on it (`CASE WHEN pg_typeof(x) = 'jsonb' ...`).
        // NULL has no resolved value-level type → 'unknown' per
        // PG semantics.
        "pg_typeof" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("pg_typeof() takes 1 arg, got {}", args.len()),
                });
            }
            Ok(Value::text::<String>(pg_typeof_name(&args[0]).into()))
        }
        // v7.17.0 — `nextval` / `currval` / `setval` are handled
        // at the top of this match against the SequenceResolver.
        // `lastval()` (no-arg session memory) still degrades to
        // NULL pending a Phase 1.1b session tracker.
        // lastval() is intercepted by the sequence pre-resolver
        // (sequence.rs) before eval; reaching this arm means a
        // read-only context with no resolver — NULL keeps it
        // parseable.
        "lastval" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — pg_sequence_last_value(regclass)
        // returns the sequence's most-recent value or NULL if not
        // yet advanced — REAL: reads the catalog's SequenceDef
        // (the pg_sequences view's last_value source). Missing
        // sequence or never advanced (is_called=false) → NULL,
        // matching PG.
        "pg_sequence_last_value" => {
            let name_arg = match args.first() {
                None | Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Text(s)) => s.as_ref(),
                Some(_) => return Ok(Value::Null),
            };
            let Some(cat) = ctx.catalog else {
                return Ok(Value::Null);
            };
            let bare = name_arg
                .strip_prefix("public.")
                .unwrap_or(name_arg)
                .trim_matches('"');
            match cat.sequence_current_value(bare) {
                Ok(v) => Ok(Value::BigInt(v)),
                Err(_) => Ok(Value::Null),
            }
        }
        // pg_sequence_parameters(oid) returns a row with (start,
        // minimum, maximum, increment, cycle, cache, data_type).
        // Scalar surface returns NULL; the real row-shape is only
        // useful through the pg_sequence catalog view.
        "pg_sequence_parameters" => Ok(Value::Null),
        // v7.15.0 — pg_trgm: similarity, show_trgm. Match PG
        // semantics: similarity returns Jaccard of trigram sets;
        // show_trgm returns the trigram set as TEXT[]. NULL on
        // any NULL arg.
        "similarity" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("similarity() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            let a = match &args[0] {
                Value::Text(s) => s.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("similarity() needs text, got {:?}", other.data_type()),
                    });
                }
            };
            let b = match &args[1] {
                Value::Text(s) => s.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("similarity() needs text, got {:?}", other.data_type()),
                    });
                }
            };
            // PG returns REAL (f32) — we use Float (f64) and let
            // coerce_value narrow on assignment to a REAL column.
            Ok(Value::Float(spg_storage::trgm::similarity(a, b)))
        }
        "show_trgm" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("show_trgm() takes 1 arg, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            let s = match &args[0] {
                Value::Text(s) => s.as_ref(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("show_trgm() needs text, got {:?}", other.data_type()),
                    });
                }
            };
            // PG returns the trigram set sorted lexicographically.
            // `extract_trigrams` already returns a BTreeSet so the
            // order is canonical.
            let trigrams: Vec<Option<String>> = spg_storage::trgm::extract_trigrams(s)
                .into_iter()
                .map(Some)
                .collect();
            Ok(Value::TextArray(trigrams))
        }
        other => {
            // v7.38 (read01) — function-style typecast: PG treats `typename(expr)`
            // as shorthand for `expr::typename` (`int4('5')`, `float8('1.5')`,
            // `text(42)`, `circle('<(0,0),5>')`, `box('...')`). Fall back to it
            // when the unmatched name resolves to a type and there is exactly one
            // argument; real functions were already matched above.
            if args.len() == 1 && crate::conversions::type_name_to_data_type(other).is_some() {
                return crate::eval::cast::cast_value(
                    args[0].clone().into_owned(),
                    spg_sql::ast::CastTarget::Named(other.to_string()),
                );
            }
            Err(EvalError::TypeMismatch {
                detail: format!("unknown function `{other}`"),
            })
        }
    }
}

// v7.38 P0 元机制 A — SQL-facing handles for the injection_points
// framework. Tests call these via `SELECT spg_injection_attach(...)` /
// `_wakeup` / `_detach`. With the `injection-points` feature OFF
// (release builds) all three return an error so a production SPG
// can't be coerced into deadlocking via SQL.

/// v7.38 (read01 P6.32) — Windows-1252 high range (0x80–0x9F). It matches
/// LATIN1 everywhere else; only these 32 bytes remap (five are undefined,
/// stored as 0). Used by convert_to / convert_from for the WIN1252 encoding.
const WIN1252_HIGH: [u32; 32] = [
    0x20AC, 0, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
    0x0152, 0, 0x017D, 0, 0, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC,
    0x2122, 0x0161, 0x203A, 0x0153, 0, 0x017E, 0x0178,
];

fn win1252_byte_to_char(b: u8) -> Option<char> {
    if (0x80..=0x9F).contains(&b) {
        let cp = WIN1252_HIGH[(b - 0x80) as usize];
        if cp == 0 { None } else { char::from_u32(cp) }
    } else {
        Some(b as char) // 0x00–0x7F and 0xA0–0xFF are identity, as in LATIN1.
    }
}

fn win1252_char_to_byte(ch: char) -> Option<u8> {
    let cp = ch as u32;
    if cp <= 0x7F || (0xA0..=0xFF).contains(&cp) {
        Some(cp as u8)
    } else {
        WIN1252_HIGH
            .iter()
            .position(|&c| c != 0 && c == cp)
            .map(|i| 0x80 + i as u8)
    }
}

/// v7.38 (read01 P6.32) — is `enc` a name for Windows-1252?
fn is_win1252(enc_up: &str) -> bool {
    matches!(enc_up, "WIN1252" | "CP1252" | "WINDOWS-1252")
}

fn expect_text_arg<'a>(
    args: &'a [Value<'_>],
    idx: usize,
    fn_name: &str,
) -> Result<&'a str, EvalError> {
    let v = args.get(idx).ok_or_else(|| EvalError::TypeMismatch {
        detail: format!("{fn_name} requires {} args, got {}", idx + 1, args.len()),
    })?;
    match v {
        Value::Text(s) => Ok(s.as_ref()),
        other => Err(EvalError::TypeMismatch {
            detail: format!(
                "{fn_name} argument {} must be TEXT, got {:?}",
                idx + 1,
                other.data_type()
            ),
        }),
    }
}

/// v7.37.17 (17.6 siblings) — parse a datetime string against a PG
/// to_date/to_timestamp format template. Numeric field tokens
/// (YYYY/YY/MM/DD/HH24/HH12/HH/MI/SS/MS/US) consume up to their
/// width in digits; MON/MONTH match English month names
/// case-insensitively; AM/PM adjust HH12; every other template
/// char consumes one input char (separator). Returns
/// (year, month, day, hour, minute, second, microsecond).
/// v7.38 (read01 U18) — element count of a 1-D array value, across every
/// array element type SPG models. Returns `None` for non-array values.
/// PG `array_position` / `array_positions` element match: IS NOT
/// DISTINCT FROM between an array element and the search value. A NULL
/// element matches only a NULL search value; otherwise the scalar `=`
/// dispatch (`apply_binary`) decides, so cross-width numerics and every
/// scalar element type compare exactly as the `=` operator would.
fn array_search_match(elem: &Value, needle: &Value) -> Result<bool, EvalError> {
    match (matches!(elem, Value::Null), matches!(needle, Value::Null)) {
        (true, true) => Ok(true),
        (true, false) | (false, true) => Ok(false),
        (false, false) => match apply_binary(
            BinOp::Eq,
            elem.clone().into_owned(),
            needle.clone().into_owned(),
        )? {
            Value::Bool(b) => Ok(b),
            Value::Null => Ok(false),
            other => Err(EvalError::TypeMismatch {
                detail: format!(
                    "array element comparison didn't return Bool: {:?}",
                    other.data_type()
                ),
            }),
        },
    }
}

/// PG `adjust_partial_year_to_2020` (formatting.c): map a partial
/// (< 4-digit) year field toward the 1970-2069 window. Two-digit years
/// pivot at 70 ('70' → 1970, '69' → 2069); three-digit years fold via
/// the same downstream ranges ('970' → 1970).
fn adjust_partial_year_to_2020(year: i32) -> i32 {
    if year < 70 {
        year + 2000
    } else if year < 100 {
        year + 1900
    } else if year < 520 {
        year + 2000
    } else if year < 1000 {
        year + 1000
    } else {
        year
    }
}

#[allow(clippy::type_complexity)]
fn parse_by_format(
    input: &str,
    fmt: &str,
) -> Result<(i32, u32, u32, u32, u32, u32, u32), String> {
    const MONTHS: [&str; 12] = [
        "JANUARY", "FEBRUARY", "MARCH", "APRIL", "MAY", "JUNE", "JULY",
        "AUGUST", "SEPTEMBER", "OCTOBER", "NOVEMBER", "DECEMBER",
    ];
    let mut year: i32 = 1;
    // Deferred year resolution: (raw value, format-token digit width). A
    // partial (< 4-digit) field gets PG's adjust_partial_year_to_2020
    // pivot; a `CC` century token, when present, overrides the high
    // digits. Resolved after the parse loop so DDD sees the final year.
    let mut year_token: Option<(i32, u8)> = None;
    let mut century: Option<i32> = None;
    let mut month: u32 = 1;
    let mut day: u32 = 1;
    let mut hour: u32 = 0;
    let mut minute: u32 = 0;
    let mut second: u32 = 0;
    let mut micros: u32 = 0;
    let mut pm_shift = false;
    let mut is_hh12 = false;
    // v7.37 — `DDD` day-of-year, resolved to month/day after the loop (the
    // year may appear after DDD in the format).
    let mut doy: Option<u32> = None;

    let fmt_bytes: alloc::vec::Vec<char> = fmt.chars().collect();
    let in_bytes: alloc::vec::Vec<char> = input.chars().collect();
    let mut fi = 0usize;
    let mut ii = 0usize;

    fn starts_with_ci(hay: &[char], pos: usize, needle: &str) -> bool {
        needle.chars().enumerate().all(|(k, c)| {
            hay.get(pos + k)
                .is_some_and(|h| h.eq_ignore_ascii_case(&c))
        })
    }
    fn take_digits(input: &[char], pos: &mut usize, max: usize) -> Option<u64> {
        let start = *pos;
        let mut val: u64 = 0;
        while *pos < input.len() && *pos - start < max {
            match input[*pos].to_digit(10) {
                Some(d) => {
                    val = val * 10 + u64::from(d);
                    *pos += 1;
                }
                None => break,
            }
        }
        if *pos == start { None } else { Some(val) }
    }

    while fi < fmt_bytes.len() {
        // Longest-token-first matching.
        if starts_with_ci(&fmt_bytes, fi, "YYYY") {
            let v = take_digits(&in_bytes, &mut ii, 4)
                .ok_or_else(|| alloc::format!("expected year digits at position {ii}"))?;
            year_token = Some((v as i32, 4));
            fi += 4;
        } else if starts_with_ci(&fmt_bytes, fi, "YYY") {
            let v = take_digits(&in_bytes, &mut ii, 3)
                .ok_or_else(|| alloc::format!("expected year digits at position {ii}"))?;
            year_token = Some((v as i32, 3));
            fi += 3;
        } else if starts_with_ci(&fmt_bytes, fi, "YY") {
            let v = take_digits(&in_bytes, &mut ii, 2)
                .ok_or_else(|| alloc::format!("expected year digits at position {ii}"))?;
            year_token = Some((v as i32, 2));
            fi += 2;
        } else if starts_with_ci(&fmt_bytes, fi, "CC") {
            // Century token. Resolved after the loop: `CC` alone gives the
            // first year of that century ((cc-1)*100+1); combined with a
            // 1-2 digit year it supplies the high digits.
            let v = take_digits(&in_bytes, &mut ii, 2)
                .ok_or_else(|| alloc::format!("expected century digits at position {ii}"))?;
            century = Some(v as i32);
            fi += 2;
        } else if starts_with_ci(&fmt_bytes, fi, "MONTH") {
            let rest: alloc::string::String =
                in_bytes[ii..].iter().collect::<alloc::string::String>();
            let upper = rest.to_ascii_uppercase();
            let m = MONTHS
                .iter()
                .position(|name| upper.starts_with(name))
                .ok_or_else(|| alloc::format!("unrecognized month name at position {ii}"))?;
            month = m as u32 + 1;
            ii += MONTHS[m].len();
            fi += 5;
        } else if starts_with_ci(&fmt_bytes, fi, "MON") {
            let rest: alloc::string::String =
                in_bytes[ii..].iter().take(3).collect();
            let upper = rest.to_ascii_uppercase();
            let m = MONTHS
                .iter()
                .position(|name| name.starts_with(&upper))
                .ok_or_else(|| alloc::format!("unrecognized month abbrev at position {ii}"))?;
            month = m as u32 + 1;
            ii += 3;
            fi += 3;
        } else if starts_with_ci(&fmt_bytes, fi, "MM") {
            let v = take_digits(&in_bytes, &mut ii, 2)
                .ok_or_else(|| alloc::format!("expected month digits at position {ii}"))?;
            month = v as u32;
            fi += 2;
        } else if starts_with_ci(&fmt_bytes, fi, "DDD") {
            // Day of year (1-366); resolved to month/day post-loop. Must
            // precede the `DD` arm which shares the prefix.
            let v = take_digits(&in_bytes, &mut ii, 3)
                .ok_or_else(|| alloc::format!("expected day-of-year digits at position {ii}"))?;
            doy = Some(v as u32);
            fi += 3;
        } else if starts_with_ci(&fmt_bytes, fi, "WW") {
            // v7.38 (read01 sweep) — week of year (1-53). PG's WW: week 1
            // starts on Jan 1 and every week is 7 days, so the week's first
            // day is day-of-year (W-1)*7 + 1. Resolved to month/day post-loop
            // via the same `doy` path as DDD.
            let v = take_digits(&in_bytes, &mut ii, 2)
                .ok_or_else(|| alloc::format!("expected week digits at position {ii}"))?;
            doy = Some((v as u32).saturating_sub(1) * 7 + 1);
            fi += 2;
        } else if starts_with_ci(&fmt_bytes, fi, "DD") {
            let v = take_digits(&in_bytes, &mut ii, 2)
                .ok_or_else(|| alloc::format!("expected day digits at position {ii}"))?;
            day = v as u32;
            fi += 2;
        } else if starts_with_ci(&fmt_bytes, fi, "HH24") {
            let v = take_digits(&in_bytes, &mut ii, 2)
                .ok_or_else(|| alloc::format!("expected hour digits at position {ii}"))?;
            hour = v as u32;
            fi += 4;
        } else if starts_with_ci(&fmt_bytes, fi, "HH12")
            || starts_with_ci(&fmt_bytes, fi, "HH")
        {
            let width = if starts_with_ci(&fmt_bytes, fi, "HH12") { 4 } else { 2 };
            let v = take_digits(&in_bytes, &mut ii, 2)
                .ok_or_else(|| alloc::format!("expected hour digits at position {ii}"))?;
            hour = v as u32;
            is_hh12 = true;
            fi += width;
        } else if starts_with_ci(&fmt_bytes, fi, "MI") {
            let v = take_digits(&in_bytes, &mut ii, 2)
                .ok_or_else(|| alloc::format!("expected minute digits at position {ii}"))?;
            minute = v as u32;
            fi += 2;
        } else if starts_with_ci(&fmt_bytes, fi, "SS") {
            let v = take_digits(&in_bytes, &mut ii, 2)
                .ok_or_else(|| alloc::format!("expected second digits at position {ii}"))?;
            second = v as u32;
            fi += 2;
        } else if starts_with_ci(&fmt_bytes, fi, "US") {
            let v = take_digits(&in_bytes, &mut ii, 6)
                .ok_or_else(|| alloc::format!("expected microsecond digits at position {ii}"))?;
            micros = v as u32;
            fi += 2;
        } else if starts_with_ci(&fmt_bytes, fi, "MS") {
            let v = take_digits(&in_bytes, &mut ii, 3)
                .ok_or_else(|| alloc::format!("expected millisecond digits at position {ii}"))?;
            micros = v as u32 * 1000;
            fi += 2;
        } else if starts_with_ci(&fmt_bytes, fi, "A.M.")
            || starts_with_ci(&fmt_bytes, fi, "P.M.")
        {
            if starts_with_ci(&in_bytes, ii, "P.M.") {
                pm_shift = true;
            }
            ii += 4;
            fi += 4;
        } else if starts_with_ci(&fmt_bytes, fi, "AM")
            || starts_with_ci(&fmt_bytes, fi, "PM")
        {
            if starts_with_ci(&in_bytes, ii, "PM") {
                pm_shift = true;
            }
            ii += 2;
            fi += 2;
        } else {
            // Literal separator — consume one input char.
            if ii < in_bytes.len() {
                ii += 1;
            }
            fi += 1;
        }
    }
    if is_hh12 {
        if pm_shift && hour < 12 {
            hour += 12;
        } else if !pm_shift && hour == 12 {
            hour = 0;
        }
    }
    // Resolve the year: a `CC` century token (alone, or supplying the high
    // digits for a 1-2 digit year) takes priority; otherwise a partial
    // (< 4-digit) year field is pivoted via PG's adjust_partial_year_to_2020
    // ('70' → 1970, '69' → 2069), and a 4-digit field is taken literally.
    match (century, year_token) {
        (Some(cc), Some((yy, digits))) if digits <= 2 => {
            year = (cc - 1) * 100 + yy;
        }
        (Some(cc), _) => {
            year = (cc - 1) * 100 + 1;
        }
        (None, Some((v, digits))) => {
            year = if digits >= 4 { v } else { adjust_partial_year_to_2020(v) };
        }
        (None, None) => {}
    }
    // v7.37 — resolve DDD (day of year) to month/day now that the year is
    // known (Jan 1 + doy - 1).
    if let Some(d) = doy {
        if d < 1 || d > 366 {
            return Err(alloc::format!("day-of-year {d} out of range"));
        }
        let abs = super::days_from_civil(year, 1, 1) + (d as i32) - 1;
        let (_y, m, dd) = super::civil_from_days(abs);
        month = m;
        day = dd;
    }
    if !(1..=12).contains(&month) {
        return Err(alloc::format!("month {month} out of range"));
    }
    if !(1..=31).contains(&day) {
        return Err(alloc::format!("day {day} out of range"));
    }
    if hour > 23 || minute > 59 || second > 60 {
        return Err(alloc::format!(
            "time {hour}:{minute}:{second} out of range"
        ));
    }
    Ok((year, month, day, hour, minute, second, micros))
}

#[cfg(feature = "injection-points")]
fn spg_injection_attach(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "spg_injection_attach takes (point_name TEXT, action TEXT), got {} args",
                args.len()
            ),
        });
    }
    let name = expect_text_arg(args, 0, "spg_injection_attach")?;
    let action_str = expect_text_arg(args, 1, "spg_injection_attach")?;
    let store = crate::testkit::injection::current().ok_or_else(|| EvalError::TypeMismatch {
        detail: "spg_injection_attach: no engine injection scope active".into(),
    })?;
    let action = crate::testkit::injection::parse_action(action_str)
        .map_err(|detail| EvalError::TypeMismatch { detail })?;
    store.attach(name.to_string(), action);
    Ok(Value::Bool(true))
}

#[cfg(feature = "injection-points")]
fn spg_injection_wakeup(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 1 {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "spg_injection_wakeup takes (point_name TEXT), got {} args",
                args.len()
            ),
        });
    }
    let name = expect_text_arg(args, 0, "spg_injection_wakeup")?;
    let store = crate::testkit::injection::current().ok_or_else(|| EvalError::TypeMismatch {
        detail: "spg_injection_wakeup: no engine injection scope active".into(),
    })?;
    store.wakeup(name);
    Ok(Value::Bool(true))
}

#[cfg(feature = "injection-points")]
fn spg_injection_detach(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 1 {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "spg_injection_detach takes (point_name TEXT), got {} args",
                args.len()
            ),
        });
    }
    let name = expect_text_arg(args, 0, "spg_injection_detach")?;
    let store = crate::testkit::injection::current().ok_or_else(|| EvalError::TypeMismatch {
        detail: "spg_injection_detach: no engine injection scope active".into(),
    })?;
    store.detach(name);
    Ok(Value::Bool(true))
}

#[cfg(not(feature = "injection-points"))]
fn spg_injection_attach(_args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    Err(EvalError::TypeMismatch {
        detail: "spg_injection_attach: injection-points feature not enabled in this build".into(),
    })
}

#[cfg(not(feature = "injection-points"))]
fn spg_injection_wakeup(_args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    Err(EvalError::TypeMismatch {
        detail: "spg_injection_wakeup: injection-points feature not enabled in this build".into(),
    })
}

#[cfg(not(feature = "injection-points"))]
fn spg_injection_detach(_args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    Err(EvalError::TypeMismatch {
        detail: "spg_injection_detach: injection-points feature not enabled in this build".into(),
    })
}
