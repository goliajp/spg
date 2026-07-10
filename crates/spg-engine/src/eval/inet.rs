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

/// A network-address function argument: accept the legacy TEXT form OR a
/// real INET / CIDR value (rendered to its canonical text first). Fixes the
/// inet builtins erroring when handed an actual inet/cidr column value.
fn inet_arg_text(v: &Value<'_>) -> Option<alloc::string::String> {
    match v {
        Value::Text(s) => Some(s.to_string()),
        Value::Inet { family, bits, addr } | Value::Cidr { family, bits, addr } => {
            Some(crate::conversions::format_inet(*family, *bits, addr))
        }
        _ => None,
    }
}

pub(super) fn inet_host(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let s = match args {
        [Value::Text(s)] => s.clone(),
        [Value::Inet { family, bits, addr }] | [Value::Cidr { family, bits, addr }] => {
            alloc::borrow::Cow::Owned(crate::conversions::format_inet(*family, *bits, addr))
        }
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
        [Value::Inet { family, bits, addr }] | [Value::Cidr { family, bits, addr }] => {
            alloc::borrow::Cow::Owned(crate::conversions::format_inet(*family, *bits, addr))
        }
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
        [Value::Inet { family, bits, addr }] | [Value::Cidr { family, bits, addr }] => {
            alloc::borrow::Cow::Owned(crate::conversions::format_inet(*family, *bits, addr))
        }
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
        [Value::Inet { family, bits, addr }] | [Value::Cidr { family, bits, addr }] => {
            alloc::borrow::Cow::Owned(crate::conversions::format_inet(*family, *bits, addr))
        }
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
    let bits: u32 = if mask == 0 {
        0
    } else {
        u32::MAX << (32 - mask)
    };
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
        [Value::Inet { family, bits, addr }] | [Value::Cidr { family, bits, addr }] => {
            alloc::borrow::Cow::Owned(crate::conversions::format_inet(*family, *bits, addr))
        }
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
    let bits: u32 = if mask == 0 {
        u32::MAX
    } else {
        !(u32::MAX << (32 - mask))
    };
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
        [Value::Inet { family, bits, addr }] | [Value::Cidr { family, bits, addr }] => {
            alloc::borrow::Cow::Owned(crate::conversions::format_inet(*family, *bits, addr))
        }
        [Value::Null] => return Ok(Value::Null),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("broadcast() takes one TEXT arg, got {} args", args.len()),
            });
        }
    };
    let mut split = s.splitn(2, '/');
    let host = split.next().unwrap_or("");
    let mask: u32 = split.next().and_then(|m| m.parse().ok()).unwrap_or(32);
    if host.contains(':') {
        return Ok(Value::text(s));
    }
    let octets: Vec<u32> = host.split('.').filter_map(|o| o.parse().ok()).collect();
    if octets.len() != 4 {
        return Ok(Value::text(s));
    }
    let addr = (octets[0] << 24) | (octets[1] << 16) | (octets[2] << 8) | octets[3];
    let mask = mask.min(32);
    let host_bits: u32 = if mask == 0 {
        u32::MAX
    } else {
        !(u32::MAX << (32 - mask))
    };
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
    let fam = |s: &str| {
        if s.split('/').next().unwrap_or("").contains(':') {
            6
        } else {
            4
        }
    };
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
    let net = if common == 0 {
        0
    } else {
        addr_a & (u32::MAX << (32 - common))
    };
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
    // v7.38 (read01) — PG's macaddr8_set7bit takes (and returns) a macaddr8.
    // An unadorned string literal is the unknown-type form PG casts for you.
    let mut out: [u8; 8] = match args {
        [Value::Macaddr8(b)] => *b,
        [Value::Null] => return Ok(Value::Null),
        [Value::Text(s)] => {
            let bytes: Vec<u8> = s
                .split([':', '-'])
                .filter_map(|part| u8::from_str_radix(part, 16).ok())
                .collect();
            <[u8; 8]>::try_from(bytes.as_slice()).map_err(|_| EvalError::TypeMismatch {
                detail: alloc::format!("macaddr8_set7bit(): invalid macaddr8 '{s}'"),
            })?
        }
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "macaddr8_set7bit() takes one macaddr8 arg, got {} args",
                    args.len()
                ),
            });
        }
    };
    out[0] |= 0x02;
    Ok(Value::Macaddr8(out))
}

