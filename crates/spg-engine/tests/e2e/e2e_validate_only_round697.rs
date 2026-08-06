//! Round 697 — the F31 sweep's second batch: three more statements that
//! named something and never looked.
//!
//! Ten `_no_op` statements measured against PG18. Four already agreed
//! (CLUSTER, REINDEX TABLE, VACUUM, ANALYZE all refuse a missing relation,
//! as does DROP TYPE). Of the rest:
//!
//!   * `SET SESSION AUTHORIZATION <role>` took any name. Its comment said
//!     "SPG has no role system so this is a strict no-op" — true when it was
//!     written, and roles became real in round 58. The comment outlived the
//!     fact, and with it the reason the check was absent. It still switches
//!     no authorization; it refuses a role that does not exist.
//!
//!   * `CREATE EXTENSION <e>` reported success for any name, and
//!     `pg_extension` then did not list what had just been "created".
//!     `DROP EXTENSION <e>` likewise. Both read the same list `pg_extension`
//!     is built from now — one list, so the three cannot disagree.
//!
//!     They WARN rather than refuse, and the first cut of this round did
//!     refuse, which three existing tests caught. See
//!     `round697_an_unprovided_extension_warns_rather_than_refusing` for
//!     why the tests were right.
//!
//! Two are left, measured and recorded rather than half-done:
//!
//!   * `DROP AGGREGATE nosuch(int)` — PG says `aggregate nosuch(integer)
//!     does not exist`, rendering the signature with canonical type names.
//!     Reproducing that faithfully is its own piece of work, and a message
//!     that gets the signature wrong would be worse than none.
//!
//!   * `ALTER TABLE t SET SCHEMA nosuch` — PG refuses the missing schema.
//!     Round 652 judged this unfixable while `CREATE SCHEMA` accepts a name
//!     without registering it: a check would refuse sequences PG accepts.
//!     Re-measured this round — `CREATE SCHEMA s697` still does not appear
//!     in `pg_namespace` — so the judgement stands on a current reading.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql}: {r:?}");
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(&format!("PG18 refuses: {sql}")))
}

#[test]
fn round697_set_session_authorization_refuses_a_missing_role() {
    let mut e = Engine::new();
    assert!(err_of(&mut e, "SET SESSION AUTHORIZATION nosuch697").contains("nosuch697"));
    assert!(err_of(&mut e, "SET SESSION AUTHORIZATION 'nosuch697'").contains("nosuch697"));
    // The forms a pg_dump preamble emits still pass.
    ok(&mut e, "SET SESSION AUTHORIZATION DEFAULT");
    ok(&mut e, "SET SESSION AUTHORIZATION postgres");
}

/// An extension this build does not provide is WARNED about, not refused —
/// and the first version of this round did refuse it, which is why the
/// reason is written down here.
///
/// PG18 errors (`extension "x" is not available`). PG can: an extension is
/// installable there. SPG cannot be installed into, so refusing turns a
/// customer dump carrying `CREATE EXTENSION pgcrypto` from something that
/// restores into something that needs editing — the zero-customer-change
/// line, which outranks matching PG's error here.
///
/// Three existing tests caught it: `create_extension_with_schema`
/// (pgcrypto), `create_extension_with_cascade` (hstore) and
/// `create_extension_vector_no_op` (pgvector) all went red. They were
/// right and the change was wrong.
///
/// Saying NOTHING was still the defect: `CREATE EXTENSION hstore` reported
/// plain success and nothing hstore-shaped worked afterwards. It warns.
#[test]
fn round697_an_unprovided_extension_warns_rather_than_refusing() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE EXTENSION hstore");
    ok(&mut e, "CREATE EXTENSION IF NOT EXISTS hstore CASCADE");
    ok(&mut e, "DROP EXTENSION hstore");
    // And the ones it does provide pass without comment.
    for sql in [
        "CREATE EXTENSION vector",
        "CREATE EXTENSION IF NOT EXISTS pg_trgm",
        "CREATE EXTENSION plpgsql WITH SCHEMA public",
        "CREATE EXTENSION pgcrypto",
    ] {
        ok(&mut e, sql);
    }
}

