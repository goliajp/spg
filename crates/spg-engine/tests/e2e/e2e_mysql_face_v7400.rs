//! v7.40.0 — what `xtests/mysqlcorpus` found, pinned where it can be
//! reached without a wire.
//!
//! The corpus is the instrument; these are the fences. Every expected
//! value was read off MySQL 9.7.2 running the same statement.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// ```text
///   MySQL 9.7.2   CREATE TABLE t (id BIGINT UNSIGNED AUTO_INCREMENT …)  accepted
///   SPG 7.39.13   ERROR: AUTO_INCREMENT applies to integer columns only
/// ```
///
/// SPG models `BIGINT UNSIGNED` as a scale-0 NUMERIC, because its range
/// does not fit an i64 — so the check that asked for an integer type
/// refused a column that is one.
#[test]
fn auto_increment_works_on_an_unsigned_bigint() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ai (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, s TEXT)")
        .unwrap();
    e.execute("INSERT INTO ai (s) VALUES ('x')").unwrap();
    e.execute("INSERT INTO ai (s) VALUES ('y')").unwrap();
    assert_eq!(rows(&mut e, "SELECT id, s FROM ai ORDER BY id"), ["1|x", "2|y"]);
}

/// MySQL's `CREATE TABLE b LIKE a` is the copy PostgreSQL spells
/// `CREATE TABLE b (LIKE a INCLUDING ALL)`. Measured on 9.7.2, the copy
/// takes the columns, their defaults, the PRIMARY KEY and the indexes
/// UNDER THEIR OWN NAMES, and takes neither the rows nor the foreign
/// keys.
#[test]
fn create_table_like_copies_the_shape_and_not_the_rows() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (id INT NOT NULL PRIMARY KEY, s VARCHAR(8) DEFAULT 'z', KEY ks (s))")
        .unwrap();
    e.execute("INSERT INTO a VALUES (1,'q')").unwrap();
    e.execute("CREATE TABLE b LIKE a").unwrap();
    assert_eq!(rows(&mut e, "SELECT COUNT(*) FROM b"), ["0"]);
    let ddl = rows(&mut e, "SHOW CREATE TABLE b").join("\n");
    assert!(ddl.contains("PRIMARY KEY (`id`)"), "{ddl}");
    assert!(ddl.contains("KEY `ks` (`s`)"), "{ddl}");
    // A PostgreSQL cast suffix is not part of a MySQL default: 9.7.2
    // writes `DEFAULT 'z'`, and SPG carried the catalog's stored source
    // text — `'z'::character varying` — into DDL MySQL cannot parse.
    assert!(ddl.contains("DEFAULT 'z'"), "{ddl}");
    assert!(!ddl.contains("::"), "a PostgreSQL cast reached the DDL: {ddl}");
}

/// PostgreSQL 18.6 renames a `LIKE` copy's indexes after the new table;
/// MySQL keeps the source's names. Both measured. The spelling decides.
#[test]
fn the_postgres_spelling_of_like_still_renames_its_copies() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (id INT PRIMARY KEY, s VARCHAR(8))")
        .unwrap();
    e.execute("CREATE INDEX ks ON a(s)").unwrap();
    e.execute("CREATE TABLE b (LIKE a INCLUDING ALL)").unwrap();
    let names = rows(
        &mut e,
        "SELECT indexname FROM pg_indexes WHERE tablename = 'b' ORDER BY 1",
    );
    assert_eq!(names, ["b_pkey", "b_s_idx"], "PostgreSQL renames the copies");
}

/// The composite key on the MySQL surface — the defect v7.39.10 and
/// v7.39.12 each closed for a different shape.
#[test]
fn a_composite_primary_key_is_one_index_named_primary() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE mc (a INT NOT NULL, b VARCHAR(32) NOT NULL, c INT, PRIMARY KEY (a,b), KEY kc (c))")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT index_name, seq_in_index, column_name, non_unique \
             FROM information_schema.statistics WHERE table_name = 'mc'"
        ),
        ["PRIMARY|1|a|0", "PRIMARY|2|b|0", "kc|1|c|1"]
    );
    let show = rows(&mut e, "SHOW INDEX FROM mc");
    assert_eq!(show.len(), 3, "{show:?}");
    assert!(show[0].starts_with("mc|0|PRIMARY|1|a|"), "{show:?}");
    assert!(show[1].starts_with("mc|0|PRIMARY|2|b|"), "{show:?}");
    assert!(show[2].starts_with("mc|1|kc|1|c|"), "{show:?}");
    // And SHOW CREATE TABLE must not print the two internal indexes SPG
    // builds to back the key.
    let ddl = rows(&mut e, "SHOW CREATE TABLE mc").join("\n");
    assert!(ddl.contains("PRIMARY KEY (`a`,`b`)"), "{ddl}");
    assert!(!ddl.contains("pkey_0_"), "an internal index reached the DDL: {ddl}");
}

