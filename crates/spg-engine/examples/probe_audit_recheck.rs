//! v7.38.14 Phase A — re-check the three audit items marked silent-wrong
//! on 2026-07-25. A dozen releases have shipped since; a ledger entry is
//! not evidence. PG18 answers substr('abcdef','2') = 'bcdef' and puts a
//! TEMP object in a pg_temp_NN namespace (both measured today).
use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => rows[0]
            .values
            .iter()
            .map(|v| format!("{v:?}"))
            .collect::<Vec<_>>()
            .join(","),
        Ok(QueryResult::Rows { .. }) => "(no rows)".into(),
        Ok(other) => format!("{other:?}"),
        Err(err) => format!("ERR {err:?}"),
    }
}

fn main() {
    let mut e = Engine::new();
    println!(
        "C9   substr('abcdef','2')        -> {}   [PG18: Text(\"bcdef\")]",
        one(&mut e, "SELECT substr('abcdef','2')")
    );

    println!(
        "C11a CREATE TEMP SEQUENCE        -> {}",
        one(&mut e, "CREATE TEMP SEQUENCE c11_seq")
    );
    println!(
        "C11b its namespace               -> {}   [PG18: pg_temp_NN]",
        one(
            &mut e,
            "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relname='c11_seq'"
        )
    );
    println!(
        "C11c CREATE TEMP VIEW            -> {}",
        one(&mut e, "CREATE TEMP VIEW c11_v AS SELECT 1 AS x")
    );
    println!(
        "C11d its namespace               -> {}   [PG18: pg_temp_NN]",
        one(
            &mut e,
            "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relname='c11_v'"
        )
    );

    let mut m = Engine::new();
    m.execute("SET sql_mode = 'STRICT_TRANS_TABLES'")
        .expect("mysql dialect");
    println!(
        "C14  UNSIGNED arithmetic overflow -> {}   [MariaDB: ERROR 1690, not a silent -1]",
        one(&mut m, "SELECT CAST(0 AS UNSIGNED) - 1")
    );
    println!(
        "C13  BIGINT UNSIGNED full range   -> {}   [MySQL: 18446744073709551615]",
        one(&mut m, "SELECT CAST(18446744073709551615 AS UNSIGNED)")
    );
}
