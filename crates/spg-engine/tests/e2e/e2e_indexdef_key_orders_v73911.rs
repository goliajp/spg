//! v7.39.11 — `pg_get_indexdef` renders every key column's ordering
//! clause, and every key column.
//!
//! Reported by sentori against 7.39.10: `CREATE INDEX … (a, b DESC)`
//! read back as `(a, b)`, so a dump lost the clause and a schema diff
//! saw drift on every run. Two defects sat behind it.
//!
//!   1. The parser parsed each extra column's `ASC` / `DESC` /
//!      `NULLS …` and discarded it. Only the LEADING column's survived
//!      (round 537 kept that one).
//!   2. A bare column WITH an ordering clause is stored as an
//!      expression, and the expression branch of the renderer appended
//!      no extra columns at all — so `(a DESC, b)` read back as
//!      `(a DESC)`, the second column gone from the definition.
//!
//! Every expectation below is PostgreSQL 18.6's own output for the
//! same DDL, byte for byte:
//!
//! ```text
//!   CREATE INDEX dsc_desc  ON public.dsc USING btree (a, b DESC)
//!   CREATE INDEX dsc_lead  ON public.dsc USING btree (a DESC, b)
//!   CREATE INDEX dsc_mix   ON public.dsc USING btree (a DESC NULLS LAST, b NULLS FIRST, c)
//!   CREATE INDEX dsc_nulls ON public.dsc USING btree (a, b DESC NULLS LAST)
//!   CREATE INDEX dsc_plain ON public.dsc USING btree (a, b)
//! ```
//!
//! Note `b NULLS FIRST` in `dsc_mix`: PostgreSQL omits the word when
//! it matches the default for that column's direction — ascending
//! defaults to NULLS LAST, descending to NULLS FIRST — so `ASC NULLS
//! FIRST` keeps the words and `ASC NULLS LAST` drops them.
//!
//! SPG's index does not scan in a per-column direction, so none of
//! this changes a lookup. It changes what a dump says, which is the
//! whole job of `pg_get_indexdef`.

use spg_engine::{Engine, QueryResult};

fn defs(e: &mut Engine) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute("SELECT indexdef FROM pg_indexes WHERE tablename = 'dsc' ORDER BY indexname")
        .unwrap()
    else {
        panic!("expected Rows")
    };
    rows.iter()
        .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
        .collect()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE dsc (a int, b int, c text)",
        "CREATE INDEX dsc_desc  ON dsc (a, b DESC)",
        "CREATE INDEX dsc_nulls ON dsc (a, b DESC NULLS LAST)",
        "CREATE INDEX dsc_lead  ON dsc (a DESC, b)",
        "CREATE INDEX dsc_mix   ON dsc (a DESC NULLS LAST, b ASC NULLS FIRST, c)",
        "CREATE INDEX dsc_plain ON dsc (a, b)",
    ] {
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
    }
    e
}

const PG_18_6: [&str; 5] = [
    "CREATE INDEX dsc_desc ON public.dsc USING btree (a, b DESC)",
    "CREATE INDEX dsc_lead ON public.dsc USING btree (a DESC, b)",
    "CREATE INDEX dsc_mix ON public.dsc USING btree (a DESC NULLS LAST, b NULLS FIRST, c)",
    "CREATE INDEX dsc_nulls ON public.dsc USING btree (a, b DESC NULLS LAST)",
    "CREATE INDEX dsc_plain ON public.dsc USING btree (a, b)",
];

#[test]
fn every_definition_matches_postgresql_byte_for_byte() {
    assert_eq!(defs(&mut seeded()), PG_18_6);
}

#[test]
fn a_definition_that_is_replayed_reproduces_itself() {
    // The round trip is the point: a dump is only correct if reloading
    // it produces the same catalog. Feeding our own output back has to
    // land on the same output.
    let first = defs(&mut seeded());
    let mut e = Engine::new();
    e.execute("CREATE TABLE dsc (a int, b int, c text)")
        .unwrap();
    for def in &first {
        e.execute(def)
            .unwrap_or_else(|x| panic!("replaying our own indexdef {def:?}: {x:?}"));
    }
    assert_eq!(defs(&mut e), first);
}

#[test]
fn the_clauses_survive_a_catalog_snapshot() {
    // Catalog FILE_VERSION 95 carries the extra columns' orders. A
    // field that is not persisted is a field that vanishes at the next
    // restart, which is how this project has lost one five times.
    let mut e = seeded();
    let before = defs(&mut e);
    let snapshot = e.catalog().serialize();
    let reloaded = spg_storage::Catalog::deserialize(&snapshot).expect("catalog roundtrip");
    let mut e2 = Engine::restore(reloaded);
    assert_eq!(defs(&mut e2), before);
}
