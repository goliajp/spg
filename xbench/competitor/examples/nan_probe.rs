fn main() {
    use spg_engine::Engine;
    let mut eng = Engine::new();
    for (label, sql) in [
        (
            "order",
            "SELECT x::text FROM (VALUES ('NaN'::float8),(1.0),('NaN'),(2.0),('-0'::float8),(0.0)) t(x) ORDER BY x",
        ),
        (
            "distinct",
            "SELECT DISTINCT x::text FROM (VALUES ('NaN'::float8),(1.0),('NaN'),(2.0)) t(x) ORDER BY 1",
        ),
        (
            "cnt",
            "SELECT count(DISTINCT x) FROM (VALUES ('NaN'::float8),(1.0),('NaN'),(2.0),('-0'::float8),(0.0)) t(x)",
        ),
    ] {
        println!("== {label}");
        match eng.execute(sql) {
            Ok(r) => println!("{r:?}"),
            Err(e) => println!("ERR {e:?}"),
        }
    }
}
