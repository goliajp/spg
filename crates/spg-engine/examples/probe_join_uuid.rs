//! r1036 — sentori reports a JOIN dropping its only row when the join key
//! is `uuid` and the WHERE names the RIGHT-hand table
//! (`SENTORI_2026-08-16_REPORT.md`). It returns zero and raises nothing,
//! which is how it reached them as "signed in, then immediately 401".
//!
//! Their matrix, reproduced here in process so the wire is not in the way:
//!
//! | join key | no WHERE | WHERE on left | WHERE on right | swapped |
//! |---|---|---|---|---|
//! | int  | 1 | 1 | 1 | 1 |
//! | text | 1 | 1 | 1 | 1 |
//! | uuid | 1 | 1 | **0** | 1 |
//!
//! The `int` and `text` rows are the control: same shape, same data, same
//! one row, so anything that differs is the key type and not the plan.
//!
//!   cargo run --release --example probe_join_uuid

use spg_engine::Engine;

const P: &str = "11111111-1111-4111-8111-111111111111";
const C: &str = "22222222-2222-4222-8222-222222222222";

fn scalar(eng: &mut Engine, sql: &str) -> String {
    match eng.execute(sql) {
        Ok(out) => {
            let text = format!("{out:?}");
            // The count is the only integer in a one-cell result.
            text.rsplit_once("Int(")
                .and_then(|(_, r)| r.split(')').next())
                .unwrap_or("?")
                .to_string()
        }
        Err(e) => format!("ERROR {e:?}"),
    }
}

fn build(key_ty: &str, p_val: &str, c_val: &str) -> Engine {
    let mut eng = Engine::new();
    eng.execute(&format!(
        "CREATE TABLE p (id {key_ty} PRIMARY KEY, tag TEXT)"
    ))
    .expect("create p");
    eng.execute(&format!(
        "CREATE TABLE c (id {key_ty} PRIMARY KEY, parent_id {key_ty}, note TEXT)"
    ))
    .expect("create c");
    eng.execute(&format!("INSERT INTO p VALUES ({p_val}, 'keep')"))
        .expect("insert p");
    eng.execute(&format!("INSERT INTO c VALUES ({c_val}, {p_val}, 'n')"))
        .expect("insert c");
    eng
}