/// Measured on MySQL 9.7.2, the whole options tail and the two column
/// lists that spell their commas differently.
#[test]
fn show_create_table_matches_mysqls_own_spelling() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (a INT NOT NULL, b INT NOT NULL, PRIMARY KEY (a,b))")
        .unwrap();
    e.execute(
        "CREATE TABLE c (x INT NOT NULL, y INT NOT NULL, z INT, \
         CONSTRAINT uq UNIQUE (x,y), KEY kz (z), FOREIGN KEY (x,y) REFERENCES p(a,b))",
    )
    .unwrap();
    let ddl = rows(&mut e, "SHOW CREATE TABLE c").join("\n");
    assert!(ddl.contains("UNIQUE KEY `uq` (`x`,`y`)"), "{ddl}");
    assert!(ddl.contains("KEY `kz` (`z`)"), "{ddl}");
    assert!(
        ddl.contains("CONSTRAINT `c_ibfk_1` FOREIGN KEY (`x`, `y`) REFERENCES `p` (`a`, `b`)"),
        "a foreign key is named, and ITS list keeps the spaces: {ddl}"
    );
    assert!(
        ddl.ends_with("ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"),
        "{ddl}"
    );
}

/// `ISNULL(expr)` takes one argument and returns an integer.
#[test]
fn isnull_is_a_function_that_returns_an_integer() {
    let mut e = Engine::new();
    assert_eq!(
        rows(&mut e, "SELECT ISNULL(NULL), ISNULL(0), ISNULL('')"),
        ["1|0|0"]
    );
}

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e
}

/// A `_bin` collation is byte-wise AND `PAD SPACE`; `BINARY` is neither.
/// SPG lowered `COLLATE utf8mb4_bin` onto a `CAST(… AS BINARY)`, which
/// carries both properties, so the pad went with the fold.
///
/// ```text
///                                        MySQL 9.7.2   SPG 7.39.13
///   'a ' = 'a' COLLATE utf8mb4_bin            1             0
///   'a ' = 'a' COLLATE utf8mb4_0900_bin       0             0
///   'AB' = 'ab' COLLATE utf8mb4_bin           0             0
///   'B' COLLATE utf8mb4_bin < 'a'             1             1
/// ```
#[test]
fn a_bin_collation_does_not_fold_but_it_pads() {
    let mut e = mysql();
    assert_eq!(
        rows(
            &mut e,
            "SELECT 'a ' = 'a' COLLATE utf8mb4_bin, \
                    'a ' = 'a' COLLATE utf8mb4_0900_bin, \
                    'AB' = 'ab' COLLATE utf8mb4_bin, \
                    'B' COLLATE utf8mb4_bin < 'a', \
                    'a ' = 'a' COLLATE utf8mb4_general_ci, \
                    'a ' = 'a'"
        ),
        ["true|false|false|true|true|false"]
    );
}

/// MySQL's `ANY_VALUE` is not an aggregate: it returns its argument and
/// suppresses the grouping check. PostgreSQL 16+ has one that IS an
/// aggregate. Both measured, over the same three rows.
#[test]
fn any_value_aggregates_on_one_engine_and_not_the_other() {
    for (mysql_dialect, bare, both) in [
        (false, vec!["10"], vec!["10|3"]),
        (true, vec!["10", "20", "30"], vec!["10|3"]),
    ] {
        let mut e = Engine::new();
        e.set_mysql_dialect(mysql_dialect);
        e.execute("CREATE TABLE t (g INT, v INT)").unwrap();
        e.execute("INSERT INTO t VALUES (1,10),(1,20),(2,30)")
            .unwrap();
        assert_eq!(rows(&mut e, "SELECT ANY_VALUE(v) FROM t"), bare);
        // With another aggregate beside it, or a GROUP BY, both engines
        // aggregate.
        assert_eq!(rows(&mut e, "SELECT ANY_VALUE(v), COUNT(*) FROM t"), both);
        assert_eq!(
            rows(&mut e, "SELECT g, ANY_VALUE(v) FROM t GROUP BY g ORDER BY g"),
            ["1|10", "2|30"]
        );
    }
}

