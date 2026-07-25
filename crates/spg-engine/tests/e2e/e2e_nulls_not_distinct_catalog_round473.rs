//! read01 round 473 (C2) — what `NULLS NOT DISTINCT` looks like from outside.
//!
//! The ENFORCEMENT has worked since round 52: all three declaration forms
//! reject the second all-NULL key, as PG18 does. What a client could see of
//! it was another matter, and the measurement split cleanly:
//!
//!   surface                          PG18                    SPG before
//!   pg_index.indnullsnotdistinct     t                       f
//!   pg_indexes.indexdef              … (a) NULLS NOT DISTINCT   clause absent
//!   indexrelid::regclass             ix1                     100001
//!   23505 DETAIL, index form         Key (a)=(null) …        no DETAIL at all
//!   23505 DETAIL, NULL rendering     null                    NULL
//!
//! The indexdef one is the worst of them: a dump that loses the clause
//! restores into a table that ACCEPTS rows the original rejected.
//!
//! Every expectation is copied from a PG18 run.

use spg_engine::{Engine, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> String {
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
            .collect::<Vec<_>>()
            .join(";"),
        other => panic!("{sql} -> {other:?}"),
    }
}

#[test]
fn round473_the_catalog_reports_the_flag_and_the_definition_carries_it() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t1 (a INT)").unwrap();
    e.execute("CREATE UNIQUE INDEX ix1 ON t1 (a) NULLS NOT DISTINCT")
        .unwrap();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT indexrelid::regclass::text, indnullsnotdistinct \
             FROM pg_index WHERE indrelid = 't1'::regclass"
        ),
        "ix1|true"
    );
    assert_eq!(
        scalar(&mut e, "SELECT indexdef FROM pg_indexes WHERE tablename = 't1'"),
        "CREATE UNIQUE INDEX ix1 ON public.t1 USING btree (a) NULLS NOT DISTINCT"
    );
}

#[test]
fn round473_a_plain_unique_index_still_reports_neither() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT)").unwrap();
    e.execute("CREATE UNIQUE INDEX ix ON t (a)").unwrap();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT indnullsnotdistinct FROM pg_index WHERE indrelid = 't'::regclass"
        ),
        "false"
    );
    assert_eq!(
        scalar(&mut e, "SELECT indexdef FROM pg_indexes WHERE tablename = 't'"),
        "CREATE UNIQUE INDEX ix ON public.t USING btree (a)"
    );
}

#[test]
fn round473_the_clause_sits_before_where_on_a_partial_index() {
    // PG18: CREATE UNIQUE INDEX pix ON public.p USING btree (a)
    //       NULLS NOT DISTINCT WHERE (b > 0)
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (a INT, b INT)").unwrap();
    e.execute("CREATE UNIQUE INDEX pix ON p (a) NULLS NOT DISTINCT WHERE b > 0")
        .unwrap();
    assert_eq!(
        scalar(&mut e, "SELECT indexdef FROM pg_indexes WHERE tablename = 'p'"),
        "CREATE UNIQUE INDEX pix ON public.p USING btree (a) NULLS NOT DISTINCT WHERE (b > 0)"
    );
}

#[test]
fn round473_the_index_form_carries_pgs_detail() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t1 (a INT)").unwrap();
    e.execute("CREATE UNIQUE INDEX ix1 ON t1 (a) NULLS NOT DISTINCT")
        .unwrap();
    e.execute("INSERT INTO t1 VALUES (NULL)").unwrap();
    let err = e
        .execute("INSERT INTO t1 VALUES (NULL)")
        .expect_err("the second all-NULL key collides");
    let msg = format!("{err}");
    // PG18: DETAIL: Key (a)=(null) already exists.  — lowercase null.
    assert!(
        msg.contains("DETAIL: Key (a)=(null) already exists."),
        "message was: {msg}"
    );
}

#[test]
fn round473_regclass_names_tables_and_sequences_too() {
    // The oid → name direction now mirrors the name → oid one, so it must
    // agree for every relation kind, not only indexes.
    let mut e = Engine::new();
    e.execute("CREATE TABLE tt (a INT)").unwrap();
    e.execute("CREATE SEQUENCE sq").unwrap();
    e.execute("CREATE VIEW vv AS SELECT a FROM tt").unwrap();
    for name in ["tt", "sq", "vv"] {
        assert_eq!(
            scalar(&mut e, &format!("SELECT '{name}'::regclass::oid::regclass::text")),
            name,
            "round trip for {name}"
        );
    }
}

#[test]
fn round473_enforcement_is_unchanged_in_all_three_forms() {
    // The part that already worked, pinned so the catalog work above cannot
    // quietly change it.
    let mut e = Engine::new();
    e.execute("CREATE TABLE c1 (a INT, b INT, UNIQUE NULLS NOT DISTINCT (a, b))")
        .unwrap();
    e.execute("INSERT INTO c1 VALUES (1, NULL)").unwrap();
    assert!(e.execute("INSERT INTO c1 VALUES (1, NULL)").is_err());

    e.execute("CREATE TABLE c2 (a INT)").unwrap();
    e.execute("ALTER TABLE c2 ADD CONSTRAINT u2 UNIQUE NULLS NOT DISTINCT (a)")
        .unwrap();
    e.execute("INSERT INTO c2 VALUES (NULL)").unwrap();
    assert!(e.execute("INSERT INTO c2 VALUES (NULL)").is_err());

    // And the default is still NULLS DISTINCT.
    e.execute("CREATE TABLE c3 (a INT, b INT, UNIQUE (a, b))")
        .unwrap();
    e.execute("INSERT INTO c3 VALUES (1, NULL), (1, NULL)")
        .unwrap();
    assert_eq!(scalar(&mut e, "SELECT count(*) FROM c3"), "2");
}
