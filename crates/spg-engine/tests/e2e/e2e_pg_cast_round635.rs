//! v7.39 (round 635, F18) — `pg_cast` was an empty registry.
//!
//! PG has 235 rows; SPG had none, so a tool asking "what conversions exist"
//! was told: nothing. The table is now the 129 casts PG registers between
//! the 33 base types SPG has, with PG's `castcontext` and `castmethod` for
//! each.
//!
//! It could only be published because rounds 633 and 634 probed every one
//! of them against the engine first and closed the 23 it could not perform.
//! An earlier cut was deliberately held back for exactly that reason — a
//! catalog claiming conversions the engine refuses is worse than an empty
//! one.
//!
//! Publishing it surfaced one more gap, and the canonical join is what
//! surfaced it: `pg_cast JOIN pg_type` dropped the eight bit-string rows,
//! because SPG has had the bit VALUES since the family shipped and pg_type
//! never listed the TYPES. Both are there now, read off PG18.
//!
//! Recorded, not invented: `castfunc` is 0 on every row. PG names an
//! implementation function per row and SPG has no pg_proc entry for one; a
//! client reads the context and the method.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
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
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn round635_pg_cast_has_the_registry() {
    let mut e = Engine::new();
    assert_eq!(vals(&mut e, "SELECT count(*) FROM pg_cast"), vec!["129"]);
    // The three contexts, in PG's proportions for this set.
    assert_eq!(
        vals(
            &mut e,
            "SELECT castcontext, count(*) FROM pg_cast GROUP BY 1 ORDER BY 1"
        ),
        vec!["a|53", "e|24", "i|52"]
    );
    // …and the three methods.
    assert_eq!(
        vals(
            &mut e,
            "SELECT castmethod, count(*) FROM pg_cast GROUP BY 1 ORDER BY 1"
        ),
        vec!["b|16", "f|111", "i|2"]
    );
}

/// The canonical join — which is how a client actually reads it.
#[test]
fn round635_the_join_against_pg_type_resolves() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT ts.typname, tt.typname, c.castcontext, c.castmethod FROM pg_cast c \
             JOIN pg_type ts ON ts.oid = c.castsource \
             JOIN pg_type tt ON tt.oid = c.casttarget \
             WHERE ts.typname = 'int4' ORDER BY 2"
        ),
        vec![
            "int4|bit|e|f",
            "int4|bool|e|f",
            "int4|bytea|e|f",
            "int4|char|e|f",
            "int4|float4|i|f",
            "int4|float8|i|f",
            "int4|int2|a|f",
            "int4|int8|i|f",
            "int4|money|a|f",
            "int4|numeric|i|f",
            "int4|oid|i|b",
            "int4|regproc|i|b",
        ],
        "byte for byte what PG18 answers for the same query, for the types SPG has"
    );
    // Every row's endpoints resolve — nothing is orphaned by the join,
    // which is what the bit rows were before pg_type listed the type.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_cast c \
             JOIN pg_type ts ON ts.oid = c.castsource \
             JOIN pg_type tt ON tt.oid = c.casttarget"
        ),
        vec!["129"]
    );
}

/// The bit-string types the join needed.
#[test]
fn round635_pg_type_lists_the_bit_family() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT typname, oid, typlen, typcategory FROM pg_type \
             WHERE typname IN ('bit','varbit') ORDER BY 1"
        ),
        vec!["bit|1560|-1|V", "varbit|1562|-1|V"]
    );
    // The values were always there; only the catalog entry was missing.
    assert_eq!(
        vals(&mut e, "SELECT B'0110', B'01'::VARBIT"),
        vec!["0110|01"]
    );
}
