//! Round 772 (F31 July J2) — a reversed character range refuses with
//! PG's sentence, PG18-measured: `[z-a]` is "invalid regular
//! expression: invalid character range"; SPG recorded the range and
//! silently matched nothing. Ordinary and trailing-dash forms keep
//! working.

use spg_engine::Engine;

#[test]
fn round772_reversed_range_refuses() {
    let mut e = Engine::new();
    let err = format!(
        "{}",
        e.execute("SELECT regexp_match('a', '[z-a]')")
            .expect_err("reversed range must refuse")
    );
    assert!(
        err.contains("invalid regular expression: invalid character range"),
        "{err}"
    );
    // The healthy shapes stay.
    for (sql, ok) in [
        ("SELECT 'b' ~ '[a-c]'", true),
        ("SELECT '-' ~ '[a-]'", true),
    ] {
        assert!(e.execute(sql).is_ok() == ok, "{sql}");
    }
}
