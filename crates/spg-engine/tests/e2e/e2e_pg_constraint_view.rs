//! v7.17.0 Phase 3.P0-54 + v7.37.24 (24.8b-3) — pg_constraint view.
//! Widened to 20 PG-canonical columns; conrelid + confrelid are
//! now BigInt OIDs (joinable with pg_class.oid). conkey / confkey
//! are PG int2vector strings with the column names appended in
//! square brackets for human readability.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn pg_constraint_lists_primary_key() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, PRIMARY KEY (id))")
        .unwrap();
    let r = rows(
        e.execute(
            "SELECT contype, conkey FROM pg_catalog.pg_constraint \
             WHERE contype = 'p'",
        )
        .unwrap(),
    );
    assert!(!r.is_empty());
    assert_eq!(r[0][0], Value::text("p"));
    // conkey is now `<1-based int2vector> [name1,name2,…]`.
    if let Value::Text(s) = &r[0][1] {
        assert!(s.starts_with("1 ["), "conkey shape: {s:?}");
        assert!(s.contains("id"), "conkey must include `id`: {s:?}");
    } else {
        panic!("conkey wrong type");
    }
}

#[test]
fn pg_constraint_lists_foreign_key() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE parents (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    e.execute(
        "CREATE TABLE children (id INT NOT NULL, parent_id INT NOT NULL, \
         FOREIGN KEY (parent_id) REFERENCES parents (id))",
    )
    .unwrap();
    let r = rows(
        e.execute(
            "SELECT contype, conrelid, confrelid, conkey, confkey \
             FROM pg_catalog.pg_constraint WHERE contype = 'f'",
        )
        .unwrap(),
    );
    assert!(!r.is_empty());
    assert_eq!(r[0][0], Value::text("f"));
    // conrelid + confrelid are now BigInt OIDs that match
    // pg_class.oid for the respective tables.
    let cls = rows(
        e.execute("SELECT oid, relname FROM pg_catalog.pg_class")
            .unwrap(),
    );
    let oid_of = |t: &str| -> i64 {
        cls.iter()
            .find(|row| matches!(&row[1], Value::Text(s) if s.as_ref() == t))
            .map(|row| match row[0] {
                Value::BigInt(o) => o,
                _ => panic!(),
            })
            .unwrap_or_else(|| panic!("no pg_class row for {t}"))
    };
    assert_eq!(r[0][1], Value::BigInt(oid_of("children")));
    assert_eq!(r[0][2], Value::BigInt(oid_of("parents")));
    // conkey / confkey shapes — PG int2vector + name suffix.
    for (idx, (want_pos, want_name)) in [(3, ("2", "parent_id")), (4, ("1", "id"))].iter() {
        if let Value::Text(s) = &r[0][*idx] {
            assert!(
                s.starts_with(*want_pos) && s.contains(*want_name),
                "col {idx} shape (got {s:?}, expected pos {want_pos} name {want_name})"
            );
        } else {
            panic!("col {idx} wrong type");
        }
    }
}

#[test]
fn pg_constraint_lists_composite_unique() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT NOT NULL, b INT NOT NULL, UNIQUE (a, b))")
        .unwrap();
    let r = rows(
        e.execute(
            "SELECT contype, conkey FROM pg_catalog.pg_constraint \
             WHERE contype = 'u'",
        )
        .unwrap(),
    );
    assert!(!r.is_empty());
    assert_eq!(r[0][0], Value::text("u"));
    if let Value::Text(s) = &r[0][1] {
        // Two columns: `1 2 [a,b]`.
        assert!(s.starts_with("1 2 ["), "conkey shape: {s:?}");
        assert!(s.contains("a") && s.contains("b"), "names: {s:?}");
    } else {
        panic!("conkey wrong type");
    }
}

#[test]
fn pg_constraint_emits_pg_canonical_column_set() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_constraint").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "oid",
        "conname",
        "connamespace",
        "contype",
        "condeferrable",
        "convalidated",
        "conrelid",
        "conindid",
        "confrelid",
        "confupdtype",
        "confdeltype",
        "confmatchtype",
        "conislocal",
        "conkey",
        "confkey",
    ] {
        assert!(
            names.contains(&must),
            "pg_constraint missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn pg_constraint_fk_action_chars_match_pg() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    e.execute(
        "CREATE TABLE c (id INT NOT NULL, pid INT NOT NULL, \
         FOREIGN KEY (pid) REFERENCES p (id) ON DELETE CASCADE ON UPDATE SET NULL)",
    )
    .unwrap();
    let r = rows(
        e.execute(
            "SELECT confupdtype, confdeltype FROM pg_catalog.pg_constraint WHERE contype = 'f'",
        )
        .unwrap(),
    );
    assert!(!r.is_empty());
    // ON UPDATE SET NULL → 'n'; ON DELETE CASCADE → 'c'.
    assert_eq!(r[0][0], Value::text("n"));
    assert_eq!(r[0][1], Value::text("c"));
}

#[test]
fn fk_default_name_and_referential_constraints_match_pg() {
    // Verified vs live PG18.4: an unnamed FK is named
    // `{table}_{col}_fkey`, information_schema.referential_constraints
    // exposes the parent's unique_constraint_name, and the default
    // referential action (no ON DELETE/UPDATE clause) is NO ACTION.
    let mut e = Engine::new();
    e.execute("CREATE TABLE parent (id INT PRIMARY KEY, code TEXT UNIQUE)")
        .unwrap();
    e.execute(
        "CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent (id), tag TEXT)",
    )
    .unwrap();

    // key_column_usage: FK named child_parent_id_fkey (not child_fk0).
    let r = rows(
        e.execute(
            "SELECT constraint_name, column_name FROM information_schema.key_column_usage \
             WHERE table_name = 'child' ORDER BY constraint_name, ordinal_position",
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::text("child_parent_id_fkey"));
    assert_eq!(r[0][1], Value::text("parent_id"));

    // referential_constraints: unique_constraint_name = parent_pkey,
    // both rules NO ACTION by default.
    let r = rows(
        e.execute(
            "SELECT unique_constraint_name, update_rule, delete_rule \
             FROM information_schema.referential_constraints \
             WHERE constraint_name = 'child_parent_id_fkey'",
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::text("parent_pkey"));
    assert_eq!(r[0][1], Value::text("NO ACTION"));
    assert_eq!(r[0][2], Value::text("NO ACTION"));

    // An explicit ON DELETE clause is still honoured.
    e.execute(
        "CREATE TABLE gc (id INT PRIMARY KEY, cid INT REFERENCES child (id) ON DELETE CASCADE)",
    )
    .unwrap();
    let r = rows(
        e.execute(
            "SELECT delete_rule FROM information_schema.referential_constraints \
             WHERE constraint_name = 'gc_cid_fkey'",
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::text("CASCADE"));
}
