//! v7.39 (read01 utils/adt, round 30) — the regclass dual-shape fix:
//! `'t'::regclass` carries BOTH the synthetic oid (for catalog joins)
//! and the name (for display), so ORM introspection like
//! `WHERE conrelid = 't'::regclass` works end to end. Partial unique
//! indexes no longer appear in pg_constraint. Byte-locked vs PG18.

use spg_engine::{Engine, QueryResult};

fn col_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn constraint_introspection_by_regclass() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE ru_t (id int PRIMARY KEY, v text NOT NULL, \
         k int CHECK (k > 0), u int UNIQUE)",
    )
    .unwrap();
    e.execute("CREATE UNIQUE INDEX ru_uidx ON ru_t (k) WHERE k > 5")
        .unwrap();
    // The canonical ORM shape: conrelid (oid) vs a ::regclass cast.
    // PG18 rows: not-null constraints included; the PARTIAL unique
    // index is NOT a constraint.
    assert_eq!(
        col_of(
            &mut e,
            "SELECT conname FROM pg_constraint WHERE conrelid = 'ru_t'::regclass \
             ORDER BY conname"
        ),
        vec![
            "ru_t_id_not_null",
            "ru_t_k_check",
            "ru_t_pkey",
            "ru_t_u_key",
            "ru_t_v_not_null"
        ]
    );
    // Dual shape: renders as the name, compares as the oid, and the
    // ::text round-trip keeps the name.
    assert_eq!(
        col_of(
            &mut e,
            "SELECT 'ru_t'::regclass, 'ru_t'::regclass::text, \
             pg_typeof('ru_t'::regclass)"
        ),
        vec!["ru_t|ru_t|regclass"]
    );
}
