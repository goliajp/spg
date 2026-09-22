//! 9.0.2 — sentori's reply to 9.0.1, the items that are observable in the
//! engine itself. Every expectation is PostgreSQL 18.6's answer, measured
//! by sentori on the published 9.0.1 image and re-measured here against
//! a PostgreSQL 18.6 container before the fix.

use spg_engine::{Engine, QueryResult};

fn cells(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(" "),
        other => panic!("{sql}: {other:?}"),
    }
}

fn column_names(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { columns, .. } => columns.iter().map(|c| c.name.clone()).collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
}

/// The shipped configuration: the database collates by a locale.
fn locale_engine() -> Engine {
    let mut e = Engine::new();
    e.set_database_collation("en_US.UTF-8")
        .expect("the shipped collation");
    e
}

// §4.1 — a DEFAULT that names a sequence the schema created draws from it.
#[test]
fn a_default_nextval_draws_from_the_sequence_it_names() {
    let mut e = Engine::new();
    run(&mut e, "CREATE SEQUENCE s902 START 100");
    run(
        &mut e,
        "CREATE TABLE a902 (id int PRIMARY KEY DEFAULT nextval('s902'), v text)",
    );
    run(
        &mut e,
        "CREATE TABLE b902 (id int PRIMARY KEY DEFAULT nextval('s902'), v text)",
    );
    run(&mut e, "INSERT INTO a902 (v) VALUES ('a'), ('b')");
    run(&mut e, "INSERT INTO b902 (v) VALUES ('c'), ('d')");
    // PostgreSQL 18.6: 100,101 and 102,103 — one counter, two tables,
    // no id shared. 9.0.1 gave 1,2 to both.
    assert_eq!(cells(&mut e, "SELECT id FROM a902 ORDER BY id"), "100 101");
    assert_eq!(cells(&mut e, "SELECT id FROM b902 ORDER BY id"), "102 103");
    assert_eq!(cells(&mut e, "SELECT last_value FROM s902"), "103");
}

// §4.2 — `_` before a capital letter.
#[test]
fn an_underscore_before_a_capital_collates_the_way_glibc_does() {
    let mut e = locale_engine();
    // PostgreSQL 18.6, `en_US.utf8`: every one of these is `t`. SPG broke
    // the tie by bytes, and `_` (0x5F) sits between the capitals and the
    // small letters — so the capital rows were `f` and the small one `t`.
    assert_eq!(
        cells(
            &mut e,
            "SELECT '_Z' < 'Z', '_A' < 'A', 'A_B' < 'AB', '_z' < 'z', 'A B' < 'AB'"
        ),
        "true|true|true|true|true"
    );
    run(&mut e, "CREATE TABLE u902 (s text)");
    run(
        &mut e,
        "INSERT INTO u902 VALUES ('Z'), ('_Z'), ('Y'), ('_Y')",
    );
    assert_eq!(cells(&mut e, "SELECT s FROM u902 ORDER BY s"), "_Y Y _Z Z");
    // The control, in the same engine: punctuation alone still orders by
    // its bytes, which is PostgreSQL's answer for that shape.
    run(&mut e, "CREATE TABLE p902 (s text)");
    run(&mut e, "INSERT INTO p902 VALUES ('_'), ('.'), ('-'), (' ')");
    assert_eq!(cells(&mut e, "SELECT s FROM p902 ORDER BY s"), "  - . _");
}

// §5 — `x COLLATE "C"` keeps x's name.
#[test]
fn a_collate_clause_keeps_the_columns_name() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE w902 (s text)");
    assert_eq!(
        column_names(&mut e, "SELECT s COLLATE \"C\" FROM w902"),
        ["s"]
    );
}

// §5 — DROP SCHEMA … CASCADE says what it took.
#[test]
fn drop_schema_cascade_names_what_it_takes() {
    let mut e = Engine::new();
    run(&mut e, "CREATE SCHEMA c902");
    run(&mut e, "CREATE TABLE c902.t (id int)");
    run(&mut e, "CREATE VIEW c902.v1 AS SELECT id FROM c902.t");
    run(&mut e, "CREATE VIEW c902.v3 AS SELECT id FROM c902.t");
    let _ = e.take_notices(); // the CREATEs above said nothing worth keeping
    run(&mut e, "DROP SCHEMA c902 CASCADE");
    let notices: Vec<String> = e.take_notices().into_iter().map(|n| n.message).collect();
    assert_eq!(
        notices,
        [
            "drop cascades to 3 other objects\nDETAIL:  drop cascades to table c902.t\n\
          drop cascades to view c902.v1\ndrop cascades to view c902.v3"
        ]
    );
}

