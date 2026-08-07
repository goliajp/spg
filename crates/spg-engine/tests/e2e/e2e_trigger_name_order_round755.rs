//! Round 755 (F31-B2) — same-event triggers fire in NAME order,
//! PG18-measured (round-753 probe): z_trig created FIRST, a_trig
//! second, and PG's log still reads a_trig, z_trig. SPG fired in
//! insertion order until this round; the old storage comment even
//! described insertion order as "matching PG's alphabetical-by-default
//! with insertion-stable tie-break", which was never measured.

use spg_engine::{Engine, QueryResult};

fn log_rows(e: &mut Engine) -> Vec<String> {
    match e.execute("SELECT who FROM f755log ORDER BY seq").unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round755_insert_triggers_fire_in_name_order() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE f755t (id INT)").unwrap();
    e.execute("CREATE TABLE f755log (seq SERIAL, who TEXT)")
        .unwrap();
    e.execute(
        "CREATE FUNCTION f755_z() RETURNS trigger AS $$ BEGIN \
         INSERT INTO f755log(who) VALUES ('z_trig'); RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .unwrap();
    e.execute(
        "CREATE FUNCTION f755_a() RETURNS trigger AS $$ BEGIN \
         INSERT INTO f755log(who) VALUES ('a_trig'); RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .unwrap();
    // Created in REVERSE name order — the firing must not follow it.
    e.execute(
        "CREATE TRIGGER z_trig BEFORE INSERT ON f755t FOR EACH ROW EXECUTE FUNCTION f755_z()",
    )
    .unwrap();
    e.execute(
        "CREATE TRIGGER a_trig BEFORE INSERT ON f755t FOR EACH ROW EXECUTE FUNCTION f755_a()",
    )
    .unwrap();
    e.execute("INSERT INTO f755t VALUES (1)").unwrap();
    assert_eq!(log_rows(&mut e), ["a_trig", "z_trig"], "PG fires by name");
}

#[test]
fn round755_update_triggers_fire_in_name_order() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE f755t (id INT)").unwrap();
    e.execute("CREATE TABLE f755log (seq SERIAL, who TEXT)")
        .unwrap();
    e.execute(
        "CREATE FUNCTION f755_z() RETURNS trigger AS $$ BEGIN \
         INSERT INTO f755log(who) VALUES ('z_trig'); RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .unwrap();
    e.execute(
        "CREATE FUNCTION f755_a() RETURNS trigger AS $$ BEGIN \
         INSERT INTO f755log(who) VALUES ('a_trig'); RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .unwrap();
    e.execute("INSERT INTO f755t VALUES (1)").unwrap();
    e.execute("CREATE TRIGGER z_up BEFORE UPDATE ON f755t FOR EACH ROW EXECUTE FUNCTION f755_z()")
        .unwrap();
    e.execute("CREATE TRIGGER a_up BEFORE UPDATE ON f755t FOR EACH ROW EXECUTE FUNCTION f755_a()")
        .unwrap();
    e.execute("UPDATE f755t SET id = 2").unwrap();
    assert_eq!(log_rows(&mut e), ["a_trig", "z_trig"], "PG fires by name");
}
