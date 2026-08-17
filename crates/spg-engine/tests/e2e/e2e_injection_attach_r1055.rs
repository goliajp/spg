//! r1055 (7.38 S3.4) — the injection framework, driven end to end
//! through its own SQL surface. The framework and six call sites
//! landed earlier in the 7.38 groundwork with unit pins; nothing had
//! ever ATTACHED through SQL and watched a real statement fail at a
//! real site. (The e2e build gets the `injection-points` feature via
//! the crate's dev-dependency on itself.)

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
}

/// error-action at `index_build_post_seal`: CREATE INDEX fails with
/// the injected message, the engine survives, detach restores it.
#[test]
fn r1055_error_injection_fires_at_a_real_site_and_detaches() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE inj (a INT)");
    ok(&mut e, "INSERT INTO inj VALUES (1), (2)");
    ok(
        &mut e,
        "SELECT spg_injection_attach('index_build_post_seal', 'error:injected boom')",
    );
    let err = e
        .execute("CREATE INDEX inj_a ON inj (a)")
        .expect_err("the injected error must surface");
    assert!(format!("{err}").contains("injected boom"), "{err}");
    // The engine is not wedged, and detach restores the path.
    ok(
        &mut e,
        "SELECT spg_injection_detach('index_build_post_seal')",
    );
    ok(&mut e, "CREATE INDEX inj_a2 ON inj (a)");
    match e.execute("SELECT count(*) FROM inj WHERE a > 0").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "2");
        }
        other => panic!("{other:?}"),
    }
}

/// notice-action tallies without blocking — the observability half.
#[test]
fn r1055_notice_injection_counts() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE injn (a INT)");
    ok(
        &mut e,
        "SELECT spg_injection_attach('index_build_post_seal', 'notice:seen')",
    );
    ok(&mut e, "CREATE INDEX injn_a ON injn (a)");
    match e
        .execute("SELECT spg_injection_notice_count('index_build_post_seal')")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "1");
        }
        other => panic!("{other:?}"),
    }
}
