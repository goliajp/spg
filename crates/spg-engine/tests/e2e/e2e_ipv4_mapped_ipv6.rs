//! v7.38 (read01, T19) — IPv4-in-IPv6: parse a trailing dotted-quad
//! (`::ffff:192.168.1.1`, `64:ff9b::192.0.2.1`) and render the IPv4-mapped
//! range (`::ffff:0:0/96`) with a dotted-quad tail, matching PG (independent of
//! the input spelling). Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn ipv4_mapped_ipv6() {
    let mut e = Engine::new();
    // Dotted-quad input, IPv4-mapped → dotted-quad render.
    assert_eq!(text(&mut e, "SELECT ('::ffff:192.168.1.1'::inet)::text"), "::ffff:192.168.1.1/128");
    // Hex input in the mapped range still renders dotted-quad.
    assert_eq!(text(&mut e, "SELECT ('::ffff:c0a8:101'::inet)::text"), "::ffff:192.168.1.1/128");
    // Non-mapped embeddings parse the dotted-quad but render hex.
    assert_eq!(text(&mut e, "SELECT ('64:ff9b::192.0.2.1'::inet)::text"), "64:ff9b::c000:201/128");
    assert_eq!(text(&mut e, "SELECT ('2001:db8::192.168.1.1'::inet)::text"), "2001:db8::c0a8:101/128");
    // Mask preserved.
    assert_eq!(text(&mut e, "SELECT ('::ffff:10.0.0.1/96'::inet)::text"), "::ffff:10.0.0.1/96");
    // Plain v6 / v4 unaffected.
    assert_eq!(text(&mut e, "SELECT ('2001:db8::1'::inet)::text"), "2001:db8::1/128");
    assert_eq!(text(&mut e, "SELECT ('192.168.1.1'::inet)::text"), "192.168.1.1/32");
}
