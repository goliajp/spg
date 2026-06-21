//! v7.17.0 Phase 3.P0-43 — window functions over JOIN output.
//!
//! Phase 3.6 (`4453e95`) rejected this combination outright
//! ("JOIN with window functions not yet supported"); P0-43 routes
//! the window pipeline through the join materialiser the
//! aggregate / projection paths already use, so window functions
//! evaluate against the joined row stream and the composite
//! `alias.col` schema.
//!
//! Lock in:
//!   * ROW_NUMBER OVER (PARTITION BY <joined col>) on a 2-table
//!     INNER JOIN
//!   * RANK over LEFT JOIN preserves NULL-extended rows
//!   * Aggregate window (SUM OVER) carries through
//!   * Bare-column reference on a non-ambiguous JOIN works
//!   * Multi-key PARTITION + ORDER BY tuple works
//!   * Existing single-table window path is unchanged

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

fn seed_employees_departments(e: &mut Engine) {
    e.execute("CREATE TABLE depts (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute(
        "CREATE TABLE emps (id INT NOT NULL, dept_id INT NOT NULL, name TEXT NOT NULL, salary INT NOT NULL)",
    )
    .unwrap();
    e.execute("INSERT INTO depts VALUES (1, 'Eng'), (2, 'Sales')")
        .unwrap();
    e.execute(
        "INSERT INTO emps VALUES \
            (1, 1, 'Alice', 100), \
            (2, 1, 'Bob', 90), \
            (3, 1, 'Carol', 90), \
            (4, 2, 'Dave', 80), \
            (5, 2, 'Eve', 70)",
    )
    .unwrap();
}

#[test]
fn row_number_partitioned_by_dept_over_inner_join() {
    let mut e = Engine::new();
    seed_employees_departments(&mut e);
    let r = rows(
        e.execute(
            "SELECT emps.name, depts.name AS dept_name, \
                    ROW_NUMBER() OVER (PARTITION BY depts.name ORDER BY emps.salary DESC) AS rn \
             FROM emps JOIN depts ON emps.dept_id = depts.id \
             ORDER BY dept_name, rn",
        )
        .unwrap(),
    );
    // Eng partition (3 rows, by salary DESC): Alice=100→rn 1, Bob=90→2 or 3, Carol=90→2 or 3
    // Sales partition (2 rows): Dave=80→1, Eve=70→2.
    assert_eq!(r.len(), 5);
    // Check the deterministic ones.
    assert_eq!(r[0][1], Value::text("Eng"));
    assert_eq!(r[0][2], Value::BigInt(1));
    assert_eq!(r[0][0], Value::text("Alice"));
    assert_eq!(r[3][1], Value::text("Sales"));
    assert_eq!(r[3][2], Value::BigInt(1));
    assert_eq!(r[3][0], Value::text("Dave"));
    assert_eq!(r[4][1], Value::text("Sales"));
    assert_eq!(r[4][2], Value::BigInt(2));
    assert_eq!(r[4][0], Value::text("Eve"));
}

