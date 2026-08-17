//! r1058 (7.38 CP3 groundwork) — `SPG_TEST_FIXED_CLOCK_MICROS` pins
//! the engine clock through the GUC snapshot alone, no `with_clock`
//! call. This is the knob that lets the SERVER permutations run the
//! time-sensitive corpus deterministically; the wire legs of the
//! perm-runner caught its absence (embedded runner injects the clock
//! programmatically, the server host could not).

use spg_engine::{Engine, QueryResult, testkit};

/// 2025-06-15T12:00:00Z — the corpus runner's fixed instant.
const MICROS: i64 = 1_749_988_800_000_000;

fn one_cell(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn fixed_clock_guc_pins_current_date() {
    let cfg = testkit::EnvConfig::builder()
        .fixed_clock_micros(MICROS)
        .build();
    let mut e = Engine::new().with_env_cfg(cfg);
    assert_eq!(one_cell(&mut e, "SELECT CURRENT_DATE"), "2025-06-15");
}

#[test]
fn unset_guc_leaves_the_clock_alone() {
    // Production default: no fixed clock. A clockless engine has NO
    // ambient time source (no_std discipline), so CURRENT_DATE must
    // keep erroring — proof the GUC snapshot alone didn't install one.
    let cfg = testkit::EnvConfig::builder().build();
    let mut e = Engine::new().with_env_cfg(cfg);
    let err = e
        .execute("SELECT CURRENT_DATE")
        .expect_err("clockless engine must not answer CURRENT_DATE");
    assert!(
        format!("{err}").contains("current_date"),
        "unexpected error: {err}"
    );
}
