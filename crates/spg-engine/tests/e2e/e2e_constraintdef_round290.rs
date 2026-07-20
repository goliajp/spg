//! v7.39 (round 290) — `pg_get_constraintdef` for foreign keys, and
//! declared constraint names.
//!
//! Every FOREIGN KEY deparsed to an EMPTY STRING. Not an error, not a
//! NULL — empty, which reads as "this constraint has no definition".
//! The cause was a name mismatch: the deparse fell back to `{t}_fk{i}`
//! while `pg_constraint` reports PG's `{t}_{col}_fkey`, so the lookup
//! never matched and fell through. The uniqueness arm right above it
//! carries a comment promising exactly the alignment the FK arm did
//! not keep.
//!
//! A DECLARED constraint name was stored and then ignored by both the
//! catalog views and the deparse, so `CONSTRAINT rdc_uq UNIQUE (code)`
//! reported as the synthesised `rdc_code_key` — a dump would name the
//! constraint something the user never wrote. The inline copy of the
//! naming rule in the deparse is how the two drifted; it calls the
//! shared helper now.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    rows.iter()
        .map(|row| {
            row.values
                .iter()
                .map(|v| match v {
                    spg_storage::Value::Null => String::new(),
                    other => spg_engine::eval::value_to_text(other),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rdp (id int primary key, code text UNIQUE)")
        .unwrap();
    e.execute(
        "CREATE TABLE rdc (
           id int PRIMARY KEY,
           pid int REFERENCES rdp(id) ON DELETE CASCADE ON UPDATE SET NULL,
           qty int NOT NULL CHECK (qty > 0),
           code text,
           CONSTRAINT rdc_uq UNIQUE (code),
           CONSTRAINT rdc_ck2 CHECK (qty < 100 AND code IS NOT NULL)
         )",
    )
    .unwrap();
    e
}

#[test]
fn a_foreign_key_deparses_instead_of_returning_empty() {
    let mut e = fixture();
    assert_eq!(
        rows(
            &mut e,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conname = 'rdc_pid_fkey'",
        ),
        vec!["FOREIGN KEY (pid) REFERENCES rdp(id) ON UPDATE SET NULL ON DELETE CASCADE"],
    );
}

#[test]
fn the_referential_actions_print_in_pgs_order() {
    // Declared ON DELETE first, printed ON UPDATE first — PG's order is
    // fixed, not the declaration's.
    let mut e = fixture();
    let def = &rows(
        &mut e,
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'rdc_pid_fkey'",
    )[0];
    let upd = def.find("ON UPDATE").expect("ON UPDATE present");
    let del = def.find("ON DELETE").expect("ON DELETE present");
    assert!(upd < del, "{def}");
}

#[test]
fn a_declared_constraint_name_is_reported_not_a_synthesised_one() {
    let mut e = fixture();
    assert_eq!(
        rows(
            &mut e,
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = 'rdc'::regclass AND contype = 'u' ORDER BY conname",
        ),
        vec!["rdc_uq|UNIQUE (code)"],
    );
}

#[test]
fn an_undeclared_one_still_gets_pgs_synthesised_name() {
    // The other side of the same rule — `code text UNIQUE` on rdp was
    // never named, so PG's `{t}_{col}_key` stands.
    let mut e = fixture();
    assert_eq!(
        rows(
            &mut e,
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = 'rdp'::regclass AND contype = 'u' ORDER BY conname",
        ),
        vec!["rdp_code_key|UNIQUE (code)"],
    );
}

#[test]
fn the_whole_constraint_set_matches_pg() {
    let mut e = fixture();
    assert_eq!(
        rows(
            &mut e,
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = 'rdc'::regclass ORDER BY conname",
        ),
        vec![
            "rdc_ck2|CHECK (((qty < 100) AND (code IS NOT NULL)))",
            "rdc_id_not_null|NOT NULL id",
            "rdc_pid_fkey|FOREIGN KEY (pid) REFERENCES rdp(id) ON UPDATE SET NULL ON DELETE CASCADE",
            "rdc_pkey|PRIMARY KEY (id)",
            "rdc_qty_check|CHECK ((qty > 0))",
            "rdc_qty_not_null|NOT NULL qty",
            "rdc_uq|UNIQUE (code)",
        ],
    );
}
