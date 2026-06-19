//! v7.17.0 Phase 7 — INET / CIDR text helpers.
//!
//! SPG stores network address types as Text. The host() / network() /
//! masklen() helpers parse the textual `addr[/mask]` form and return
//! the constituent pieces, matching PG's contract for the dominant
//! customer surface (Django ORM / Rails ORM normalisation). The
//! `inet_op_bool_result` arm backs the INET/CIDR containment operators.
//! Split out of `eval.rs` (cut 24).

use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;

use spg_sql::ast::BinOp;
use spg_storage::Value;

use super::EvalError;

pub(super) fn inet_host(args: &[Value]) -> Result<Value, EvalError> {
    let s = match args {
        [Value::Text(s)] => s.clone(),
        [Value::Null] => return Ok(Value::Null),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("host() takes one TEXT arg, got {} args", args.len()),
            });
        }
    };
    let host = s.split('/').next().unwrap_or("").to_string();
    Ok(Value::Text(host))
}

pub(super) fn inet_network(args: &[Value]) -> Result<Value, EvalError> {
    let s = match args {
        [Value::Text(s)] => s.clone(),
        [Value::Null] => return Ok(Value::Null),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("network() takes one TEXT arg, got {} args", args.len()),
            });
        }
    };
    // For a `host/mask` form return the masked-network address.
    // SPG ships the simple "drop trailing octets per byte" path
    // for IPv4; full bit-level masking is out of v7.17 scope.
    let mut split = s.splitn(2, '/');
    let host = split.next().unwrap_or("").to_string();
    let mask: u32 = split.next().and_then(|m| m.parse().ok()).unwrap_or(32);
    if !host.contains('.') {
        // IPv6 / MACADDR — return the input unmasked.
        return Ok(Value::Text(s));
    }
    let octets: Vec<&str> = host.split('.').collect();
    if octets.len() != 4 {
        return Ok(Value::Text(s));
    }
    let keep_bytes = ((mask + 7) / 8) as usize;
    let mut out = alloc::string::String::new();
    for (i, oct) in octets.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        if i < keep_bytes {
            out.push_str(oct);
        } else {
            out.push('0');
        }
    }
    out.push('/');
    out.push_str(&mask.to_string());
    Ok(Value::Text(out))
}

pub(super) fn inet_masklen(args: &[Value]) -> Result<Value, EvalError> {
    let s = match args {
        [Value::Text(s)] => s.clone(),
        [Value::Null] => return Ok(Value::Null),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("masklen() takes one TEXT arg, got {} args", args.len()),
            });
        }
    };
    let mask: i32 = s
        .split_once('/')
        .and_then(|(_, m)| m.parse().ok())
        .unwrap_or(32);
    Ok(Value::Int(mask))
}

// ─── v7.17.0 Phase 3.P0-47 — INET / CIDR containment + overlap ────────
//
// SPG stores INET / CIDR as Text (Phase 7 design); these helpers parse
// the textual `addr[/mask]` form into a (family, bytes, prefix_bits)
// triple and implement PG's network-comparison operators on that
// representation.
//
// PG semantics:
//   * `<<`  — strictly contained-in (LHS ⊊ RHS)
//   * `<<=` — contained-in-or-equal (LHS ⊆ RHS)
//   * `>>`, `>>=` — mirrors of the above
//   * `&&`  — overlap (either LHS ⊆ RHS or RHS ⊆ LHS)
//
// NULL on either side → NULL (3VL). Mixed family (v4 vs v6) is never
// contained / never overlaps but is not an error — same as PG.

/// Parsed inet network: address bytes (4 for v4, 16 for v6) and the
/// network prefix length in bits.
struct InetNet {
    bytes: [u8; 16],
    /// 4 for IPv4, 16 for IPv6.
    family_bytes: u8,
    /// 0..=32 for v4, 0..=128 for v6.
    prefix_bits: u8,
}

fn parse_inet_text(s: &str) -> Option<InetNet> {
    let mut split = s.splitn(2, '/');
    let host = split.next()?;
    let mask_str = split.next();
    if host.contains(':') {
        let bytes = parse_ipv6(host)?;
        let prefix_bits = match mask_str {
            Some(m) => m.parse::<u8>().ok().filter(|&n| n <= 128)?,
            None => 128,
        };
        let mut out = [0u8; 16];
        out.copy_from_slice(&bytes);
        Some(InetNet {
            bytes: out,
            family_bytes: 16,
            prefix_bits,
        })
    } else {
        let bytes = parse_ipv4(host)?;
        let prefix_bits = match mask_str {
            Some(m) => m.parse::<u8>().ok().filter(|&n| n <= 32)?,
            None => 32,
        };
        let mut out = [0u8; 16];
        out[..4].copy_from_slice(&bytes);
        Some(InetNet {
            bytes: out,
            family_bytes: 4,
            prefix_bits,
        })
    }
}

fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut out = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p.parse::<u8>().ok()?;
    }
    Some(out)
}

fn parse_ipv6(s: &str) -> Option<[u8; 16]> {
    // Split on the `::` shorthand at most once.
    let (head, tail) = match s.find("::") {
        Some(idx) => (&s[..idx], Some(&s[idx + 2..])),
        None => (s, None),
    };
    let head_groups: Vec<&str> = if head.is_empty() {
        Vec::new()
    } else {
        head.split(':').collect()
    };
    let tail_groups: Vec<&str> = match tail {
        Some(t) if !t.is_empty() => t.split(':').collect(),
        _ => Vec::new(),
    };
    let head_len = head_groups.len();
    let tail_len = tail_groups.len();
    // Without `::` we need exactly 8 groups; with `::` we need ≤ 7.
    if tail.is_none() {
        if head_len != 8 {
            return None;
        }
    } else if head_len + tail_len > 7 {
        return None;
    }
    let mut words = [0u16; 8];
    for (i, g) in head_groups.iter().enumerate() {
        words[i] = u16::from_str_radix(g, 16).ok()?;
    }
    let tail_start = 8 - tail_len;
    for (i, g) in tail_groups.iter().enumerate() {
        words[tail_start + i] = u16::from_str_radix(g, 16).ok()?;
    }
    let mut out = [0u8; 16];
    for (i, w) in words.iter().enumerate() {
        out[i * 2] = (w >> 8) as u8;
        out[i * 2 + 1] = (w & 0xff) as u8;
    }
    Some(out)
}

/// Compare the first `prefix_bits` bits of `a` and `b`. Returns true if
/// they match. `prefix_bits` must not exceed the family size.
fn network_prefix_eq(a: &InetNet, b: &InetNet, prefix_bits: u8) -> bool {
    let full_bytes = (prefix_bits / 8) as usize;
    if a.bytes[..full_bytes] != b.bytes[..full_bytes] {
        return false;
    }
    let extra = prefix_bits % 8;
    if extra == 0 {
        return true;
    }
    let mask: u8 = 0xff << (8 - extra);
    (a.bytes[full_bytes] & mask) == (b.bytes[full_bytes] & mask)
}

/// True iff network `a` is fully contained in network `b` (a ⊆ b).
fn inet_contained_eq(a: &InetNet, b: &InetNet) -> bool {
    if a.family_bytes != b.family_bytes {
        return false;
    }
    if a.prefix_bits < b.prefix_bits {
        return false;
    }
    network_prefix_eq(a, b, b.prefix_bits)
}

/// True iff a and b are exactly the same network (same family + same
/// prefix + same masked address).
fn inet_networks_equal(a: &InetNet, b: &InetNet) -> bool {
    if a.family_bytes != b.family_bytes {
        return false;
    }
    if a.prefix_bits != b.prefix_bits {
        return false;
    }
    network_prefix_eq(a, b, a.prefix_bits)
}

pub(super) fn inet_op_bool_result(op: BinOp, l: &Value, r: &Value) -> Result<Value, EvalError> {
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    // v7.37.5 ζ-A — INET/CIDR are first-class typed Values. Accept
    // both the typed shape (no re-parse) and the legacy Text shape
    // (re-parse on demand, since old columns / `'1.2.3.4'::text`
    // literals still flow through here).
    let to_inet = |v: &Value| -> Result<InetNet, EvalError> {
        match v {
            Value::Inet { family, bits, addr } | Value::Cidr { family, bits, addr } => {
                Ok(InetNet {
                    bytes: *addr,
                    family_bytes: *family,
                    prefix_bits: *bits,
                })
            }
            Value::Text(s) => parse_inet_text(s).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("invalid inet text: {s:?}"),
            }),
            _ => Err(EvalError::TypeMismatch {
                detail: format!(
                    "inet operator requires INET/CIDR/TEXT operands, got {:?}",
                    v.data_type()
                ),
            }),
        }
    };
    let a = to_inet(l)?;
    let b = to_inet(r)?;
    let result = match op {
        BinOp::InetContainedByEq => inet_contained_eq(&a, &b),
        BinOp::InetContainedBy => inet_contained_eq(&a, &b) && !inet_networks_equal(&a, &b),
        BinOp::InetContainsEq => inet_contained_eq(&b, &a),
        BinOp::InetContains => inet_contained_eq(&b, &a) && !inet_networks_equal(&a, &b),
        BinOp::InetOverlap => inet_contained_eq(&a, &b) || inet_contained_eq(&b, &a),
        _ => unreachable!("inet_op_bool_result called with non-inet op"),
    };
    Ok(Value::Bool(result))
}
