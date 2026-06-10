//! v7.9.27 → v7.16.2 — DO $$ ... $$ acceptance.
//!
//! Pre-v7.16.2 the body was silently no-op'd; mailrs round-10
//! flagged that as SEV-1 (idempotent migrations were invisible).
//! v7.16.2 actually runs the body through the trigger
//! executor's PlPgSqlBlock walker. Tests below cover both the
//! new positive cases (DO actually applies its effect) and the
//! shapes that should remain accept-as-no-op.

use spg_engine::{Engine, QueryResult};

fn ok(eng: &mut Engine, sql: &str) {
    let r = eng
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"));
    assert!(
        matches!(r, QueryResult::CommandOk { .. }),
        "{sql:?} → {r:?}"
    );
}

#[test]
fn empty_do_block_is_accepted() {
    let mut eng = Engine::new();
    // Minimum body that parses as a PlPgSqlBlock — `BEGIN END`
    // with no statements (equivalent to v7.9.27's silent no-op
    // but now goes through the actual executor).
    ok(&mut eng, "DO $$ BEGIN END $$");
}

#[test]
fn do_block_with_language_plpgsql() {
    let mut eng = Engine::new();
    ok(
        &mut eng,
        "DO $$ BEGIN RAISE NOTICE 'spg logs this'; END $$ LANGUAGE plpgsql",
    );
}

#[test]
fn tagged_dollar_quoted_do_block() {
    let mut eng = Engine::new();
    // Tagged $body$ quoting + real plpgsql block (must include
    // BEGIN/END now). Uses pg_class which v7.16.2 synthesises.
    ok(
        &mut eng,
        "DO $body$ \
            BEGIN \
                IF EXISTS (SELECT 1 FROM pg_catalog.pg_class WHERE relname = 'foo') THEN \
                    ALTER TABLE foo ADD COLUMN x INT; \
                END IF; \
            END \
        $body$ LANGUAGE plpgsql",
    );
}

#[test]
fn do_block_executes_its_body() {
    // v7.16.2 — the dogfood guarantee mailrs round-10 wanted:
    // DO blocks ACTUALLY alter the catalog. Pre-v7.16.2 this
    // was a silent no-op and the assertion below would have
    // failed at the SELECT.
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (a INT NOT NULL)").unwrap();
    eng.execute("DO $$ BEGIN ALTER TABLE t ADD COLUMN b INT; UPDATE t SET a = a; END $$")
        .unwrap();
    let r = eng.execute("SELECT b FROM t WHERE a = 0").unwrap();
    assert!(matches!(r, QueryResult::Rows { .. }), "{r:?}");
}

#[test]
fn pg_dump_idempotent_do_block_pattern() {
    // The shape mailrs migrate-038 / -040 / -042 use — IF
    // EXISTS over information_schema.columns followed by an
    // ALTER. The condition resolves against the v7.16.2 virtual
    // view; the THEN branch fires only if the column genuinely
    // exists. Idempotent on re-run.
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE accounts (id INT NOT NULL)")
        .unwrap();
    ok(
        &mut eng,
        "DO $$ \
        BEGIN \
            IF NOT EXISTS ( \
                SELECT 1 FROM information_schema.columns \
                WHERE table_name = 'accounts' AND column_name = 'user_id' \
            ) THEN \
                ALTER TABLE accounts ADD COLUMN user_id INT; \
            END IF; \
        END $$",
    );
    // Second run is a no-op — column already exists.
    ok(
        &mut eng,
        "DO $$ \
        BEGIN \
            IF NOT EXISTS ( \
                SELECT 1 FROM information_schema.columns \
                WHERE table_name = 'accounts' AND column_name = 'user_id' \
            ) THEN \
                ALTER TABLE accounts ADD COLUMN user_id INT; \
            END IF; \
        END $$",
    );
}
