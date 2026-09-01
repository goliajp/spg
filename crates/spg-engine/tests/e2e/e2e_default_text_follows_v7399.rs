//! v7.39.9 — a column's DEFAULT, as the catalog views and the DUMP
//! report it, follows the column.
//!
//! `ColumnSchema.default_text` is the source text
//! `information_schema.columns.column_default`, `pg_attrdef` and
//! `Engine::dump_sql` all read, and it was written once at CREATE TABLE
//! and never again. So after `ALTER TABLE t ALTER COLUMN b DROP
//! DEFAULT` — PostgreSQL's own spelling, not some dialect corner — the
//! catalog held no default and the dump still wrote `DEFAULT 5`:
//!
//! ```text
//!   CREATE TABLE m1 (id INT PRIMARY KEY, b INT NOT NULL DEFAULT 5);
//!   ALTER TABLE m1 ALTER COLUMN b DROP DEFAULT;
//!
//!   catalog                       default = None
//!   information_schema            column_default = '5'
//!   dump_sql()                    b integer NOT NULL DEFAULT 5,
//! ```
//!
//! Restore that dump and the default is back: the schema you get is not
//! the schema you dumped. `SET DEFAULT` had the mirror problem,
//! reporting and dumping the value it replaced.
//!
//! Every published version with `default_text` behaved this way — it
//! arrived in v7.38 for catalog introspection and nothing taught it
//! about ALTER.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE m1 (id INT PRIMARY KEY, b INT NOT NULL DEFAULT 5, c INT DEFAULT 1)")
        .unwrap();
    e
}

fn view_default(e: &mut Engine, column: &str) -> Option<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(&format!(
            "SELECT column_default FROM information_schema.columns \
             WHERE table_name = 'm1' AND column_name = '{column}'"
        ))
        .unwrap()
    else {
        panic!("expected Rows");
    };
    match &rows.first().expect("the column is there").values[0] {
        spg_storage::Value::Text(t) => Some(t.to_string()),
        spg_storage::Value::Null => None,
        other => panic!("{other:?}"),
    }
}

fn catalog_default(e: &Engine, pos: usize) -> bool {
    let t = e.catalog().get("m1").expect("m1");
    t.schema().columns[pos].default.is_some() || t.schema().columns[pos].runtime_default.is_some()
}

fn dump_line(e: &mut Engine, column: &str) -> String {
    e.dump_sql()
        .unwrap()
        .lines()
        .find(|l| l.trim_start().starts_with(&format!("{column} ")))
        .unwrap_or_else(|| panic!("no dump line for {column}"))
        .trim()
        .to_string()
}

#[test]
fn dropping_a_default_drops_it_from_the_view() {
    let mut e = seeded();
    assert_eq!(view_default(&mut e, "b").as_deref(), Some("5"));
    e.execute("ALTER TABLE m1 ALTER COLUMN b DROP DEFAULT")
        .unwrap();
    assert!(!catalog_default(&e, 1), "the catalog kept a default");
    assert_eq!(
        view_default(&mut e, "b"),
        None,
        "information_schema still reports a default the catalog does not have"
    );
}

#[test]
fn dropping_a_default_drops_it_from_the_dump() {
    let mut e = seeded();
    e.execute("ALTER TABLE m1 ALTER COLUMN b DROP DEFAULT")
        .unwrap();
    let line = dump_line(&mut e, "b");
    assert!(
        !line.to_uppercase().contains("DEFAULT"),
        "the dump writes back a default that was dropped: {line}"
    );
}

#[test]
fn setting_a_default_replaces_the_old_one_everywhere() {
    let mut e = seeded();
    e.execute("ALTER TABLE m1 ALTER COLUMN c SET DEFAULT 9")
        .unwrap();
    assert_eq!(view_default(&mut e, "c").as_deref(), Some("9"));
    let line = dump_line(&mut e, "c");
    assert!(
        line.contains("DEFAULT 9") && !line.contains("DEFAULT 1"),
        "the dump kept the replaced default: {line}"
    );
}

#[test]
fn a_dump_restores_to_the_same_schema() {
    // The point of the whole thing: what comes back has to be what was
    // there.
    let mut e = seeded();
    e.execute("ALTER TABLE m1 ALTER COLUMN b DROP DEFAULT")
        .unwrap();
    e.execute("ALTER TABLE m1 ALTER COLUMN c SET DEFAULT 9")
        .unwrap();
    let sql = e.dump_sql().unwrap();

    let mut restored = Engine::new();
    for stmt in sql.split(";\n") {
        // Strip the dump's leading comment LINES rather than skipping a
        // chunk that begins with one — the header comment shares its
        // chunk with the first CREATE TABLE, and dropping the pair was
        // what made this test fail on a dump that was already correct.
        let body: String = stmt
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        let s = body.trim();
        if s.is_empty() {
            continue;
        }
        restored
            .execute(s)
            .unwrap_or_else(|e| panic!("restoring {s:?}: {e:?}"));
    }
    assert_eq!(
        view_default(&mut restored, "b"),
        None,
        "b's default came back"
    );
    assert_eq!(
        view_default(&mut restored, "c").as_deref(),
        Some("9"),
        "c restored with the wrong default"
    );
}

#[test]
fn a_runtime_default_follows_too() {
    // The volatile path stores the expression rather than a value, and
    // it has its own branch in the setter.
    let mut e = seeded();
    e.execute("ALTER TABLE m1 ALTER COLUMN c SET DEFAULT now()")
        .unwrap();
    let v = view_default(&mut e, "c").expect("a default");
    assert!(
        v.contains("now"),
        "the view reports {v:?} rather than the expression just set"
    );
}