/// `pgcrypto` is on the provided list because SPG really answers it. The
/// list is a claim about capability, so it is checked against capability.
/// v7.39 (round 780) — `hstore` joined it for the same reason.
#[test]
fn round697_the_provided_list_is_a_claim_that_holds() {
    let mut e = Engine::new();
    let one = |e: &mut Engine, sql: &str| match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        other => panic!("{sql}: {other:?}"),
    };
    assert_eq!(one(&mut e, "SELECT digest('x','sha256') IS NOT NULL"), "true");
    assert_eq!(one(&mut e, "SELECT gen_random_uuid() IS NOT NULL"), "true");
    assert_eq!(one(&mut e, "SELECT '[1,2]'::vector::text"), "[1,2]");
    // v7.39 (round 780, F31-D1) — hstore joined the list: its type,
    // codec and both text conversions were always there, and the
    // type-NAME map now resolves the spelling, so the claim holds.
    assert_eq!(one(&mut e, "SELECT 'a=>1'::hstore::text"), "\"a\"=>\"1\"");
}

#[test]
fn round697_drop_extension_takes_the_forms_a_dump_emits() {
    let mut e = Engine::new();
    ok(&mut e, "DROP EXTENSION IF EXISTS nosuch697");
    ok(&mut e, "DROP EXTENSION vector");
    ok(&mut e, "DROP EXTENSION pg_trgm, plpgsql CASCADE");
}

/// The three read one list, so `pg_extension` cannot list something
/// `CREATE EXTENSION` rejects, nor reject something it lists.
#[test]
fn round697_the_extension_list_and_the_catalog_agree() {
    let mut e = Engine::new();
    let listed = match e.execute("SELECT extname FROM pg_extension").unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert!(!listed.is_empty());
    for name in &listed {
        ok(&mut e, &format!("CREATE EXTENSION {name}"));
        ok(&mut e, &format!("DROP EXTENSION {name}"));
    }
}

/// Residuals pinned as differences so the day one changes, someone sees it
/// — and round 707 proved the mechanism: the DROP AGGREGATE half of this
/// pin went red the day that gap closed, and left this file carrying only
/// the SET SCHEMA half.
#[test]
fn round697_the_two_recorded_residuals() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t697(i INT)").unwrap();
    // PG: `schema "nosuch697" does not exist`. See the header for why this
    // one cannot be checked while CREATE SCHEMA does not register.
    ok(&mut e, "ALTER TABLE t697 SET SCHEMA nosuch697");
    ok(&mut e, "CREATE SCHEMA s697");
    let schemas = match e.execute("SELECT nspname FROM pg_namespace").unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert!(
        !schemas.iter().any(|s| s == "s697"),
        "if CREATE SCHEMA starts registering, SET SCHEMA becomes checkable: {schemas:?}"
    );
}

/// v7.39 (round 706) — the foreign-data family (S05g ②). `CREATE SERVER`,
/// `CREATE FOREIGN TABLE` and `CREATE FOREIGN DATA WRAPPER` were consumed
/// silently; PG refuses the first two when the wrapper / server they name
/// does not exist. SPG cannot copy the refusal — PG can refuse because an
/// FDW is installable there, and SPG has no foreign-data machinery at all,
/// so refusing turns a dump that restores today into one that needs
/// editing. The extension resolution applies: accept, and WARN, so a
/// restore log says what will not function instead of reporting success.
#[test]
fn round706_the_foreign_family_warns_rather_than_lying() {
    let mut e = Engine::new();
    for sql in [
        "CREATE SERVER myserver FOREIGN DATA WRAPPER pgfdw OPTIONS (host 'h')",
        "CREATE FOREIGN DATA WRAPPER myfdw",
        "CREATE FOREIGN TABLE ft(id INT) SERVER myserver",
    ] {
        e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
        let notices = e.take_notices();
        assert!(
            notices
                .iter()
                .any(|n| n.message.contains("foreign-data infrastructure is not provided")),
            "{sql}: expected the warning, got {notices:?}"
        );
    }
}

