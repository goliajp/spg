//! 7.38.4 — a SQL function declared to return an array returns its value.
//!
//! sentori step 54. `def.returns` holds the type as the user wrote it
//! (`bigint[]`); a `CastTarget::Named` spells the same type
//! `bigint_array`. The coercion of a body's value to its declared
//! return type could not resolve the bracket spelling, and the
//! `or_else(NULL)` under it turned "I could not coerce this" into a
//! NULL answer. The body computed `{1,2}` and the caller got nothing,
//! with no error anywhere.
//!
//! What that cost them: `sentori_version_key(text) RETURNS bigint[]`
//! turns '4.10.0' into {4,10,0,0} so versions compare as numbers.
//! Returning NULL made `COALESCE(key(a) >= key(b), false)` false for
//! every pair, so every version-targeted push reached zero devices
//! while reporting success.
//!
//! The same line existed at BOTH coercion sites — the pure-expression
//! body and the one with its own FROM — so both are pinned here.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn pin_v7384_array_returning_sql_function() {
    let mut e = Engine::new();
    // The scalar control: this always worked, and must keep working.
    e.execute(
        "CREATE FUNCTION gi() RETURNS BIGINT LANGUAGE sql IMMUTABLE AS $$ SELECT 7::bigint $$",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT gi()"), "7");

    // The bracket spellings, which returned NULL.
    e.execute(
        "CREATE FUNCTION ga() RETURNS BIGINT[] LANGUAGE sql IMMUTABLE \
         AS $$ SELECT ARRAY[1,2]::bigint[] $$",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT ga()"), "{1,2}");
    e.execute(
        "CREATE FUNCTION gt() RETURNS TEXT[] LANGUAGE sql IMMUTABLE \
         AS $$ SELECT ARRAY['a','b']::text[] $$",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT gt()"), "{a,b}");

    // The same expression inline never lost its value; that is what said
    // the body was fine and the coercion was not.
    assert_eq!(one(&mut e, "SELECT ARRAY[1,2]::bigint[]"), "{1,2}");
}

#[test]
fn pin_v7384_version_key_orders_numerically() {
    // Their shape, and the reason this was worse than a wall: as text
    // '4.10.0' < '4.2.0' is backwards, and the whole point of the key is
    // to fix that. A NULL key made every comparison false.
    let mut e = Engine::new();
    e.execute(
        "CREATE FUNCTION vk(v TEXT) RETURNS BIGINT[] LANGUAGE sql IMMUTABLE \
         AS $$ SELECT (string_to_array(v, '.')::bigint[] || ARRAY[0,0,0,0]::bigint[])[1:4] $$",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT vk('4.10.0')"), "{4,10,0,0}");
    assert_eq!(one(&mut e, "SELECT vk('4.10.0') >= vk('4.2')"), "true");
    assert_eq!(one(&mut e, "SELECT vk('4.2') >= vk('4.10.0')"), "false");
}

#[test]
fn pin_v7384_body_with_its_own_from_also_coerces() {
    // The second coercion site: a body with a FROM goes through the real
    // executor and coerced its result with the same broken spelling.
    let mut e = Engine::new();
    e.execute("CREATE TABLE src (id INT, n BIGINT)").unwrap();
    e.execute("INSERT INTO src VALUES (1, 10), (1, 20)")
        .unwrap();
    e.execute(
        "CREATE FUNCTION agg_of(k INT) RETURNS BIGINT[] LANGUAGE sql STABLE \
         AS $$ SELECT array_agg(n ORDER BY n) FROM src WHERE id = k $$",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT agg_of(1)"), "{10,20}");
}
