//! 8.0.3 — what `pg_dump` reads from the catalog, answered the way
//! PostgreSQL answers it, so a dump of SPG restores into PostgreSQL.
//!
//! sentori (their §4.4): a dump of 8.0.2 failed to restore into PG 18.6 on
//! `role "postgres" does not exist` and on five extensions the schema never
//! created. Against their seventeen migrations it did not dump at all
//! (`pg_get_function_arguments` was missing), and once it did it restored
//! nowhere (`SUPPORT 0`). Every expectation here is PG 18.6's own answer.
//! The release gate's `pgdump-roundtrip` step compares the whole dump text
//! with PG's; these pin the catalog answers one at a time.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn one(e: &mut Engine, sql: &str) -> String {
    let mut v = col(e, sql);
    assert_eq!(v.len(), 1, "{sql}: {v:?}");
    v.remove(0)
}

/// An engine as the official image starts one: a single superuser named
/// by POSTGRES_USER, and the session logged in as that user.
fn as_image_user() -> Engine {
    let mut e = Engine::new();
    ok(&mut e, "CREATE USER u WITH SUPERUSER PASSWORD 'p'");
    e.set_session_user("u");
    e
}

#[test]
fn every_object_belongs_to_the_role_that_created_it() {
    let mut e = as_image_user();
    ok(&mut e, "CREATE TABLE t (id int PRIMARY KEY)");
    ok(&mut e, "CREATE VIEW v AS SELECT id FROM t");
    ok(&mut e, "CREATE TYPE mood AS ENUM ('sad', 'ok')");
    ok(
        &mut e,
        "CREATE MATERIALIZED VIEW mv AS SELECT count(*) AS n FROM t",
    );
    let owners = col(
        &mut e,
        "SELECT r.rolname FROM pg_class c JOIN pg_roles r ON r.oid = c.relowner \
         WHERE c.relname IN ('t', 'v', 'mv', 't_pkey') ORDER BY c.relname",
    );
    assert_eq!(owners, ["u", "u", "u", "u"]);
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_get_userbyid(typowner) FROM pg_type WHERE typname = 'mood'"
        ),
        "u"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT tableowner FROM pg_tables WHERE tablename = 't'"
        ),
        "u"
    );
    // PostgreSQL gives oid 10 to the bootstrap superuser, which the image
    // names after POSTGRES_USER; there is no `postgres` role there.
    assert_eq!(
        one(&mut e, "SELECT rolname FROM pg_roles WHERE oid = 10"),
        "u"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_roles WHERE rolname = 'postgres'"
        ),
        "0"
    );
    // A second role's objects are its own — not the bootstrap role's, which
    // is what every owner column used to report.
    ok(&mut e, "CREATE USER w WITH SUPERUSER PASSWORD 'p'");
    e.set_session_user("w");
    ok(&mut e, "CREATE TABLE tw (id int)");
    ok(&mut e, "CREATE VIEW vw AS SELECT id FROM tw");
    assert_eq!(
        col(
            &mut e,
            "SELECT r.rolname FROM pg_class c JOIN pg_roles r ON r.oid = c.relowner \
             WHERE c.relname IN ('tw', 'vw') ORDER BY c.relname"
        ),
        ["w", "w"]
    );
}

#[test]
fn public_belongs_to_pg_database_owner_and_keeps_its_comment() {
    let mut e = as_image_user();
    assert_eq!(
        one(
            &mut e,
            "SELECT nspowner::bigint FROM pg_namespace WHERE nspname = 'public'"
        ),
        "6171"
    );
    assert_eq!(
        one(&mut e, "SELECT rolname FROM pg_roles WHERE oid = 6171"),
        "pg_database_owner"
    );
    assert_eq!(
        one(&mut e, "SELECT obj_description(2200, 'pg_namespace')"),
        "standard public schema"
    );
}

#[test]
fn pg_extension_lists_what_was_created() {
    let mut e = as_image_user();
    assert_eq!(col(&mut e, "SELECT extname FROM pg_extension"), ["plpgsql"]);
    ok(&mut e, "CREATE EXTENSION pgcrypto");
    assert_eq!(
        col(
            &mut e,
            "SELECT extname || ':' || extnamespace::bigint FROM pg_extension ORDER BY extname"
        ),
        ["pgcrypto:2200", "plpgsql:11"]
    );
    let dup = format!("{}", e.execute("CREATE EXTENSION pgcrypto").unwrap_err());
    assert!(
        dup.contains("extension \"pgcrypto\" already exists"),
        "{dup}"
    );
    ok(&mut e, "CREATE EXTENSION IF NOT EXISTS pgcrypto");
    ok(&mut e, "DROP EXTENSION IF EXISTS nosuch, pgcrypto");
    assert_eq!(col(&mut e, "SELECT extname FROM pg_extension"), ["plpgsql"]);
    let missing = format!("{}", e.execute("DROP EXTENSION pgcrypto").unwrap_err());
    assert!(
        missing.contains("extension \"pgcrypto\" does not exist"),
        "{missing}"
    );
}

