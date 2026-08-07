//! v7.39 (round 228, EXPLAIN epic follow-up) — FORMAT XML and FORMAT
//! YAML render the real plan tree, matching live PG18.4's element names,
//! nesting and indentation (r228 probe against PG 18.4). Before this
//! round both formats wrapped the *text* lines (`<line>…</line>` /
//! `- "…"`), which no PG-plan parser accepts. FORMAT JSON gains PG's
//! pretty-printing and the `Async Capable` / `Partial Mode` /
//! `Scan Direction` keys the r226 version was missing.

use spg_engine::{Engine, QueryResult};

fn body(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE r228 (id int PRIMARY KEY, v int, s text)")
        .unwrap();
    e.execute("INSERT INTO r228 VALUES (1,2,'a'),(2,4,'b'),(3,6,'c'),(4,4,'d')")
        .unwrap();
    e
}

#[test]
fn yaml_matches_pg_tree_shape() {
    let mut e = seeded();
    // Byte-identical to live PG18.4 for this query (r228 probe), including
    // PG's trailing space after the `Plan:` / `Group Key:` / `Plans:` keys.
    assert_eq!(
        body(
            &mut e,
            "EXPLAIN (FORMAT YAML, COSTS OFF) SELECT v, count(*) FROM r228 GROUP BY v"
        ),
        "- Plan: \n\
         \x20   Node Type: \"Aggregate\"\n\
         \x20   Strategy: \"Hashed\"\n\
         \x20   Partial Mode: \"Simple\"\n\
         \x20   Parallel Aware: false\n\
         \x20   Async Capable: false\n\
         \x20   Disabled: false\n\
         \x20   Group Key: \n\
         \x20     - \"v\"\n\
         \x20   Plans: \n\
         \x20     - Node Type: \"Seq Scan\"\n\
         \x20       Parent Relationship: \"Outer\"\n\
         \x20       Parallel Aware: false\n\
         \x20       Async Capable: false\n\
         \x20       Relation Name: \"r228\"\n\
         \x20       Alias: \"r228\"\n\
         \x20       Disabled: false\n"
    );
}

#[test]
fn xml_matches_pg_tree_shape() {
    let mut e = seeded();
    // Byte-identical to live PG18.4 (r228 probe): PG namespace, one
    // <Query>, hyphenated element names, <Item> sequence members.
    assert_eq!(
        body(
            &mut e,
            "EXPLAIN (FORMAT XML, COSTS OFF) SELECT * FROM r228 ORDER BY v"
        ),
        "<explain xmlns=\"http://www.postgresql.org/2009/explain\">\n\
         \x20 <Query>\n\
         \x20   <Plan>\n\
         \x20     <Node-Type>Sort</Node-Type>\n\
         \x20     <Parallel-Aware>false</Parallel-Aware>\n\
         \x20     <Async-Capable>false</Async-Capable>\n\
         \x20     <Disabled>false</Disabled>\n\
         \x20     <Sort-Key>\n\
         \x20       <Item>v</Item>\n\
         \x20     </Sort-Key>\n\
         \x20     <Plans>\n\
         \x20       <Plan>\n\
         \x20         <Node-Type>Seq Scan</Node-Type>\n\
         \x20         <Parent-Relationship>Outer</Parent-Relationship>\n\
         \x20         <Parallel-Aware>false</Parallel-Aware>\n\
         \x20         <Async-Capable>false</Async-Capable>\n\
         \x20         <Relation-Name>r228</Relation-Name>\n\
         \x20         <Alias>r228</Alias>\n\
         \x20         <Disabled>false</Disabled>\n\
         \x20       </Plan>\n\
         \x20     </Plans>\n\
         \x20   </Plan>\n\
         \x20 </Query>\n\
         </explain>"
    );
}

#[test]
fn json_is_pretty_printed_like_pg() {
    let mut e = seeded();
    // Byte-identical to live PG18.4 (r228 probe).
    assert_eq!(
        body(
            &mut e,
            "EXPLAIN (FORMAT JSON, COSTS OFF) SELECT v, count(*) FROM r228 GROUP BY v"
        ),
        "[\n\
         \x20 {\n\
         \x20   \"Plan\": {\n\
         \x20     \"Node Type\": \"Aggregate\",\n\
         \x20     \"Strategy\": \"Hashed\",\n\
         \x20     \"Partial Mode\": \"Simple\",\n\
         \x20     \"Parallel Aware\": false,\n\
         \x20     \"Async Capable\": false,\n\
         \x20     \"Disabled\": false,\n\
         \x20     \"Group Key\": [\"v\"],\n\
         \x20     \"Plans\": [\n\
         \x20       {\n\
         \x20         \"Node Type\": \"Seq Scan\",\n\
         \x20         \"Parent Relationship\": \"Outer\",\n\
         \x20         \"Parallel Aware\": false,\n\
         \x20         \"Async Capable\": false,\n\
         \x20         \"Relation Name\": \"r228\",\n\
         \x20         \"Alias\": \"r228\",\n\
         \x20         \"Disabled\": false\n\
         \x20       }\n\
         \x20     ]\n\
         \x20   }\n\
         \x20 }\n\
         ]"
    );
}

#[test]
fn index_scan_carries_pg_scan_keys() {
    let mut e = seeded();
    // PG spells an index descent Scan Direction "Forward" + Index Name,
    // and puts the cost keys before Disabled.
    let y = body(
        &mut e,
        "EXPLAIN (FORMAT YAML) SELECT * FROM r228 WHERE id = 2",
    );
    for want in [
        "Node Type: \"Index Scan\"",
        "Scan Direction: \"Forward\"",
        "Index Name: \"r228_pkey\"",
        "Index Cond: \"(id = 2)\"",
    ] {
        assert!(y.contains(want), "missing {want}: {y}");
    }
    assert!(y.contains("Startup Cost: "), "costs on by default: {y}");
    assert!(
        y.find("Total Cost: ").unwrap() < y.find("Disabled: ").unwrap(),
        "PG orders costs before Disabled: {y}"
    );
}

#[test]
fn analyze_actuals_reach_the_structured_formats() {
    let mut e = seeded();
    // The r227 actuals are genuine measurements; they must show up under
    // PG's Actual Rows / Actual Loops keys too, not only in the text tree.
    let j = body(
        &mut e,
        "EXPLAIN (ANALYZE, FORMAT JSON, COSTS OFF, TIMING OFF, SUMMARY OFF) \
         SELECT * FROM r228 WHERE v = 4",
    );
    assert!(j.contains("\"Actual Rows\": 2.00"), "{j}");
    assert!(j.contains("\"Actual Loops\": 1"), "{j}");
    assert!(j.contains("\"Rows Removed by Filter\": 2"), "{j}");
    // No clock injected → no invented time keys (r227's honesty rule).
    assert!(!j.contains("Actual Total Time"), "{j}");
}
