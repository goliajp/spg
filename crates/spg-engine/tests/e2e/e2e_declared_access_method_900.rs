//! 9.0.0 — `USING gist / spgist / hash` came back as `USING btree`.
//!
//! SPG backs all three with a B-tree, and the catalog said so —
//! deliberately, per the code's own note that it "reports what the
//! index actually IS". The cost of that honesty was a dump that does
//! not round-trip: `USING gist` came back as `USING btree`, so
//! restoring into PostgreSQL silently changed the index type.
//!
//! Answering is unaffected either way. SPG evaluates `&&`, `<@` and `%`
//! by scan whether or not an index exists, so what those AMs buy is
//! speed, and what the catalog owes is the name the statement wrote.
//! That is the line the project's IRON RULE draws: align on the
//! observable, keep the internals SPG's own.
//!
//! Measured on PG 18.6 (with a `gist`-able column type):
//!
//! ```text
//!   pg_indexes.indexdef   CREATE INDEX zz_gi ON public.zz_g USING gist (p)
//!   pg_class JOIN pg_am   zz_gi -> gist
//! ```

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE g (p int, q text)",
        "CREATE INDEX gi ON g USING gist (p)",
        "CREATE INDEX si ON g USING spgist (p)",
        "CREATE INDEX hi ON g USING hash (q)",
        "CREATE INDEX bi ON g (p)",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

#[test]
fn the_definition_names_the_method_the_statement_wrote() {
    let mut e = seeded();
    assert_eq!(
        col(
            &mut e,
            "SELECT indexdef FROM pg_indexes WHERE tablename='g' ORDER BY indexname"
        ),
        vec![
            "CREATE INDEX bi ON public.g USING btree (p)".to_string(),
            "CREATE INDEX gi ON public.g USING gist (p)".to_string(),
            "CREATE INDEX hi ON public.g USING hash (q)".to_string(),
            "CREATE INDEX si ON public.g USING spgist (p)".to_string(),
        ]
    );
}

#[test]
fn pg_class_relam_joins_to_the_same_name() {
    // A client that reads the access method through the catalog join
    // must get the same answer as one that reads the definition text.
    let mut e = seeded();
    assert_eq!(
        col(
            &mut e,
            "SELECT c.relname||'->'||a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam \
             WHERE c.relname IN ('gi','si','hi','bi') ORDER BY 1"
        ),
        vec![
            "bi->btree".to_string(),
            "gi->gist".to_string(),
            "hi->hash".to_string(),
            "si->spgist".to_string(),
        ]
    );
}

#[test]
fn the_index_still_answers_and_a_btree_is_still_a_btree() {
    let mut e = seeded();
    e.execute("INSERT INTO g VALUES (1,'a'),(2,'b')")
        .expect("insert");
    // The B-tree underneath is what answers; the name did not change
    // that.
    assert_eq!(col(&mut e, "SELECT q FROM g WHERE p = 2"), vec!["b"]);
    assert_eq!(
        col(
            &mut e,
            "SELECT indexdef FROM pg_indexes WHERE indexname='bi'"
        ),
        vec!["CREATE INDEX bi ON public.g USING btree (p)".to_string()]
    );
}
