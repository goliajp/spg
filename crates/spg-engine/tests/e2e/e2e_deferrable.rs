//! v7.17.0 Phase 3.1 — `[NOT] DEFERRABLE [INITIALLY {DEFERRED |
//! IMMEDIATE}]` constraint-timing clauses on FOREIGN KEY now
//! parse and run as immediate. Pre-3.1 the bare `DEFERRABLE`
//! form hard-errored, breaking pg_dump restores where the
//! source schema declared a deferred FK. SPG is single-writer
//! with no deferred-constraint window, so every spelling maps
//! to the immediate-check path.

use spg_engine::Engine;

fn setup_parent(e: &mut Engine) {
    e.execute("CREATE TABLE parent (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    e.execute("INSERT INTO parent VALUES (1), (2), (3)")
        .unwrap();
}

#[test]
fn fk_deferrable_initially_deferred_parses() {
    // The exact pg_dump shape for a `DEFERRABLE INITIALLY
    // DEFERRED` foreign key.
    let mut e = Engine::new();
    setup_parent(&mut e);
    e.execute(
        "CREATE TABLE child (\
             id INT NOT NULL, \
             parent_id INT NOT NULL, \
             CONSTRAINT child_parent_fkey FOREIGN KEY (parent_id) \
                 REFERENCES parent (id) DEFERRABLE INITIALLY DEFERRED\
         )",
    )
    .unwrap();
    e.execute("INSERT INTO child VALUES (1, 1)").unwrap();
}

#[test]
fn fk_deferrable_initially_immediate_parses() {
    let mut e = Engine::new();
    setup_parent(&mut e);
    e.execute(
        "CREATE TABLE child (\
             id INT NOT NULL, \
             parent_id INT NOT NULL, \
             FOREIGN KEY (parent_id) REFERENCES parent (id) \
                 DEFERRABLE INITIALLY IMMEDIATE\
         )",
    )
    .unwrap();
}

#[test]
fn fk_bare_deferrable_no_initially_parses() {
    let mut e = Engine::new();
    setup_parent(&mut e);
    e.execute(
        "CREATE TABLE child (\
             id INT NOT NULL, \
             parent_id INT NOT NULL, \
             FOREIGN KEY (parent_id) REFERENCES parent (id) DEFERRABLE\
         )",
    )
    .unwrap();
}

#[test]
fn fk_not_deferrable_still_parses() {
    let mut e = Engine::new();
    setup_parent(&mut e);
    e.execute(
        "CREATE TABLE child (\
             id INT NOT NULL, \
             parent_id INT NOT NULL, \
             FOREIGN KEY (parent_id) REFERENCES parent (id) NOT DEFERRABLE\
         )",
    )
    .unwrap();
}

#[test]
fn fk_not_deferrable_initially_immediate_n5_shape() {
    // Phase 1.5 audit finding N5 — pg_dump's `NOT DEFERRABLE
    // INITIALLY IMMEDIATE` for FKs.
    let mut e = Engine::new();
    setup_parent(&mut e);
    e.execute(
        "CREATE TABLE child (\
             id INT NOT NULL, \
             parent_id INT NOT NULL, \
             FOREIGN KEY (parent_id) REFERENCES parent (id) \
                 NOT DEFERRABLE INITIALLY IMMEDIATE\
         )",
    )
    .unwrap();
}

#[test]
fn fk_initially_deferred_alone_parses() {
    // PG also accepts bare `INITIALLY DEFERRED` without a
    // leading [NOT] DEFERRABLE keyword.
    let mut e = Engine::new();
    setup_parent(&mut e);
    e.execute(
        "CREATE TABLE child (\
             id INT NOT NULL, \
             parent_id INT NOT NULL, \
             FOREIGN KEY (parent_id) REFERENCES parent (id) INITIALLY DEFERRED\
         )",
    )
    .unwrap();
}

#[test]
fn fk_deferrable_runs_constraint_immediately() {
    // SPG accepts DEFERRABLE but still runs FK checks at
    // statement time, not at COMMIT. Verify by attempting an
    // FK violation INSERT — it should error immediately rather
    // than defer to commit.
    let mut e = Engine::new();
    setup_parent(&mut e);
    e.execute(
        "CREATE TABLE child (\
             id INT NOT NULL, \
             parent_id INT NOT NULL, \
             FOREIGN KEY (parent_id) REFERENCES parent (id) \
                 DEFERRABLE INITIALLY DEFERRED\
         )",
    )
    .unwrap();
    // parent_id = 999 violates the FK; we expect an immediate
    // error (single-writer no-deferred-window).
    let r = e.execute("INSERT INTO child VALUES (1, 999)");
    assert!(
        r.is_err(),
        "FK violation should error immediately even with DEFERRABLE"
    );
}

#[test]
fn fk_deferrable_on_delete_combo() {
    // Combined with ON DELETE — DEFERRABLE can appear before or
    // after the ON clauses. pg_dump emits DEFERRABLE last.
    let mut e = Engine::new();
    setup_parent(&mut e);
    e.execute(
        "CREATE TABLE child (\
             id INT NOT NULL, \
             parent_id INT NOT NULL, \
             FOREIGN KEY (parent_id) REFERENCES parent (id) \
                 ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED\
         )",
    )
    .unwrap();
}

#[test]
fn inline_column_fk_with_deferrable() {
    // Inline column-level form: `col TYPE REFERENCES tbl(col)
    // DEFERRABLE INITIALLY DEFERRED`.
    let mut e = Engine::new();
    setup_parent(&mut e);
    e.execute(
        "CREATE TABLE child (\
             id INT NOT NULL, \
             parent_id INT NOT NULL REFERENCES parent (id) \
                 DEFERRABLE INITIALLY DEFERRED\
         )",
    )
    .unwrap();
}
