//! v7.38 (read01 sweep) — INET/CIDR IPv6 addresses render in RFC 5952
//! canonical form: the longest run of consecutive zero groups (leftmost among
//! ties, ≥2 groups) compresses to `::`. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn ipv6_renders_rfc5952_canonical() {
    let mut e = Engine::new();
    let cases = [
        ("2001:db8::1", "2001:db8::1"),
        ("::1", "::1"),
        ("::", "::"),
        ("fe80::1:2:3", "fe80::1:2:3"),
        // Fully-expanded input compresses on output.
        ("2001:0db8:0000:0000:0000:ff00:0042:8329", "2001:db8::ff00:42:8329"),
        // Two zero runs: the LONGER (second, 3 groups) compresses, not the first (2).
        ("1:0:0:2:0:0:0:3", "1:0:0:2::3"),
        // CIDR bits survive the compression.
        ("2001:db8::1/64", "2001:db8::1/64"),
    ];
    for (input, want) in cases {
        assert_eq!(
            text(&mut e, &format!("SELECT ('{input}'::inet)::text")),
            want,
            "input {input}"
        );
    }
}
