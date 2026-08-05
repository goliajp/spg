//! Round 750 — `ALTER ROLE|USER … PASSWORD` really rotates the
//! credential. It was a recorded no-op (round-710 ledger, a SECURITY
//! defect): the statement answered ALTER ROLE and the OLD password
//! kept working. Now every derived form rotates (legacy hash, both
//! MySQL hashes, the SCRAM-SHA-256 verifier), PASSWORD NULL clears the
//! credential, and a missing role refuses with PG's sentence.

use spg_engine::{Engine, Role};

#[test]
fn round750_password_rotation_takes_effect() {
    let mut e = Engine::new();
    e.execute("CREATE USER u750 WITH PASSWORD 'oldpw'").unwrap();
    assert_eq!(e.verify_user("u750", "oldpw"), Some(Role::ReadOnly));
    // Rotate.
    e.execute("ALTER USER u750 WITH PASSWORD 'newpw'").unwrap();
    assert_eq!(
        e.verify_user("u750", "oldpw"),
        None,
        "the OLD password must stop working — this was the round-710 hole"
    );
    assert_eq!(e.verify_user("u750", "newpw"), Some(Role::ReadOnly));
    // The SCRAM verifier re-derived too (pgwire SASL reads it).
    assert!(e.user_scram("u750").is_some(), "scram must survive rotation");
    // The ALTER ROLE spelling and the no-WITH form work the same.
    e.execute("ALTER ROLE u750 PASSWORD 'thirdpw'").unwrap();
    assert_eq!(e.verify_user("u750", "newpw"), None);
    assert_eq!(e.verify_user("u750", "thirdpw"), Some(Role::ReadOnly));
}

#[test]
fn round750_password_null_clears_the_credential() {
    let mut e = Engine::new();
    e.execute("CREATE USER u750 WITH PASSWORD 'pw'").unwrap();
    e.execute("ALTER ROLE u750 PASSWORD NULL").unwrap();
    assert_eq!(e.verify_user("u750", "pw"), None);
    assert_eq!(e.verify_user("u750", ""), None);
    assert!(e.user_scram("u750").is_none(), "NULL clears the verifier");
}

#[test]
fn round750_missing_role_refuses_and_other_attrs_still_no_op() {
    let mut e = Engine::new();
    let err = format!(
        "{}",
        e.execute("ALTER USER nosuch750 PASSWORD 'x'")
            .expect_err("PG refuses a missing role")
    );
    assert!(err.contains("role \"nosuch750\" does not exist"), "{err}");
    // Attribute-only ALTERs stay validated no-ops (the r708 behaviour).
    e.execute("CREATE USER u750 WITH PASSWORD 'pw'").unwrap();
    e.execute("ALTER USER u750 WITH CONNECTION LIMIT 5").unwrap();
    assert_eq!(
        e.verify_user("u750", "pw"),
        Some(Role::ReadOnly),
        "a non-password ALTER must not disturb the credential"
    );
    // And PASSWORD mixed among other attributes is still caught.
    e.execute("ALTER USER u750 WITH NOSUPERUSER PASSWORD 'mix' CONNECTION LIMIT 5")
        .unwrap();
    assert_eq!(e.verify_user("u750", "mix"), Some(Role::ReadOnly));
    assert_eq!(e.verify_user("u750", "pw"), None);
}