#[test]
fn a_primary_key_and_a_unique_constraint_name_their_index() {
    let mut e = Engine::new();
    ok(
        &mut e,
        "CREATE TABLE k (id int PRIMARY KEY, a text, b text, UNIQUE (a, b))",
    );
    // Dropping a column rebuilds the table's indexes, which lost the flags
    // that tie an index to its constraint.
    ok(&mut e, "ALTER TABLE k ADD COLUMN c int");
    ok(&mut e, "ALTER TABLE k DROP COLUMN c");
    let joined = col(
        &mut e,
        "SELECT c.conname || '=' || i.relname FROM pg_constraint c \
         JOIN pg_class i ON i.oid = c.conindid WHERE c.conrelid = 'k'::regclass \
         AND c.contype IN ('p', 'u') ORDER BY 1",
    );
    assert_eq!(joined, ["k_a_b_key=k_a_b_key", "k_pkey=k_pkey"]);
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_index WHERE indrelid = 'k'::regclass"
        ),
        "2"
    );
}

#[test]
fn a_serial_column_has_its_sequence_before_anything_names_it() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE s (id bigserial PRIMARY KEY, v text)");
    assert_eq!(
        one(
            &mut e,
            "SELECT relkind FROM pg_class WHERE relname = 's_id_seq'"
        ),
        "S"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT deptype FROM pg_depend WHERE objid = 's_id_seq'::regclass \
             AND refobjid = 's'::regclass AND refobjsubid = 1"
        ),
        "a"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 's'::regclass"
        ),
        "nextval('s_id_seq'::regclass)"
    );
}

#[test]
fn names_are_qualified_when_the_search_path_leaves_public_out() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TYPE addr AS (street text, zip int)");
    ok(
        &mut e,
        "CREATE TABLE p (id bigserial PRIMARY KEY, home addr)",
    );
    ok(&mut e, "CREATE TABLE c (id int, p bigint REFERENCES p(id))");
    ok(&mut e, "CREATE VIEW pv AS SELECT id FROM p");
    ok(
        &mut e,
        "SELECT pg_catalog.set_config('search_path', '', false)",
    );
    assert_eq!(
        one(&mut e, "SELECT pg_catalog.current_schemas(false)::text"),
        "{}"
    );
    assert!(
        one(
            &mut e,
            "SELECT pg_catalog.pg_get_viewdef('public.pv'::regclass)"
        )
        .contains("FROM public.p"),
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_catalog.format_type(atttypid, atttypmod) FROM pg_catalog.pg_attribute \
             WHERE attrelid = 'public.p'::regclass AND attname = 'home'"
        ),
        "public.addr"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_catalog.pg_get_constraintdef(oid) FROM pg_catalog.pg_constraint \
             WHERE conrelid = 'public.c'::regclass AND contype = 'f'"
        ),
        "FOREIGN KEY (p) REFERENCES public.p(id)"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_catalog.pg_get_expr(adbin, adrelid) FROM pg_catalog.pg_attrdef \
             WHERE adrelid = 'public.p'::regclass"
        ),
        "nextval('public.p_id_seq'::regclass)"
    );
}

#[test]
fn a_check_and_a_predicate_read_as_postgresql_prints_them() {
    let mut e = Engine::new();
    ok(
        &mut e,
        "CREATE TABLE q (s text CHECK (s IN ('a', 'b')), active boolean, status text)",
    );
    ok(&mut e, "CREATE INDEX q_live ON q (s) WHERE active");
    ok(&mut e, "CREATE INDEX q_open ON q (s) WHERE status = 'open'");
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE contype = 'c'"
        ),
        "CHECK ((s = ANY (ARRAY['a'::text, 'b'::text])))"
    );
    assert_eq!(
        col(
            &mut e,
            "SELECT pg_get_indexdef(indexrelid) FROM pg_index ORDER BY 1"
        ),
        [
            "CREATE INDEX q_live ON public.q USING btree (s) WHERE active",
            "CREATE INDEX q_open ON public.q USING btree (s) WHERE (status = 'open'::text)",
        ]
    );
}

#[test]
fn a_function_dumps_and_its_comment_is_found() {
    let mut e = Engine::new();
    ok(
        &mut e,
        "CREATE FUNCTION vk(v text) RETURNS bigint[] LANGUAGE sql IMMUTABLE AS $$ SELECT ARRAY[1]::bigint[] $$",
    );
    ok(&mut e, "COMMENT ON FUNCTION vk(text) IS 'comparable'");
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_get_function_arguments(oid) || '|' || pg_get_function_result(oid) || '|' \
             || prosupport FROM pg_proc WHERE proname = 'vk'"
        ),
        "v text|bigint[]|-"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT d.description FROM pg_description d JOIN pg_proc p ON p.oid = d.objoid \
             AND d.classoid = 'pg_proc'::regclass WHERE p.proname = 'vk'"
        ),
        "comparable"
    );
}

