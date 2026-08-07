//! v7.39 (round 210) — EXCLUDE constraints (Phase 0). The drop-in
//! killer feature for booking / scheduling non-overlap:
//! `EXCLUDE USING gist (during WITH &&)` forbids two rows whose
//! ranges overlap. Live-PG18.4 differential (2026-07-18):
//!   CREATE TABLE ov (during int4range, EXCLUDE USING gist (during WITH &&));
//!   INSERT [1,5) OK; [5,10) OK (5 exclusive); [3,7) →
//!   ERROR:  conflicting key value violates exclusion constraint "ov_during_excl"
//!   DETAIL: Key (during)=([3,7)) conflicts with existing key (during)=([1,5)).
//! UPDATE into an overlapping range raises the same error; a NULL
//! range column exempts the row. Auto-name is `<table>_<col>_excl`.
//! Enforcement is a full live-row scan re-checking `&&` (a real GiST
//! index is a later perf phase); correctness is index-independent.

use spg_engine::Engine;

#[test]
fn overlap_rejected_with_pg_message() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ov (during int4range, EXCLUDE USING gist (during WITH &&))")
        .unwrap();
    e.execute("INSERT INTO ov VALUES ('[1,5)')").unwrap();
    // 5 is exclusive on the left range and inclusive-excluded here — no overlap.
    e.execute("INSERT INTO ov VALUES ('[5,10)')").unwrap();
    let err = e
        .execute("INSERT INTO ov VALUES ('[3,7)')")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("conflicting key value violates exclusion constraint \"ov_during_excl\""),
        "main: {err}"
    );
    assert!(
        err.contains("Key (during)=([3,7)) conflicts with existing key (during)=([1,5))."),
        "detail: {err}"
    );
}

#[test]
fn update_into_overlap_rejected() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ov (during int4range, EXCLUDE USING gist (during WITH &&))")
        .unwrap();
    e.execute("INSERT INTO ov VALUES ('[1,5)')").unwrap();
    e.execute("INSERT INTO ov VALUES ('[5,10)')").unwrap();
    let err = e
        .execute("UPDATE ov SET during = '[4,6)' WHERE during = '[5,10)'")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("conflicting key value violates exclusion constraint \"ov_during_excl\"")
            && err.contains("Key (during)=([4,6)) conflicts with existing key (during)=([1,5))."),
        "{err}"
    );
}

#[test]
fn noop_update_of_same_row_ok() {
    // The updated row's pre-image must leave the conflict set, so an
    // UPDATE that keeps (or shrinks within) its own range does not
    // self-collide.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ov (during int4range, EXCLUDE USING gist (during WITH &&))")
        .unwrap();
    e.execute("INSERT INTO ov VALUES ('[1,5)')").unwrap();
    e.execute("UPDATE ov SET during = '[1,4)' WHERE during = '[1,5)'")
        .unwrap();
}

#[test]
fn null_range_is_exempt() {
    // A NULL constrained column never conflicts (PG / UNIQUE NULL semantics).
    let mut e = Engine::new();
    e.execute("CREATE TABLE ov (during int4range, EXCLUDE USING gist (during WITH &&))")
        .unwrap();
    e.execute("INSERT INTO ov VALUES ('[1,5)')").unwrap();
    e.execute("INSERT INTO ov VALUES (NULL)").unwrap();
    e.execute("INSERT INTO ov VALUES (NULL)").unwrap();
}

#[test]
fn intra_batch_overlap_rejected() {
    // Two overlapping rows in ONE multi-row INSERT collide with each other.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ov (during int4range, EXCLUDE USING gist (during WITH &&))")
        .unwrap();
    let err = e
        .execute("INSERT INTO ov VALUES ('[1,5)'), ('[3,7)')")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("conflicting key value violates exclusion constraint"),
        "{err}"
    );
}

#[test]
fn non_overlapping_batch_ok() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ov (during int4range, EXCLUDE USING gist (during WITH &&))")
        .unwrap();
    e.execute("INSERT INTO ov VALUES ('[1,5)'), ('[5,10)'), ('[10,20)')")
        .unwrap();
}

#[test]
fn constraint_survives_catalog_round_trip() {
    // The EXCLUDE constraint must persist across a catalog serialize →
    // deserialize (the durable embedded path); otherwise a booking table
    // would silently drop its non-overlap guarantee after a restart.
    use spg_storage::Catalog;
    let mut e = Engine::new();
    e.execute("CREATE TABLE ov (during int4range, EXCLUDE USING gist (during WITH &&))")
        .unwrap();
    e.execute("INSERT INTO ov VALUES ('[1,5)')").unwrap();

    let bytes = e.catalog().serialize();
    let restored = Catalog::deserialize(&bytes).unwrap();
    let schema = restored.get("ov").unwrap().schema();
    assert_eq!(schema.exclusion_constraints.len(), 1);
    let ex = &schema.exclusion_constraints[0];
    assert_eq!(ex.name, "ov_during_excl");
    assert_eq!(ex.method.as_deref(), Some("gist"));
    assert_eq!(ex.elements.len(), 1);
    assert_eq!(ex.elements[0].1, "&&");
}