/// `KEY kb (b(4))` was accepted and then dropped twice over: the length
/// was skipped by the parser, and the index itself was skipped because
/// the composite key had already put a B-tree on that column. So
/// `SHOW INDEX` did not list it and `DROP INDEX kb` answered
/// `ERROR 1091 Can't DROP 'kb'`.
#[test]
fn a_declared_index_survives_a_key_on_the_same_column() {
    let mut e = mysql();
    e.execute(
        "CREATE TABLE mc (a INT NOT NULL, b VARCHAR(32) NOT NULL, c INT, \
         PRIMARY KEY (a,b), KEY kb (b(4)), KEY kc (c))",
    )
    .unwrap();
    let show = rows(&mut e, "SHOW INDEX FROM mc");
    assert_eq!(show.len(), 4, "{show:?}");
    // Sub_part is the seventh column after Table: the declared prefix.
    assert!(show[2].starts_with("mc|1|kb|1|b|A|0|4|"), "{show:?}");
    let ddl = rows(&mut e, "SHOW CREATE TABLE mc").join("\n");
    assert!(ddl.contains("KEY `kb` (`b`(4))"), "{ddl}");
    e.execute("DROP INDEX kb ON mc").unwrap();
}

/// A prefixed UNIQUE key is a DIFFERENT constraint, and the difference
/// is observable: MySQL 9.7.2 rejects the second row below. SPG enforces
/// it as a unique expression index over `left(b, 4)`, which is that
/// rule; the error text is MySQL's own, measured.
#[test]
fn a_prefixed_unique_key_rejects_a_shared_prefix() {
    let mut e = mysql();
    e.execute("CREATE TABLE u (id INT, b VARCHAR(32), UNIQUE KEY uq (b(4)))")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1,'abcdef')").unwrap();
    let err = e
        .execute("INSERT INTO u VALUES (2,'abcdzz')")
        .expect_err("the prefix collides");
    assert!(
        format!("{err}").contains("Duplicate entry 'abcd' for key 'u.uq'"),
        "{err}"
    );
    e.execute("INSERT INTO u VALUES (3,'zzzz')").unwrap();
    let ddl = rows(&mut e, "SHOW CREATE TABLE u").join("\n");
    assert!(ddl.contains("UNIQUE KEY `uq` (`b`(4))"), "{ddl}");
}

/// `information_schema.table_constraints` answers the engine the session
/// is speaking to. PostgreSQL 18.6 lists a NOT NULL column as a CHECK
/// constraint and names the key `<table>_pkey`; MySQL 9.7.2 has neither.
/// Both measured.
#[test]
fn table_constraints_answers_the_engine_you_are_speaking_to() {
    let q = "SELECT constraint_name, constraint_type \
             FROM information_schema.table_constraints \
             WHERE table_name = 'mc' ORDER BY 1";
    let ddl = "CREATE TABLE mc (a INT NOT NULL, b VARCHAR(32) NOT NULL, PRIMARY KEY (a,b))";

    let mut pg = Engine::new();
    pg.execute(ddl).unwrap();
    assert_eq!(
        rows(&mut pg, q),
        ["mc_a_not_null|CHECK", "mc_b_not_null|CHECK", "mc_pkey|PRIMARY KEY"]
    );

    let mut my = mysql();
    my.execute(ddl).unwrap();
    assert_eq!(rows(&mut my, q), ["PRIMARY|PRIMARY KEY"]);
}

