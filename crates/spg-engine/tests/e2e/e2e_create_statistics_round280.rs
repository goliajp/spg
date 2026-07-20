//! v7.39 (round 280) — CREATE / DROP STATISTICS as a real catalog
//! object.
//!
//! Both were consumed by the CREATE-noise and DROP-noise arms, so a
//! pg_dump that declares extended statistics restored silently without
//! them and `pg_statistic_ext` showed nothing. The planner still does
//! not consult them — recording the object is what makes the restore
//! and the reflection honest.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn lines(e: &mut Engine, sql: &str) -> Vec<String> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    rows.into_iter()
        .map(|row| {
            row.values
                .iter()
                .map(|v| spg_engine::eval::value_to_text(v))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}").replace("unsupported: ", ""),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE stt (a int, b int, c text)").unwrap();
    e
}

#[test]
fn an_object_is_recorded_and_reflected() {
    let mut e = fixture();
    e.execute("CREATE STATISTICS s1 ON a, b FROM stt").unwrap();
    e.execute("CREATE STATISTICS s2 (ndistinct) ON a, c FROM stt")
        .unwrap();
    // PG's default kind set is all three (d ndistinct, f dependencies,
    // m mcv); naming one narrows it.
    assert_eq!(
        lines(
            &mut e,
            "SELECT stxname, stxkind FROM pg_statistic_ext ORDER BY stxname",
        ),
        vec!["s1|{d,f,m}", "s2|{d}"],
    );
}

#[test]
fn the_duplicate_and_missing_wordings_are_pgs() {
    let mut e = fixture();
    e.execute("CREATE STATISTICS s1 ON a, b FROM stt").unwrap();
    assert_eq!(
        err(&mut e, "CREATE STATISTICS s1 ON a, b FROM stt"),
        "statistics object \"s1\" already exists",
    );
    // IF NOT EXISTS skips instead.
    e.execute("CREATE STATISTICS IF NOT EXISTS s1 ON a, b FROM stt")
        .unwrap();
    assert_eq!(
        err(&mut e, "DROP STATISTICS nosuch"),
        "statistics object \"nosuch\" does not exist",
    );
    e.execute("DROP STATISTICS IF EXISTS nosuch").unwrap();
}

#[test]
fn fewer_than_two_columns_is_rejected() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "CREATE STATISTICS s3 ON a FROM stt"),
        "extended statistics require at least 2 columns",
    );
}

#[test]
fn drop_removes_it() {
    let mut e = fixture();
    e.execute("CREATE STATISTICS s1 ON a, b FROM stt").unwrap();
    e.execute("CREATE STATISTICS s2 ON b, c FROM stt").unwrap();
    e.execute("DROP STATISTICS s2").unwrap();
    assert_eq!(
        lines(&mut e, "SELECT count(*) FROM pg_statistic_ext"),
        vec!["1"],
    );
}

#[test]
fn the_object_survives_a_catalog_round_trip() {
    // FILE_VERSION 76 -> 77 adds the block; this is the check that the
    // bump actually carries the object, and that an image written with
    // it reads back identically.
    let mut e = fixture();
    e.execute("CREATE STATISTICS s1 (ndistinct, mcv) ON a, b FROM stt")
        .unwrap();
    let bytes = e.catalog().serialize();
    let mut restored = Engine::restore_envelope(&bytes).expect("reload");
    assert_eq!(
        lines(
            &mut restored,
            "SELECT stxname, stxkind FROM pg_statistic_ext ORDER BY stxname",
        ),
        vec!["s1|{d,m}"],
    );
}
