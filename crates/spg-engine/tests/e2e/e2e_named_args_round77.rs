//! v7.39 (read01 round 77) — a differential sweep of the date/time family.
//!
//! Four gaps, and three of them are the same shape: something that *exists* and
//! is simply unreachable from the spelling people actually write.
//!
//!   1. `f(x := 1)` — PG's own docs lead with this spelling for named arguments,
//!      and the parser never lexed it. Only `f(x => 1)` got through.
//!   2. …and `=>` only worked for five hardcoded `make_*` builtins, because the
//!      parser resolved names to slots itself, out of a table baked into
//!      parser.rs. Every user function got "does not support named arguments" —
//!      although the catalog has been storing each function's parameter names
//!      since the day CREATE FUNCTION wrote them. The parser has no catalog; it
//!      never could have resolved them there. Resolution now happens once, in
//!      eval, for builtins and user functions alike.
//!   3. `'15 min'::interval` — "cannot parse as INTERVAL". The unit table
//!      matched long names only, with an ad-hoc `strip_suffix('s')` in front,
//!      so every abbreviation PG accepts (min, mins, secs, hrs, yrs, mons, and
//!      the single letters) was rejected — while the table had grown arms for
//!      the debris stripping leaves behind (`centurie`, `millenniu`). One
//!      canonicaliser now feeds both the integer and the fractional unit match.
//!   4. `date_trunc('day', <timestamptz>)` came back `timestamp`, and
//!      `coalesce(NULL, <timestamptz>)` did too — the first because the return
//!      type was pinned to Timestamp regardless of what was handed in, the
//!      second because the conditional family took `args[0]`'s type, so an
//!      untyped NULL in the first slot erased the type of everything after it.
//!      Type by argument POSITION rather than by the arguments.
//!
//! Oracle: live PG 18.4. Unit spellings were enumerated against it, not guessed.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_named_args_on_builtins_both_spellings() {
    let mut e = Engine::new();
    assert_eq!(r1(&mut e, "SELECT make_interval(months := 2, days := 3)"), "2 mons 3 days");
    assert_eq!(r1(&mut e, "SELECT make_interval(months => 2, days => 3)"), "2 mons 3 days");
    // Positional and named mixed; unfilled make_* slots default to zero.
    assert_eq!(r1(&mut e, "SELECT make_date(2020, day := 17, month := 5)"), "2020-05-17");
    assert_eq!(r1(&mut e, "SELECT make_interval(2, 0, 0, 3)"), "2 years 3 days");
}

#[test]
fn b_named_args_on_user_functions() {
    let mut e = Engine::new();
    ok(
        &mut e,
        "CREATE FUNCTION addup(a int, b int) RETURNS int AS $$ BEGIN RETURN a + b; END; $$ \
         LANGUAGE plpgsql",
    );
    // The catalog knew these names all along.
    assert_eq!(r1(&mut e, "SELECT addup(b := 5, a := 1)"), "6");
    assert_eq!(r1(&mut e, "SELECT addup(1, b => 5)"), "6");
    assert_eq!(r1(&mut e, "SELECT addup(1, 5)"), "6");
    // A name the function does not declare is an error, not a silent slot 0.
    assert!(e.execute("SELECT addup(a := 1, zzz := 5)").is_err());
    // A function that declares no parameter names rejects the notation, as PG
    // does (it reports no matching candidate; SPG names the reason).
    assert!(e.execute("SELECT lpad(string := 'x', length := 3)").is_err());
}

#[test]
fn c_interval_unit_abbreviations() {
    let mut e = Engine::new();
    assert_eq!(r1(&mut e, "SELECT '15 min'::interval"), "00:15:00");
    assert_eq!(r1(&mut e, "SELECT '2 mins'::interval"), "00:02:00");
    assert_eq!(r1(&mut e, "SELECT '3 secs'::interval"), "00:00:03");
    assert_eq!(r1(&mut e, "SELECT '4 hrs'::interval"), "04:00:00");
    assert_eq!(r1(&mut e, "SELECT '2 yrs'::interval"), "2 years");
    assert_eq!(r1(&mut e, "SELECT '2 mons'::interval"), "2 mons");
    // Single letters: `m` is MINUTES, not months.
    assert_eq!(r1(&mut e, "SELECT '2 m'::interval"), "00:02:00");
    assert_eq!(r1(&mut e, "SELECT '2 h'::interval"), "02:00:00");
    assert_eq!(r1(&mut e, "SELECT '2 d'::interval"), "2 days");
    assert_eq!(r1(&mut e, "SELECT '2 w'::interval"), "14 days");
    assert_eq!(r1(&mut e, "SELECT '2 y'::interval"), "2 years");
    // Fractional amounts read the same table.
    assert_eq!(r1(&mut e, "SELECT '1.5 hrs'::interval"), "01:30:00");
    // A spelling PG does not accept stays rejected.
    assert!(e.execute("SELECT '3 wks'::interval").is_err());
    // The stride argument that could not be spelled before.
    assert_eq!(
        r1(
            &mut e,
            "SELECT date_bin('15 min', '2020-05-17 13:47:12'::timestamp, \
             '2020-05-17 13:00:00'::timestamp)"
        ),
        "2020-05-17 13:45:00"
    );
}

#[test]
fn d_timestamptz_survives_date_trunc_and_coalesce() {
    let mut e = Engine::new();
    assert_eq!(
        r1(&mut e, "SELECT pg_typeof(date_trunc('day', '2020-05-17'::timestamptz))"),
        "timestamp with time zone"
    );
    assert_eq!(
        r1(&mut e, "SELECT date_trunc('day', '2020-05-17 13:00'::timestamptz)::text"),
        "2020-05-17 00:00:00+00"
    );
    // A plain timestamp still comes back plain.
    assert_eq!(
        r1(&mut e, "SELECT date_trunc('day', '2020-05-17 13:00'::timestamp)::text"),
        "2020-05-17 00:00:00"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT date_bin('15 min', '2020-05-17 13:47:12'::timestamptz, \
             '2020-05-17 13:00'::timestamptz)::text"
        ),
        "2020-05-17 13:45:00+00"
    );
    // An untyped NULL in the first slot must not erase the type of the rest.
    assert_eq!(
        r1(&mut e, "SELECT coalesce(NULL, '2020-05-17 00:00:00'::timestamptz)::text"),
        "2020-05-17 00:00:00+00"
    );
}
