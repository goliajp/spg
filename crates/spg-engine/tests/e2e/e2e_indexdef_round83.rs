//! v7.39 (read01 round 83) — a sweep of the pg_get_*def introspection family.
//! The focus that paid off was pg_get_indexdef(regclass), which had its OWN
//! copy of the CREATE INDEX renderer — a poorer copy than the one
//! `pg_indexes.indexdef` used:
//!
//!   * it ignored `idx.expression`, so `CREATE INDEX ON t (lower(name))` came
//!     back as `... (name)` — a wrong DDL that pg_dump would replay incorrectly;
//!   * it never printed UNIQUE for a primary-key or UNIQUE-constraint index, so
//!     `t_pkey` reconstructed as `CREATE INDEX` (droppable-looking, non-unique);
//!   * it did not double-parenthesise an operator expression key.
//!
//! One renderer (`system_catalog::render_indexdef`) now feeds both.
//!
//! Fixing the UNIQUE case surfaced a SECOND bug that an existing test had frozen:
//! the constraint-backing check matched by COLUMN SET, so a plain
//! `CREATE INDEX idx ON t (a)` over a table that also declares `UNIQUE (a)`
//! printed as UNIQUE — but in PG that constraint is enforced by its OWN
//! auto-index (`t_a_key`), a different relation, and `idx` stays a plain index.
//! The witness is the auto-index NAME, not the column set.
//!
//! DESC key columns (`CREATE INDEX ON t (a DESC, b)`) are NOT modelled in
//! storage yet — the parser drops the direction — so that one shape is left for
//! its own round; every other indexdef form now matches live PG18.4.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_expression_and_unique_and_parens() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int PRIMARY KEY, name text, a int, b int)");
    ok(&mut e, "CREATE INDEX i1 ON t (name)");
    ok(&mut e, "CREATE INDEX i2 ON t (lower(name))");
    ok(&mut e, "CREATE INDEX i3 ON t (a, b)");
    ok(&mut e, "CREATE INDEX i5 ON t ((a+b))");
    ok(&mut e, "CREATE UNIQUE INDEX i6 ON t (name)");
    ok(&mut e, "CREATE INDEX i7 ON t (name) WHERE a > 0");

    let d = |e: &mut Engine, n: &str| r1(e, &format!("SELECT pg_get_indexdef('{n}'::regclass)"));
    assert_eq!(d(&mut e, "i1"), "CREATE INDEX i1 ON public.t USING btree (name)");
    // The expression is preserved, not collapsed to the column name.
    assert_eq!(d(&mut e, "i2"), "CREATE INDEX i2 ON public.t USING btree (lower(name))");
    assert_eq!(d(&mut e, "i3"), "CREATE INDEX i3 ON public.t USING btree (a, b)");
    // Operator expression key is double-parenthesised; function call is not.
    assert_eq!(d(&mut e, "i5"), "CREATE INDEX i5 ON public.t USING btree (((a + b)))");
    assert_eq!(d(&mut e, "i6"), "CREATE UNIQUE INDEX i6 ON public.t USING btree (name)");
    assert_eq!(
        d(&mut e, "i7"),
        "CREATE INDEX i7 ON public.t USING btree (name) WHERE (a > 0)"
    );
    // The primary key's index is UNIQUE.
    assert_eq!(d(&mut e, "t_pkey"), "CREATE UNIQUE INDEX t_pkey ON public.t USING btree (id)");
}

#[test]
fn b_plain_index_over_a_constrained_column_stays_plain() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE d (id int PRIMARY KEY, a int, UNIQUE (a))");
    ok(&mut e, "CREATE INDEX idx_d_a ON d (a)");
    let d = |e: &mut Engine, n: &str| r1(e, &format!("SELECT pg_get_indexdef('{n}'::regclass)"));
    // The constraint's OWN auto-index is UNIQUE…
    assert_eq!(d(&mut e, "d_a_key"), "CREATE UNIQUE INDEX d_a_key ON public.d USING btree (a)");
    assert_eq!(d(&mut e, "d_pkey"), "CREATE UNIQUE INDEX d_pkey ON public.d USING btree (id)");
    // …but a user index that merely shadows the same column is PLAIN.
    assert_eq!(d(&mut e, "idx_d_a"), "CREATE INDEX idx_d_a ON public.d USING btree (a)");
}

#[test]
fn c_view_and_function_forms_agree_between_the_two_paths() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int PRIMARY KEY, name text)");
    ok(&mut e, "CREATE INDEX i2 ON t (lower(name))");
    // pg_indexes.indexdef and pg_get_indexdef now come from the same renderer.
    let via_view = r1(
        &mut e,
        "SELECT indexdef FROM pg_indexes WHERE indexname = 'i2'",
    );
    let via_fn = r1(&mut e, "SELECT pg_get_indexdef('i2'::regclass)");
    assert_eq!(via_view, via_fn);
    assert_eq!(via_view, "CREATE INDEX i2 ON public.t USING btree (lower(name))");
}
