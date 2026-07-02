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

pub(super) fn inet_host(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
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
    Ok(Value::text(host))
}

pub(super) fn inet_network(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
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
        return Ok(Value::text(s));
    }
    let octets: Vec<&str> = host.split('.').collect();
    if octets.len() != 4 {
        return Ok(Value::text(s));
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
    Ok(Value::text(out))
}

/// v7.37.17 (17.6 siblings) — family(inet) returns 4 for IPv4,
/// 6 for IPv6.
pub(super) fn inet_family(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let s = match args {
        [Value::Text(s)] => s.clone(),
        [Value::Null] => return Ok(Value::Null),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("family() takes one TEXT arg, got {} args", args.len()),
            });
        }
    };
    let host = s.split('/').next().unwrap_or("");
    if host.contains(':') {
        Ok(Value::Int(6))
    } else {
        Ok(Value::Int(4))
    }
}

/// v7.37.17 (17.6 siblings) — netmask(inet) builds the dotted-quad
/// netmask from the prefix length (IPv4 only for now).
pub(super) fn inet_netmask(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let s = match args {
        [Value::Text(s)] => s.clone(),
        [Value::Null] => return Ok(Value::Null),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("netmask() takes one TEXT arg, got {} args", args.len()),
            });
        }
    };
    let mut split = s.splitn(2, '/');
    let host = split.next().unwrap_or("");
    let mask: u32 = split.next().and_then(|m| m.parse().ok()).unwrap_or(32);
    if host.contains(':') {
        // IPv6 — punt to text passthrough until full v6 support.
        return Ok(Value::text(s));
    }
    let mask = mask.min(32);
    let bits: u32 = if mask == 0 { 0 } else { u32::MAX << (32 - mask) };
    Ok(Value::text(alloc::format!(
        "{}.{}.{}.{}",
        (bits >> 24) & 0xFF,
        (bits >> 16) & 0xFF,
        (bits >> 8) & 0xFF,
        bits & 0xFF
    )))
}

/// v7.37.17 (17.6 siblings) — hostmask(inet) — the complement of
/// netmask (IPv4).
pub(super) fn inet_hostmask(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let s = match args {
        [Value::Text(s)] => s.clone(),
        [Value::Null] => return Ok(Value::Null),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("hostmask() takes one TEXT arg, got {} args", args.len()),
            });
        }
    };
    let mut split = s.splitn(2, '/');
    let host = split.next().unwrap_or("");
    let mask: u32 = split.next().and_then(|m| m.parse().ok()).unwrap_or(32);
    if host.contains(':') {
        return Ok(Value::text(s));
    }
    let mask = mask.min(32);
    let bits: u32 = if mask == 0 { u32::MAX } else { !(u32::MAX << (32 - mask)) };
    Ok(Value::text(alloc::format!(
        "{}.{}.{}.{}",
        (bits >> 24) & 0xFF,
        (bits >> 16) & 0xFF,
        (bits >> 8) & 0xFF,
        bits & 0xFF
    )))
}

/// v7.37.17 (17.6 siblings) — broadcast(inet) — network address
/// with host bits set to 1 (IPv4).
pub(super) fn inet_broadcast(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let s = match args {
        [Value::Text(s)] => s.clone(),
        [Value::Null] => return Ok(Value::Null),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "broadcast() takes one TEXT arg, got {} args",
                    args.len()
                ),
            });
        }
    };
    let mut split = s.splitn(2, '/');
    let host = split.next().unwrap_or("");
    let mask: u32 = split.next().and_then(|m| m.parse().ok()).unwrap_or(32);
    if host.contains(':') {
        return Ok(Value::text(s));
    }
    let octets: Vec<u32> = host
        .split('.')
        .filter_map(|o| o.parse().ok())
        .collect();
    if octets.len() != 4 {
        return Ok(Value::text(s));
    }
    let addr = (octets[0] << 24) | (octets[1] << 16) | (octets[2] << 8) | octets[3];
    let mask = mask.min(32);
    let host_bits: u32 = if mask == 0 { u32::MAX } else { !(u32::MAX << (32 - mask)) };
    let bcast = addr | host_bits;
    Ok(Value::text(alloc::format!(
        "{}.{}.{}.{}/{mask}",
        (bcast >> 24) & 0xFF,
        (bcast >> 16) & 0xFF,
        (bcast >> 8) & 0xFF,
        bcast & 0xFF
    )))
}

