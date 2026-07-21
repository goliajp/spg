//! v7.39 (round 306, V28) — the large-object DESCRIPTOR family.
//!
//! Round 287 landed the bytea-shaped calls (`lo_from_bytea` / `lo_get` /
//! `lo_put` / `lo_unlink` / `lo_create`) and recorded the descriptor
//! half as a residual, for one reason: `lo_open` hands back a handle
//! that is per-session state, and round 283's lesson was that per-session
//! state has to sit on the session bag from the first commit rather than
//! be built process-wide and unpicked later (r277, r279 and r283 each
//! paid that bill). So the descriptor table lives on `SessionBag`, and
//! PG additionally scopes it to the transaction.
//!
//! Every expectation here was read off live PG 18.4 (2026-07-21),
//! including the two that are counter-intuitive:
//!
//!   * the mode constants are INV_WRITE = 0x20000 and INV_READ =
//!     0x40000 — the opposite way round from the guess;
//!   * reading needs no permission at all. A descriptor opened
//!     write-only reads fine; only writes check the flag.

use spg_engine::{Engine, QueryResult};

fn val(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

const INV_WRITE: &str = "131072";
const INV_READ: &str = "262144";

/// A 16-byte object, bytes 0x00..0x0f, and its oid.
fn seeded() -> (Engine, String) {
    let mut e = Engine::new();
    let oid = val(
        &mut e,
        "SELECT lo_from_bytea(0, '\\x000102030405060708090a0b0c0d0e0f'::bytea)",
    );
    (e, oid)
}

#[test]
fn descriptors_number_from_zero_and_track_position() {
    let (mut e, oid) = seeded();
    e.execute("BEGIN").unwrap();
    assert_eq!(val(&mut e, &format!("SELECT lo_open({oid}, {INV_READ})")), "0");
    assert_eq!(val(&mut e, &format!("SELECT lo_open({oid}, {INV_READ})")), "1");
    // Reading advances the position.
    assert_eq!(val(&mut e, "SELECT loread(0, 4)"), "\\x00010203");
    assert_eq!(val(&mut e, "SELECT lo_tell(0)"), "4");
    // The second descriptor has its own position, untouched by the first.
    assert_eq!(val(&mut e, "SELECT lo_tell(1)"), "0");
    e.execute("COMMIT").unwrap();
}

#[test]
fn seek_takes_all_three_whence_values() {
    let (mut e, oid) = seeded();
    e.execute("BEGIN").unwrap();
    val(&mut e, &format!("SELECT lo_open({oid}, {INV_READ})"));
    assert_eq!(val(&mut e, "SELECT lo_lseek(0, 2, 0)"), "2"); // SEEK_SET
    assert_eq!(val(&mut e, "SELECT lo_lseek(0, 3, 1)"), "5"); // SEEK_CUR
    assert_eq!(val(&mut e, "SELECT lo_lseek(0, -2, 2)"), "14"); // SEEK_END
    // A read at the tail comes back short rather than padded.
    assert_eq!(val(&mut e, "SELECT loread(0, 100)"), "\\x0e0f");
    // The 64-bit spellings answer the same numbers.
    assert_eq!(val(&mut e, "SELECT lo_lseek64(0, 2, 0)"), "2");
    assert_eq!(val(&mut e, "SELECT lo_tell64(0)"), "2");
    // A target before the start is refused, quoting the target.
    assert!(
        err(&mut e, "SELECT lo_lseek(0, -5, 0)")
            .contains("invalid large object seek target: -5")
    );
    e.execute("ROLLBACK").unwrap();
}

#[test]
fn reads_need_no_permission_but_writes_do() {
    let (mut e, oid) = seeded();
    e.execute("BEGIN").unwrap();
    // Write-only descriptor: reading is still allowed (measured).
    val(&mut e, &format!("SELECT lo_open({oid}, {INV_WRITE})"));
    assert_eq!(val(&mut e, "SELECT loread(0, 3)"), "\\x000102");
    e.execute("ROLLBACK").unwrap();

    // Read-only descriptor: writing is not. Each refusal aborts the
    // block (PG does the same), so they take a transaction each.
    e.execute("BEGIN").unwrap();
    val(&mut e, &format!("SELECT lo_open({oid}, {INV_READ})"));
    assert!(
        err(&mut e, "SELECT lowrite(0, '\\x99'::bytea)")
            .contains("large object descriptor 0 was not opened for writing")
    );
    e.execute("ROLLBACK").unwrap();

    e.execute("BEGIN").unwrap();
    val(&mut e, &format!("SELECT lo_open({oid}, {INV_READ})"));
    assert!(
        err(&mut e, "SELECT lo_truncate(0, 2)")
            .contains("large object descriptor 0 was not opened for writing")
    );
    e.execute("ROLLBACK").unwrap();
}

#[test]
fn writing_overwrites_in_place_and_zero_fills_a_gap() {
    let (mut e, oid) = seeded();
    e.execute("BEGIN").unwrap();
    val(&mut e, &format!("SELECT lo_open({oid}, {INV_WRITE})"));
    assert_eq!(val(&mut e, "SELECT lowrite(0, '\\xffee'::bytea)"), "2");
    assert_eq!(val(&mut e, "SELECT lo_tell(0)"), "2");
    assert_eq!(val(&mut e, "SELECT lo_close(0)"), "0");
    e.execute("COMMIT").unwrap();
    assert_eq!(
        val(&mut e, &format!("SELECT encode(lo_get({oid}), 'hex')")),
        "ffee02030405060708090a0b0c0d0e0f"
    );

    // Seeking past the end and writing fills the gap with zeroes.
    e.execute("BEGIN").unwrap();
    val(&mut e, &format!("SELECT lo_open({oid}, {INV_WRITE})"));
    val(&mut e, "SELECT lo_lseek(0, 20, 0)");
    val(&mut e, "SELECT lowrite(0, '\\xaabb'::bytea)");
    val(&mut e, "SELECT lo_close(0)");
    e.execute("COMMIT").unwrap();
    assert_eq!(
        val(&mut e, &format!("SELECT encode(lo_get({oid}), 'hex')")),
        "ffee02030405060708090a0b0c0d0e0f00000000aabb"
    );
}

/// PG's truncate sets the size in both directions — it grows with zero
/// fill just as readily as it shortens.
#[test]
fn truncate_shortens_and_also_grows() {
    let (mut e, oid) = seeded();
    e.execute("BEGIN").unwrap();
    val(&mut e, &format!("SELECT lo_open({oid}, {INV_WRITE})"));
    assert_eq!(val(&mut e, "SELECT lo_truncate(0, 4)"), "0");
    val(&mut e, "SELECT lo_close(0)");
    e.execute("COMMIT").unwrap();
    assert_eq!(
        val(&mut e, &format!("SELECT encode(lo_get({oid}), 'hex')")),
        "00010203"
    );

    e.execute("BEGIN").unwrap();
    val(&mut e, &format!("SELECT lo_open({oid}, {INV_WRITE})"));
    assert_eq!(val(&mut e, "SELECT lo_truncate(0, 8)"), "0");
    val(&mut e, "SELECT lo_close(0)");
    e.execute("COMMIT").unwrap();
    assert_eq!(
        val(&mut e, &format!("SELECT encode(lo_get({oid}), 'hex')")),
        "0001020300000000"
    );
}

/// The lifetime rule: a descriptor belongs to the transaction that
/// opened it. This is the half that made V28 wait for its own round.
#[test]
fn descriptors_do_not_outlive_their_transaction() {
    let (mut e, oid) = seeded();
    e.execute("BEGIN").unwrap();
    val(&mut e, &format!("SELECT lo_open({oid}, {INV_READ})"));
    assert_eq!(val(&mut e, "SELECT loread(0, 1)"), "\\x00");
    e.execute("COMMIT").unwrap();
    assert!(err(&mut e, "SELECT loread(0, 1)").contains("invalid large-object descriptor: 0"));

    // Same after a ROLLBACK.
    e.execute("BEGIN").unwrap();
    val(&mut e, &format!("SELECT lo_open({oid}, {INV_READ})"));
    e.execute("ROLLBACK").unwrap();
    assert!(err(&mut e, "SELECT lo_tell(0)").contains("invalid large-object descriptor: 0"));

    // And in autocommit the implicit transaction ends with the
    // statement, so the handle is gone by the next one — PG hands back
    // a descriptor here too, it is simply already dead.
    assert_eq!(val(&mut e, &format!("SELECT lo_open({oid}, {INV_READ})")), "0");
    assert!(err(&mut e, "SELECT loread(0, 1)").contains("invalid large-object descriptor: 0"));

    // Numbering restarts from 0 in the next transaction.
    e.execute("BEGIN").unwrap();
    assert_eq!(val(&mut e, &format!("SELECT lo_open({oid}, {INV_READ})")), "0");
    e.execute("COMMIT").unwrap();
}

#[test]
fn closing_and_bad_arguments_take_pgs_wording() {
    let (mut e, oid) = seeded();
    // One transaction per expected error: a failed statement aborts the
    // block, in SPG as in PG.
    e.execute("BEGIN").unwrap();
    val(&mut e, &format!("SELECT lo_open({oid}, {INV_READ})"));
    assert_eq!(val(&mut e, "SELECT lo_close(0)"), "0");
    assert!(err(&mut e, "SELECT loread(0, 1)").contains("invalid large-object descriptor: 0"));
    e.execute("ROLLBACK").unwrap();

    // Closing an already-closed descriptor takes the same wording.
    e.execute("BEGIN").unwrap();
    val(&mut e, &format!("SELECT lo_open({oid}, {INV_READ})"));
    val(&mut e, "SELECT lo_close(0)");
    assert!(err(&mut e, "SELECT lo_close(0)").contains("invalid large-object descriptor: 0"));
    e.execute("ROLLBACK").unwrap();

    // A mode with neither bit set is refused, quoting the mode.
    assert!(
        err(&mut e, &format!("SELECT lo_open({oid}, 0)"))
            .contains("invalid flags for opening a large object: 0")
    );
    assert!(
        err(&mut e, &format!("SELECT lo_open(999999, {INV_READ})"))
            .contains("large object 999999 does not exist")
    );
}

/// A negative length reads nothing and does NOT error — PG answers an
/// empty bytea.
#[test]
fn a_negative_read_length_yields_an_empty_bytea() {
    let (mut e, oid) = seeded();
    e.execute("BEGIN").unwrap();
    val(&mut e, &format!("SELECT lo_open({oid}, {INV_READ})"));
    assert_eq!(val(&mut e, "SELECT loread(0, -1)"), "\\x");
    e.execute("ROLLBACK").unwrap();
}

/// The reason the table sits on the session bag: two connections share
/// one Engine, and a descriptor opened by one must be invisible to the
/// other. Guards the r277/r279/r283 class of bug at the point it would
/// have been introduced rather than after.
///
/// Driven through `execute_in` with a slot per session, because that is
/// what the server does — `execute()` alone always lands on IMPLICIT_TX,
/// so two sessions would share one transaction and the test would be
/// measuring the wrong thing.
#[test]
fn descriptors_are_per_session() {
    let (mut e, oid) = seeded();
    let tx_a = e.alloc_tx_id();
    let tx_b = e.alloc_tx_id();

    e.set_current_session(1);
    e.execute_in("BEGIN", tx_a).unwrap();
    let opened = e
        .execute_in(&format!("SELECT lo_open({oid}, {INV_READ})"), tx_a)
        .unwrap();
    let QueryResult::Rows { rows, .. } = opened else {
        panic!("expected rows")
    };
    assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "0");
    e.execute_in("SELECT loread(0, 4)", tx_a).unwrap();

    // Session 2 has its own table: session 1's descriptor is invisible,
    // and its own numbering starts at 0 again.
    e.set_current_session(2);
    assert!(
        format!("{:?}", e.execute_in("SELECT lo_tell(0)", tx_b).unwrap_err())
            .contains("invalid large-object descriptor: 0")
    );
    e.execute_in("BEGIN", tx_b).unwrap();
    let opened_b = e
        .execute_in(&format!("SELECT lo_open({oid}, {INV_READ})"), tx_b)
        .unwrap();
    let QueryResult::Rows { rows, .. } = opened_b else {
        panic!("expected rows")
    };
    assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "0");
    e.execute_in("ROLLBACK", tx_b).unwrap();

    // Session 1's position survived the excursion untouched.
    e.set_current_session(1);
    let told = e.execute_in("SELECT lo_tell(0)", tx_a).unwrap();
    let QueryResult::Rows { rows, .. } = told else {
        panic!("expected rows")
    };
    assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "4");
    e.execute_in("ROLLBACK", tx_a).unwrap();
}
