//! 7.38.1 L12 — the composite-keyed B-tree, exercised through SQL.
//!
//! A multi-column PRIMARY KEY / CREATE INDEX now keys a real composite
//! B-tree (the leading index carries the whole tuple), so an equality
//! on the full key — or any prefix of it — descends instead of
//! flooding candidates through the leading column. These pins hold the
//! ANSWERS fixed against a seq-scan oracle built in the same schema
//! without indexes: an index may make a query faster, never different.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect()
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed(e: &mut Engine, table_ddl: &str) {
    e.execute(table_ddl).unwrap();
    for w in 1..=3i64 {
        for d in 1..=4i64 {
            for id in 1..=5i64 {
                e.execute(&format!(
                    "INSERT INTO c VALUES ({w}, {d}, {id}, 'p{w}{d}{id}')"
                ))
                .unwrap();
            }
        }
    }
}

/// The same battery of probe shapes against the indexed table and an
/// index-free twin must agree row-for-row.
fn assert_matches_unindexed(e: &mut Engine) {
    e.execute("CREATE TABLE oracle (w BIGINT, d BIGINT, id BIGINT, pad TEXT)")
        .unwrap();
    e.execute("INSERT INTO oracle SELECT * FROM c").unwrap();
    for q in [
        "WHERE w = 2 AND d = 3 AND id = 4",
        "WHERE d = 3 AND id = 4 AND w = 2", // order-insensitive
        "WHERE w = 2 AND d = 3",            // prefix (w, d)
        "WHERE w = 2",                      // prefix (w)
        "WHERE w = 2 AND id = 4",           // gap: not a prefix
        "WHERE w = 9 AND d = 1 AND id = 1", // absent key
        "WHERE w = 2 AND d = 3 AND id = 4 AND pad = 'p234'",
        "WHERE w = 2 AND d = 3 AND id IS NULL", // NULL never seeks wrong
    ] {
        let got = rows(
            e,
            &format!("SELECT w, d, id, pad FROM c {q} ORDER BY 1,2,3,4"),
        );
        let want = rows(
            e,
            &format!("SELECT w, d, id, pad FROM oracle {q} ORDER BY 1,2,3,4"),
        );
        assert_eq!(got, want, "shape {q:?} diverged from the seq-scan oracle");
    }
    e.execute("DROP TABLE oracle").unwrap();
}

#[test]
fn pin_v7381_composite_pk_answers_match_a_seq_scan() {
    let mut e = Engine::new();
    seed(
        &mut e,
        "CREATE TABLE c (w BIGINT, d BIGINT, id BIGINT, pad TEXT, PRIMARY KEY (w, d, id))",
    );
    assert_matches_unindexed(&mut e);
    // The composite PK still enforces uniqueness.
    let err = e
        .execute("INSERT INTO c VALUES (1, 1, 1, 'dup')")
        .unwrap_err();
    assert!(format!("{err}").contains("duplicate"), "{err}");
    // Mutations keep the map honest.
    e.execute("UPDATE c SET id = 99 WHERE w = 1 AND d = 1 AND id = 1")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT pad FROM c WHERE w = 1 AND d = 1 AND id = 99"
        ),
        vec![vec![String::from("p111")]]
    );
    assert!(rows(&mut e, "SELECT pad FROM c WHERE w = 1 AND d = 1 AND id = 1").is_empty());
    e.execute("DELETE FROM c WHERE w = 1 AND d = 1 AND id = 99")
        .unwrap();
    assert!(
        rows(
            &mut e,
            "SELECT pad FROM c WHERE w = 1 AND d = 1 AND id = 99"
        )
        .is_empty()
    );
}

#[test]
fn pin_v7381_composite_create_index_answers_match_a_seq_scan() {
    let mut e = Engine::new();
    seed(
        &mut e,
        "CREATE TABLE c (w BIGINT, d BIGINT, id BIGINT, pad TEXT)",
    );
    // Rows with NULL components must not vanish from prefix probes.
    e.execute("INSERT INTO c VALUES (2, 3, NULL, 'null-id')")
        .unwrap();
    e.execute("CREATE INDEX c_wdi ON c (w, d, id)").unwrap();
    assert_matches_unindexed(&mut e);
    // The indexdef still names every column, exactly as before L12.
    let def = rows(
        &mut e,
        "SELECT indexdef FROM pg_indexes WHERE indexname = 'c_wdi'",
    );
    assert_eq!(def.len(), 1);
    assert!(def[0][0].contains("(w, d, id)"), "{}", def[0][0]);
}

#[test]
fn pin_v7381_composite_index_with_text_component() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cust (w BIGINT, d BIGINT, last TEXT, first TEXT)")
        .unwrap();
    for w in 1..=2i64 {
        for d in 1..=2i64 {
            for (l, f) in [("BARBAR", "alice"), ("BARBAR", "bob"), ("OUGHT", "carol")] {
                e.execute(&format!("INSERT INTO cust VALUES ({w}, {d}, '{l}', '{f}')"))
                    .unwrap();
            }
        }
    }
    e.execute("CREATE INDEX idx_cust ON cust (w, d, last)")
        .unwrap();
    let got = rows(
        &mut e,
        "SELECT first FROM cust WHERE w = 2 AND d = 1 AND last = 'BARBAR' ORDER BY 1",
    );
    assert_eq!(
        got,
        vec![vec![String::from("alice")], vec![String::from("bob")]]
    );
    assert!(
        rows(
            &mut e,
            "SELECT first FROM cust WHERE w = 2 AND d = 1 AND last = 'barbar'"
        )
        .is_empty()
    );
}