#[test]
fn an_index_comment_and_its_operator_class_survive() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE g (doc jsonb)");
    ok(
        &mut e,
        "CREATE INDEX g_doc ON g USING gin (doc jsonb_path_ops)",
    );
    ok(&mut e, "COMMENT ON INDEX g_doc IS 'paths'");
    assert_eq!(
        one(
            &mut e,
            "SELECT obj_description('g_doc'::regclass, 'pg_class')"
        ),
        "paths"
    );
    assert_eq!(
        one(&mut e, "SELECT pg_get_indexdef('g_doc'::regclass)"),
        "CREATE INDEX g_doc ON public.g USING gin (doc jsonb_path_ops)"
    );
    // And both are still there after a restart.
    let mut e =
        Engine::restore(spg_storage::Catalog::deserialize(&e.snapshot()).expect("roundtrip"));
    assert_eq!(
        one(&mut e, "SELECT pg_get_indexdef('g_doc'::regclass)"),
        "CREATE INDEX g_doc ON public.g USING gin (doc jsonb_path_ops)"
    );
}

#[test]
fn an_insert_computes_an_index_over_a_user_function() {
    let mut e = Engine::new();
    ok(
        &mut e,
        "CREATE FUNCTION norm(n text) RETURNS text LANGUAGE sql IMMUTABLE AS $$ SELECT lower(n) $$",
    );
    ok(&mut e, "CREATE TABLE people (name text)");
    ok(&mut e, "CREATE INDEX people_norm ON people (norm(name))");
    ok(&mut e, "INSERT INTO people VALUES ('Ann'), ('BOB')");
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM people WHERE norm(name) = 'bob'"
        ),
        "1"
    );
}

#[test]
fn a_domain_trigger_and_joined_view_deparse() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE DOMAIN posint AS integer CHECK (VALUE > 0)");
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE contypid = 'posint'::regtype"
        ),
        "CHECK ((VALUE > 0))"
    );
    ok(
        &mut e,
        "CREATE TABLE a (id int, state text, at timestamptz)",
    );
    ok(&mut e, "CREATE TABLE b (id int)");
    ok(
        &mut e,
        "CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.at := now(); RETURN NEW; END $$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER a_touch BEFORE UPDATE OR INSERT ON a FOR EACH ROW EXECUTE FUNCTION touch()",
    );
    assert_eq!(
        one(&mut e, "SELECT pg_get_triggerdef(oid) FROM pg_trigger"),
        "CREATE TRIGGER a_touch BEFORE INSERT OR UPDATE ON public.a FOR EACH ROW EXECUTE FUNCTION touch()"
    );
    ok(
        &mut e,
        "CREATE VIEW ab AS SELECT a.id FROM a JOIN b ON b.id = a.id",
    );
    // `pg_dump` drops the definition's last character, taking it to be ';'.
    let def = one(&mut e, "SELECT pg_get_viewdef('ab'::regclass)");
    assert!(def.starts_with(' ') && def.ends_with(");"), "{def:?}");
}

#[test]
fn an_enum_default_is_a_constant_of_the_enum() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TYPE mood AS ENUM ('sad', 'ok')");
    ok(&mut e, "CREATE TABLE m (a mood DEFAULT 'ok')");
    // The spelling every dump writes back.
    ok(&mut e, "CREATE TABLE m2 (a mood DEFAULT 'ok'::public.mood)");
    assert_eq!(
        col(
            &mut e,
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef ORDER BY adrelid"
        ),
        ["'ok'::mood", "'ok'::mood"]
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT attcollation::bigint || attstorage FROM pg_attribute \
             WHERE attrelid = 'm'::regclass AND attname = 'a'"
        ),
        "0p"
    );
}

#[test]
fn dump_ownership_statements_restore() {
    let mut e = as_image_user();
    ok(&mut e, "CREATE SEQUENCE sq");
    ok(&mut e, "CREATE DOMAIN d AS text");
    ok(&mut e, "ALTER SEQUENCE public.sq OWNER TO u");
    ok(&mut e, "ALTER DOMAIN public.d OWNER TO u");
    ok(&mut e, "CREATE TABLE o (id int)");
    ok(&mut e, "CREATE USER w WITH PASSWORD 'p'");
    ok(&mut e, "ALTER TABLE o OWNER TO w");
    assert_eq!(
        one(
            &mut e,
            "SELECT tableowner FROM pg_tables WHERE tablename = 'o'"
        ),
        "w"
    );
}
