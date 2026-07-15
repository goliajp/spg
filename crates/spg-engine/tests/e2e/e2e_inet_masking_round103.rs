//! v7.39 (read01 round 103) — IPv4 `network()` bit-level masking.
//!
//! `network(inet '192.168.1.130/25')` returned `192.168.1.130/25` — SPG only
//! zeroed whole trailing octets, so any non-octet-aligned prefix (/25, /26, …)
//! kept its host bits. PG masks bit-for-bit: `192.168.1.128/25`. Fixed by
//! parsing the dotted quad to a u32 and masking the prefix. Locked
//! byte-identical against live PG 18.4.
//!
//! (`netmask`/`hostmask` were NOT changed: PG returns `inet`, whose NATIVE
//! text omits the `/32` — SPG's text form already matches that. The `/32` only
//! appears under an explicit `::text` cast in PG, which SPG's text return can't
//! reproduce; that cast-only nuance is deferred.)

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn network_masks_partial_octet_prefixes() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT network(inet '192.168.1.130/25')::text"), "192.168.1.128/25");
    assert_eq!(text(&mut e, "SELECT network(inet '192.168.1.200/26')::text"), "192.168.1.192/26");
    assert_eq!(text(&mut e, "SELECT network(inet '10.5.6.7/12')::text"), "10.0.0.0/12");
    assert_eq!(text(&mut e, "SELECT network(inet '172.16.5.4/20')::text"), "172.16.0.0/20");
}

#[test]
fn network_octet_aligned_and_full_still_work() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT network(inet '10.1.2.3/8')::text"), "10.0.0.0/8");
    assert_eq!(text(&mut e, "SELECT network(inet '192.168.1.5/24')::text"), "192.168.1.0/24");
    assert_eq!(text(&mut e, "SELECT network(inet '192.168.1.5')::text"), "192.168.1.5/32");
}

#[test]
fn netmask_hostmask_native_form_unchanged() {
    // Regression guard: PG's native (uncast) netmask/hostmask text omits /32,
    // which SPG's text form matches — don't regress that.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT netmask(inet '192.168.1.130/25')"), "255.255.255.128");
    assert_eq!(text(&mut e, "SELECT hostmask(inet '192.168.1.130/25')"), "0.0.0.127");
}
