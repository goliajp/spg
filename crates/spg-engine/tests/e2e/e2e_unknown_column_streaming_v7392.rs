//! v7.39.2 — the streaming SELECT entry refuses an unknown column too.
//!
//! `execute_readonly_select_streaming` runs BELOW `exec_select_cancel`,
//! which is where the check for an unknown column in WHERE / ORDER BY /
//! GROUP BY / HAVING went first. On an empty table that route therefore
//! kept answering zero rows while the materialising one raised — and
//! the ACL check had been patched into the same entries, for the same
//! reason, in v7.39.
//!
//! Pinned here rather than on the wire because the wire reaches this
//! entry only when the statement fails to prepare, which a well-formed
//! SELECT never does; the engine tests call it directly.

use spg_engine::{CancelToken, Engine, StreamItem};

fn stream(e: &mut Engine, sql: &str) -> Result<usize, String> {
    let mut n = 0usize;
    e.execute_readonly_select_streaming(sql, CancelToken::none(), |item| {
        if matches!(item, StreamItem::Row(_)) {
            n += 1;
        }
        Ok(())
    })
    .map(|_| n)
    .map_err(|e| e.to_string())
}

#[test]
fn the_streaming_entry_refuses_an_unknown_column_on_an_empty_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE es (a int)").expect("create");
    for sql in [
        "SELECT a FROM es WHERE nosuch = 1",
        "SELECT a FROM es ORDER BY nosuch",
        "SELECT * FROM es WHERE nosuch = 1",
    ] {
        let got = stream(&mut e, sql);
        assert!(
            got.as_ref().is_err_and(|m| m.contains("nosuch")),
            "{sql}: {got:?}"
        );
    }
    // The control: names that resolve still stream, empty or not.
    assert_eq!(stream(&mut e, "SELECT a FROM es WHERE a = 1"), Ok(0));
    e.execute("INSERT INTO es VALUES (1)").expect("insert");
    assert_eq!(stream(&mut e, "SELECT a FROM es WHERE a = 1"), Ok(1));
}
