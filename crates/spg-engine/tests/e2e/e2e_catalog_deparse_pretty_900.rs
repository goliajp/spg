//! 9.0.0 — the pretty deparse was a SECOND deparser, and it printed the
//! expression as written.
//!
//! `pg_get_constraintdef(oid, true)` and `pg_get_indexdef(oid, 0, true)`
//! ask for PostgreSQL's pretty form: the same ANALYZED expression, with
//! the parentheses the grammar can put back left out. SPG answered the
//! first from `spg_sql::ast::pretty_expr` — which prints what the user
//! typed, so the literal lost the type the plain form gives it — and
//! ignored the third argument of the second entirely.
//!
//! Measured on PostgreSQL 18.6 (`||` is the whole second half: an index
//! key's literal is typed in BOTH forms, and SPG typed it in neither):
//!
//! ```text
//!   CHECK ((c or b > 1) and a <> 'q')
//!     plain   CHECK (((c OR (b > 1)) AND (a <> 'q'::text)))
//!     pretty  CHECK ((c OR b > 1) AND a <> 'q'::text)
//!   CHECK (b + d * 2 > 3)          pretty  CHECK ((b + d * 2) > 3)
//!   CHECK ((c and e) or b > 1)     pretty  CHECK (c AND e OR b > 1)
//!   CHECK (not (b > 0))            pretty  CHECK (NOT b > 0)
//!   INDEX ((a || 'x'))
//!     plain   … USING btree (((a || 'x'::text)))
//!     pretty  … USING btree ((a || 'x'::text))
//!   INDEX (coalesce(a, 'z'))       both    … (COALESCE(a, 'z'::text))
//!   INDEX ((b::text))              pretty  … ((b::text))
//!   INDEX ((a || 'x')) WHERE b > 0 pretty  … WHERE b > 0
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
        "CREATE TABLE q (a text, b int, c bool, e bool, d int, v varchar(10))",
        "ALTER TABLE q ADD CONSTRAINT k1 CHECK ((c or b > 1) and a <> 'q')",
        "ALTER TABLE q ADD CONSTRAINT k2 CHECK (b + d * 2 > 3)",
        "ALTER TABLE q ADD CONSTRAINT k3 CHECK ((c and e) or b > 1)",
        "ALTER TABLE q ADD CONSTRAINT k4 CHECK (not (b > 0))",
        "ALTER TABLE q ADD CONSTRAINT k5 CHECK (a in ('x','y') or b is null)",
        "CREATE INDEX i1 ON q ((a || 'x'))",
        "CREATE INDEX i2 ON q (lower('Q' || a))",
        "CREATE INDEX i3 ON q (coalesce(a, 'z'))",
        "CREATE INDEX i4 ON q ((b::text))",
        "CREATE INDEX i5 ON q ((v || 'y'))",
        "CREATE INDEX i6 ON q ((a || 'x')) WHERE b > 0",
        "CREATE INDEX i7 ON q ((a like 'p%'))",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

fn con(e: &mut Engine, name: &str, pretty: bool) -> String {
    let sql = format!(
        "SELECT pg_get_constraintdef(oid, {pretty}) FROM pg_constraint WHERE conname = '{name}'"
    );
    col(e, &sql).into_iter().next().expect("one row")
}

fn idx(e: &mut Engine, name: &str, pretty: bool) -> String {
    let sql = format!(
        "SELECT pg_get_indexdef(c.oid, 0, {pretty}) FROM pg_class c WHERE c.relname = '{name}'"
    );
    col(e, &sql).into_iter().next().expect("one row")
}

#[test]
fn the_pretty_check_types_its_literal_like_the_plain_one() {
    let mut e = seeded();
    assert_eq!(
        con(&mut e, "k1", true),
        "CHECK ((c OR b > 1) AND a <> 'q'::text)"
    );
    assert_eq!(
        con(&mut e, "k1", false),
        "CHECK (((c OR (b > 1)) AND (a <> 'q'::text)))"
    );
}

#[test]
fn the_pretty_check_keeps_only_the_parentheses_postgresql_keeps() {
    let mut e = seeded();
    // An operator under a comparison keeps them; AND under OR does not;
    // a comparison under NOT does not.
    assert_eq!(con(&mut e, "k2", true), "CHECK ((b + d * 2) > 3)");
    assert_eq!(con(&mut e, "k3", true), "CHECK (c AND e OR b > 1)");
    assert_eq!(con(&mut e, "k4", true), "CHECK (NOT b > 0)");
    assert_eq!(
        con(&mut e, "k5", true),
        "CHECK ((a = ANY (ARRAY['x'::text, 'y'::text])) OR b IS NULL)"
    );
}

#[test]
fn an_index_key_literal_is_typed_in_both_forms() {
    let mut e = seeded();
    assert_eq!(
        idx(&mut e, "i1", false),
        "CREATE INDEX i1 ON public.q USING btree (((a || 'x'::text)))"
    );
    assert_eq!(
        idx(&mut e, "i2", false),
        "CREATE INDEX i2 ON public.q USING btree (lower(('Q'::text || a)))"
    );
    assert_eq!(
        idx(&mut e, "i3", false),
        "CREATE INDEX i3 ON public.q USING btree (COALESCE(a, 'z'::text))"
    );
    assert_eq!(
        idx(&mut e, "i5", false),
        "CREATE INDEX i5 ON public.q USING btree ((((v)::text || 'y'::text)))"
    );
    assert_eq!(
        idx(&mut e, "i7", false),
        "CREATE INDEX i7 ON public.q USING btree (((a ~~ 'p%'::text)))"
    );
}

#[test]
fn the_pretty_index_drops_the_schema_a_pair_and_the_predicate_parens() {
    let mut e = seeded();
    assert_eq!(
        idx(&mut e, "i1", true),
        "CREATE INDEX i1 ON q USING btree ((a || 'x'::text))"
    );
    // A function call carries no pair of its own, in either form.
    assert_eq!(
        idx(&mut e, "i3", true),
        "CREATE INDEX i3 ON q USING btree (COALESCE(a, 'z'::text))"
    );
    // A cast's operand loses its parentheses here and keeps them there.
    assert_eq!(
        idx(&mut e, "i4", true),
        "CREATE INDEX i4 ON q USING btree ((b::text))"
    );
    assert_eq!(
        idx(&mut e, "i5", true),
        "CREATE INDEX i5 ON q USING btree ((v::text || 'y'::text))"
    );
    assert_eq!(
        idx(&mut e, "i6", true),
        "CREATE INDEX i6 ON q USING btree ((a || 'x'::text)) WHERE b > 0"
    );
}

#[test]
fn an_expression_key_that_reads_a_column_through_a_predicate_is_accepted() {
    // `extract_first_column` listed six node kinds and refused the rest,
    // so ordinary PostgreSQL index keys were rejected as referencing no
    // column — while naming one.
    let mut e = Engine::new();
    e.execute("CREATE TABLE z (a text, b int)").expect("table");
    for sql in [
        "CREATE INDEX z1 ON z ((a LIKE 'p%'))",
        "CREATE INDEX z2 ON z ((a IN ('p','q')))",
        "CREATE INDEX z3 ON z ((a IS NULL))",
        "CREATE INDEX z4 ON z ((CASE WHEN a = 'x' THEN 1 ELSE 2 END))",
        "CREATE INDEX z5 ON z ((ARRAY[a, 'p']))",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM pg_indexes WHERE tablename = 'z'"
        ),
        vec!["5"]
    );
}