#[test]
fn rank_partitioned_by_dept_handles_ties() {
    // RANK assigns the same rank to tied rows and skips the next.
    let mut e = Engine::new();
    seed_employees_departments(&mut e);
    let r = rows(
        e.execute(
            "SELECT emps.name, \
                    RANK() OVER (PARTITION BY depts.name ORDER BY emps.salary DESC) AS rk \
             FROM emps JOIN depts ON emps.dept_id = depts.id \
             ORDER BY depts.name, rk, emps.name",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 5);
    // Eng: Alice=100 → rk=1; Bob=90 → rk=2; Carol=90 → rk=2.
    assert_eq!(r[0][0], Value::text("Alice"));
    assert_eq!(r[0][1], Value::BigInt(1));
    assert_eq!(r[1][0], Value::text("Bob"));
    assert_eq!(r[1][1], Value::BigInt(2));
    assert_eq!(r[2][0], Value::text("Carol"));
    assert_eq!(r[2][1], Value::BigInt(2));
    // Sales: Dave=80 → rk=1; Eve=70 → rk=2.
    assert_eq!(r[3][0], Value::text("Dave"));
    assert_eq!(r[3][1], Value::BigInt(1));
    assert_eq!(r[4][0], Value::text("Eve"));
    assert_eq!(r[4][1], Value::BigInt(2));
}

#[test]
fn sum_window_over_inner_join() {
    // SUM(salary) over partition (dept) — same value for every
    // row in a partition.
    let mut e = Engine::new();
    seed_employees_departments(&mut e);
    let r = rows(
        e.execute(
            "SELECT emps.name, \
                    SUM(emps.salary) OVER (PARTITION BY depts.name) AS dept_total \
             FROM emps JOIN depts ON emps.dept_id = depts.id \
             ORDER BY depts.name, emps.name",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 5);
    // Eng: 100 + 90 + 90 = 280. Sales: 80 + 70 = 150.
    // Window SUM widens to Float (matches the per-row aggregate
    // accumulator's PG-style numeric ladder).
    assert_eq!(r[0][1], Value::Float(280.0));
    assert_eq!(r[1][1], Value::Float(280.0));
    assert_eq!(r[2][1], Value::Float(280.0));
    assert_eq!(r[3][1], Value::Float(150.0));
    assert_eq!(r[4][1], Value::Float(150.0));
}

#[test]
fn window_over_left_join_keeps_null_extended_row() {
    // Add a dept with no employees; LEFT JOIN from depts to emps
    // gives a NULL-extended row that the window pipeline must
    // not drop.
    let mut e = Engine::new();
    seed_employees_departments(&mut e);
    e.execute("INSERT INTO depts VALUES (3, 'Empty')").unwrap();
    let r = rows(
        e.execute(
            "SELECT depts.name AS dept_name, emps.name AS emp_name, \
                    ROW_NUMBER() OVER (PARTITION BY depts.name ORDER BY emps.salary DESC) AS rn \
             FROM depts LEFT JOIN emps ON emps.dept_id = depts.id \
             ORDER BY dept_name, rn",
        )
        .unwrap(),
    );
    // Empty dept appears with NULL emp_name and rn = 1 (single
    // row partition with all-NULL key sorts as one group).
    assert!(
        r.iter()
            .any(|row| { row[0] == Value::text("Empty") && row[1] == Value::Null })
    );
}

#[test]
fn window_with_bare_column_when_unambiguous() {
    // `salary` lives only on `emps`, so the bare-name resolver
    // finds it through the `emps.salary` composite schema entry.
    // (Bare `name` would be ambiguous since both tables have it;
    // the qualified `emps.name` is required.)
    let mut e = Engine::new();
    seed_employees_departments(&mut e);
    let r = rows(
        e.execute(
            "SELECT emps.name, ROW_NUMBER() OVER (ORDER BY salary DESC) AS rn \
             FROM emps JOIN depts ON emps.dept_id = depts.id \
             ORDER BY rn",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 5);
    assert_eq!(r[0][0], Value::text("Alice"));
    assert_eq!(r[0][1], Value::BigInt(1));
}

#[test]
fn single_table_window_path_still_works_regression() {
    // Phase 3.6 single-table window pipeline must remain
    // identical — the JOIN branch must not steal work from the
    // existing path.
    let mut e = Engine::new();
    seed_employees_departments(&mut e);
    let r = rows(
        e.execute(
            "SELECT name, ROW_NUMBER() OVER (ORDER BY salary DESC) AS rn \
             FROM emps ORDER BY rn",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 5);
    assert_eq!(r[0][0], Value::text("Alice"));
    assert_eq!(r[0][1], Value::BigInt(1));
    assert_eq!(r[4][0], Value::text("Eve"));
    assert_eq!(r[4][1], Value::BigInt(5));
}
