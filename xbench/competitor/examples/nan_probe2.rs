//! Float -0/NaN aggregate-DISTINCT / GROUP BY differential probe over a
//! real table (the VALUES form takes a different executor path).
//! PG18 anchor: count(DISTINCT x) = 4 over {NaN,1,NaN,2,-0,0}.

fn main() {
    use spg_engine::Engine;
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE fx (x DOUBLE PRECISION)").unwrap();
    eng.execute("INSERT INTO fx VALUES ('NaN'::float8),(1.0),('NaN'),(2.0),('-0'::float8),(0.0)")
        .unwrap();
    eng.execute("CREATE TABLE fn2 (x DOUBLE PRECISION)").unwrap();
    eng.execute("INSERT INTO fn2 VALUES (1.0),('NaN'::float8),(NULL),(2.0)")
        .unwrap();
    for sql in [
        "SELECT count(DISTINCT x) FROM fx",
        "SELECT DISTINCT x FROM fx ORDER BY x",
        "SELECT x, count(*) FROM fx GROUP BY x ORDER BY x",
        "SELECT count(DISTINCT x) FROM (VALUES ('NaN'::float8),(1.0),('NaN'),(2.0),('-0'::float8),(0.0)) t(x)",
        // PG anchors: ASC → 1,2,NaN,NULL · DESC → NULL,NaN,2,1
        // ASC NULLS FIRST → NULL,1,2,NaN · DESC NULLS LAST → NaN,2,1,NULL
        "SELECT x FROM fn2 ORDER BY x",
        "SELECT x FROM fn2 ORDER BY x DESC",
        "SELECT x FROM fn2 ORDER BY x NULLS FIRST",
        "SELECT x FROM fn2 ORDER BY x DESC NULLS LAST",
    ] {
        println!("== {sql}");
        match eng.execute(sql) {
            Ok(r) => println!("{r:?}"),
            Err(e) => println!("ERR {e:?}"),
        }
    }
}
