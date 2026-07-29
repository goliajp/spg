//! v7.39 (round 621) — a partition-key UPDATE moves the row, and ATTACH scans
//! instead of demanding emptiness.
//!
//! Both were honest refusals: the key-touching parent UPDATE because fanning
//! it out in place leaves a row in the wrong partition (silent-wrong), the
//! non-empty ATTACH because the bound was never checked against existing rows.
//! Honest, but each refused the ordinary thing — `UPDATE pm SET v = 120` and
//! "build a table, load it, attach it", which is the migration partitioning
//! is adopted FOR.
//!
//! Row movement is capture + reinsert through machinery that already existed:
//! each child's victims are taken with `DELETE … RETURNING *` (one statement,
//! so the pre-image and the removal cannot disagree), the SET list is
//! evaluated against the PRE-image (PG's rule — `SET v = v + 100` reads the
//! old v), and the post-images go back through the parent's own INSERT
//! routing. ALL children are captured before ANY reinsert: the first cut
//! interleaved them, and a child processed later saw the rows an earlier
//! child had just routed into it — deleted them again, and RETURNING reported
//! the row twice. A key no partition takes raises PG's exact wording, and the
//! pre-images go back before the error leaves, so a failed statement does not
//! eat rows.
//!
//! The ATTACH scan checks every visible row's key against the new bound —
//! range, list and hash — BEFORE the role is installed, so a failed attach
//! changes nothing and the child stays a standalone table.
//!
//! The whole probe set matches live PG18 byte for byte (measured on a clean
//! database — the shared oracle's publications refuse UPDATEs outright, which
//! round 621 learned the hard way). Three legacy pins asserting the refusals
//! were updated to assert the behaviours.
//!
//! Measured and NOT closed (F26): `tableoid::regclass` on a partition parent
//! reports the internal `__spg_partition_pm` for every row where PG names the
//! child each row lives in — an internal name leaking through a catalog cast.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE pm (id INT, v INT) PARTITION BY RANGE (v)").unwrap();
    e.execute("CREATE TABLE pm1 PARTITION OF pm FOR VALUES FROM (0) TO (100)").unwrap();
    e.execute("CREATE TABLE pm2 PARTITION OF pm FOR VALUES FROM (100) TO (200)").unwrap();
    e.execute("INSERT INTO pm VALUES (1, 50), (2, 150)").unwrap();
    e
}

/// The movement itself, and where the row physically lands.
#[test]
fn round621_the_row_moves() {
    let mut e = seed();
    e.execute("UPDATE pm SET v = 120 WHERE id = 1").unwrap();
    assert_eq!(vals(&mut e, "SELECT id, v FROM pm ORDER BY id"), vec!["1|120", "2|150"]);
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM pm1"),
        vec!["0"],
        "the row LEFT the source partition"
    );
    assert_eq!(vals(&mut e, "SELECT count(*) FROM pm2"), vec!["2"]);
    assert_eq!(
        vals(&mut e, "SELECT id FROM pm2 ORDER BY id"),
        vec!["1", "2"]
    );
}

/// The SET list reads the PRE-image, and RETURNING reports each moved row
/// once.
#[test]
fn round621_pre_image_and_returning() {
    let mut e = seed();
    e.execute("UPDATE pm SET v = v - 100 WHERE id = 2").unwrap();
    assert_eq!(
        vals(&mut e, "SELECT id, v FROM pm ORDER BY id"),
        vec!["1|50", "2|50"],
        "v - 100 read the OLD v (150), not some half-updated state"
    );
    assert_eq!(
        vals(&mut e, "UPDATE pm SET v = 130 WHERE id = 2 RETURNING id, v"),
        vec!["2|130"],
        "ONE row back — the first cut reported it twice, because the \
         destination child re-processed what had just moved into it"
    );
}

/// The failure contract: nothing is eaten.
#[test]
fn round621_a_key_no_partition_takes() {
    let mut e = seed();
    let err = e
        .execute("UPDATE pm SET v = 999 WHERE id = 1")
        .expect_err("999 fits no partition");
    assert!(
        format!("{err}").contains("no partition of relation"),
        "PG's wording: {err}"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, v FROM pm ORDER BY id"),
        vec!["1|50", "2|150"],
        "the failed statement left both rows exactly where they were"
    );
}

/// ATTACH: the scan, in both verdicts.
#[test]
fn round621_attach_scans_the_rows() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE pa (id INT, v INT) PARTITION BY RANGE (v)").unwrap();
    e.execute("CREATE TABLE pax (id INT, v INT)").unwrap();
    e.execute("INSERT INTO pax VALUES (9, 250)").unwrap();
    e.execute("ALTER TABLE pa ATTACH PARTITION pax FOR VALUES FROM (200) TO (300)")
        .expect("a loaded child whose rows all fit attaches");
    assert_eq!(vals(&mut e, "SELECT count(*) FROM pa"), vec!["1"]);
    e.execute("CREATE TABLE pay (id INT, v INT)").unwrap();
    e.execute("INSERT INTO pay VALUES (8, 999)").unwrap();
    let err = e
        .execute("ALTER TABLE pa ATTACH PARTITION pay FOR VALUES FROM (300) TO (400)")
        .expect_err("999 violates the bound");
    assert!(
        format!("{err}").contains("partition constraint of relation"),
        "PG's wording: {err}"
    );
    e.execute("INSERT INTO pay VALUES (5, 5)")
        .expect("the failed attach installed no role, so pay is still standalone");
}
