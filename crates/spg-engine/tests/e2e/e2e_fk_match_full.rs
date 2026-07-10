//! v7.38 (read01, T29) — FK MATCH FULL: the referential check is skipped only
//! when ALL referencing columns are NULL; a mixed-NULL key errors. MATCH SIMPLE
//! (default) skips on ANY NULL; MATCH PARTIAL is rejected at parse time.
//! Oracle: live PG 18.4.

use spg_engine::Engine;

#[test]
fn fk_match_full() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p(a int, b int, PRIMARY KEY(a,b))")
        .unwrap();
    e.execute("INSERT INTO p VALUES (1,2)").unwrap();
    e.execute("CREATE TABLE cf(x int, y int, FOREIGN KEY(x,y) REFERENCES p(a,b) MATCH FULL)")
        .unwrap();
    // All-NULL and a real match are allowed; a mixed-NULL key errors.
    e.execute("INSERT INTO cf VALUES (NULL, NULL)").unwrap();
    e.execute("INSERT INTO cf VALUES (1, 2)").unwrap();
    assert!(e.execute("INSERT INTO cf VALUES (1, NULL)").is_err());
    // No parent → error even for a full key.
    assert!(e.execute("INSERT INTO cf VALUES (9, 9)").is_err());

    // MATCH SIMPLE (default) skips the check on ANY NULL.
    e.execute("CREATE TABLE cs(x int, y int, FOREIGN KEY(x,y) REFERENCES p(a,b))")
        .unwrap();
    e.execute("INSERT INTO cs VALUES (1, NULL)").unwrap();

    // MATCH PARTIAL is rejected at parse time (PG does not implement it either).
    assert!(
        e.execute(
            "CREATE TABLE cp(x int, y int, FOREIGN KEY(x,y) REFERENCES p(a,b) MATCH PARTIAL)"
        )
        .is_err()
    );
}
