//! v7.37.20 (20.4) — PL/pgSQL FOR i IN start..end LOOP.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn for_range_sum_inclusive_bounds() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; i INT; \
         BEGIN FOR i IN 1..5 LOOP n := n + i; END LOOP; \
         ASSERT n = 15; END $$;",
    );
}

#[test]
fn for_range_zero_iterations_when_start_gt_end() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; i INT; \
         BEGIN FOR i IN 10..1 LOOP n := n + 1; END LOOP; \
         ASSERT n = 0; END $$;",
    );
}

#[test]
fn for_range_reverse_counts_down() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; i INT; \
         BEGIN FOR i IN REVERSE 5..1 LOOP n := n + i; END LOOP; \
         ASSERT n = 15; END $$;",
    );
}

#[test]
fn for_range_assert_inside_propagates() {
    let mut e = Engine::new();
    let err = e.execute(
        "DO $$ DECLARE i INT; \
         BEGIN FOR i IN 1..5 LOOP ASSERT i < 3; END LOOP; END $$;",
    );
    assert!(err.is_err(), "ASSERT inside FOR should propagate");
}