/// v7.37.17 (17.6 siblings) — inet_same_family(a, b).
pub(super) fn inet_same_family(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    // Accept real inet/cidr values (family is stored directly) as well
    // as the textual form — mirrors the other inet builtins that no
    // longer insist on TEXT.
    let fam_of = |v: &Value| -> Option<u8> {
        match v {
            Value::Inet { family, .. } | Value::Cidr { family, .. } => Some(*family),
            Value::Text(s) => Some(if s.split('/').next().unwrap_or("").contains(':') {
                6
            } else {
                4
            }),
            _ => None,
        }
    };
    match (args.first().and_then(fam_of), args.get(1).and_then(fam_of)) {
        (Some(a), Some(b)) => Ok(Value::Bool(a == b)),
        _ => Err(EvalError::TypeMismatch {
            detail: "inet_same_family() takes two inet/cidr/text arguments".into(),
        }),
    }
}

pub(super) fn inet_masklen(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let s = match args {
        [Value::Text(s)] => s.clone(),
        [Value::Inet { family, bits, addr }] | [Value::Cidr { family, bits, addr }] => {
            alloc::borrow::Cow::Owned(crate::conversions::format_inet(*family, *bits, addr))
        }
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

/// `set_masklen(inet|cidr, n)` — change the prefix length, keeping the address
/// and the argument's type. PG clamps to the family's maximum.
pub(super) fn inet_set_masklen(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let (family, addr, is_cidr) = match args.first() {
        Some(Value::Inet { family, addr, .. }) => (*family, *addr, false),
        Some(Value::Cidr { family, addr, .. }) => (*family, *addr, true),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: "set_masklen() first arg must be inet/cidr".into(),
            });
        }
    };
    let n = match args.get(1) {
        Some(Value::SmallInt(v)) => i64::from(*v),
        Some(Value::Int(v)) => i64::from(*v),
        Some(Value::BigInt(v)) => *v,
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: "set_masklen() second arg must be an integer".into(),
            });
        }
    };
    let max = if family == 4 { 32 } else { 128 };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bits = n.clamp(0, max) as u8;
    if is_cidr {
        Ok(Value::Cidr { family, bits, addr })
    } else {
        Ok(Value::Inet { family, bits, addr })
    }
}

/// `abbrev(cidr)` — the shortest text form, dropping octets past the prefix
/// (`192.168.1.0/24` → `192.168.1/24`, `10.0.0.0/8` → `10/8`). IPv6 falls
/// back to the full text form.
pub(super) fn inet_abbrev(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let (family, bits, addr, is_cidr) = match args.first() {
        Some(Value::Cidr { family, bits, addr }) => (*family, *bits, *addr, true),
        Some(Value::Inet { family, bits, addr }) => (*family, *bits, *addr, false),
        Some(Value::Null) => return Ok(Value::Null),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: "abbrev() arg must be inet/cidr".into(),
            });
        }
    };
    // abbrev(inet) keeps the full canonical text (PG: `192.168.1.0/24`
    // stays intact, a /32 host drops the mask); only abbrev(cidr) drops
    // the octets past the network prefix.
    if !is_cidr || family != 4 {
        return Ok(Value::text(crate::conversions::format_inet(
            family, bits, &addr,
        )));
    }
    let sig = ((usize::from(bits) + 7) / 8).max(1);
    let parts: alloc::vec::Vec<alloc::string::String> = addr[0..sig]
        .iter()
        .map(alloc::string::ToString::to_string)
        .collect();
    Ok(Value::text(alloc::format!("{}/{}", parts.join("."), bits)))
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

/// v7.37.17 (17.6 siblings) — MySQL INET_ATON(text) → the IPv4
/// address as a BigInt. Invalid input → NULL (MySQL semantics, not
/// an error).
pub(super) fn mysql_inet_aton(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let s = match args {
        [Value::Null] => return Ok(Value::Null),
        [Value::Text(s)] => s,
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("inet_aton() takes one TEXT arg, got {} args", args.len()),
            });
        }
    };
    match parse_ipv4(s) {
        Some(b) => Ok(Value::BigInt(
            (i64::from(b[0]) << 24)
                | (i64::from(b[1]) << 16)
                | (i64::from(b[2]) << 8)
                | i64::from(b[3]),
        )),
        None => Ok(Value::Null),
    }
}

/// v7.37.17 (17.6 siblings) — MySQL INET_NTOA(n) → dotted-quad text.
/// Out-of-range input → NULL (MySQL semantics).
pub(super) fn mysql_inet_ntoa(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let n = match args {
        [Value::Null] => return Ok(Value::Null),
        [Value::Int(n)] => i64::from(*n),
        [Value::SmallInt(n)] => i64::from(*n),
        [Value::BigInt(n)] => *n,
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "inet_ntoa() takes one integer arg, got {:?}",
                    args.first().map(|a| a.data_type())
                ),
            });
        }
    };
    if !(0..=0xFFFF_FFFF).contains(&n) {
        return Ok(Value::Null);
    }
    let n = n as u32;
    Ok(Value::text(alloc::format!(
        "{}.{}.{}.{}",
        (n >> 24) & 0xFF,
        (n >> 16) & 0xFF,
        (n >> 8) & 0xFF,
        n & 0xFF
    )))
}

