//! Float -0/NaN aggregate-DISTINCT / GROUP BY differential probe over a
//! real table (the VALUES form takes a different executor path).
//! PG18 anchor: count(DISTINCT x) = 4 over {NaN,1,NaN,2,-0,0}.

fn main() {
    use spg_engine::Engine;
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE fx (x DOUBLE PRECISION)").unwrap();
    eng.execute("INSERT INTO fx VALUES ('NaN'::float8),(1.0),('NaN'),(2.0),('-0'::float8),(0.0)")
        .unwrap();
    for sql in [
        "SELECT count(DISTINCT x) FROM fx",
        "SELECT DISTINCT x FROM fx ORDER BY x",
        "SELECT x, count(*) FROM fx GROUP BY x ORDER BY x",
        "SELECT count(DISTINCT x) FROM (VALUES ('NaN'::float8),(1.0),('NaN'),(2.0),('-0'::float8),(0.0)) t(x)",
    ] {
        println!("== {sql}");
        match eng.execute(sql) {
            Ok(r) => println!("{r:?}"),
            Err(e) => println!("ERR {e:?}"),
        }
    }
}
