//! v7.37.7 B.4 — LATERAL probe beyond jsonb_each_text.
//! Tests bare `CROSS JOIN LATERAL (SELECT ...)` and `LEFT JOIN
//! LATERAL` correlation against a regular SELECT source.

use spg_engine::Engine;

#[test]
fn probe_cross_join_lateral_subquery() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE outer_t (id INT NOT NULL, n INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO outer_t VALUES (1, 2), (2, 3)")
        .unwrap();
    e.execute("CREATE TABLE inner_t (key INT NOT NULL, val INT NOT NULL)")
        .unwrap();
    e.execute(
        "INSERT INTO inner_t VALUES (1, 100), (1, 101), (1, 102), (2, 200), (2, 201), (2, 202)",
    )
    .unwrap();
    let r = e.execute(
        "SELECT outer_t.id, x.val \
         FROM outer_t CROSS JOIN LATERAL (\
             SELECT inner_t.val FROM inner_t \
             WHERE inner_t.key = outer_t.id LIMIT 1\
         ) AS x ORDER BY outer_t.id",
    );
    eprintln!("CROSS JOIN LATERAL (SELECT ...): {r:?}");
}

#[test]
fn probe_left_join_lateral_subquery() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO a VALUES (1), (2), (3)").unwrap();
    e.execute("CREATE TABLE b (k INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO b VALUES (1, 10), (2, 20)").unwrap();
    let r = e.execute(
        "SELECT a.id, sub.v \
         FROM a LEFT JOIN LATERAL (\
             SELECT b.v FROM b WHERE b.k = a.id LIMIT 1\
         ) AS sub ON TRUE ORDER BY a.id",
    );
    eprintln!("LEFT JOIN LATERAL (SELECT ...): {r:?}");
}