// §5 — a serial's sequence has the column's type.
#[test]
fn a_serial_sequence_has_its_columns_type() {
    let mut e = Engine::new();
    run(
        &mut e,
        "CREATE TABLE q902 (a smallserial, b serial, c bigserial)",
    );
    // PostgreSQL 18.6: smallint to 32767, integer to 2147483647, bigint.
    assert_eq!(
        cells(
            &mut e,
            "SELECT c.relname, format_type(seqtypid, NULL), seqmax FROM pg_sequence s \
             JOIN pg_class c ON c.oid = s.seqrelid WHERE c.relname LIKE 'q902%' ORDER BY 1"
        ),
        "q902_a_seq|smallint|32767 q902_b_seq|integer|2147483647 \
         q902_c_seq|bigint|9223372036854775807"
    );
}

// Found while re-measuring: a serial's sequence outlived its table.
#[test]
fn dropping_a_table_drops_the_sequence_its_column_owns() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE d902 (id serial PRIMARY KEY, v text)");
    run(&mut e, "INSERT INTO d902 (v) VALUES ('a'), ('b')");
    run(&mut e, "DROP TABLE d902");
    assert_eq!(
        cells(
            &mut e,
            "SELECT count(*) FROM pg_class WHERE relname = 'd902_id_seq'"
        ),
        "0"
    );
    // …so a new table of the same shape numbers from 1, as PostgreSQL's
    // does. 9.0.1 carried on from 3.
    run(&mut e, "CREATE TABLE d902 (id serial PRIMARY KEY, v text)");
    run(&mut e, "INSERT INTO d902 (v) VALUES ('c')");
    assert_eq!(cells(&mut e, "SELECT id FROM d902"), "1");
}

// DROP SCHEMA without CASCADE names each dependent, in PostgreSQL's words.
#[test]
fn drop_schema_restrict_names_each_dependent() {
    let mut e = Engine::new();
    run(&mut e, "CREATE SCHEMA r902");
    run(&mut e, "CREATE TABLE r902.t (id int)");
    run(&mut e, "CREATE VIEW r902.v AS SELECT id FROM r902.t");
    let err = e
        .execute("DROP SCHEMA r902")
        .expect_err("dependents")
        .to_string();
    assert!(
        err.contains(
            "cannot drop schema r902 because other objects depend on it\n\
             DETAIL:  table r902.t depends on schema r902\n\
             view r902.v depends on schema r902\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too."
        ),
        "{err}"
    );
}

// pg_dump's serial spelling in a schema: the sequence stays in its schema.
#[test]
fn a_set_default_nextval_keeps_the_sequences_schema() {
    let mut e = Engine::new();
    run(&mut e, "CREATE SCHEMA n902");
    run(&mut e, "CREATE TABLE n902.s (id integer NOT NULL, v text)");
    run(
        &mut e,
        "CREATE SEQUENCE n902.s_id_seq AS integer START WITH 1",
    );
    run(&mut e, "ALTER SEQUENCE n902.s_id_seq OWNED BY n902.s.id");
    run(
        &mut e,
        "ALTER TABLE ONLY n902.s ALTER COLUMN id SET DEFAULT nextval('n902.s_id_seq'::regclass)",
    );
    run(&mut e, "SELECT pg_catalog.setval('n902.s_id_seq', 7, true)");
    run(&mut e, "INSERT INTO n902.s (v) VALUES ('x')");
    // Restoring SPG's own dump created this sequence in `public` too:
    // the schema was dropped from the name the default carries.
    assert_eq!(
        cells(
            &mut e,
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind = 'S' AND n.nspname = 'public'"
        ),
        "0"
    );
    assert_eq!(cells(&mut e, "SELECT id FROM n902.s"), "8");
}
