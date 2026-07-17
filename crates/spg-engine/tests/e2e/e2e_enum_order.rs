//! v7.39 (enum order knife) — enum values order by catalog MEMBER order
//! (PG's enumsortorder), not label text, across every ordering channel:
//! comparisons (incl. the compiled WHERE path and BETWEEN), min/max,
//! greatest/least, array_agg(ORDER BY), grouped-output ORDER BY, HAVING,
//! and index-backed scans. All outputs differential-locked against PG18.
//! mood = sad < meh < ok < happy (ADD VALUE BEFORE reorders mid-list, so
//! member order and lexicographic order disagree on every pair below).

use spg_engine::{Engine, QueryResult};

fn text_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn col_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TYPE mood AS ENUM ('sad','ok','happy')")
        .unwrap();
    e.execute("ALTER TYPE mood ADD VALUE 'meh' BEFORE 'ok'")
        .unwrap();
    e.execute("CREATE TABLE em (id INT, m mood)").unwrap();
    e.execute("INSERT INTO em VALUES (1,'happy'),(2,'sad'),(3,'meh'),(4,'ok'),(5,NULL),(6,'meh')")
        .unwrap();
}

#[test]
fn comparisons_use_member_order() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        col_of(
            &mut e,
            "SELECT ('sad'::mood < 'meh'::mood) OR false" // via eval tree
        ),
        vec!["true"]
    );
    assert_eq!(text_of(&mut e, "SELECT 'happy'::mood > 'ok'::mood"), "true");
    // Unknown literal takes the enum side (PG).
    assert_eq!(text_of(&mut e, "SELECT 'sad'::mood < 'meh'"), "true");
    // WHERE goes through the compiled step path — must agree.
    assert_eq!(
        col_of(&mut e, "SELECT m FROM em WHERE m > 'meh' ORDER BY m"),
        vec!["ok", "happy"]
    );
    // BETWEEN desugars to comparisons.
    assert_eq!(
        col_of(
            &mut e,
            "SELECT m FROM em WHERE m BETWEEN 'sad' AND 'ok' ORDER BY m, id"
        ),
        vec!["sad", "meh", "meh", "ok"]
    );
}

#[test]
fn min_max_and_greatest_least() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(col_of(&mut e, "SELECT min(m) FROM em"), vec!["sad"]);
    assert_eq!(col_of(&mut e, "SELECT max(m) FROM em"), vec!["happy"]);
    assert_eq!(
        text_of(&mut e, "SELECT min(m) FROM em WHERE m > 'sad'"),
        "meh"
    );
    assert_eq!(
        text_of(&mut e, "SELECT greatest('sad'::mood, 'meh'::mood)"),
        "meh"
    );
    assert_eq!(
        text_of(&mut e, "SELECT least('ok'::mood, 'happy'::mood)"),
        "ok"
    );
    assert_eq!(
        text_of(&mut e, "SELECT greatest(m, 'meh') FROM em WHERE id = 2"),
        "meh"
    );
}

#[test]
fn ordered_collection_and_grouped_output() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        text_of(
            &mut e,
            "SELECT array_agg(m ORDER BY m) FROM em WHERE m IS NOT NULL"
        ),
        "{sad,meh,meh,ok,happy}"
    );
    assert_eq!(
        text_of(
            &mut e,
            "SELECT array_agg(m ORDER BY m DESC) FROM em WHERE m IS NOT NULL"
        ),
        "{happy,ok,meh,meh,sad}"
    );
    // Grouped-output ORDER BY rides the synth-row sort.
    assert_eq!(
        col_of(
            &mut e,
            "SELECT m FROM em WHERE m IS NOT NULL GROUP BY m ORDER BY m"
        ),
        vec!["sad", "meh", "ok", "happy"]
    );
    // HAVING compares the group key by member order.
    assert_eq!(
        col_of(
            &mut e,
            "SELECT m FROM em GROUP BY m HAVING m > 'meh' ORDER BY m"
        ),
        vec!["ok", "happy"]
    );
}

#[test]
fn index_backed_scans_stay_member_ordered() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE INDEX em_m ON em (m)").unwrap();
    // The index orders labels lexicographically, so range seeks bail to
    // the seq scan whose comparisons are member-order aware.
    assert_eq!(
        col_of(&mut e, "SELECT m FROM em WHERE m > 'meh' ORDER BY m"),
        vec!["ok", "happy"]
    );
    assert_eq!(
        col_of(
            &mut e,
            "SELECT m FROM em WHERE m >= 'meh' AND m <= 'ok' ORDER BY m, id"
        ),
        vec!["meh", "meh", "ok"]
    );
    assert_eq!(
        text_of(
            &mut e,
            "SELECT count(*) FROM em WHERE m >= 'meh' AND m <= 'ok'"
        ),
        "3"
    );
    // Equality seeks stay on the index (labels are exact).
    assert_eq!(
        col_of(&mut e, "SELECT id FROM em WHERE m = 'meh' ORDER BY id"),
        vec!["3", "6"]
    );
}
