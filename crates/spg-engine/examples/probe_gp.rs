fn main() {
    let mut e = spg_engine::Engine::new();
    e.set_backslash_escapes(true);
    e.execute("CREATE TABLE t (a VARCHAR(8) COLLATE utf8mb4_0900_ai_ci, b VARCHAR(8) COLLATE utf8mb4_bin, c VARCHAR(8) COLLATE utf8mb4_general_ci, d VARCHAR(8) COLLATE utf8mb4_unicode_ci, e VARCHAR(8))").unwrap();
    let t = e.catalog().get("t").unwrap();
    for c in &t.schema().columns {
        println!(
            "{:<3} collation={:?}  name={:?}",
            c.name, c.collation, c.collation_name
        );
    }
}
