//! v7.39 (round 623, S05b) — the catalogs did not describe themselves.
//!
//! `SELECT count(*) FROM pg_class WHERE relname = 'pg_class'` answered 0
//! where PG answers 1, and `pg_attribute` — which is where a tool learns
//! what it may select before it selects it — held the user's columns and
//! none of the catalogs'. Measured against PG18:
//!
//!     pg_attribute rows for catalog relations   PG 2584    SPG 0
//!     catalog relations in pg_class             PG 5/5     SPG 0
//!
//! Both come from one place: the twenty-two relations SPG publishes are now
//! listed, with the oid PG gives each. Those oids are a contract a client
//! can observe (`'pg_type'::regclass` is 1247 in PG and now here), and a
//! stable one is what lets a cached lookup keep working. Only PG's relkind
//! 'r' catalogs are listed — its `pg_stat_*` and `pg_tables` are views
//! initdb creates, whose oids sit in the 12000s and vary by build.
//!
//! The column lists come from the synths themselves rather than a second
//! copy written out here, so a catalog that gains a column is described
//! with it.
//!
//! `relnamespace` is pg_catalog's oid, which is what keeps them out of
//! everything that lists the user's relations — every such query filters on
//! the namespace. That is pinned below too: the catalogs appearing must not
//! change what `WHERE nspname = 'public'` answers.
//!
//! Measuring this turned up a second gap in the same relation: PG lists six
//! SYSTEM columns for every table at negative attnums (ctid -1, xmin -2,
//! cmin -3, xmax -4, cmax -5, tableoid -6), and `attnum < 0` is how a tool
//! tells a system column from a user one. SPG had no negative attnums at
//! all. PG answers ten rows for pg_namespace; SPG answered four. It answers
//! ten now, and the whole list is byte-identical to PG18's — including that
//! a VIEW has none of them, which PG also reports.
//!
//! Recorded, not closed: SPG publishes 22 catalog relations where PG has 64,
//! and its pg_class has 34 columns where PG's has 40. A relation can only
//! describe what it has; the coverage gap is its own item.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The catalogs are in pg_class, at PG's oids, in pg_catalog.
#[test]
fn round623_catalogs_are_listed_with_pgs_oids() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT relname, oid, relkind FROM pg_class \
             WHERE relname IN ('pg_type','pg_attribute','pg_proc','pg_class') ORDER BY relname"
        ),
        vec![
            "pg_attribute|1249|r",
            "pg_class|1259|r",
            "pg_proc|1255|r",
            "pg_type|1247|r",
        ],
        "PG18's own oids, which a client can observe through ::regclass"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT 'pg_type'::regclass::oid, 'pg_class'::regclass::oid, \
             'pg_attribute'::regclass::oid"
        ),
        vec!["1247|1259|1249"],
        "and the cast agrees with the row"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = 'pg_class'"
        ),
        vec!["pg_catalog"]
    );
}

/// …and listing them must not change what "the user's tables" means.
#[test]
fn round623_catalogs_do_not_pollute_the_user_namespace() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE alpha (id INT)").unwrap();
    e.execute("CREATE TABLE beta (id INT)").unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'public' AND c.relkind = 'r' ORDER BY c.relname"
        ),
        vec!["alpha", "beta"],
        "the namespace filter every tool writes is what keeps them apart"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename"
        ),
        vec!["alpha", "beta"]
    );
}

/// pg_attribute describes the catalogs' own columns.
#[test]
fn round623_pg_attribute_describes_the_catalogs() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT attname, attnum FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
             WHERE c.relname = 'pg_namespace' ORDER BY attnum"
        ),
        vec![
            "tableoid|-6",
            "cmax|-5",
            "xmax|-4",
            "cmin|-3",
            "xmin|-2",
            "ctid|-1",
            "oid|1",
            "nspname|2",
            "nspowner|3",
            "nspacl|4",
        ],
        "byte-identical to PG18's answer for the same query"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
             WHERE c.relname = 'pg_namespace'"
        ),
        vec!["10"],
        "PG says ten; SPG said four"
    );
    // Every listed catalog is described, and none is described twice.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM (SELECT DISTINCT c.relname FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_attribute a ON a.attrelid = c.oid \
             WHERE n.nspname = 'pg_catalog') q"
        ),
        // v7.39 (round 650) — 22 to 26 with the four text-search
        // catalogs. This number is SUPPOSED to move when a catalog is
        // added: that is what makes it catch one that was published
        // without being self-described. Registering a catalog takes four
        // separate lists, and this pin is the one that notices when the
        // `pg_attribute` half was missed.
        // 7.38.1 S5.1 — 27 to 31: pg_opclass, pg_opfamily, pg_amop and
        // pg_amproc joined the wall (the pg_dump campaign).
        vec!["31"]
    );
}

/// The six system columns, on a user table, and not on a view.
#[test]
fn round623_system_columns_have_negative_attnums() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (a INT, b TEXT)").unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT attname, attnum FROM pg_attribute WHERE attrelid = 'u'::regclass \
             ORDER BY attnum"
        ),
        vec![
            "tableoid|-6",
            "cmax|-5",
            "xmax|-4",
            "cmin|-3",
            "xmin|-2",
            "ctid|-1",
            "a|1",
            "b|2",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT attname FROM pg_attribute WHERE attrelid = 'u'::regclass AND attnum > 0 \
             ORDER BY attnum"
        ),
        vec!["a", "b"],
        "the `attnum > 0` filter tools already write still yields the user's columns"
    );
    // PG's types for them: tid / xid / cid / oid.
    assert_eq!(
        vals(
            &mut e,
            "SELECT attname, atttypid, attlen, attnotnull FROM pg_attribute \
             WHERE attrelid = 'u'::regclass AND attnum < 0 ORDER BY attnum DESC"
        ),
        vec![
            "ctid|27|6|true",
            "xmin|28|4|true",
            "cmin|29|4|true",
            "xmax|28|4|true",
            "cmax|29|4|true",
            "tableoid|26|4|true",
        ]
    );
    e.execute("CREATE VIEW v AS SELECT 1 AS a").unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 'v'::regclass AND attnum < 0"
        ),
        vec!["0"],
        "a VIEW has none of them — PG reports zero here too"
    );
}