/// v7.39 (round 707) — `DROP AGGREGATE` answers as PG18 answers (S05g ③).
/// It was consumed whole by the dump-noise list, so `DROP AGGREGATE
/// nosuch(int)` reported success. Every shape here is a PG18 measurement:
/// existence is validated across the WHOLE list before anything else (a
/// list with one unknown fails on the unknown even when an earlier entry
/// exists), the signature renders with canonical type names, `(*)` stays
/// `(*)`, and a name that exists is a built-in and therefore undroppable.
#[test]
fn round707_drop_aggregate_answers_as_pg_does() {
    let mut e = Engine::new();
    let err = |e: &mut Engine, sql: &str| -> String {
        format!("{}", e.execute(sql).expect_err(&format!("PG18 refuses: {sql}")))
    };
    assert!(
        err(&mut e, "DROP AGGREGATE nosuch707(int)")
            .contains("aggregate nosuch707(integer) does not exist"),
    );
    assert!(
        err(&mut e, "DROP AGGREGATE nosuch707(int, text)")
            .contains("aggregate nosuch707(integer, text) does not exist"),
    );
    assert!(
        err(&mut e, "DROP AGGREGATE nosuch707(*)")
            .contains("aggregate nosuch707(*) does not exist"),
    );
    assert!(
        err(&mut e, "DROP AGGREGATE nosuch707(double precision)")
            .contains("aggregate nosuch707(double precision) does not exist"),
    );
    // Existence first, across the list — sum exists, nosuch wins the error.
    assert!(
        err(&mut e, "DROP AGGREGATE sum(integer), nosuch707(int)")
            .contains("aggregate nosuch707(integer) does not exist"),
    );
    // A built-in is undroppable, with PG's sentence.
    assert!(
        err(&mut e, "DROP AGGREGATE sum(int)")
            .contains("cannot drop function sum(integer) because it is required by the database system"),
    );
    // IF EXISTS keeps the unknown quiet.
    e.execute("DROP AGGREGATE IF EXISTS nosuch707(int)").unwrap();
}

/// v7.39 (round 708) — the S05g ④ batch: twelve probes, six real gaps, all
/// "the object named does not exist and SPG said fine". Each expectation is
/// a PG18 measurement. The four accepted-and-recorded shapes (ALTER
/// AGGREGATE rename of a built-in, ALTER OPERATOR SET SCHEMA, ALTER
/// SERVER, ALTER TABLESPACE) are NOT pinned as errors, because PG genuinely
/// performs the first two and SPG's ForeignInfra/no-op stance covers the
/// rest — recorded in the ledger, not frozen in a test.
#[test]
fn round708_the_alter_and_drop_batch_validates_its_names() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rt708(i INT)").unwrap();
    for (sql, want) in [
        (
            "ALTER AGGREGATE nosuch708(int) RENAME TO x",
            "aggregate nosuch708(integer) does not exist",
        ),
        (
            "ALTER USER nosuch708 WITH PASSWORD 'x'",
            "role \"nosuch708\" does not exist",
        ),
        (
            "ALTER TYPE nosuch708 RENAME TO x",
            "type \"nosuch708\" does not exist",
        ),
        (
            "DROP RULE nosuch708 ON nosuch708t",
            "relation \"nosuch708t\" does not exist",
        ),
        (
            "DROP RULE nosuch708 ON rt708",
            "rule \"nosuch708\" for relation \"rt708\" does not exist",
        ),
        (
            "DROP CONVERSION nosuch708",
            "conversion \"nosuch708\" does not exist",
        ),
        (
            "DROP LANGUAGE nosuch708",
            "language \"nosuch708\" does not exist",
        ),
    ] {
        let err = err_of(&mut e, sql);
        assert!(err.contains(want), "{sql}\n  got: {err}\n  want: {want}");
        assert!(!err.contains("corrupt on-disk format"), "{sql}: {err}");
    }
    // The rule error carries PG's `for relation`, not the old `on`.
    // And a known type still no-ops through the unmodelled forms.
    e.execute("CREATE TYPE mood708 AS ENUM ('a')").unwrap();
    e.execute("ALTER TYPE mood708 RENAME TO mood709").unwrap();
    e.execute("DROP PROCEDURAL LANGUAGE IF EXISTS nosuch708").unwrap();
}

