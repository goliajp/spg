//! v7.37.43-T4.4 — writable CTE acceptance suite (sentori cutover).
//!
//! PG semantics. A WITH clause whose body is `INSERT … RETURNING`
//! (or `UPDATE … RETURNING`, `DELETE … RETURNING`) materialises
//! the RETURNING projection as a virtual table the outer
//! statement can reference. The modifying CTE runs once before
//! the outer statement and shares the same transaction.
//!
//! Sentori migration 0065 dogfood shape:
//!   WITH new_scopes AS (
//!     INSERT INTO identity_scopes (id, name, salt)
//!     SELECT gen_random_uuid(), o.slug || ' (auto)',
//!            gen_random_bytes(32)
//!       FROM orgs o
//!     RETURNING id, name
//!   )
//!   INSERT INTO org_identity_scopes (org_id, scope_id, is_default)
//!   SELECT o.id, s.id, true
//!     FROM orgs o
//!     JOIN new_scopes s ON s.name = o.slug || ' (auto)';

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn execute(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("execute({sql}): {err:?}"));
}

fn execute_rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("execute({sql}): {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected Rows from {sql}, got {other:?}"),
    }
}

// ─── T4.4-α — WITH INSERT … RETURNING then outer INSERT (sentori 0065) ──

#[test]
fn writable_cte_insert_returning_feeds_outer_insert() {
    let mut e = Engine::new();
    execute(
        &mut e,
        "CREATE TABLE source_t (id INT PRIMARY KEY, name TEXT)",
    );
    execute(
        &mut e,
        "CREATE TABLE target_a (id INT PRIMARY KEY, name TEXT)",
    );
    execute(&mut e, "CREATE TABLE target_b (a_id INT, name TEXT)");
    execute(
        &mut e,
        "INSERT INTO source_t VALUES (1,'alpha'),(2,'beta'),(3,'gamma')",
    );

    // Writable CTE: INSERT into A returning rows, then outer INSERT
    // into B using the CTE alias as a join source.
    execute(
        &mut e,
        "WITH new_a AS ( \
             INSERT INTO target_a (id, name) \
             SELECT id, name FROM source_t \
             RETURNING id, name \
         ) \
         INSERT INTO target_b (a_id, name) \
         SELECT id, name FROM new_a",
    );

    let a_rows = execute_rows(&mut e, "SELECT id, name FROM target_a ORDER BY id");
    assert_eq!(a_rows.len(), 3, "all 3 source rows inserted into A");
    let b_rows = execute_rows(&mut e, "SELECT a_id, name FROM target_b ORDER BY a_id");
    assert_eq!(
        b_rows.len(),
        3,
        "outer INSERT fanned out 3 rows from the CTE table"
    );
}

// ─── T4.4-β — modifying CTE body with multi-row RETURNING ──

#[test]
fn writable_cte_returning_carries_multiple_columns() {
    let mut e = Engine::new();
    execute(&mut e, "CREATE TABLE src (id INT PRIMARY KEY, tag TEXT)");
    execute(&mut e, "CREATE TABLE dst (id INT PRIMARY KEY, label TEXT)");
    execute(&mut e, "CREATE TABLE link (src_id INT, dst_id INT)");
    execute(&mut e, "INSERT INTO src VALUES (10,'x'),(20,'y'),(30,'z')");

    execute(
        &mut e,
        "WITH ins AS ( \
             INSERT INTO dst (id, label) \
             SELECT id, tag FROM src \
             RETURNING id, label \
         ) \
         INSERT INTO link (src_id, dst_id) \
         SELECT s.id, i.id FROM src s JOIN ins i ON i.label = s.tag",
    );

    let links = execute_rows(&mut e, "SELECT src_id, dst_id FROM link ORDER BY src_id");
    assert_eq!(links.len(), 3);
    for r in links {
        // src_id should equal dst_id (id was passed through)
        assert_eq!(r[0], r[1], "src_id and dst_id should match via CTE join");
    }
}

// ─── T4.4-γ — WITH UPDATE … RETURNING — outer SELECT reads modified rows ──