/// v7.37.17 (17.6 siblings) — inet_merge(a, b) — the smallest
/// network that includes both arguments (IPv4; IPv6 mixed input
/// errors like PG's "cannot merge addresses from different
/// families" when families differ, text passthrough of the first
/// arg when both are IPv6).
pub(super) fn inet_merge(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let (a, b) = match args {
        [Value::Null, _] | [_, Value::Null] => return Ok(Value::Null),
        [Value::Text(a), Value::Text(b)] => (a.as_ref(), b.as_ref()),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: "inet_merge() takes 2 TEXT args".into(),
            });
        }
    };
    let fam = |s: &str| if s.split('/').next().unwrap_or("").contains(':') { 6 } else { 4 };
    if fam(a) != fam(b) {
        return Err(EvalError::TypeMismatch {
            detail: "inet_merge(): cannot merge addresses from different families".into(),
        });
    }
    if fam(a) == 6 {
        // IPv6 — text passthrough until full v6 bit math lands.
        return Ok(Value::text(a.to_string()));
    }
    let parse = |s: &str| -> Option<(u32, u32)> {
        let mut split = s.splitn(2, '/');
        let host = split.next()?;
        let mask: u32 = split.next().and_then(|m| m.parse().ok()).unwrap_or(32);
        let octets: Vec<u32> = host.split('.').filter_map(|o| o.parse().ok()).collect();
        if octets.len() != 4 {
            return None;
        }
        Some((
            (octets[0] << 24) | (octets[1] << 16) | (octets[2] << 8) | octets[3],
            mask.min(32),
        ))
    };
    let (Some((addr_a, mask_a)), Some((addr_b, mask_b))) = (parse(a), parse(b)) else {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("inet_merge(): invalid inet input '{a}' / '{b}'"),
        });
    };
    // Common prefix length, capped by both input masks.
    let diff = addr_a ^ addr_b;
    let common = diff.leading_zeros().min(mask_a).min(mask_b);
    let net = if common == 0 { 0 } else { addr_a & (u32::MAX << (32 - common)) };
    Ok(Value::text(alloc::format!(
        "{}.{}.{}.{}/{common}",
        (net >> 24) & 0xFF,
        (net >> 16) & 0xFF,
        (net >> 8) & 0xFF,
        net & 0xFF
    )))
}

/// v7.37.17 (17.6 siblings) — macaddr8_set7bit(macaddr8) — sets
/// the 7th bit (0x02, locally-administered) of the first byte,
/// converting an EUI-64 into a modified EUI-64 for IPv6 autoconf.
pub(super) fn macaddr8_set7bit(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let s = match args {
        [Value::Text(s)] => s.clone(),
        [Value::Null] => return Ok(Value::Null),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "macaddr8_set7bit() takes one TEXT arg, got {} args",
                    args.len()
                ),
            });
        }
    };
    let bytes: Vec<u8> = s
        .split(|c| c == ':' || c == '-')
        .filter_map(|part| u8::from_str_radix(part, 16).ok())
        .collect();
    if bytes.len() != 8 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("macaddr8_set7bit(): invalid macaddr8 '{s}'"),
        });
    }
    let mut out = bytes;
    out[0] |= 0x02;
    let hex: Vec<alloc::string::String> =
        out.iter().map(|b| alloc::format!("{b:02x}")).collect();
    Ok(Value::text(hex.join(":")))
}

/// v7.37.17 (17.6 siblings) — inet_same_family(a, b).
pub(super) fn inet_same_family(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    match args {
        [Value::Null, _] | [_, Value::Null] => Ok(Value::Null),
        [Value::Text(a), Value::Text(b)] => {
            let fam = |s: &str| if s.split('/').next().unwrap_or("").contains(':') { 6 } else { 4 };
            Ok(Value::Bool(fam(a) == fam(b)))
        }
        _ => Err(EvalError::TypeMismatch {
            detail: "inet_same_family() takes 2 TEXT args".into(),
        }),
    }
}

pub(super) fn inet_masklen(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
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

pub(super) fn inet_op_bool_result(
    op: BinOp,
    l: &Value,
    r: &Value,
) -> Result<Value<'static>, EvalError> {
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
