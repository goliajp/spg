//! Round 688 — a declared collation reaches every ordinary sort.
//!
//! Four query shapes, all PG18-verified. Each took a different fix, and the
//! reason they differed is the point of this file: a collation lives outside
//! the DataType lattice, so every schema rebuilt for an intermediate result
//! drops it unless told not to.
//!
//!   * plain scan — the comparator itself (round 683)
//!   * GROUP BY   — plus the collation carried onto the synthetic
//!                  `__grp_j` column (round 686)
//!   * join       — plus `build_combined_schema`'s qualified copy, plus
//!                  `ProjectedItem`, plus a resolver call at the join's own
//!                  sort (round 688)
//!   * DISTINCT   — free once the scan worked
//!
//! Two of those layers already carried `user_enum_type` and `mysql_fsp` for
//! exactly this reason, with doc comments describing the same failure for
//! enum ordering. Collation was the third.
//!
//! Rounds 682 and 685 wired eleven sites between them without first checking
//! whether the failing query reached any of them; none did, and both rounds
//! were reverted. What located each of these was forcing a candidate to
//! reverse, or to panic, and watching the query react.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed(e: &mut Engine) {
    e.execute("CREATE TABLE pa(id INT, loc TEXT COLLATE \"en_US.utf8\", plain TEXT)")
        .unwrap();
    e.execute("CREATE TABLE pb(id INT, tag TEXT)").unwrap();
    e.execute(
        "INSERT INTO pa VALUES (1,'apple','apple'),(2,'Banana','Banana'),\
         (3,'cherry','cherry'),(4,'Ápple','Ápple'),(5,'_under','_under'),(6,'Zebra','Zebra')",
    )
    .unwrap();
    e.execute("INSERT INTO pb VALUES (1,'x'),(2,'x'),(3,'x'),(4,'x'),(5,'x'),(6,'x')")
        .unwrap();
}

/// The order PG18 gives for these six values under en_US.utf8. `Ápple` beside
/// `apple` rather than after `Zebra` is the whole difference from byte order.
const EN_US: &str = "apple,Ápple,Banana,cherry,_under,Zebra";
/// And what byte order gives, which is still right for a column that
/// declares nothing.
const BYTES: &str = "Banana,Zebra,_under,apple,cherry,Ápple";

#[test]
fn round688_every_ordinary_shape_honours_the_collation() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(rows(&mut e, "SELECT loc FROM pa ORDER BY loc"), EN_US, "scan");
    assert_eq!(
        rows(&mut e, "SELECT DISTINCT loc FROM pa ORDER BY loc"),
        EN_US,
        "distinct"
    );
    assert_eq!(
        rows(&mut e, "SELECT loc FROM pa GROUP BY loc ORDER BY loc"),
        EN_US,
        "group by"
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT a.loc FROM pa a JOIN pb b ON a.id = b.id ORDER BY a.loc"
        ),
        EN_US,
        "join"
    );
}

/// A column that declares nothing keeps byte order — the same query, the
/// same data, the other column. Without this the pins above would pass
/// equally well if the collation were applied to everything.
#[test]
fn round688_an_undeclared_column_still_sorts_by_bytes() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(rows(&mut e, "SELECT plain FROM pa ORDER BY plain"), BYTES);
    assert_eq!(
        rows(
            &mut e,
            "SELECT a.plain FROM pa a JOIN pb b ON a.id = b.id ORDER BY a.plain"
        ),
        BYTES
    );
}

/// DESC reverses the collated order rather than falling back to bytes.
#[test]
fn round688_desc_reverses_the_collated_order() {
    let mut e = Engine::new();
    seed(&mut e);
    let asc: Vec<&str> = EN_US.split(',').collect();
    let want: Vec<&str> = asc.iter().rev().copied().collect();
    assert_eq!(
        rows(&mut e, "SELECT loc FROM pa ORDER BY loc DESC"),
        want.join(",")
    );
}
