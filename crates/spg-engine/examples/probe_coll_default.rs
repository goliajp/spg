//! What collation does a text column actually get, per dialect? The
//! comment at `eval/resolve.rs:225` says a MySQL folding default stores
//! `CaseInsensitive` and only a declared `utf8mb4_bin` stores `Binary`.
//! `Collation::Binary` is also the struct's DEFAULT value, which is how
//! a dropped declaration has read as a deliberate one before, so this
//! asks rather than trusts.
use spg_engine::Engine;

fn main() {
    for mysql in [false, true] {
        let mut e = Engine::new();
        e.set_backslash_escapes(mysql);
        e.execute("CREATE TABLE c (a TEXT, b VARCHAR(8), d CHAR(4), f TEXT COLLATE utf8mb4_bin)")
            .unwrap();
        let t = e.catalog().get("c").unwrap();
        let cols: Vec<String> = t
            .schema()
            .columns
            .iter()
            .map(|c| format!("{}={:?}/{:?}", c.name, c.collation, c.collation_name))
            .collect();
        println!("mysql={mysql:<5} {}", cols.join("  "));
    }
}