/// v7.39 (round 709) — the S05g ④ second batch: twelve probes, nine now
/// byte-identical with PG18. The three that stay different are judgements,
/// not gaps: DROP SERVER / DROP FOREIGN TABLE join the foreign-data
/// warning family (a dump's CREATE→DROP sequence must stay consistent with
/// round 706's accepted CREATE), and the two CREATEs that reference a
/// FUNCTION (event trigger, access method) wait on a function-existence
/// predicate — recorded, not half-done.
#[test]
fn round709_the_second_batch_validates_its_names() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "ALTER COLLATION nosuch709 RENAME TO x",
            "collation \"nosuch709\" for encoding \"UTF8\" does not exist",
        ),
        (
            "ALTER TEXT SEARCH CONFIGURATION nosuch709 OWNER TO bench",
            "text search configuration \"nosuch709\" does not exist",
        ),
        (
            "ALTER EVENT TRIGGER nosuch709 DISABLE",
            "event trigger \"nosuch709\" does not exist",
        ),
        (
            "DROP EVENT TRIGGER nosuch709",
            "event trigger \"nosuch709\" does not exist",
        ),
        (
            "DROP TABLESPACE nosuch709",
            "tablespace \"nosuch709\" does not exist",
        ),
        (
            "DROP TABLESPACE pg_default",
            "permission denied for tablespace pg_default",
        ),
        (
            "DROP TEXT SEARCH CONFIGURATION nosuch709",
            "text search configuration \"nosuch709\" does not exist",
        ),
        (
            "DROP COLLATION nosuch709",
            "collation \"nosuch709\" for encoding \"UTF8\" does not exist",
        ),
        (
            "ALTER LARGE OBJECT 999999 OWNER TO bench",
            "large object 999999 does not exist",
        ),
    ] {
        let err = err_of(&mut e, sql);
        assert!(err.contains(want), "{sql}\n  got: {err}\n  want: {want}");
    }
    // The legitimate forms: a performable collation, a shipped ts config,
    // a real large object, and every IF EXISTS spelling.
    ok(&mut e, "ALTER COLLATION \"en_US\" RENAME TO whatever");
    ok(&mut e, "ALTER TEXT SEARCH CONFIGURATION english OWNER TO postgres");
    let lo = match e.execute("SELECT lo_create(4242)").unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    };
    assert_eq!(lo, "4242");
    ok(&mut e, "ALTER LARGE OBJECT 4242 OWNER TO postgres");
    for sql in [
        "DROP TABLESPACE IF EXISTS nosuch709",
        "DROP COLLATION IF EXISTS nosuch709",
        "DROP EVENT TRIGGER IF EXISTS nosuch709",
        "DROP TEXT SEARCH CONFIGURATION IF EXISTS nosuch709",
    ] {
        ok(&mut e, sql);
    }
    // The foreign-data DROPs warn like their CREATEs.
    e.execute("DROP SERVER nosuch709").unwrap();
    assert!(
        e.take_notices()
            .iter()
            .any(|n| n.message.contains("foreign-data infrastructure is not provided")),
    );
}

