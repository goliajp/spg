//! v7.37.17 (17.6 siblings) — pg_dump-compat session-state
//! statements: DISCARD / DEALLOCATE / SECURITY LABEL.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn discard_all_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DISCARD ALL");
    ddl(&mut e, "DISCARD PLANS");
    ddl(&mut e, "DISCARD SEQUENCES");
    ddl(&mut e, "DISCARD TEMPORARY");
    ddl(&mut e, "DISCARD TEMP");
}

#[test]
fn deallocate_names_a_missing_statement() {
    // v7.39 (round 277) — was `deallocate_no_op`, which asserted that
    // dropping a name that was never prepared succeeds. PG raises
    // `prepared statement "myplan" does not exist`; only ALL is
    // unconditional.
    let mut e = Engine::new();
    for sql in ["DEALLOCATE myplan", "DEALLOCATE PREPARE myplan"] {
        let msg = format!("{:?}", e.execute(sql).unwrap_err());
        assert!(
            msg.contains(r#"prepared statement \"myplan\" does not exist"#),
            "{sql}: {msg}",
        );
    }
    ddl(&mut e, "DEALLOCATE ALL");
}

#[test]
fn security_label_is_refused_because_no_provider_is_loaded() {
    // v7.39 (round 696) — was `security_label_on_object_no_op`, which
    // asserted SPG ACCEPTS this. PG18 refuses it unconditionally — `no
    // security label providers have been loaded` — whatever object it
    // names, because none is. SPG has none either, so accepting it told the
    // caller a label had been applied when nothing anywhere records one.
    //
    // This is not a pg_dump concern: pg_dump only emits SECURITY LABEL for
    // labels it read from a database that HAD a provider loaded, and such a
    // dump cannot restore into a PG without one either.
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    for sql in [
        "SECURITY LABEL ON TABLE t IS 'unclassified'",
        "SECURITY LABEL FOR selinux ON TABLE t IS 'system_u:object_r:sepgsql_table_t:s0'",
    ] {
        let err = e.execute(sql).expect_err(sql);
        assert!(
            format!("{err}").contains("no security label providers have been loaded"),
            "{sql}: {err}"
        );
    }
}
