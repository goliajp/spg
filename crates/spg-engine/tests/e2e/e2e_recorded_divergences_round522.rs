//! v7.39 (round 522) — the three divergences round 521's audit recorded
//! but did not fix.
//!
//! Every expectation below is a PG18 reading, taken with the object type
//! or parameter named in the test.

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

/// The default ACL of every object type PG accepts. This arm was wrong in
/// five ways at once, and the two that changed an answer rather than
/// erroring are the interesting ones: a relation was missing `m`
/// (MAINTAIN, added in PG 17), and `L` was described as a language when
/// PG's `L` is a LARGE OBJECT — so it reported `U` where PG says `rw`.
#[test]
fn round522_acldefault_matches_pg_for_every_object_type() {
    let mut e = engine();
    // Owner oid 10 is the bootstrap superuser, which SPG publishes as
    // `postgres` — the same name `pg_roles` and `pg_get_userbyid` give.
    for (code, expect) in [
        ("r", "{postgres=arwdDxtm/postgres}"),
        ("s", "{postgres=rwU/postgres}"),
        ("n", "{postgres=UC/postgres}"),
        ("t", "{postgres=C/postgres}"),
        ("F", "{postgres=U/postgres}"),
        ("S", "{postgres=U/postgres}"),
        ("L", "{postgres=rw/postgres}"),
        ("p", "{postgres=sA/postgres}"),
    ] {
        assert_eq!(
            text(&mut e, &format!("SELECT acldefault('{code}'::\"char\", 10)")),
            expect,
            "acldefault('{code}')"
        );
    }
}

/// Four types grant PUBLIC something by default, and PUBLIC is the empty
/// name before the `=`. Dropping that entry made `acldefault('f', …)`
/// describe an execute-restricted function where PG grants execute to
/// the world — the exact question a privilege audit asks.
#[test]
fn round522_acldefault_keeps_the_public_entry() {
    let mut e = engine();
    for (code, expect) in [
        ("f", "{=X/postgres,postgres=X/postgres}"),
        ("T", "{=U/postgres,postgres=U/postgres}"),
        ("l", "{=U/postgres,postgres=U/postgres}"),
        // A database grants PUBLIC less than it grants the owner.
        ("d", "{=Tc/postgres,postgres=CTc/postgres}"),
    ] {
        assert_eq!(
            text(&mut e, &format!("SELECT acldefault('{code}'::\"char\", 10)")),
            expect,
            "acldefault('{code}')"
        );
    }
    // A column's default really is empty — it inherits the table's.
    assert_eq!(text(&mut e, "SELECT acldefault('c'::\"char\", 10)"), "{}");
}

/// The owner comes from the OID, and an oid naming no role renders as
/// the number — both as grantee and as grantor. This ignored its second
/// argument and said `admin` about whoever was asked.
#[test]
fn round522_acldefault_names_the_owner_from_the_oid() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT acldefault('r'::\"char\", 999)"),
        "{999=arwdDxtm/999}"
    );
    // NULL in either argument is NULL, not an error.
    assert_eq!(text(&mut e, "SELECT acldefault(NULL::\"char\", 10)"), "NULL");
    assert_eq!(text(&mut e, "SELECT acldefault('r'::\"char\", NULL)"), "NULL");
}

/// PG declares `acldefault` over `"char"`, so the canonical call passes
/// one — and SPG rejected it with "objtype must be \"char\"", which is
/// precisely what it had been handed. TEXT keeps working.
#[test]
fn round522_acldefault_accepts_the_type_it_declares() {
    let mut e = engine();
    let via_char = text(&mut e, "SELECT acldefault('r'::\"char\", 10)");
    let via_text = text(&mut e, "SELECT acldefault('r', 10)");
    assert_eq!(via_char, "{postgres=arwdDxtm/postgres}");
    assert_eq!(via_char, via_text);
}

/// PG has no timestamp overload of `date_add` — the argument is coerced
/// to timestamptz and the answer is one, offset and all.
#[test]
fn round522_date_add_answers_timestamptz() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT pg_typeof(date_add(TIMESTAMP '2020-01-01', INTERVAL '1 hour'))"
        ),
        "timestamp with time zone"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT date_add(TIMESTAMP '2020-01-01', INTERVAL '1 hour')::text"
        ),
        "2020-01-01 01:00:00+00"
    );
    // And its sibling.
    assert_eq!(
        text(
            &mut e,
            "SELECT pg_typeof(date_subtract(TIMESTAMP '2020-01-01', INTERVAL '1 hour'))"
        ),
        "timestamp with time zone"
    );
}

/// `pg_settings.setting` is a bare number counting `unit`s. Reporting
/// the human form while `vartype` says `integer` contradicts itself: a
/// client that believes the vartype and parses the setting gets nothing.
#[test]
fn round522_pg_settings_reports_raw_counts_and_units() {
    let mut e = engine();
    for (name, expect) in [
        ("work_mem", "4096|kB|integer"),
        ("maintenance_work_mem", "65536|kB|integer"),
        // Counted in blocks, and PG names the block size as the unit.
        ("shared_buffers", "16384|8kB|integer"),
        ("effective_cache_size", "524288|8kB|integer"),
        ("statement_timeout", "0|ms|integer"),
        // Was absent from the ms list entirely.
        ("transaction_timeout", "0|ms|integer"),
        // A parameter with no unit keeps NULL there.
        ("max_connections", "100|NULL|integer"),
    ] {
        assert_eq!(
            text(
                &mut e,
                &format!(
                    "SELECT setting, unit, vartype FROM pg_settings WHERE name = '{name}'"
                )
            ),
            expect,
            "pg_settings row for {name}"
        );
    }
}

/// SHOW / `current_setting` keep the HUMAN form — the two spellings are
/// PG's, and they are not the same string.
#[test]
fn round522_show_keeps_the_human_form_after_a_set() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT current_setting('work_mem')"), "4MB");
    e.execute("SET work_mem = '8MB'").unwrap();
    assert_eq!(text(&mut e, "SELECT current_setting('work_mem')"), "8MB");
    // A bare number is already in the GUC's unit, so this is the same
    // setting written the other way.
    e.execute("SET work_mem = 8192").unwrap();
    assert_eq!(text(&mut e, "SELECT current_setting('work_mem')"), "8MB");
    assert_eq!(
        text(
            &mut e,
            "SELECT setting, boot_val, reset_val FROM pg_settings WHERE name = 'work_mem'"
        ),
        // boot_val / reset_val stay at the compiled-in default, raw.
        "8192|4096|4096"
    );
}