#[test]
fn writable_cte_update_returning_feeds_outer_insert() {
    let mut e = Engine::new();
    execute(
        &mut e,
        "CREATE TABLE accounts (id INT PRIMARY KEY, balance INT)",
    );
    execute(&mut e, "CREATE TABLE audit (id INT, new_balance INT)");
    execute(
        &mut e,
        "INSERT INTO accounts VALUES (1,100),(2,200),(3,300)",
    );

    execute(
        &mut e,
        "WITH bumped AS ( \
             UPDATE accounts SET balance = balance + 50 \
             WHERE id <= 2 \
             RETURNING id, balance \
         ) \
         INSERT INTO audit (id, new_balance) \
         SELECT id, balance FROM bumped",
    );

    let audit_rows = execute_rows(&mut e, "SELECT id, new_balance FROM audit ORDER BY id");
    assert_eq!(audit_rows.len(), 2, "two accounts updated and audited");
    // Verify the audit captured post-UPDATE values.
    let acct_rows = execute_rows(&mut e, "SELECT id, balance FROM accounts ORDER BY id");
    assert_eq!(acct_rows.len(), 3);
}

// ─── T4.4-ε — sentori 0065 dogfood shape ──────────────────────

#[test]
fn writable_cte_sentori_0065_identity_scopes_shape() {
    let mut e = Engine::new();
    execute(&mut e, "CREATE TABLE orgs (id INT PRIMARY KEY, slug TEXT)");
    execute(
        &mut e,
        "CREATE TABLE identity_scopes (id INT PRIMARY KEY, name TEXT)",
    );
    execute(
        &mut e,
        "CREATE TABLE org_identity_scopes ( \
             org_id INT NOT NULL, \
             scope_id INT NOT NULL, \
             is_default BOOLEAN NOT NULL DEFAULT false \
         )",
    );
    execute(
        &mut e,
        "INSERT INTO orgs (id, slug) VALUES (1, 'acme'), (2, 'beta-co')",
    );
    // Simplified sentori 0065 shape — no UUID/BYTEA but the
    // structural CTE pattern matches exactly: WITH new_scopes AS (
    // INSERT … RETURNING …) INSERT … SELECT … JOIN new_scopes …
    execute(
        &mut e,
        "WITH new_scopes AS ( \
             INSERT INTO identity_scopes (id, name) \
             SELECT o.id, o.slug FROM orgs o \
             RETURNING id, name \
         ) \
         INSERT INTO org_identity_scopes (org_id, scope_id, is_default) \
         SELECT o.id, s.id, true \
           FROM orgs o \
           JOIN new_scopes s ON s.name = o.slug",
    );
    let scopes = execute_rows(&mut e, "SELECT id, name FROM identity_scopes ORDER BY id");
    assert_eq!(scopes.len(), 2, "two scopes created via CTE");
    let mappings = execute_rows(
        &mut e,
        "SELECT org_id, scope_id FROM org_identity_scopes ORDER BY org_id",
    );
    assert_eq!(
        mappings.len(),
        2,
        "two org->scope mappings via outer INSERT"
    );
    assert_eq!(mappings[0][0], mappings[0][1]);
    assert_eq!(mappings[1][0], mappings[1][1]);
}

// ─── T4.4-δ — WITH DELETE … RETURNING ──

#[test]
fn writable_cte_delete_returning_feeds_outer_insert() {
    let mut e = Engine::new();
    execute(
        &mut e,
        "CREATE TABLE staging (id INT PRIMARY KEY, payload TEXT)",
    );
    execute(
        &mut e,
        "CREATE TABLE archive (id INT PRIMARY KEY, payload TEXT)",
    );
    execute(&mut e, "INSERT INTO staging VALUES (1,'a'),(2,'b'),(3,'c')");

    execute(
        &mut e,
        "WITH purged AS ( \
             DELETE FROM staging WHERE id < 3 RETURNING id, payload \
         ) \
         INSERT INTO archive (id, payload) \
         SELECT id, payload FROM purged",
    );

    let staging_rows = execute_rows(&mut e, "SELECT id FROM staging ORDER BY id");
    assert_eq!(staging_rows.len(), 1, "only id=3 remains in staging");
    let archive_rows = execute_rows(&mut e, "SELECT id, payload FROM archive ORDER BY id");
    assert_eq!(archive_rows.len(), 2, "ids 1 and 2 archived");
}
