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

pub(super) fn apply_function(
    name: &str,
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    apply_function_dispatch(&name.to_ascii_lowercase(), args, ctx)
}

fn apply_function_dispatch(
    name: &str,
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
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
        // pg_stat_get_snapshot_timestamp — timestamp of stats
        // snapshot. Return NULL.
        "pg_stat_get_snapshot_timestamp" | "pg_stat_get_stat_snapshot_timestamp" => {
            Ok(Value::Null)
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
                Value::Json(s) => {
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
                Value::Json(s) => {
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
                    let ch = s.chars().next().ok_or_else(|| EvalError::TypeMismatch {
                        detail: "ascii(): empty string has no first character".into(),
                    })?;
                    Ok(Value::Int(ch as i32))
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
                        // log10(x) = ln(x) / ln(10).
                        Ok(Value::Float(f64_ln(x) / f64_ln(10.0)))
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
                    // cbrt(x) = sign(x) * (|x|)^(1/3). Preserve sign
                    // for negative inputs (matches PG).
                    let sign = if x < 0.0 { -1.0 } else { 1.0 };
                    let mag = if x < 0.0 { -x } else { x };
                    // (|x|)^(1/3) via exp(ln(|x|)/3). Zero short-circuit.
                    let mag_cbrt = if mag == 0.0 {
                        0.0
                    } else {
                        f64_exp(f64_ln(mag) / 3.0)
                    };
                    Ok(Value::Float(sign * mag_cbrt))
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
            let len = match &args[0] {
                Value::TextArray(items) => items.len(),
                Value::IntArray(items) => items.len(),
                Value::BigIntArray(items) => items.len(),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "cardinality() arg must be an array, got {:?}",
                            args[0].data_type()
                        ),
                    });
                }
            };
            let n = i32::try_from(len).unwrap_or(i32::MAX);
            Ok(Value::Int(n))
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
            let len = match &args[0] {
                Value::TextArray(items) => items.len(),
                Value::IntArray(items) => items.len(),
                Value::BigIntArray(items) => items.len(),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "array_length() first arg must be an array, got {:?}",
                            args[0].data_type()
                        ),
                    });
                }
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
            if dim != 1 {
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
            let len = match &args[0] {
                Value::TextArray(items) => items.len(),
                Value::IntArray(items) => items.len(),
                Value::BigIntArray(items) => items.len(),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "{name}() first arg must be an array, got {:?}",
                            args[0].data_type()
                        ),
                    });
                }
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
                Value::TextArray(items) => Ok(if items.is_empty() {
                    Value::Null
                } else {
                    Value::Int(1)
                }),
                Value::IntArray(items) => Ok(if items.is_empty() {
                    Value::Null
                } else {
                    Value::Int(1)
                }),
                Value::BigIntArray(items) => Ok(if items.is_empty() {
                    Value::Null
                } else {
                    Value::Int(1)
                }),
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_ndims() needs array, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        "array_dims" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_dims() takes 1 arg, got {}", args.len()),
                });
            }
            let len = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::TextArray(items) => items.len(),
                Value::IntArray(items) => items.len(),
                Value::BigIntArray(items) => items.len(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "array_dims() needs array, got {:?}",
                            other.data_type()
                        ),
                    });
                }
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
            if matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            match (&args[0], &args[1]) {
                (Value::TextArray(items), Value::Text(needle)) => {
                    for (idx, item) in items.iter().enumerate() {
                        if let Some(s) = item
                            && s == needle
                        {
                            return Ok(Value::Int(i32::try_from(idx + 1).unwrap_or(i32::MAX)));
                        }
                    }
                    Ok(Value::Null)
                }
                (Value::IntArray(items), needle_v)
                    if matches!(
                        needle_v,
                        Value::Int(_) | Value::SmallInt(_) | Value::BigInt(_)
                    ) =>
                {
                    let needle: i64 = match *needle_v {
                        Value::Int(n) => i64::from(n),
                        Value::SmallInt(n) => i64::from(n),
                        Value::BigInt(n) => n,
                        _ => unreachable!(),
                    };
                    for (idx, item) in items.iter().enumerate() {
                        if let Some(n) = item
                            && i64::from(*n) == needle
                        {
                            return Ok(Value::Int(i32::try_from(idx + 1).unwrap_or(i32::MAX)));
                        }
                    }
                    Ok(Value::Null)
                }
                (Value::BigIntArray(items), needle_v)
                    if matches!(
                        needle_v,
                        Value::Int(_) | Value::SmallInt(_) | Value::BigInt(_)
                    ) =>
                {
                    let needle: i64 = match *needle_v {
                        Value::Int(n) => i64::from(n),
                        Value::SmallInt(n) => i64::from(n),
                        Value::BigInt(n) => n,
                        _ => unreachable!(),
                    };
                    for (idx, item) in items.iter().enumerate() {
                        if let Some(n) = item
                            && *n == needle
                        {
                            return Ok(Value::Int(i32::try_from(idx + 1).unwrap_or(i32::MAX)));
                        }
                    }
                    Ok(Value::Null)
                }
                (
                    arr @ (Value::TextArray(_) | Value::IntArray(_) | Value::BigIntArray(_)),
                    other,
                ) => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_position() needle type {:?} doesn't match array {:?}",
                        other.data_type(),
                        arr.data_type()
                    ),
                }),
                (other, _) => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_position() first arg must be an array, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.11.15 — `substring(s, start)` / `substring(s, start, length)`
        // for both TEXT and BYTEA. PG semantics: `start` is 1-based;
        // values ≤ 0 clamp into the string (i.e. effective start is
        // adjusted so the window still begins at index 1 — but
        // `length` is reduced by the clipped prefix). A NULL arg
        // makes the result NULL. Out-of-range windows return an
        // empty value, not NULL.
        "substring" | "substr" => {
            if !matches!(args.len(), 2 | 3) {
                return Err(EvalError::TypeMismatch {
                    detail: format!("substring() takes 2 or 3 args, got {}", args.len()),
                });
            }
            if args.iter().any(|a| matches!(a, Value::Null)) {
                return Ok(Value::Null);
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
                    let end = start.saturating_add(len);
                    if end <= 1 || len < 0 {
                        return Ok(match &args[0] {
                            Value::Text(_) => Value::text(String::new()),
                            Value::Bytes(_) => Value::bytes(Vec::new()),
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
        "upper" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("upper() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::text(s.to_uppercase())),
                other => Err(EvalError::TypeMismatch {
                    detail: format!("upper() needs text, got {:?}", other.data_type()),
                }),
            }
        }
        "lower" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("lower() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::text(s.to_lowercase())),
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
                Value::Int(n) => Ok(Value::Int(n.wrapping_abs())),
                Value::BigInt(n) => Ok(Value::BigInt(n.wrapping_abs())),
                Value::Float(x) => Ok(Value::Float(x.abs())),
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
        "date_part" => date_part(args),
        "age" => age(args),
        "to_char" => to_char(args),
        // v7.17.0 Phase 3.P0-29 — MySQL time aliases. WordPress,
        // Laravel, mysql-connector-python emit these constantly.
        // `unix_timestamp()` (bare) is folded by clock_replacement_for
        // into a BigInt literal — this arm only handles the 1-arg
        // form (TIMESTAMP / DATE → epoch seconds).
        "date_format" => date_format_mysql(args),
        "unix_timestamp" => unix_timestamp_of(args),
        "from_unixtime" => from_unixtime(args),
        // v7.17.0 Phase 3.8 — PG `format(fmt, args…)` sprintf-style.
        // Conversion specifiers: `%s` (literal string from arg),
        // `%I` (quoted identifier), `%L` (quoted SQL literal),
        // `%%` (literal `%`). `%n$X` argument-position prefix
        // (1-based). NULL arg → empty string for %s; NULL for %I
        // is an error in PG; NULL for %L renders as the SQL
        // literal `NULL`. Args missing for a position → error.
        "format" => format_string(args),
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
        // v7.17.0 — PG `gen_random_uuid()` (built-in, no extension)
        // and the historical uuid-ossp `uuid_generate_v4()` alias.
        // Both produce a RFC 4122 v4 (random) UUID. This is the
        // function Django / Rails / Hibernate emit in `id UUID
        // PRIMARY KEY DEFAULT gen_random_uuid()`, the modern
        // default PK pattern.
        "gen_random_uuid" | "uuid_generate_v4" => {
            if !args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("{name}() takes 0 args, got {}", args.len()),
                });
            }
            Ok(Value::Uuid(gen_random_uuid_bytes()))
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
        "mod" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("mod() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
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
        "greatest" | "least" => {
            if args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "{lc}() takes at least 1 arg",
                        lc = if name.eq_ignore_ascii_case("greatest") {
                            "greatest"
                        } else {
                            "least"
                        }
                    ),
                });
            }
            let non_null: alloc::vec::Vec<&Value> =
                args.iter().filter(|v| !matches!(v, Value::Null)).collect();
            if non_null.is_empty() {
                return Ok(Value::Null);
            }
            let is_greatest = name.eq_ignore_ascii_case("greatest");
            let mut best: Value<'static> = non_null[0].clone().into_owned();
            for v in &non_null[1..] {
                let ord = value_cmp_for_min_max(&best, v);
                let take = if is_greatest {
                    ord == core::cmp::Ordering::Less
                } else {
                    ord == core::cmp::Ordering::Greater
                };
                if take {
                    best = (*v).clone().into_owned();
                }
            }
            Ok(best)
        }
        // MySQL `ifnull(a, b)` — alias for coalesce(a, b).
        // Used by every ORM with a MySQL target (Hibernate /
        // Laravel / Sequelize).
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
        "trunc" => {
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
                        let q = scaled / factor;
                        Ok(Value::Numeric {
                            scaled: q * factor,
                            scale: *scale,
                        })
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
                    Value::Float(x) => Ok(Value::Float(f64_round_half_away(*x))),
                    Value::Numeric { scaled, scale } => {
                        let factor = pow10_i128(*scale);
                        let q = scaled.div_euclid(factor);
                        let r = scaled.rem_euclid(factor);
                        // Half-away-from-zero: if 2*r >= factor → round up.
                        let result = if 2 * r >= factor { q + 1 } else { q };
                        Ok(Value::Numeric {
                            scaled: result * factor,
                            scale: *scale,
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
                    Ok(Value::Numeric {
                        scaled: result * factor,
                        scale: *scale,
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
                    // negative values.
                    Ok(Value::Numeric {
                        scaled: q * factor,
                        scale: *scale,
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
        "regexp_replace" => regexp_replace(args),
        "regexp_split_to_array" => regexp_split_to_array(args),
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
            if let Value::Json(s) = &args[0] {
                return Ok(Value::json(s.clone()));
            }
            Ok(Value::json(crate::json::value_to_json_text(&args[0])))
        }
        "json_build_object" | "jsonb_build_object" => crate::json::build_object(args),
        "json_build_array" | "jsonb_build_array" => crate::json::build_array(args),
        "jsonb_set" | "json_set" => crate::json::set(args),
        "jsonb_insert" | "json_insert" => crate::json::insert(args),
        // v7.17.0 Phase 3.9 — PG `jsonb_path_query` family.
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
        // v6.4.3 — encode/decode + error_on_null SQL function bundle.
        "encode" => encode_text(args),
        "decode" => decode_text(args),
        // v7.37.17 (17.6 siblings) — convert_from / convert_to
        // handle text ↔ bytea encoding conversion. SPG stores text
        // as UTF-8 always so both simply reinterpret the bytes.
        //
        //   convert_from(bytea, 'UTF8') → text
        //   convert_to(text, 'UTF8')    → bytea
        //
        // Any encoding name other than 'UTF8' / 'SQL_ASCII' errors;
        // real transcoding queues with the collation/encoding epic.
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
            {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "convert_from(): unsupported encoding {enc:?} — SPG stores UTF-8 only; use UTF8 / SQL_ASCII / LATIN1"
                    ),
                });
            }
            let s = match core::str::from_utf8(b) {
                Ok(v) => v,
                Err(e) => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "convert_from(): input is not valid UTF-8: {e}"
                        ),
                    });
                }
            };
            Ok(Value::text(alloc::string::String::from(s)))
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
            {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "convert_to(): unsupported encoding {enc:?} — SPG stores UTF-8 only; use UTF8 / SQL_ASCII / LATIN1"
                    ),
                });
            }
            Ok(Value::Bytes(s.into_bytes().into()))
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
        // v7.24 (round-15) — string_to_array(text, delim): inverse
        // of array_to_string. PG semantics: NULL text → NULL,
        // '' → empty array, NULL delim → one element per char.
        "string_to_array" => fn_string_to_array(args),
        "plainto_tsquery" => fts_plainto_tsquery(args, ctx),
        "phraseto_tsquery" => fts_phraseto_tsquery(args, ctx),
        "websearch_to_tsquery" => fts_websearch_to_tsquery(args, ctx),
        "to_tsquery" => fts_to_tsquery(args, ctx),
        // v7.12.2 — ranking functions. mailrs's fallback search
        // query ORDERs BY ts_rank(search_vector, q) DESC.
        "ts_rank" => fts_ts_rank(args),
        "ts_rank_cd" => fts_ts_rank_cd(args),
        // v7.14.0 — PG dump preamble emits
        // `SELECT pg_catalog.set_config('search_path', '', false);`
        // and friends. SPG is single-schema; accept-as-no-op
        // returning either the new value or NULL.
        "set_config" => Ok(args
            .get(1)
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null)),
        "current_setting" => Ok(Value::text(String::new())),
        // PG `pg_catalog.*` discovery / cast helpers commonly
        // emitted by ORMs probing the server. Accept-as-no-op
        // with sensible defaults so the dump preamble doesn't
        // fail. `pg_get_serial_sequence` returns NULL (no
        // sequence — SPG has AUTO_INCREMENT instead).
        "pg_get_serial_sequence" | "pg_get_constraintdef" | "pg_get_indexdef" => Ok(Value::Null),
        // v7.37.17 (17.6 siblings) — additional pg_catalog probe
        // helpers that ORMs / migration tools emit. All return
        // NULL / empty text where PG would return real DDL text.
        // Full DDL reconstruction queues with the get_ddl surface
        // in v7.40 that reuses spgctl's DDL round-tripper.
        "pg_get_viewdef"
        | "pg_get_functiondef"
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
        "pg_size_pretty" => Ok(Value::text::<String>("0 bytes".into())),
        // pg_database_size / pg_relation_size / pg_total_relation_size:
        // monitoring dashboards + Postgres exporter emit these.
        // Return 0 — SPG doesn't yet track per-relation on-disk
        // size across hot + cold tiers with the same shape.
        "pg_database_size"
        | "pg_relation_size"
        | "pg_total_relation_size"
        | "pg_table_size"
        | "pg_indexes_size" => Ok(Value::BigInt(0)),
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
        | "has_type_privilege" => Ok(Value::Bool(true)),
        // pg_backend_pid — session identifier. SPG uses u64 slot
        // ids; return a low deterministic value for embedded runs.
        "pg_backend_pid" => Ok(Value::Int(1)),
        // pg_conf_load_time / pg_postmaster_start_time — return
        // process start time in the embedded case; wire-layer has
        // real timestamps.
        "pg_conf_load_time" | "pg_postmaster_start_time" => Ok(Value::Null),
        // pg_notify(channel, payload) — LISTEN/NOTIFY delivery.
        // SPG has no async notification channel yet; accept + return
        // void (NULL).
        "pg_notify" => Ok(Value::Null),
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
            Some(Value::Text(s)) => {
                let escaped = s.replace('"', "\"\"");
                Ok(Value::text(alloc::format!("\"{escaped}\"")))
            }
            Some(Value::Null) | None => Ok(Value::Null),
            Some(other) => Ok(Value::text(alloc::format!("\"{other:?}\""))),
        },
        "quote_literal" => match args.first() {
            Some(Value::Null) | None => Ok(Value::Null),
            Some(Value::Text(s)) => {
                let escaped = s.replace('\'', "''");
                Ok(Value::text(alloc::format!("'{escaped}'")))
            }
            Some(other) => Ok(Value::text(alloc::format!("'{other:?}'"))),
        },
        "quote_nullable" => match args.first() {
            None | Some(Value::Null) => Ok(Value::text::<String>("NULL".into())),
            Some(Value::Text(s)) => {
                let escaped = s.replace('\'', "''");
                Ok(Value::text(alloc::format!("'{escaped}'")))
            }
            Some(other) => Ok(Value::text(alloc::format!("'{other:?}'"))),
        },
        // format_type(type_oid[, typmod]) — returns the canonical
        // display name of a type. Real implementation walks the
        // pg_type oid map; for parse-through we return NULL when
        // the oid is unknown.
        "format_type" => Ok(Value::text::<String>("unknown".into())),
        // obj_description / col_description / shobj_description —
        // COMMENT ON reader helpers. SPG doesn't yet retain
        // comments in the catalog; return NULL.
        "obj_description" | "col_description" | "shobj_description" => Ok(Value::Null),
        // to_regclass / to_regtype / to_regnamespace / to_regproc:
        // string → oid lookup. Return NULL until the OID reverse-
        // resolver is wired (queues with system_catalog v7.40
        // widening).
        "to_regclass"
        | "to_regtype"
        | "to_regnamespace"
        | "to_regproc"
        | "to_regprocedure"
        | "to_regoperator"
        | "to_regrole" => Ok(Value::Null),
        // pg_client_encoding — SPG always speaks UTF8.
        "pg_client_encoding" => Ok(Value::text::<String>("UTF8".into())),
        // pg_is_in_recovery / pg_is_wal_replay_paused — replication /
        // recovery status probes. SPG is primary-only in the drop-in
        // model.
        "pg_is_in_recovery" | "pg_is_wal_replay_paused" => Ok(Value::Bool(false)),
        // pg_wal_lsn_diff — WAL byte-position arithmetic. Return 0
        // until real LSN types land.
        "pg_wal_lsn_diff" => Ok(Value::BigInt(0)),
        "version" => Ok(Value::text("PostgreSQL 16 (SPG-compat)")),
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
        "pg_current_wal_lsn"
        | "pg_current_wal_flush_lsn"
        | "pg_current_wal_insert_lsn"
        | "pg_last_wal_receive_lsn"
        | "pg_last_wal_replay_lsn" => Ok(Value::Null),
        // pg_last_xact_replay_timestamp — replica lag probe.
        "pg_last_xact_replay_timestamp" => Ok(Value::Null),
        // Range comparison helpers.
        "lower_inc" | "upper_inc" | "lower_inf" | "upper_inf" => Ok(Value::Bool(false)),
        // Container empty check for ranges.
        "isempty" => Ok(Value::Bool(false)),
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
        "lastval" => Ok(Value::Null),
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
        other => Err(EvalError::TypeMismatch {
            detail: format!("unknown function `{other}`"),
        }),
    }
}

// v7.38 P0 元机制 A — SQL-facing handles for the injection_points
// framework. Tests call these via `SELECT spg_injection_attach(...)` /
// `_wakeup` / `_detach`. With the `injection-points` feature OFF
// (release builds) all three return an error so a production SPG
// can't be coerced into deadlocking via SQL.

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
