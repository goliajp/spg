//! Round 764 (F31 tranche 3 #81/#64) — PG splits the TIME refusals:
//! a time-shaped literal with an impossible component is "date/time
//! field value out of range" (the 22008 family), only junk is
//! "invalid input syntax"; `24:00:00` is the accepted day-end
//! special. And WITH TIES without ORDER BY speaks PG's sentence.
//! All PG18-measured in the round-764 differential.

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

#[test]
fn round764_time_refusals_split_as_pg() {
    let mut e = Engine::new();
    assert!(
        err(&mut e, "SELECT '25:00:00'::time")
            .contains("date/time field value out of range: \"25:00:00\"")
    );
    assert!(err(&mut e, "SELECT '10:61:00'::time").contains("date/time field value out of range"));
    assert!(err(&mut e, "SELECT 'abc'::time").contains("invalid input syntax for type time"));
    // The day-end special is accepted.
    e.execute("SELECT '24:00:00'::time").unwrap();
    assert!(
        err(&mut e, "SELECT 1 FETCH FIRST 2 ROWS WITH TIES")
            .contains("WITH TIES cannot be specified without ORDER BY clause")
    );
}