/// v7.39 (round 710) — the S05g ④ tail batch: ten probes, five fixed, two
/// of them PARSE holes rather than validation holes. `COMMENT ON FUNCTION
/// f(int)` — the spelling pg_dump writes — was a syntax error at the `(`,
/// so one function comment in a dump failed the restore; `ALTER INDEX i
/// SET (fillfactor = 90)` was a syntax error at SET. Both parse now; the
/// signature's per-overload validation stays with the function-predicate
/// follow-up, and index storage params no-op as ALTER TABLE's already do.
///
/// Recorded, not fixed: LOAD accepts (SPG has no loadable libraries — the
/// foreign-infra reasoning), and ALTER TABLE SET's parameter names and
/// bounds go unchecked pending a parameter catalog.
#[test]
fn round710_the_tail_batch() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t710(i INT)").unwrap();
    e.execute("CREATE INDEX ix710 ON t710(i)").unwrap();
    for (sql, want) in [
        ("ALTER TABLE t710 OF nosuch710", "type \"nosuch710\" does not exist"),
        (
            "ALTER TABLE t710 REPLICA IDENTITY USING INDEX nosuch710",
            "index \"nosuch710\" for table \"t710\" does not exist",
        ),
        (
            "ALTER INDEX nosuch710 SET (fillfactor = 90)",
            "relation \"nosuch710\" does not exist",
        ),
        (
            "CREATE TRIGGER tg710 BEFORE INSERT ON t710 FOR EACH ROW \
             EXECUTE FUNCTION nosuch_fn710()",
            "function nosuch_fn710() does not exist",
        ),
    ] {
        let err = err_of(&mut e, sql);
        assert!(err.contains(want), "{sql}\n  got: {err}\n  want: {want}");
    }
    // The legitimate forms, including the dump spellings that used to be
    // syntax errors.
    e.execute("CREATE TYPE ct710 AS (a INT, b TEXT)").unwrap();
    e.execute("CREATE FUNCTION f710() RETURNS INT LANGUAGE sql AS $$ SELECT 1 $$")
        .unwrap();
    for sql in [
        "ALTER TABLE t710 OF ct710",
        "ALTER TABLE t710 NOT OF",
        "ALTER TABLE t710 REPLICA IDENTITY USING INDEX ix710",
        "ALTER TABLE t710 REPLICA IDENTITY FULL",
        "ALTER INDEX ix710 SET (fillfactor = 90)",
        "ALTER INDEX ix710 RESET (fillfactor)",
        "COMMENT ON FUNCTION f710() IS 'x'",
        "COMMENT ON FUNCTION f710(int, text) IS 'x'",
    ] {
        ok(&mut e, sql);
    }
}

/// v7.39 (round 711) — F08's storing half (S05d step 1). Round 621 taught
/// the parser to CONSUME `DEFERRABLE INITIALLY DEFERRED` on PK/UNIQUE; the
/// FK path had stored the flags since round 288, and `UniquenessConstraint`
/// had nowhere to put them — so the declaration died between the parser and
/// the catalog. They persist now (FILE_VERSION 89 timing appendix, same bit
/// layout as the FK byte), through both the CREATE TABLE and the ALTER
/// TABLE ADD CONSTRAINT spellings, and `pg_constraint` reports them.
///
/// The ENFORCEMENT half — the transaction-scoped check queue — is the next
/// knife; nothing here claims the checks defer yet.
#[test]
fn round711_pk_unique_deferrable_flags_are_stored() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d711 (id INT PRIMARY KEY DEFERRABLE INITIALLY DEFERRED)")
        .unwrap();
    e.execute("CREATE TABLE d711b (id INT, CONSTRAINT u711 UNIQUE (id) DEFERRABLE)")
        .unwrap();
    e.execute("CREATE TABLE d711c (id INT PRIMARY KEY)").unwrap();
    e.execute("CREATE TABLE d711d (id INT)").unwrap();
    e.execute("ALTER TABLE d711d ADD CONSTRAINT p711 PRIMARY KEY (id) DEFERRABLE INITIALLY DEFERRED")
        .unwrap();
    let rows = match e
        .execute(
            "SELECT conname, condeferrable, condeferred FROM pg_constraint \
             WHERE contype IN ('p','u') ORDER BY conname",
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert!(rows.contains(&"d711_pkey|true|true".to_string()), "{rows:?}");
    assert!(rows.contains(&"u711|true|false".to_string()), "{rows:?}");
    assert!(rows.contains(&"d711c_pkey|false|false".to_string()), "{rows:?}");
    assert!(rows.contains(&"p711|true|true".to_string()), "{rows:?}");
}