fn main() {
    let cases: [(&str, &str, &str); 3] = [
        ("int", "1", "2"),
        ("text", "'p-1'", "'c-2'"),
        ("uuid", &format!("'{P}'"), &format!("'{C}'")),
    ];
    println!(
        "{:<6} {:>9} {:>10} {:>11} {:>9}",
        "key", "no WHERE", "left", "right", "swapped"
    );
    for (ty, p_val, c_val) in cases {
        let mut e = build(ty, p_val, c_val);
        let none = scalar(
            &mut e,
            "SELECT count(*) FROM c JOIN p ON p.id = c.parent_id",
        );
        let left = scalar(
            &mut e,
            "SELECT count(*) FROM c JOIN p ON p.id = c.parent_id WHERE c.note = 'n'",
        );
        let right = scalar(
            &mut e,
            "SELECT count(*) FROM c JOIN p ON p.id = c.parent_id WHERE p.tag = 'keep'",
        );
        let swapped = scalar(
            &mut e,
            "SELECT count(*) FROM p JOIN c ON c.parent_id = p.id WHERE p.tag = 'keep'",
        );
        println!("{ty:<6} {none:>9} {left:>10} {right:>11} {swapped:>9}");
    }
    println!("\nevery cell must be 1");

    // Which ingredient is required? Drop them one at a time from the
    // failing cell; whatever makes the row come back is the one.
    println!("\n== what the failing cell needs (uuid, WHERE on the right)");
    let variants: [(&str, &str, &str); 7] = [
        (
            "as reported",
            "CREATE TABLE p (id uuid PRIMARY KEY, tag TEXT)",
            "SELECT count(*) FROM c JOIN p ON p.id = c.parent_id WHERE p.tag = 'keep'",
        ),
        (
            "p.id not a PK",
            "CREATE TABLE p (id uuid, tag TEXT)",
            "SELECT count(*) FROM c JOIN p ON p.id = c.parent_id WHERE p.tag = 'keep'",
        ),
        (
            "predicate IS NOT NULL",
            "CREATE TABLE p (id uuid PRIMARY KEY, tag TEXT)",
            "SELECT count(*) FROM c JOIN p ON p.id = c.parent_id WHERE p.tag IS NOT NULL",
        ),
        (
            "predicate on both sides",
            "CREATE TABLE p (id uuid PRIMARY KEY, tag TEXT)",
            "SELECT count(*) FROM c JOIN p ON p.id = c.parent_id WHERE p.tag = 'keep' AND c.note = 'n'",
        ),
        (
            "select rows not count",
            "CREATE TABLE p (id uuid PRIMARY KEY, tag TEXT)",
            "SELECT count(*) FROM (SELECT c.id FROM c JOIN p ON p.id = c.parent_id WHERE p.tag = 'keep') q",
        ),
        (
            "uuid index but not PK",
            "CREATE TABLE p (id uuid, tag TEXT)",
            "SELECT count(*) FROM c JOIN p ON p.id = c.parent_id WHERE p.tag = 'keep'",
        ),
        (
            "ON reversed",
            "CREATE TABLE p (id uuid PRIMARY KEY, tag TEXT)",
            "SELECT count(*) FROM c JOIN p ON c.parent_id = p.id WHERE p.tag = 'keep'",
        ),
    ];
    for (label, create_p, sql) in variants {
        let mut e = Engine::new();
        e.execute(create_p).expect("create p");
        if label == "uuid index but not PK" {
            e.execute("CREATE INDEX p_id_ix ON p (id)").expect("index");
        }
        e.execute("CREATE TABLE c (id uuid PRIMARY KEY, parent_id uuid, note TEXT)")
            .expect("create c");
        e.execute(&format!("INSERT INTO p VALUES ('{P}', 'keep')"))
            .expect("insert p");
        e.execute(&format!("INSERT INTO c VALUES ('{C}', '{P}', 'n')"))
            .expect("insert c");
        println!("  {label:<24} {}", scalar(&mut e, sql));
    }

    // Is the join key being NULLed out as "unreferenced"? Then naming it
    // in the select list or the predicate brings the row back.
    println!("\n== is the join key surviving the peer's projection?");
    for (label, sql) in [
        (
            "count(*)",
            "SELECT count(*) FROM c JOIN p ON p.id = c.parent_id WHERE p.tag = 'keep'",
        ),
        (
            "select p.id",
            "SELECT count(*) FROM (SELECT p.id FROM c JOIN p ON p.id = c.parent_id WHERE p.tag = 'keep') q",
        ),
        (
            "p.id named in WHERE",
            "SELECT count(*) FROM c JOIN p ON p.id = c.parent_id WHERE p.tag = 'keep' AND p.id IS NOT NULL",
        ),
        (
            "select p.tag",
            "SELECT count(*) FROM (SELECT p.tag FROM c JOIN p ON p.id = c.parent_id WHERE p.tag = 'keep') q",
        ),
    ] {
        let mut e = Engine::new();
        e.execute("CREATE TABLE p (id uuid PRIMARY KEY, tag TEXT)")
            .expect("create p");
        e.execute("CREATE TABLE c (id uuid PRIMARY KEY, parent_id uuid, note TEXT)")
            .expect("create c");
        e.execute(&format!("INSERT INTO p VALUES ('{P}', 'keep')"))
            .expect("insert p");
        e.execute(&format!("INSERT INTO c VALUES ('{C}', '{P}', 'n')"))
            .expect("insert c");
        println!("  {label:<24} {}", scalar(&mut e, sql));
    }

    // If the index is being chosen by a position taken from the wrong
    // list, then moving the join column away from position 0 changes the
    // answer. That is a prediction the shape makes; here it is tested.
    println!("\n== does the join column's POSITION in the table matter?");
    for (label, create_p, ins) in [
        (
            "join col first",
            "CREATE TABLE p (id uuid PRIMARY KEY, tag TEXT)",
            "('{P}', 'keep')",
        ),
        (
            "join col second",
            "CREATE TABLE p (tag TEXT, id uuid PRIMARY KEY)",
            "('keep', '{P}')",
        ),
    ] {
        let mut e = Engine::new();
        e.execute(create_p).expect("create p");
        e.execute("CREATE TABLE c (id uuid PRIMARY KEY, parent_id uuid, note TEXT)")
            .expect("create c");
        e.execute(&format!("INSERT INTO p VALUES {}", ins.replace("{P}", P)))
            .expect("insert p");
        e.execute(&format!("INSERT INTO c VALUES ('{C}', '{P}', 'n')"))
            .expect("insert c");
        let r = scalar(
            &mut e,
            "SELECT count(*) FROM c JOIN p ON p.id = c.parent_id WHERE p.tag = 'keep'",
        );
        println!("  {label:<24} {r}");
    }
}
