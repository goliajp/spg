//! v7.39 (round 287) — server-side large objects.
//!
//! `lo_from_bytea` was an unknown function and `pg_largeobject` an
//! unknown relation; nothing of the subsystem existed. This lands the
//! bytea-oriented half — the part modern drivers and pg_dump actually
//! use — on the existing catalog storage, no new storage form.
//!
//! Two things were measured rather than assumed:
//!
//!   * offsets are 0-BASED here, unlike `substring`. `lo_get(o,1,3)`
//!     over 'Hello' is 'ell'.
//!   * PG's 2 KB page split is OBSERVABLE through `pg_largeobject`: a
//!     5000-byte object is three rows of 2048 / 2048 / 904. SPG stores
//!     the whole byte string and slices it in the view, so the storage
//!     stays SPG's while the surface matches.
//!
//! The descriptor half (lo_open / loread / lowrite / lo_lseek /
//! lo_tell / lo_close / lo_truncate) is NOT here: those need
//! transaction-scoped per-session descriptors, and after round 283 that
//! state has to hang off the session from the start rather than be
//! retrofitted. Recorded as a residual.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    rows.iter()
        .map(|row| {
            row.values
                .iter()
                // NULL renders empty, the way `psql -tA` prints it —
                // these expectations were read off psql.
                .map(|v| match v {
                    spg_storage::Value::Null => String::new(),
                    other => spg_engine::eval::value_to_text(other),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}").replace("unsupported: ", ""),
    }
}

#[test]
fn a_round_trip_through_bytea() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT lo_from_bytea(0, '\\x48656c6c6f'::bytea)"),
        "500000"
    );
    assert_eq!(one(&mut e, "SELECT lo_get(500000)"), "\\x48656c6c6f");
    assert_eq!(
        one(&mut e, "SELECT encode(lo_get(500000), 'escape')"),
        "Hello"
    );
    // The result must still BE bytea downstream — folding it to a bare
    // literal handed `length()` the hex text and it answered 12.
    assert_eq!(one(&mut e, "SELECT length(lo_get(500000))"), "5");
}

#[test]
fn the_offsets_are_zero_based() {
    let mut e = Engine::new();
    e.execute("SELECT lo_from_bytea(0, '\\x48656c6c6f'::bytea)")
        .unwrap();
    // 'Hello' — byte 1 for 3 bytes is 'ell', not 'Hel'.
    assert_eq!(one(&mut e, "SELECT lo_get(500000, 1, 3)"), "\\x656c6c");
    // …and lo_put writes at the same 0-based offset.
    e.execute("SELECT lo_put(500000, 1, '\\x41'::bytea)")
        .unwrap();
    assert_eq!(
        one(&mut e, "SELECT encode(lo_get(500000),'escape')"),
        "HAllo"
    );
}

#[test]
fn lo_put_returns_void() {
    let mut e = Engine::new();
    e.execute("SELECT lo_from_bytea(0, '\\x4142'::bytea)")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT lo_put(500000, 0, '\\x5a'::bytea)"), "");
}

#[test]
fn unlink_removes_it_and_the_wording_is_pgs() {
    let mut e = Engine::new();
    e.execute("SELECT lo_from_bytea(0, '\\x41'::bytea)")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT lo_unlink(500000)"), "1");
    assert_eq!(
        err(&mut e, "SELECT lo_get(500000)"),
        "large object 500000 does not exist",
    );
    assert_eq!(
        err(&mut e, "SELECT lo_unlink(9999999)"),
        "large object 9999999 does not exist",
    );
}

#[test]
fn the_page_split_matches_pgs_2kb() {
    let mut e = Engine::new();
    e.execute("SELECT lo_from_bytea(0, repeat('a',5000)::bytea)")
        .unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT loid, pageno, length(data) FROM pg_largeobject WHERE loid = 500000 ORDER BY pageno",
        ),
        "500000|0|2048;500000|1|2048;500000|2|904",
    );
}

#[test]
fn an_empty_object_has_metadata_but_no_pages() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT lo_from_bytea(0, ''::bytea)"), "500000");
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_largeobject WHERE loid = 500000"
        ),
        "0",
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT oid, lomowner, lomacl FROM pg_largeobject_metadata"
        ),
        "500000|10|",
    );
}

#[test]
fn lo_creat_and_lo_create_both_allocate() {
    let mut e = Engine::new();
    // PG spells "pick an OID" as lo_creat(-1) and lo_create(0).
    assert_eq!(one(&mut e, "SELECT lo_creat(-1)"), "500000");
    assert_eq!(one(&mut e, "SELECT lo_create(0)"), "500001");
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM pg_largeobject_metadata"),
        "2"
    );
}

#[test]
fn the_objects_survive_a_catalog_round_trip() {
    // FILE_VERSION 78 adds the block; a reload that lost it would
    // answer "does not exist" here.
    let mut e = Engine::new();
    e.execute("SELECT lo_from_bytea(0, '\\x48656c6c6f'::bytea)")
        .unwrap();
    let bytes = e.catalog().serialize();
    let mut restored = Engine::restore_envelope(&bytes).expect("reload");
    assert_eq!(
        one(&mut restored, "SELECT encode(lo_get(500000),'escape')"),
        "Hello",
    );
    assert_eq!(
        one(
            &mut restored,
            "SELECT count(*) FROM pg_largeobject_metadata"
        ),
        "1",
    );
}
