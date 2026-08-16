//! r1036 — sentori's repro script, in process. Every count must be 1.
use spg_engine::Engine;
fn n(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(o) => {
            let t = format!("{o:?}");
            t.rsplit_once("Int(")
                .and_then(|(_, r)| r.split(')').next())
                .unwrap_or("?")
                .to_string()
        }
        Err(er) => format!("ERR {er:?}"),
    }
}
fn main() {
    let mut e = Engine::new();
    for ddl in [
        "CREATE TABLE p_int  (id int  PRIMARY KEY, tag text)",
        "CREATE TABLE c_int  (id int  PRIMARY KEY, parent_id int,  note text)",
        "CREATE TABLE p_uuid (id uuid PRIMARY KEY, tag text)",
        "CREATE TABLE c_uuid (id uuid PRIMARY KEY, parent_id uuid, note text)",
        "INSERT INTO p_int  VALUES (1, 'keep')",
        "INSERT INTO c_int  VALUES (2, 1, 'n')",
        "INSERT INTO p_uuid VALUES ('11111111-1111-4111-8111-111111111111', 'keep')",
        "INSERT INTO c_uuid VALUES ('22222222-2222-4222-8222-222222222222','11111111-1111-4111-8111-111111111111','n')",
        "CREATE TABLE owner_t (id uuid PRIMARY KEY, role text)",
        "CREATE TABLE sess_t  (id_hash bytea PRIMARY KEY, user_id uuid NOT NULL)",
        "INSERT INTO owner_t VALUES ('11111111-1111-4111-8111-111111111111', 'superadmin')",
        "INSERT INTO sess_t  VALUES (decode('deadbeef','hex'), '11111111-1111-4111-8111-111111111111')",
    ] {
        e.execute(ddl).unwrap_or_else(|x| panic!("{ddl}: {x:?}"));
    }

    for (label, sql) in [
        (
            "int, predicate right",
            "SELECT count(*) FROM c_int  c JOIN p_int  p ON p.id = c.parent_id WHERE p.tag = 'keep'",
        ),
        (
            "uuid, predicate right",
            "SELECT count(*) FROM c_uuid c JOIN p_uuid p ON p.id = c.parent_id WHERE p.tag = 'keep'",
        ),
        (
            "uuid, tables swapped",
            "SELECT count(*) FROM p_uuid p JOIN c_uuid c ON c.parent_id = p.id WHERE p.tag = 'keep'",
        ),
        (
            "uuid, predicate left",
            "SELECT count(*) FROM c_uuid c JOIN p_uuid p ON p.id = c.parent_id WHERE c.note = 'n'",
        ),
        (
            "bytea literal, with JOIN",
            "SELECT count(*) FROM sess_t s JOIN owner_t u ON u.id = s.user_id WHERE s.id_hash = decode('deadbeef','hex')",
        ),
    ] {
        println!("{label:<28} {}", n(&mut e, sql));
    }
    println!("\nevery count must be 1");
}
