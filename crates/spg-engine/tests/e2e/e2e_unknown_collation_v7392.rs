//! v7.39.2 — a collation name that does not exist is refused, not ignored.
//!
//! `SELECT 'a' COLLATE nosuch_ci` answered `a` on the MySQL wire, and
//! `'B' COLLATE nosuch_ci < 'a'` answered whatever the session would have
//! answered anyway. The client named a collation and was never told it
//! had not been used. MySQL 9.7.2: `ERROR 1273 (HY000): Unknown
//! collation: 'nosuch_ci'`.
//!
//! Two suffix rules were behind it, and each let a different family
//! through. The parser lowered any `_bin` name onto a BINARY cast and
//! absorbed any `_ci` name as a no-op ("the MySQL default already
//! folds"), which is true of a REAL `_ci` collation and says nothing
//! about `nosuch_ci`; and the engine's own `is_known` ended in
//! "anything ending in `_ci`, `_cs` or `_bin`". Both now ask MySQL's own
//! list — 286 names, read from its `information_schema.collations`.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode=''")
        .expect("enter the MySQL dialect");
    e
}

fn answer(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        Ok(_) => "<none>".to_string(),
        Err(err) => format!("ERR {err}"),
    }
}

#[test]
fn an_unknown_name_is_refused_in_mysqls_words() {
    let mut e = mysql();
    // One from each suffix family: `_ci` was absorbed, `_bin` became a
    // BINARY cast, `_cs` already reached the node. Pinning one of the
    // three would leave the other two accepted.
    for name in ["nosuch_ci", "nosuch_bin", "nosuch_cs"] {
        let got = answer(&mut e, &format!("SELECT 'a' COLLATE {name}"));
        assert!(
            got.contains(&format!("Unknown collation: '{name}'")),
            "{name}: {got}"
        );
    }
    // A real charset with a family MySQL does not have — the case a
    // charset-prefix rule would still have let through.
    let got = answer(&mut e, "SELECT 'a' COLLATE utf8mb4_madeup_ci");
    assert!(
        got.contains("Unknown collation: 'utf8mb4_madeup_ci'"),
        "{got}"
    );
}

#[test]
fn the_names_mysql_does_have_still_work() {
    // The control. Refusing everything would pass the test above.
    let mut e = mysql();
    assert_eq!(answer(&mut e, "SELECT 'a' COLLATE utf8mb4_general_ci"), "a");
    assert_eq!(answer(&mut e, "SELECT 'a' COLLATE utf8mb4_bin"), "a");
    assert_eq!(answer(&mut e, "SELECT 'a' COLLATE utf8mb4_0900_ai_ci"), "a");
    // And the collation is still PERFORMED, not merely accepted: the
    // default folds, `utf8mb4_bin` does not.
    assert_eq!(answer(&mut e, "SELECT 'B' = 'b'"), "true");
    assert_eq!(
        answer(&mut e, "SELECT 'B' COLLATE utf8mb4_bin = 'b'"),
        "false"
    );
}

#[test]
fn a_declaration_is_refused_too() {
    let mut e = mysql();
    let got = answer(&mut e, "CREATE TABLE cc (s VARCHAR(8) COLLATE nosuch_ci)");
    assert!(got.contains("Unknown collation: 'nosuch_ci'"), "{got}");
    e.execute("CREATE TABLE cd (s VARCHAR(8) COLLATE utf8mb4_bin)")
        .expect("a real one is accepted");
}

#[test]
fn postgres_keeps_postgress_words_and_postgress_names() {
    // The negative control on the other wire. PG18.6 says `collation
    // "nosuch" for encoding "UTF8" does not exist`, and it says the same
    // for MySQL's names, which are not in its catalogue.
    let mut e = Engine::new();
    let got = answer(&mut e, "SELECT 'a' COLLATE \"nosuch\"");
    assert!(
        got.contains("collation \"nosuch\" for encoding \"UTF8\" does not exist"),
        "{got}"
    );
    assert_eq!(answer(&mut e, "SELECT 'a' COLLATE \"C\""), "a");
}
