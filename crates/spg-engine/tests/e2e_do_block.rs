//! v7.9.27 — DO $$ ... $$ no-op acceptance. mailrs H1.

use spg_engine::{Engine, QueryResult};

fn ok(eng: &mut Engine, sql: &str) {
    let r = eng
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"));
    assert!(
        matches!(r, QueryResult::CommandOk { affected: 0, .. }),
        "{sql:?}"
    );
}

#[test]
fn empty_do_block_is_accepted() {
    let mut eng = Engine::new();
    ok(&mut eng, "DO $$ $$");
}

#[test]
fn do_block_with_language_plpgsql() {
    let mut eng = Engine::new();
    ok(
        &mut eng,
        "DO $$ BEGIN RAISE NOTICE 'spg ignores this'; END $$ LANGUAGE plpgsql",
    );
}

#[test]
fn tagged_dollar_quoted_do_block() {
    let mut eng = Engine::new();
    ok(
        &mut eng,
        "DO $body$ \
            IF EXISTS (SELECT 1 FROM pg_class WHERE relname = 'foo') THEN \
                ALTER TABLE foo ADD COLUMN x INT; \
            END IF; \
        $body$ LANGUAGE plpgsql",
    );
}

#[test]
fn do_block_does_not_alter_catalog() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (a INT NOT NULL)").unwrap();
    // DO block in between two real statements.
    eng.execute("DO $$ SELECT 'noop'; $$").unwrap();
    eng.execute("INSERT INTO t VALUES (1)").unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    assert_eq!(cat.table_count(), 1);
}

#[test]
fn pg_dump_idempotent_do_block_pattern() {
    // The shape pg_dump emits for `IF NOT EXISTS` constraint adds.
    let mut eng = Engine::new();
    ok(
        &mut eng,
        "DO $$ \
        BEGIN \
            IF NOT EXISTS ( \
                SELECT 1 FROM pg_constraint WHERE conname = 'fk_user' \
            ) THEN \
                ALTER TABLE accounts ADD CONSTRAINT fk_user \
                    FOREIGN KEY (user_id) REFERENCES users(id); \
            END IF; \
        END $$",
    );
}
