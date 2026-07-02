//! v7.37.17 (17.6 siblings) — information_schema.triggers view.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter()
        .map(|row| row.values.into_iter().collect())
        .collect()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn triggers_view_explodes_events() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tt (id INT)").unwrap();
    e.execute(
        "CREATE FUNCTION tfn() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .unwrap();
    e.execute(
        "CREATE TRIGGER tt_trg BEFORE INSERT OR UPDATE ON tt \
         FOR EACH ROW EXECUTE FUNCTION tfn()",
    )
    .unwrap();
    let got = rows(
        &mut e,
        "SELECT trigger_name, event_manipulation, event_object_table, \
                action_timing, action_statement \
         FROM information_schema.triggers ORDER BY event_manipulation",
    );
    // INSERT OR UPDATE explodes to two rows, PG-style.
    assert_eq!(got.len(), 2);
    assert_eq!(text(&got[0][1]), "INSERT");
    assert_eq!(text(&got[1][1]), "UPDATE");
    for row in &got {
        assert_eq!(text(&row[0]), "tt_trg");
        assert_eq!(text(&row[2]), "tt");
        assert_eq!(text(&row[3]), "BEFORE");
        assert!(text(&row[4]).contains("tfn"));
    }
}

#[test]
fn triggers_view_empty_without_triggers() {
    let mut e = Engine::new();
    let got = rows(&mut e, "SELECT * FROM information_schema.triggers");
    assert!(got.is_empty());
}