/// The scalar answers the corpus found, each measured on MySQL 9.7.2.
#[test]
fn the_scalar_answers_the_corpus_found() {
    let mut e = mysql();
    // A numeric too wide for the i128 fast path had no unary-minus arm,
    // so `SELECT -1e308` failed with "operator does not exist: - numeric"
    // — on BOTH faces. PostgreSQL 18.6 answers it.
    assert_eq!(rows(&mut e, "SELECT (-1e308 * 10) IS NULL"), ["false"]);
    // A timestamp literal gives its time of day. Both engines.
    assert_eq!(rows(&mut e, "SELECT TIME('2020-01-02 03:04:05')"), ["03:04:05"]);
    // MySQL's CAST to a temporal type answers NULL for a value that is
    // not a date; the same value in an INSERT still raises.
    assert_eq!(rows(&mut e, "SELECT CAST('2020-99-99' AS DATE)"), ["NULL"]);
    // `0b101` is a binary string there, not the number five.
    assert_eq!(rows(&mut e, "SELECT 0b101 = X'05'"), ["true"]);
    // The collation-derivation ranks, measured: literal 4, number 5,
    // NULL 6, system constant 3, explicit COLLATE 0.
    assert_eq!(
        rows(
            &mut e,
            "SELECT COERCIBILITY('x'), COERCIBILITY(1), COERCIBILITY(NULL), \
                    COERCIBILITY(VERSION()), COERCIBILITY('x' COLLATE utf8mb4_bin)"
        ),
        ["4|5|6|3|0"]
    );
    // `STD` is the population standard deviation, `STDDEV_SAMP` the
    // sample one, and MySQL's bare `VARIANCE` is the population
    // variance — answered as a DOUBLE, where PostgreSQL answers a
    // NUMERIC and renders `1.2500000000000000`. Every one of these four
    // is byte for byte what MySQL 9.7.2 printed for the same rows.
    assert_eq!(
        rows(
            &mut e,
            "SELECT STD(x), STDDEV_SAMP(x), VARIANCE(x), AVG(x) FROM \
             (SELECT 0 AS x UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3) t"
        ),
        ["1.118033988749895|1.2909944487358056|1.25|1.5000"]
    );
    // `JSON_PRETTY` indents two spaces; PostgreSQL's `jsonb_pretty`
    // indents four.
    assert_eq!(
        rows(&mut e, "SELECT JSON_PRETTY('{\"a\":1}')"),
        ["{\n  \"a\": 1\n}"]
    );
}

/// A rollup's grand total over the column it groups by.
///
/// ```text
///   SELECT qty, SUM(qty) … GROUP BY ROLLUP(qty)   total row
///     PostgreSQL 18.6   NULL | 6
///     MySQL 9.7.2       NULL | 6
///     SPG 7.39.13       NULL | NULL
/// ```
///
/// A grouping column is NULL in the OUTPUT of a set that drops it, and
/// an aggregate over it still aggregates the real values. The rewrite
/// that nullifies a dropped key "at any depth" was right about
/// `COALESCE(g,'TOTAL')` and wrong about `SUM(g)` — it replaced the
/// aggregate's own input with a NULL literal. Wrong on both faces.
#[test]
fn a_rollup_totals_the_column_it_groups_by() {
    for mysql_dialect in [false, true] {
        let mut e = Engine::new();
        e.set_mysql_dialect(mysql_dialect);
        e.execute("CREATE TABLE r (id INT, name VARCHAR(8), qty INT)")
            .unwrap();
        e.execute("INSERT INTO r VALUES (1,'a',1),(2,'b',2),(3,'c',3),(4,'',0)")
            .unwrap();
        assert_eq!(
            rows(&mut e, "SELECT qty, SUM(qty) FROM r GROUP BY ROLLUP(qty)")
                .last()
                .unwrap(),
            "NULL|6"
        );
        // A dropped key is still NULL where it is SELECTED, and
        // `grouping()` still tells the rollup's NULL from a data NULL.
        assert_eq!(
            rows(
                &mut e,
                "SELECT COALESCE(name,'TOTAL'), GROUPING(name), SUM(qty) \
                 FROM r GROUP BY ROLLUP(name)"
            )
            .last()
            .unwrap(),
            "TOTAL|1|6"
        );
    }
}

/// MySQL orders a `WITH ROLLUP` by its keys whether or not they are
/// selected. A UNION's ORDER BY can only name output columns, so the
/// synthesised order named a column that was not there and the query
/// answered `column "qty" does not exist`. MySQL 9.7.2: `0 1 2 3 6`.
#[test]
fn a_rollup_orders_by_a_key_it_does_not_select() {
    let mut e = mysql();
    e.execute("CREATE TABLE r (qty INT)").unwrap();
    e.execute("INSERT INTO r VALUES (1),(2),(3),(0)").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT SUM(qty) FROM r GROUP BY qty WITH ROLLUP"),
        ["0", "1", "2", "3", "6"]
    );
}

/// `AUTO_INCREMENT=100` is the next value the table hands out, and
/// `COLLATION()` names the session's collation. Both measured on 9.7.2;
/// SPG dropped the first and answered MariaDB's default for the second.
#[test]
fn the_table_option_and_the_collation_name() {
    let mut e = mysql();
    e.execute(
        "CREATE TABLE ai (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, s TEXT) \
         ENGINE=InnoDB AUTO_INCREMENT=100",
    )
    .unwrap();
    e.execute("INSERT INTO ai (s) VALUES ('x')").unwrap();
    assert_eq!(rows(&mut e, "SELECT id FROM ai"), ["100"]);
    assert_eq!(
        rows(&mut e, "SELECT COLLATION('x'), CHARSET('x')"),
        ["utf8mb4_0900_ai_ci|utf8mb4"]
    );
}
