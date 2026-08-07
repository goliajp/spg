//! v7.39 (round 262) — the inet / cidr surface, swept 69 cases against
//! live PG18.4 (2026-07-20). The operator and function family was
//! already solid (48/53 first pass: host / network / masklen /
//! set_masklen / broadcast / netmask / hostmask / family / abbrev, the
//! containment and overlap operators, the bitwise and arithmetic
//! operators, inet_merge, inet_same_family, macaddr and macaddr8). The
//! gaps:
//!
//!   * `inet::cidr` and `cidr::inet` did not exist — ordinary SQL raised
//!     an internal storage type mismatch. PG's rules, probed:
//!     `inet::cidr` keeps the mask length (defaulting to the family's
//!     full width) and ZEROES the host bits, so `192.168.1.5/24` becomes
//!     `192.168.1.0/24`; `cidr::inet` passes through unchanged.
//!   * A CIDR rendered through the INET formatter, which omits a
//!     full-width mask — so `'192.168.1.5'::inet::cidr` printed
//!     `192.168.1.5` where PG shows `192.168.1.5/32`. A cidr always
//!     carries its mask length; an inet only shows a partial one.
//!   * The inet input error used SPG's internal type spelling and a
//!     column suffix (`invalid input syntax for INET: "x" (column …)`)
//!     rather than PG's `invalid input syntax for type inet: "x"`. The
//!     cidr arm already had it right.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Bool(b) => String::from(if *b { "t" } else { "f" }),
            other => spg_engine::eval::value_to_text(other),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn inet_and_cidr_cast_into_each_other() {
    let mut e = Engine::new();
    for (sql, want) in [
        // The mask defaults to the family's full width, and the host
        // bits are zeroed.
        ("SELECT '192.168.1.5'::inet::cidr", "192.168.1.5/32"),
        ("SELECT '192.168.1.5/24'::inet::cidr", "192.168.1.0/24"),
        ("SELECT '::1'::inet::cidr", "::1/128"),
        // The reverse is a pass-through.
        ("SELECT '192.168.1.0/24'::cidr::inet", "192.168.1.0/24"),
        ("SELECT '10.0.0.0/8'::cidr::inet", "10.0.0.0/8"),
        ("SELECT '2001:db8::/32'::cidr::inet", "2001:db8::/32"),
        // Types and downstream use.
        ("SELECT pg_typeof('192.168.1.5'::inet::cidr)", "cidr"),
        ("SELECT pg_typeof('192.168.1.0/24'::cidr::inet)", "inet"),
        (
            "SELECT '192.168.1.5/24'::inet::cidr::text",
            "192.168.1.0/24",
        ),
        ("SELECT masklen('192.168.1.5'::inet::cidr)", "32"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn a_cidr_always_shows_its_mask_length() {
    let mut e = Engine::new();
    // Full-width: the cidr keeps `/32` and `/128`, the inet drops them.
    assert_eq!(
        one(&mut e, "SELECT '192.168.1.5'::inet::cidr"),
        "192.168.1.5/32"
    );
    assert_eq!(one(&mut e, "SELECT '192.168.1.5'::inet"), "192.168.1.5");
    assert_eq!(one(&mut e, "SELECT '::1'::inet::cidr"), "::1/128");
    assert_eq!(one(&mut e, "SELECT '::1'::inet"), "::1");
    // Partial masks are shown by both.
    assert_eq!(
        one(&mut e, "SELECT '192.168.1.0/24'::cidr"),
        "192.168.1.0/24"
    );
    assert_eq!(
        one(&mut e, "SELECT '192.168.1.5/24'::inet"),
        "192.168.1.5/24"
    );
}

#[test]
fn input_errors_take_pgs_wordings() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT '999.1.1.1'::inet",
            "invalid input syntax for type inet: \"999.1.1.1\"",
        ),
        (
            "SELECT '192.168.1.5/33'::inet",
            "invalid input syntax for type inet: \"192.168.1.5/33\"",
        ),
        (
            "SELECT 'notanip'::inet",
            "invalid input syntax for type inet: \"notanip\"",
        ),
        (
            "SELECT 'notanip'::cidr",
            "invalid input syntax for type cidr: \"notanip\"",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql} → {got}");
        // The internal spelling and the column suffix are gone.
        assert!(!got.contains("for INET"), "{sql} → {got}");
        assert!(!got.contains("(column"), "{sql} → {got}");
    }
    // A cidr with host bits set is its own error.
    let got = err(&mut e, "SELECT '192.168.1.5/24'::cidr");
    assert!(got.contains("invalid cidr value"), "{got}");
}

#[test]
fn the_network_core_is_unchanged() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT host('192.168.1.5/24'::inet)", "192.168.1.5"),
        ("SELECT network('192.168.1.5/24'::inet)", "192.168.1.0/24"),
        ("SELECT masklen('192.168.1.5/24'::inet)", "24"),
        (
            "SELECT set_masklen('192.168.1.5/24'::inet, 16)",
            "192.168.1.5/16",
        ),
        (
            "SELECT broadcast('192.168.1.5/24'::inet)",
            "192.168.1.255/24",
        ),
        ("SELECT netmask('192.168.1.5/24'::inet)", "255.255.255.0"),
        ("SELECT hostmask('192.168.1.5/24'::inet)", "0.0.0.255"),
        ("SELECT family('192.168.1.5'::inet)", "4"),
        ("SELECT family('::1'::inet)", "6"),
        // PG abbreviates a cidr by dropping trailing zero octets (probed).
        ("SELECT abbrev('192.168.1.0/24'::cidr)", "192.168.1/24"),
        ("SELECT '192.168.1.5'::inet << '192.168.1.0/24'::inet", "t"),
        ("SELECT '192.168.1.5'::inet << '10.0.0.0/8'::inet", "f"),
        (
            "SELECT '192.168.1.0/24'::inet >>= '192.168.1.0/24'::inet",
            "t",
        ),
        (
            "SELECT '192.168.1.0/24'::inet && '192.168.1.128/25'::inet",
            "t",
        ),
        ("SELECT '192.168.1.0/24'::inet && '10.0.0.0/8'::inet", "f"),
        ("SELECT '192.168.1.5'::inet + 10", "192.168.1.15"),
        ("SELECT '192.168.1.20'::inet - '192.168.1.5'::inet", "15"),
        (
            "SELECT inet_same_family('192.168.1.5'::inet, '::1'::inet)",
            "f",
        ),
        ("SELECT '08:00:2b:01:02:03'::macaddr", "08:00:2b:01:02:03"),
        (
            "SELECT trunc('08:00:2b:01:02:03'::macaddr)",
            "08:00:2b:00:00:00",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}
