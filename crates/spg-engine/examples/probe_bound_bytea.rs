//! r1038 — sentori's three parser gaps, as a checklist.
use spg_engine::Engine;
fn try_sql(e: &mut Engine, label: &str, sql: &str) {
    match e.execute(sql) {
        Ok(_) => println!("  OK    {label}"),
        Err(er) => {
            let t = format!("{er:?}");
            let t: String = t.chars().take(90).collect();
            println!("  FAIL  {label}\n          {t}");
        }
    }
}
fn main() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a bigint, doc jsonb)").unwrap();
    println!("sentori's three parser gaps:");
    try_sql(
        &mut e,
        "adjacent string literals across lines",
        "COMMENT ON TABLE t IS 'first part '\n'second part'",
    );
    try_sql(
        &mut e,
        "CREATE INDEX ... USING gin (col jsonb_path_ops)",
        "CREATE INDEX t_doc_ix ON t USING gin (doc jsonb_path_ops)",
    );
    try_sql(
        &mut e,
        "CREATE FUNCTION ... RETURNS bigint[]",
        "CREATE FUNCTION f() RETURNS bigint[] LANGUAGE sql AS $$ SELECT ARRAY[1,2] $$",
    );
    println!("\ncontrols that already worked:");
    try_sql(
        &mut e,
        "USING gin (col) with no operator class",
        "CREATE INDEX t_doc_ix2 ON t USING gin (doc)",
    );
    try_sql(&mut e, "array COLUMN type", "CREATE TABLE t2 (a bigint[])");
}