/// v7.39 (round 712) — F08's enforcement half: deferred PK/UNIQUE checks
/// really defer. Every shape is the r711 PG18 measurement:
///
///   ① a transient duplicate healed before COMMIT commits cleanly — the
///     legal PG sequence that used to fail on the second INSERT;
///   ② a violation left in place errors AT COMMIT (23505 wording) and the
///     transaction rolls back;
///   ③ `SET CONSTRAINTS ALL IMMEDIATE` pulls the check to the SET;
///   ④ INITIALLY IMMEDIATE flips into deferral via SET CONSTRAINTS;
///   ⑤ NOT DEFERRABLE is immune to SET CONSTRAINTS.
///
/// The machinery is round 288's FK deferral extended: the same tx-state
/// timing rules (`uc_deferred_in` mirrors `fk_deferred_in`), the same
/// COMMIT sweep — with a WHOLE-TABLE validator, because by COMMIT the rows
/// are already in the table and the insert-time probe would collide with
/// itself.
#[test]
fn round712_deferred_unique_checks_defer() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d712 (id INT PRIMARY KEY DEFERRABLE INITIALLY DEFERRED)")
        .unwrap();
    // ① heal before COMMIT.
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO d712 VALUES (1)").unwrap();
    e.execute("INSERT INTO d712 VALUES (1)").unwrap();
    e.execute("DELETE FROM d712 WHERE id = 1").unwrap();
    e.execute("COMMIT").unwrap();
    // ② leave the violation in place: COMMIT errors, tx rolls back.
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO d712 VALUES (2)").unwrap();
    e.execute("INSERT INTO d712 VALUES (2)").unwrap();
    let err = format!("{}", e.execute("COMMIT").expect_err("PG errors at COMMIT"));
    assert!(
        err.contains("duplicate key value violates unique constraint \"d712_pkey\""),
        "{err}"
    );
    let n = match e.execute("SELECT count(*) FROM d712").unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    };
    assert_eq!(n, "0", "the failed COMMIT must leave nothing behind");
    // ③ IMMEDIATE pulls the check forward — by the SYNTHESISED name, which
    // is how PG reaches an unnamed constraint.
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO d712 VALUES (3)").unwrap();
    e.execute("INSERT INTO d712 VALUES (3)").unwrap();
    assert!(
        format!(
            "{}",
            e.execute("SET CONSTRAINTS d712_pkey IMMEDIATE")
                .expect_err("the pending violation surfaces at the SET")
        )
        .contains("d712_pkey")
    );
    let _ = e.execute("ROLLBACK");
    // ④ INITIALLY IMMEDIATE flips into deferral.
    e.execute("CREATE TABLE d712b (id INT PRIMARY KEY DEFERRABLE)").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("SET CONSTRAINTS ALL DEFERRED").unwrap();
    e.execute("INSERT INTO d712b VALUES (4)").unwrap();
    e.execute("INSERT INTO d712b VALUES (4)").unwrap();
    e.execute("DELETE FROM d712b WHERE id = 4").unwrap();
    e.execute("COMMIT").unwrap();
    // ⑤ NOT DEFERRABLE is immune.
    e.execute("CREATE TABLE d712c (id INT PRIMARY KEY)").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("SET CONSTRAINTS ALL DEFERRED").unwrap();
    e.execute("INSERT INTO d712c VALUES (5)").unwrap();
    assert!(
        format!(
            "{}",
            e.execute("INSERT INTO d712c VALUES (5)")
                .expect_err("immediate, whatever SET CONSTRAINTS says")
        )
        .contains("d712c_pkey")
    );
    let _ = e.execute("ROLLBACK");
}
