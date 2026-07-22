//! read01 round 334 (V55) — `SECURITY DEFINER` switches the executing role.
//!
//! Round 322 made `CREATE FUNCTION … SECURITY DEFINER` parse and report
//! itself faithfully (`pg_proc.prosecdef`, `pg_get_functiondef`). The
//! behaviour was still the invoker's, which is worse than it sounds: the
//! catalogue said "definer rights" while the function was refused on the
//! very table it exists to expose. The form's whole purpose is letting an
//! owner grant controlled access to something the caller cannot read.
//!
//! PG 18.4 measured, with table `sd` owned by `owner55` and readable by
//! nobody else, `caller55` holding only EXECUTE:
//!
//! ```text
//! SET ROLE caller55;
//! SELECT f_def();   -- 2          (definer: authorised as owner55)
//! SELECT f_inv();   -- ERROR: permission denied for table sd
//! SELECT f_who();   -- owner55    (current_user inside a definer body)
//! SELECT current_user, session_user;  -- caller55 | unmei
//! ```

use spg_engine::Engine;
use spg_storage::Value;

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}"),
    }
}

/// `sd` is readable only by its owner (admin, who creates everything
/// here); `caller55` gets nothing but the right to call the functions.
fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE ROLE caller55").unwrap();
    e.execute("CREATE TABLE sd (id INT)").unwrap();
    e.execute("INSERT INTO sd VALUES (1), (2)").unwrap();
    e.execute("REVOKE ALL ON sd FROM PUBLIC").unwrap();
    e.execute(
        "CREATE FUNCTION f_def() RETURNS bigint LANGUAGE sql SECURITY DEFINER \
         AS $$ SELECT count(*) FROM sd $$",
    )
    .unwrap();
    e.execute(
        "CREATE FUNCTION f_inv() RETURNS bigint LANGUAGE sql SECURITY INVOKER \
         AS $$ SELECT count(*) FROM sd $$",
    )
    .unwrap();
    e.execute(
        "CREATE FUNCTION f_who() RETURNS text LANGUAGE sql SECURITY DEFINER \
         AS $$ SELECT current_user $$",
    )
    .unwrap();
    e
}

/// The point of the form: the body reads what the caller cannot.
#[test]
fn a_definer_function_reads_as_its_owner() {
    let mut e = fixture();
    e.execute("SET ROLE caller55").unwrap();
    assert!(
        err(&mut e, "SELECT count(*) FROM sd").contains("permission denied for table sd"),
        "the caller cannot read the table directly"
    );
    assert_eq!(
        one(&mut e, "SELECT f_def()"),
        Value::BigInt(2),
        "…but the definer function can, on its behalf"
    );
}

/// Its SECURITY INVOKER sibling stays refused — the contrast that makes
/// the first assertion mean something.
#[test]
fn an_invoker_function_is_still_refused() {
    let mut e = fixture();
    e.execute("SET ROLE caller55").unwrap();
    assert!(
        err(&mut e, "SELECT f_inv()").contains("permission denied for table sd"),
        "an invoker-rights function runs as the caller"
    );
}

/// `current_user` inside a definer body is the OWNER; outside it is the
/// caller's role.
#[test]
fn current_user_inside_a_definer_body_is_the_owner() {
    let mut e = fixture();
    e.execute("SET ROLE caller55").unwrap();
    assert_eq!(one(&mut e, "SELECT current_user"), Value::text("caller55"));
    assert_eq!(
        one(&mut e, "SELECT f_who()"),
        Value::text("admin"),
        "the body runs as the function's owner"
    );
    // …and the switch does not leak past the call.
    assert_eq!(one(&mut e, "SELECT current_user"), Value::text("caller55"));
}

/// A superuser session is unaffected either way.
#[test]
fn a_superuser_session_is_unchanged() {
    let mut e = fixture();
    assert_eq!(one(&mut e, "SELECT f_def()"), Value::BigInt(2));
    assert_eq!(one(&mut e, "SELECT f_inv()"), Value::BigInt(2));
    assert_eq!(one(&mut e, "SELECT count(*) FROM sd"), Value::BigInt(2));
}