/// RFC 5952 text rendering for a 16-byte IPv6 address: lowercase
/// hex, longest zero run (length ≥ 2) compressed to `::`.
fn format_ipv6(bytes: &[u8; 16]) -> alloc::string::String {
    let mut groups = [0u16; 8];
    for (i, g) in groups.iter_mut().enumerate() {
        *g = (u16::from(bytes[i * 2]) << 8) | u16::from(bytes[i * 2 + 1]);
    }
    // Longest run of zero groups (first among ties), min length 2.
    let (mut best_start, mut best_len) = (usize::MAX, 0usize);
    let mut i = 0;
    while i < 8 {
        if groups[i] == 0 {
            let start = i;
            while i < 8 && groups[i] == 0 {
                i += 1;
            }
            let len = i - start;
            if len > best_len {
                best_start = start;
                best_len = len;
            }
        } else {
            i += 1;
        }
    }
    let mut out = alloc::string::String::new();
    if best_len >= 2 {
        for (idx, g) in groups.iter().enumerate().take(best_start) {
            if idx > 0 {
                out.push(':');
            }
            out.push_str(&alloc::format!("{g:x}"));
        }
        out.push_str("::");
        for (idx, g) in groups.iter().enumerate().skip(best_start + best_len) {
            if idx > best_start + best_len {
                out.push(':');
            }
            out.push_str(&alloc::format!("{g:x}"));
        }
    } else {
        for (idx, g) in groups.iter().enumerate() {
            if idx > 0 {
                out.push(':');
            }
            out.push_str(&alloc::format!("{g:x}"));
        }
    }
    out
}

/// v7.37.17 (17.6 siblings) — MySQL INET6_ATON(text) → the address
/// as VARBINARY: 16 bytes for IPv6, 4 bytes for IPv4 (MySQL keeps
/// the shorter form for v4 input). Invalid → NULL.
pub(super) fn mysql_inet6_aton(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let s = match args {
        [Value::Null] => return Ok(Value::Null),
        [Value::Text(s)] => s,
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("inet6_aton() takes one TEXT arg, got {} args", args.len()),
            });
        }
    };
    if let Some(b) = parse_ipv4(s) {
        return Ok(Value::Bytes(alloc::borrow::Cow::Owned(b.to_vec())));
    }
    match parse_ipv6(s) {
        Some(b) => Ok(Value::Bytes(alloc::borrow::Cow::Owned(b.to_vec()))),
        None => Ok(Value::Null),
    }
}

/// v7.37.17 (17.6 siblings) — MySQL INET6_NTOA(varbinary) → text.
/// 4-byte input renders dotted-quad; 16-byte renders RFC 5952
/// compressed IPv6. Anything else → NULL.
pub(super) fn mysql_inet6_ntoa(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let bytes = match args {
        [Value::Null] => return Ok(Value::Null),
        [Value::Bytes(b)] => b.as_ref(),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "inet6_ntoa() takes one binary arg, got {:?}",
                    args.first().map(|a| a.data_type())
                ),
            });
        }
    };
    match bytes.len() {
        4 => Ok(Value::text(alloc::format!(
            "{}.{}.{}.{}",
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3]
        ))),
        16 => {
            let mut b = [0u8; 16];
            b.copy_from_slice(bytes);
            Ok(Value::text(format_ipv6(&b)))
        }
        _ => Ok(Value::Null),
    }
}

/// v7.37.17 (17.6 siblings) — MySQL IS_IPV4(text) / IS_IPV6(text).
pub(super) fn mysql_is_ipv4(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    match args {
        [Value::Null] => Ok(Value::Null),
        [Value::Text(s)] => Ok(Value::Bool(parse_ipv4(s).is_some())),
        _ => Err(EvalError::TypeMismatch {
            detail: alloc::format!("is_ipv4() takes one TEXT arg, got {} args", args.len()),
        }),
    }
}

pub(super) fn mysql_is_ipv6(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    match args {
        [Value::Null] => Ok(Value::Null),
        [Value::Text(s)] => Ok(Value::Bool(!s.is_empty() && parse_ipv6(s).is_some())),
        _ => Err(EvalError::TypeMismatch {
            detail: alloc::format!("is_ipv6() takes one TEXT arg, got {} args", args.len()),
        }),
    }
}
