//! 9.0.3 — the writes that stopped passing over the whole table, pinned
//! for their ANSWERS on the configuration a shipped image runs
//! (`en_US.utf8`). The uniqueness check, the ON CONFLICT lookups and the
//! locale-collated indexes now ask an index where they used to read every
//! row; this is what must not have changed in doing so. Every expected
//! value is PostgreSQL 18.6's answer to the same statements.

use crate::common;
use crate::e2e_copy_903::{ok, open, rows, run};

#[test]
fn text_keys_keep_their_answers_when_writes_ask_an_index() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-tk903-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .env("SPG_LC_COLLATE", "en_US.utf8")
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut c = open(addrs.pgwire.as_ref().unwrap());
    assert_eq!(
        rows(
            &mut c,
            "SELECT datcollate FROM pg_database WHERE datname = current_database()"
        ),
        "en_US.utf8\n"
    );

    let err = |c: &mut std::net::TcpStream, sql: &str| run(c, sql, "").error;
    let tag = |c: &mut std::net::TcpStream, sql: &str| run(c, sql, "").tag;

    // A text primary key.
    ok(
        &mut c,
        "CREATE TABLE users (email text PRIMARY KEY, n int NOT NULL DEFAULT 0)",
    );
    ok(
        &mut c,
        "INSERT INTO users (email) SELECT 'u' || g || '@x' FROM generate_series(1, 3000) g",
    );
    assert!(
        err(&mut c, "INSERT INTO users (email) VALUES ('u5@x')")
            .is_some_and(|e| e.starts_with("23505:"))
    );
    assert_eq!(
        tag(
            &mut c,
            "INSERT INTO users (email) VALUES ('u5@x') ON CONFLICT DO NOTHING"
        )
        .as_deref(),
        Some("INSERT 0 0")
    );
    assert_eq!(
        rows(
            &mut c,
            "INSERT INTO users (email) VALUES ('u5@x') ON CONFLICT (email) DO UPDATE SET n = users.n + 1 RETURNING n"
        ),
        "1\n"
    );
    ok(
        &mut c,
        "UPDATE users SET email = 'renamed@x' WHERE email = 'u8@x'",
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT (SELECT count(*) FROM users WHERE email = 'renamed@x'), (SELECT count(*) FROM users WHERE email = 'u8@x')"
        ),
        "1|0\n"
    );
    assert!(
        err(&mut c, "INSERT INTO users (email) VALUES ('renamed@x')")
            .is_some_and(|e| e.starts_with("23505:"))
    );
    assert_eq!(
        tag(&mut c, "INSERT INTO users (email) VALUES ('u8@x')").as_deref(),
        Some("INSERT 0 1")
    );

    // sentori's `issue_user_hits`: a composite key with a text part.
    ok(
        &mut c,
        "CREATE TABLE hits (issue_id int, user_key text, n bigint NOT NULL DEFAULT 1, PRIMARY KEY (issue_id, user_key))",
    );
    ok(
        &mut c,
        "INSERT INTO hits (issue_id, user_key) SELECT g % 20, 'k' || g FROM generate_series(1, 3000) g",
    );
    for want in ["2\n", "3\n"] {
        assert_eq!(
            rows(
                &mut c,
                "INSERT INTO hits VALUES (5, 'k5', 1) ON CONFLICT (issue_id, user_key) DO UPDATE SET n = hits.n + 1 RETURNING n"
            ),
            want
        );
    }
    assert!(
        err(&mut c, "INSERT INTO hits VALUES (5, 'k5', 1)")
            .is_some_and(|e| e.starts_with("23505:"))
    );

    // A unique index and an expression index on text, across an UPDATE.
    ok(&mut c, "CREATE TABLE tags (id int PRIMARY KEY, name text)");
    ok(&mut c, "CREATE UNIQUE INDEX tags_name ON tags (name)");
    ok(&mut c, "CREATE INDEX tags_lower ON tags (lower(name))");
    ok(
        &mut c,
        "INSERT INTO tags SELECT g, 'Tag' || g FROM generate_series(1, 3000) g",
    );
    ok(&mut c, "UPDATE tags SET name = 'Fresh' WHERE id = 7");
    assert!(
        err(&mut c, "INSERT INTO tags VALUES (9001, 'Fresh')")
            .is_some_and(|e| e.starts_with("23505:"))
    );
    assert_eq!(
        tag(&mut c, "INSERT INTO tags VALUES (9002, 'Tag7')").as_deref(),
        Some("INSERT 0 1")
    );
    assert_eq!(
        rows(&mut c, "SELECT id FROM tags WHERE lower(name) = 'fresh'"),
        "7\n"
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT count(*) FROM tags WHERE lower(name) = 'tag7'"
        ),
        "1\n"
    );
}
