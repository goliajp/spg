//! v7.39 (round 534) — reading a parameter SPG does not model.
//!
//! The audit lists `pg_settings` publishing 32 rows against PG's 398 as
//! a gap. It is not one: round 474 decided deliberately that the view
//! lists only the parameters SPG actually reads, because putting a knob
//! in it tells a tuning tool that turning it does something. That
//! decision stands, and the audit recorded the row count without the
//! reasoning behind it.
//!
//! What the decision left open is the READ path, and that was a real
//! bug:
//!
//!     current_setting('block_size')     PG18  8192      SPG  '' (empty)
//!     current_setting('max_wal_size')   PG18  1GB       SPG  '' (empty)
//!     SHOW fsync                        PG18  on        SPG  (blank row)
//!
//! An empty string is the worst of the three answers available. It is
//! not the value, and it is not an error a caller can branch on — a
//! tool doing `current_setting('block_size')::int` gets a cast failure
//! or silently reads nothing.
//!
//! SPG already ACCEPTS `SET random_page_cost = 3` (round 501), so the
//! session has a value for these names whether or not anything reads
//! them. Reporting the stored configuration says nothing about
//! behaviour that accepting the SET did not already say.
//!
//! The 398 defaults are a PG18 reading, and the ones carrying a unit
//! are rendered the way `current_setting` renders them — a rule checked
//! against all 72 of PG's own comparable readings before it was used.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    Engine::new()
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The read-only server facts a client branches on.
#[test]
fn round534_preset_options_report_their_values() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT current_setting('block_size')"), "8192");
    assert_eq!(
        text(&mut e, "SELECT current_setting('max_index_keys')"),
        "32"
    );
    assert_eq!(
        text(&mut e, "SELECT current_setting('integer_datetimes')"),
        "on"
    );
    // And they cast, which an empty string did not.
    assert_eq!(
        text(&mut e, "SELECT current_setting('block_size')::int"),
        "8192"
    );
}

/// A tunable SPG does not act on still reports its compiled-in default.
#[test]
fn round534_unmodelled_tunables_report_their_defaults() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT current_setting('random_page_cost')"),
        "4"
    );
    assert_eq!(text(&mut e, "SELECT current_setting('fsync')"), "on");
    assert_eq!(
        text(
            &mut e,
            "SELECT current_setting('default_statistics_target')"
        ),
        "100"
    );
}

/// A value carrying a unit reads the way `current_setting` renders it,
/// not as the raw count `pg_settings.setting` would hold.
#[test]
fn round534_unit_bearing_defaults_render_as_pg_renders_them() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT current_setting('max_wal_size')"),
        "1GB"
    );
    assert_eq!(
        text(&mut e, "SELECT current_setting('checkpoint_timeout')"),
        "5min"
    );
    assert_eq!(
        text(&mut e, "SELECT current_setting('wal_buffers')"),
        // -1 means "derive it", and a non-positive value prints bare.
        "-1"
    );
}

/// SHOW answers the same names, and a session SET still wins over the
/// default.
#[test]
fn round534_show_answers_and_a_set_still_wins() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SHOW fsync"), "on");
    assert_eq!(text(&mut e, "SHOW random_page_cost"), "4");
    e.execute("SET random_page_cost = 3").unwrap();
    assert_eq!(text(&mut e, "SHOW random_page_cost"), "3");
    assert_eq!(
        text(&mut e, "SELECT current_setting('random_page_cost')"),
        "3"
    );
}

/// A name PG does not know is still refused, and `missing_ok` still
/// answers NULL — the two readings round 501 established.
#[test]
fn round534_unknown_names_are_still_unknown() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT current_setting('myapp.nosuch', true)"),
        "NULL"
    );
    assert!(e.execute("SELECT current_setting('myapp.nosuch')").is_err());
    assert!(e.execute("SET nosuchknob = 3").is_err());
}

/// v7.38.18 (C5) — `pg_settings` lists what PG18 lists. This test used
/// to pin the opposite ("still lists only what SPG reads — round 474's
/// decision, pinned so the divergence is deliberate rather than drift"),
/// and the divergence WAS deliberate. It was also inconsistent: for the
/// same parameter on the same session, `SHOW archive_command` answered
/// `''`, `SET archive_command = 'x'` answered "cannot be changed now",
/// and `pg_settings` answered that no such parameter exists. Two
/// surfaces said it was real and one said it was not.
///
/// So the row is not a new claim about what SPG acts on — `SHOW` was
/// already making it. What separates a parameter SPG reads from one it
/// merely reports is `source`, which is what PG uses for the same
/// purpose.
#[test]
fn round534_pg_settings_reports_every_pg18_parameter() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT count(*) FROM pg_settings"), "398");
    // The one round 474 named as the case for staying curated.
    assert_eq!(
        text(
            &mut e,
            "SELECT count(*) FROM pg_settings WHERE name = 'enable_seqscan'"
        ),
        "1"
    );
    // A parameter SPG genuinely reads keeps its own value and rendering:
    // PG reports `work_mem` as a bare count with a unit beside it.
    assert_eq!(
        text(
            &mut e,
            "SELECT setting, unit, vartype FROM pg_settings WHERE name = 'work_mem'"
        ),
        "4096|kB|integer"
    );
    // ...while `current_setting` keeps the human form. Both are PG's.
    assert_eq!(text(&mut e, "SELECT current_setting('work_mem')"), "4MB");
    // Nothing appears twice, which it did the first time this landed:
    // a `SET` on a parameter outside SPG's own list was pushed as its
    // own row on top of the PG18 one.
    e.execute("SET random_page_cost = 3").unwrap();
    assert_eq!(
        text(
            &mut e,
            "SELECT count(*), max(setting), max(source) FROM pg_settings \
             WHERE name = 'random_page_cost'"
        ),
        "1|3|session"
    );
    // And a name PG does not have is still not a row.
    assert_eq!(
        text(
            &mut e,
            "SELECT count(*) FROM pg_settings WHERE name = 'nosuchknob'"
        ),
        "0"
    );
}
