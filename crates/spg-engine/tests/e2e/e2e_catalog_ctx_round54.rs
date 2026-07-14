//! v7.39 (read01, round 54) — a PROACTIVE audit of the bug family rounds 49
//! and 53 each stumbled into: an execution path that builds its own
//! `EvalContext::new(..., None)` drops the catalog, and every
//! catalog-dependent cast (regclass / enum / composite / domain) then
//! silently degrades — sometimes to an error, sometimes to WRONG ROWS.
//!
//! Rather than wait for the next report, a differential matrix put a
//! regclass / enum cast into every execution path. Six live bugs fell out;
//! this pins them. `Engine::ev_ctx()` is the canonical context constructor —
//! it threads the catalog (plus render style / tz / GUCs). A bare
//! `EvalContext::new` is a suspect.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed(e: &mut Engine) {
    ok(e, "CREATE TYPE mood AS ENUM ('sad','ok','happy')");
    ok(e, "CREATE TABLE cx(a int, m mood)");
    ok(e, "INSERT INTO cx VALUES (1,'sad'),(2,'happy'),(3,'ok')");
}

#[test]
fn dml_where_resolves_an_enum_cast() {
    let mut e = Engine::new();
    seed(&mut e);
    // Both used to fail outright with "unsupported cast target `::mood`",
    // while the same predicate on a SELECT worked — the DML contexts were
    // built without the catalog.
    ok(&mut e, "UPDATE cx SET a = a + 10 WHERE m = 'happy'::mood");
    assert_eq!(col(&mut e, "SELECT a FROM cx WHERE m = 'happy'::mood"), vec!["12"]);
    ok(&mut e, "DELETE FROM cx WHERE m = 'ok'::mood");
    assert_eq!(col(&mut e, "SELECT count(*) FROM cx"), vec!["2"]);
}

#[test]
fn regclass_survives_subquery_cte_and_derived_table() {
    let mut e = Engine::new();
    seed(&mut e);
    // IN (SELECT ::regclass) died on "subquery result type None not yet
    // materialisable" — a regclass is an oid, so it materialises as one.
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM pg_index WHERE indrelid IN (SELECT 'cx'::regclass)"
        ),
        vec!["0"]
    );
    // EXISTS with a regclass, and a CTE holding one, both used to abort the
    // query with an internal error — a PANIC, caught by the firewall:
    // Value::RegClass has no DataType, and the type checks asserted
    // `.expect("non-null")` on every non-NULL value.
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM pg_index WHERE EXISTS (SELECT 1 WHERE indrelid = 'cx'::regclass)"
        ),
        vec!["0"]
    );
    assert_eq!(
        col(
            &mut e,
            "WITH w AS (SELECT 'cx'::regclass AS r) \
             SELECT count(*) FROM pg_index x, w WHERE x.indrelid = w.r"
        ),
        vec!["0"]
    );
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM (SELECT indrelid FROM pg_index) s \
             WHERE s.indrelid = 'cx'::regclass"
        ),
        vec!["0"]
    );
}

#[test]
fn union_order_by_sorts_an_enum_by_member_order() {
    let mut e = Engine::new();
    seed(&mut e);
    // Silently WRONG ROWS, not an error: the projection dropped the column's
    // enum identity (it lives outside the DataType lattice), so the combined
    // ORDER BY sorted the labels alphabetically — happy, ok, sad.
    assert_eq!(
        col(&mut e, "SELECT m FROM cx UNION SELECT 'ok'::mood ORDER BY 1"),
        vec!["sad", "ok", "happy"]
    );
}

#[test]
fn window_order_by_sorts_an_enum_by_member_order() {
    let mut e = Engine::new();
    seed(&mut e);
    // The window's own ORDER BY (the enum-order knife's recorded residual)…
    assert_eq!(
        col(
            &mut e,
            "SELECT rn::text FROM (SELECT m, row_number() OVER (ORDER BY m) rn FROM cx) s \
             ORDER BY s.m"
        ),
        vec!["1", "2", "3"]
    );
    // …and the OUTER ORDER BY of a windowed query, which built its sort keys
    // by hand and so skipped the enum-ordinal substitution entirely: the
    // window numbers came out right while the row order silently did not.
    match e
        .execute("SELECT m, row_number() OVER (ORDER BY m) rn FROM cx ORDER BY m")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            let got: Vec<String> = rows
                .iter()
                .map(|r| {
                    format!(
                        "{}:{}",
                        spg_engine::eval::value_to_text(&r.values[0]),
                        spg_engine::eval::value_to_text(&r.values[1])
                    )
                })
                .collect();
            assert_eq!(got, vec!["sad:1", "ok:2", "happy:3"]);
        }
        other => panic!("{other:?}"),
    }
}
