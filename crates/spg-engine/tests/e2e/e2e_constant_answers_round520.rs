//! v7.39 (round 520) — the constant-answer test, generated rather than typed.
//!
//! The hand-written panel asks 31 pairs. Its value is entirely in how many
//! it asks, and 31 is thin against the ~760 functions SPG resolves, so
//! `scripts/constant-answer-sweep.py` generates the pairs from `pg_proc`:
//! two literals per declared argument type, and a function is suspect when
//! SPG answers the same thing both times while PG does not. 1308 pairs, 9
//! suspects, three of them real.
//!
//! `pg_get_userbyid` is the one that mattered. It answered the CURRENT user
//! for every oid, so `pg_get_userbyid(relowner)` named the caller rather
//! than the owner — every row of a catalog join looked self-owned.
//!
//! Two of the nine were not stubs, and both took a measurement to tell
//! apart: `obj_description` works (the sweep's oids simply named no
//! commented object), and `pg_function_is_visible(77)` is NULL here because
//! 77 names no function in SPG, which is exactly what round 518 made it say.
//!
//! Every expectation below is a PG18 reading.

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

/// How wide a character is in the NAMED encoding. This answered 4 for every
/// id, under a comment reading "SPG only speaks UTF8" — speaking one
/// encoding is not a reason to misreport the others.
#[test]
fn round520_encoding_max_length_reads_the_encoding() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT pg_encoding_max_length(0), pg_encoding_max_length(1), \
             pg_encoding_max_length(6), pg_encoding_max_length(8)"
        ),
        "1|3|4|1"
    );
    // An id that names no encoding is NULL.
    assert_eq!(text(&mut e, "SELECT pg_encoding_max_length(99)"), "NULL");
    assert_eq!(text(&mut e, "SELECT pg_encoding_max_length(NULL::int)"), "NULL");
}

/// The one that mattered: an owner oid must name the OWNER.
#[test]
fn round520_get_userbyid_names_the_role_not_the_caller() {
    let mut e = engine();
    // An oid that names no role gets PG's wording, not the current user.
    assert_eq!(
        text(&mut e, "SELECT pg_get_userbyid(999::oid)"),
        "unknown (OID=999)"
    );
    // A real role resolves to its own name — and the numbering is the one
    // `pg_roles` publishes, so the two agree.
    e.execute("CREATE USER alice PASSWORD 'x'").unwrap();
    let oid = text(&mut e, "SELECT oid FROM pg_roles WHERE rolname = 'alice'");
    assert_eq!(
        text(&mut e, &format!("SELECT pg_get_userbyid({oid}::oid)")),
        "alice"
    );
}

/// `xml(text)` is PG's constructor and answered NULL for everything, so a
/// value cast through it vanished.
#[test]
fn round520_xml_constructor_keeps_its_input() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT xml('alpha')::text, xml('<a/>')::text"),
        "alpha|<a/>"
    );
    assert_eq!(text(&mut e, "SELECT xml(NULL::text)"), "NULL");
}

/// The two the sweep flagged that were RIGHT. Pinned so a later change that
/// breaks them is not mistaken for closing a stub.
#[test]
fn round520_the_two_that_were_not_stubs() {
    let mut e = engine();
    e.execute("CREATE TABLE oc (a INT)").unwrap();
    e.execute("COMMENT ON TABLE oc IS 'hello'").unwrap();
    assert_eq!(text(&mut e, "SELECT obj_description('oc'::regclass)"), "hello");
    // An oid naming no function is NULL — round 518's rule, not a stub.
    assert_eq!(text(&mut e, "SELECT pg_function_is_visible(999999::oid)"), "NULL");
}
