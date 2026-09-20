//! 9.0.0 — a partitioned table's index DECLARATION was invisible, and a
//! dump lost it.
//!
//! SPG keeps a parent's `CREATE INDEX` as a template every partition
//! replays. The template appeared in no catalog: `pg_indexes` had no
//! row for it, `pg_class` had no relation, `pg_inherits` recorded no
//! link — so `pg_dump` (which reads all three) wrote the partitions'
//! indexes alone. Restore that dump and the parent has no declaration
//! left: a partition created afterwards inherits nothing, silently.
//!
//! Measured against PostgreSQL 18.6, which carries the declaration as a
//! relation of its own (`relkind 'I'`), lists it `ON ONLY`, records
//! `pg_inherits` from it to each partition's index, and writes all
//! three lines in a dump.
//!
//! Four things had to agree before `pg_dump` would write it, and each
//! was silent on its own: `relhasindex` on the parent, the `pg_class`
//! row, its `relkind`, and the `pg_inherits` pairing.

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
        "CREATE TABLE pt (id int, v int) PARTITION BY RANGE (id)",
        "CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (100)",
        "CREATE INDEX pt_v ON pt (v)",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

#[test]
fn the_declaration_is_listed_on_only() {
    let mut e = seeded();
    assert_eq!(
        col(
            &mut e,
            "SELECT indexdef FROM pg_indexes WHERE indexname='pt_v'"
        ),
        vec!["CREATE INDEX pt_v ON ONLY public.pt USING btree (v)".to_string()]
    );
}

#[test]
fn the_four_things_pg_dump_reads_all_agree() {
    let mut e = seeded();
    // relhasindex on the PARENT, which holds no storage index.
    assert_eq!(
        col(
            &mut e,
            "SELECT relhasindex::text FROM pg_class WHERE relname='pt'"
        ),
        vec!["true".to_string()]
    );
    // the declaration's own relation, and PG's relkind for one.
    assert_eq!(
        col(
            &mut e,
            "SELECT relkind::text FROM pg_class WHERE relname='pt_v'"
        ),
        vec!["I".to_string()]
    );
    // and the inheritance from it to the partition's index.
    assert_eq!(
        col(
            &mut e,
            "SELECT ci.relname||' <- '||pi.relname FROM pg_inherits h \
             JOIN pg_class ci ON ci.oid=h.inhparent \
             JOIN pg_class pi ON pi.oid=h.inhrelid WHERE ci.relname='pt_v'"
        ),
        vec!["pt_v <- pt1_v_idx".to_string()]
    );
}

#[test]
fn on_only_declares_without_building_and_attach_is_accepted() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE qt (id int, v int) PARTITION BY RANGE (id)")
        .unwrap();
    e.execute("CREATE TABLE qt1 PARTITION OF qt FOR VALUES FROM (0) TO (100)")
        .unwrap();
    // The two lines a dump writes, in the order it writes them.
    e.execute("CREATE INDEX qt1_v_idx ON qt1 (v)").unwrap();
    e.execute("CREATE INDEX qt_v ON ONLY qt (v)").unwrap();
    e.execute("ALTER INDEX qt_v ATTACH PARTITION qt1_v_idx")
        .unwrap();
    // ONLY declared it without a second index on the partition.
    assert_eq!(
        col(
            &mut e,
            "SELECT indexname FROM pg_indexes WHERE tablename='qt1' ORDER BY 1"
        ),
        vec!["qt1_v_idx".to_string()]
    );
    // And a partition made afterwards still inherits the declaration —
    // the loss a dump used to cause.
    e.execute("CREATE TABLE qt9 PARTITION OF qt FOR VALUES FROM (900) TO (1000)")
        .unwrap();
    assert_eq!(
        col(
            &mut e,
            "SELECT indexname FROM pg_indexes WHERE tablename='qt9' ORDER BY 1"
        ),
        vec!["qt9_v_idx".to_string()]
    );
}

#[test]
fn attach_resolves_both_indexes() {
    let mut e = seeded();
    let got = format!(
        "{}",
        e.execute("ALTER INDEX pt_v ATTACH PARTITION nosuch_idx")
            .unwrap_err()
    );
    assert!(
        got.contains("relation \"nosuch_idx\" does not exist"),
        "{got}"
    );
}
