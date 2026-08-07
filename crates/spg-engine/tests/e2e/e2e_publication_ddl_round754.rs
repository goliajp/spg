//! Round 754 (F31-B4 + F31-B5) — publication / subscription DDL edges,
//! every answer PG18-measured in the round-754 differential:
//!
//! - `DROP PUBLICATION nosuch` REFUSES (`publication "x" does not
//!   exist`) — the old executor comment claimed a "PG-compatible
//!   silent no-op", which was measured false. `IF EXISTS` (a round-753
//!   probe tripped over it as a syntax error) skips quietly. Same
//!   contract for DROP SUBSCRIPTION.
//! - `FOR TABLES t` (bare plural) refuses with PG's `invalid
//!   publication object list`; `FOR TABLES IN SCHEMA public` works and
//!   folds to the all-tables scope; any other schema refuses
//!   (`schema "x" does not exist`).
//! - Every relation in `FOR TABLE` / `FOR ALL TABLES EXCEPT` must
//!   exist (`relation "x" does not exist`).

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

#[test]
fn round754_drop_publication_missing_refuses_if_exists_skips() {
    let mut e = Engine::new();
    assert!(
        err(&mut e, "DROP PUBLICATION nosuch_pub")
            .contains("publication \"nosuch_pub\" does not exist")
    );
    e.execute("DROP PUBLICATION IF EXISTS nosuch_pub").unwrap();
    assert!(
        err(&mut e, "DROP SUBSCRIPTION nosuch_sub")
            .contains("subscription \"nosuch_sub\" does not exist")
    );
    e.execute("DROP SUBSCRIPTION IF EXISTS nosuch_sub").unwrap();
    // The real thing still drops.
    e.execute("CREATE TABLE p754 (id INT)").unwrap();
    e.execute("CREATE PUBLICATION p754pub FOR TABLE p754")
        .unwrap();
    e.execute("DROP PUBLICATION p754pub").unwrap();
    assert!(
        err(&mut e, "DROP PUBLICATION p754pub").contains("publication \"p754pub\" does not exist")
    );
}

#[test]
fn round754_for_tables_spellings_answer_as_pg() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p754 (id INT)").unwrap();
    // Bare plural refuses at parse with PG's sentence.
    assert!(
        err(&mut e, "CREATE PUBLICATION bad FOR TABLES p754")
            .contains("invalid publication object list"),
    );
    // IN SCHEMA public folds to the all-tables scope.
    e.execute("CREATE PUBLICATION pubs FOR TABLES IN SCHEMA public")
        .unwrap();
    assert!(matches!(
        e.publications().get("pubs"),
        Some(spg_sql::ast::PublicationScope::AllTables)
    ));
    // Any other schema does not exist in SPG's single-schema world.
    assert!(
        err(
            &mut e,
            "CREATE PUBLICATION bad2 FOR TABLES IN SCHEMA myschema"
        )
        .contains("schema \"myschema\" does not exist")
    );
    // Unknown relations refuse on both listing forms.
    assert!(
        err(&mut e, "CREATE PUBLICATION bad3 FOR TABLE nosuch_table")
            .contains("relation \"nosuch_table\" does not exist")
    );
    assert!(
        err(
            &mut e,
            "CREATE PUBLICATION bad4 FOR ALL TABLES EXCEPT nosuch_table"
        )
        .contains("relation \"nosuch_table\" does not exist")
    );
}
