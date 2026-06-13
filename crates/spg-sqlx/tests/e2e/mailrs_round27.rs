//! mailrs round-27 (P0) — arithmetic expressions in RETURNING were
//! wire-typed TEXT, so `RETURNING uidnext - 1 AS uid` failed i32
//! decode and every inbound-mail index write since cutover was
//! silently lost (recoverable from maildir). Verbatim repro from the
//! report; typed decode per the TESTING.md acceptance-shape rules.

use spg_sqlx::{SpgPool, SpgPoolExt};

async fn pool() -> SpgPool {
    let p = SpgPool::connect_in_memory().await.expect("in-memory spg");
    for ddl in [
        "CREATE TABLE mb (id BIGSERIAL PRIMARY KEY, name TEXT, \
         uidnext INTEGER NOT NULL DEFAULT 1, hm BIGINT NOT NULL DEFAULT 0)",
        "INSERT INTO mb (name) VALUES ('INBOX')",
    ] {
        sqlx::query(ddl).execute(&p).await.unwrap();
    }
    p
}

#[tokio::test]
async fn returning_arithmetic_types_as_int_not_text() {
    let p = pool().await;
    // The exact index_message statement shape: INT - INT must come
    // back as INT (decodable as i32), BIGINT alias as BIGINT.
    let (id, uid, new_modseq): (i64, i32, i64) = sqlx::query_as(
        "UPDATE mb SET uidnext = uidnext + 1, hm = hm + 1 WHERE name = $1 \
         RETURNING id, uidnext - 1 AS uid, hm AS new_modseq",
    )
    .bind("INBOX")
    .fetch_one(&p)
    .await
    .expect("typed decode of arithmetic RETURNING");
    assert_eq!((id, uid, new_modseq), (1, 1, 1));
    // Second delivery increments through the same path.
    let (_, uid2, modseq2): (i64, i32, i64) = sqlx::query_as(
        "UPDATE mb SET uidnext = uidnext + 1, hm = hm + 1 WHERE name = $1 \
         RETURNING id, uidnext - 1 AS uid, hm AS new_modseq",
    )
    .bind("INBOX")
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!((uid2, modseq2), (2, 2));
}

#[tokio::test]
async fn returning_bigint_plus_int_widens_to_bigint() {
    let p = pool().await;
    let (v,): (i64,) = sqlx::query_as(
        "UPDATE mb SET hm = hm + 1 WHERE name = 'INBOX' RETURNING hm + uidnext AS s",
    )
    .fetch_one(&p)
    .await
    .expect("BIGINT + INT decodes as i64");
    assert_eq!(v, 2);
}

#[tokio::test]
async fn returning_plain_columns_stay_typed() {
    let p = pool().await;
    let (id, name): (i64, String) =
        sqlx::query_as("UPDATE mb SET hm = hm WHERE name = 'INBOX' RETURNING id, name")
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!((id, name.as_str()), (1, "INBOX"));
}

// mailrs round-28 — correlated scalar subquery in UPDATE SET on the
// prepared (sqlx inline) path. The engine fix lives in the shared
// exec_update_cancel; this pins the embed consumer's actual path.
#[tokio::test]
async fn round28_correlated_subquery_in_update_set() {
    let pool = SpgPool::connect_in_memory().await.expect("in-memory spg");
    for ddl in [
        "CREATE TABLE mb (id BIGINT PRIMARY KEY, uidnext INT)",
        "CREATE TABLE msg (mailbox_id BIGINT, uid INT)",
        "INSERT INTO mb VALUES (1, 5), (2, 5)",
        "INSERT INTO msg VALUES (1, 10), (1, 12), (2, 99), (2, 7)",
    ] {
        sqlx::query(ddl).execute(&pool).await.unwrap();
    }
    sqlx::query(
        "UPDATE mb SET uidnext = (SELECT MAX(m.uid) FROM msg m WHERE m.mailbox_id = mb.id)",
    )
    .execute(&pool)
    .await
    .expect("correlated UPDATE SET over prepared path");
    let (u1,): (i32,) = sqlx::query_as("SELECT uidnext FROM mb WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (u2,): (i32,) = sqlx::query_as("SELECT uidnext FROM mb WHERE id = 2")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((u1, u2), (12, 99));
}
