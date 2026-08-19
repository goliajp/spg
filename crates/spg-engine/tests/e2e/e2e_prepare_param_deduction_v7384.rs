//! 7.38.4 — PREPARE refuses a parameter it cannot deduce consistently.
//!
//! sentori §6a, at their request. Their assert-stats upsert puts `$4`
//! in a `bigint` column and again inside `CASE WHEN $4 > 0`, where the
//! literal is `integer`. PG18 answers "inconsistent types deduced for
//! parameter $4 — integer versus bigint"; SPG let the last context win
//! silently.
//!
//! The distinction that matters: PG refuses only the DEDUCED form.
//! `PREPARE p (…, bigint, …)` is accepted and runs, and sqlx always
//! declares — which is why the statement has always worked in their
//! production, and why refusing a declared parameter would break every
//! driver that does the right thing.

use spg_engine::Engine;

const BODY: &str = "INSERT INTO asx (pid, name, rel, pass_count, fail_count, last_pass_at) \
     VALUES ($1, $2, $3, $4, $5, CASE WHEN $4 > 0 THEN now() END) \
     ON CONFLICT (pid, name, rel) DO UPDATE SET pass_count = asx.pass_count + $4";

fn table(e: &mut Engine) {
    e.execute(
        "CREATE TABLE asx (pid UUID, name TEXT, rel TEXT, pass_count BIGINT, \
         fail_count BIGINT, last_pass_at TIMESTAMPTZ, PRIMARY KEY (pid, name, rel))",
    )
    .unwrap();
}

#[test]
fn pin_v7384_deduced_conflict_is_refused() {
    let mut e = Engine::new();
    table(&mut e);
    let err = e
        .execute(&format!("PREPARE q AS {BODY}"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("inconsistent types deduced for parameter $4"),
        "{err}"
    );
    assert!(err.contains("bigint") && err.contains("integer"), "{err}");
}

#[test]
fn pin_v7384_declared_parameter_is_accepted() {
    // The half that must NOT change: sqlx declares, so this is the shape
    // that actually runs in production.
    let mut e = Engine::new();
    table(&mut e);
    e.execute(&format!(
        "PREPARE q2 (UUID, TEXT, TEXT, BIGINT, BIGINT) AS {BODY}"
    ))
    .unwrap();
}

#[test]
fn pin_v7384_ordinary_prepares_still_prepare() {
    // A parameter used consistently, and one used in only one context,
    // must both stay legal — the refusal is for a genuine conflict.
    let mut e = Engine::new();
    table(&mut e);
    e.execute("PREPARE ok1 AS SELECT * FROM asx WHERE pass_count = $1")
        .unwrap();
    e.execute(
        "PREPARE ok2 AS INSERT INTO asx (pid, name, rel, pass_count) VALUES ($1, $2, $3, $4)",
    )
    .unwrap();
    e.execute(
        "PREPARE ok3 AS INSERT INTO asx (pid, name, rel, pass_count) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (pid, name, rel) \
         DO UPDATE SET pass_count = asx.pass_count + $4",
    )
    .unwrap();
}
