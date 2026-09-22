//! 9.0.1 (C9) — the catalog answered a relation in a schema with the
//! relation in `public`, and `pg_dump` SEGFAULTED on a sequence.
//!
//! 9.0.0 keyed a relation by `schema\0name` but the catalog synths
//! resolved a relation's oid by the name a client READS. Two relations
//! of one name in two schemas therefore shared one oid, and a
//! sequence's oid was found only under its bare name — so
//! `pg_get_sequence_data(seqrelid)` returned no row for a sequence
//! `pg_class` and `pg_sequence` both list. Measured against
//! PostgreSQL 18.6's own `pg_dump` on the published 9.0.0:
//!
//! ```text
//!   pg_dump -d probe        → rc=139 (SIGSEGV)
//!   pg_class    → 300001|s|2200  300001|s|700000     (one oid, two rows)
//!   pg_dump     → "query returned 2 rows instead of one"
//!   conname     → "sa"      (the key `sa\0t_pkey` truncated at the NUL)
//! ```
//!
//! The pin is on the WIRE because that is where the defects are
//! observable: a stored key reaches a client as a C string and stops at
//! its NUL, and `pg_dump`'s own joins are what dereference the missing
//! row. Every assertion below was ablated — each one goes red on its
//! own when its fix is reverted.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("body");
    }
    PgMessage { ty, body }
}

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let mut out = vec![ty];
    out.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).expect("write");
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut startup = Vec::new();
    startup.extend_from_slice(&196_608u32.to_be_bytes());
    for (k, v) in [("user", "postgres"), ("database", "probe")] {
        startup.extend_from_slice(k.as_bytes());
        startup.push(0);
        startup.extend_from_slice(v.as_bytes());
        startup.push(0);
    }
    startup.push(0);
    let mut framed = ((startup.len() + 4) as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&startup);
    s.write_all(&framed).unwrap();
    loop {
        if read_message(&mut s).ty == b'Z' {
            break;
        }
    }
    s
}

/// Every row, each row's columns joined by `|`, or the error message.
/// A NULL column is an empty string, which is what `psql -tA` prints.
fn rows(s: &mut TcpStream, sql: &str) -> Vec<String> {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
    let mut out = Vec::new();
    let mut err = None;
    loop {
        let m = read_message(s);
        match m.ty {
            b'D' => {
                let n = i16::from_be_bytes([m.body[0], m.body[1]]);
                let mut at = 2usize;
                let mut cols = Vec::new();
                for _ in 0..n {
                    let len = i32::from_be_bytes([
                        m.body[at],
                        m.body[at + 1],
                        m.body[at + 2],
                        m.body[at + 3],
                    ]);
                    at += 4;
                    if len < 0 {
                        cols.push(String::new());
                    } else {
                        let len = len as usize;
                        cols.push(String::from_utf8_lossy(&m.body[at..at + len]).into_owned());
                        at += len;
                    }
                }
                out.push(cols.join("|"));
            }
            b'E' => {
                let mut i = 0;
                while i < m.body.len() && m.body[i] != 0 {
                    let code = m.body[i];
                    let start = i + 1;
                    let end = m.body[start..]
                        .iter()
                        .position(|&b| b == 0)
                        .map_or(m.body.len(), |p| start + p);
                    if code == b'M' {
                        err = Some(String::from_utf8_lossy(&m.body[start..end]).into_owned());
                    }
                    i = end + 1;
                }
            }
            b'Z' => break,
            _ => {}
        }
    }
    match err {
        Some(e) => vec![format!("ERROR: {e}")],
        None => out,
    }
}

fn one(s: &mut TcpStream, sql: &str) -> String {
    rows(s, sql).first().cloned().unwrap_or_default()
}

/// `public` and `sa` each hold a table, a view, a sequence and an index
/// OF THE SAME NAME — the shape under which a name-keyed lookup hands
/// both of them one answer.
fn two_schemas(s: &mut TcpStream) {
    for sql in [
        "CREATE SCHEMA sa",
        "CREATE TABLE t (id int primary key, a text)",
        "CREATE TABLE sa.t (id int primary key, b text)",
        "INSERT INTO t VALUES (1,'pub')",
        "INSERT INTO sa.t VALUES (9,'sa')",
        "CREATE VIEW v AS SELECT id FROM t",
        "CREATE VIEW sa.v AS SELECT id FROM sa.t",
        "CREATE SEQUENCE s START 5",
        "CREATE SEQUENCE sa.s START 50",
        "CREATE INDEX ti ON t(a)",
        "CREATE INDEX tj ON sa.t(b)",
    ] {
        assert_eq!(one(s, sql), "", "{sql}");
    }
}

#[test]
fn a_relation_in_a_schema_has_its_own_catalog_identity() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-c9surf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut c = open(addrs.pgwire.as_ref().unwrap());
    two_schemas(&mut c);

    // The floor: eight relations, or the emptiness below proves nothing.
    assert_eq!(
        one(
            &mut c,
            "SELECT count(*) FROM pg_class WHERE relnamespace <> 11 AND relname IN ('t','v','s','ti','tj','t_pkey')"
        ),
        "10",
        "two tables, two views, two sequences, two plain indexes and two primary-key indexes"
    );

    // No oid names two relations. `pg_dump` PREPAREs one statement per
    // object kind and EXECUTEs it per oid, so a shared oid stops the
    // dump outright rather than writing something wrong.
    assert!(
        rows(
            &mut c,
            "SELECT oid FROM pg_class GROUP BY oid HAVING count(*) > 1"
        )
        .is_empty(),
        "an oid names one relation"
    );

    // A sequence in a schema has a data row. This join is `pg_dump`'s
    // own; with no row for a sequence `pg_class` lists, it dereferences
    // the miss and SEGFAULTS.
    // Counting rows is the weakest thing to assert here: two rows under
    // ONE oid also count two. The oids must be the two `pg_class`
    // publishes, and each sequence's own START must come back with it.
    let seq_oids = rows(
        &mut c,
        "SELECT oid FROM pg_class WHERE relkind = 'S' ORDER BY oid",
    );
    assert_eq!(seq_oids.len(), 2, "two sequences in pg_class");
    assert_eq!(
        rows(
            &mut c,
            "SELECT seqrelid, last_value FROM pg_catalog.pg_sequence, \
             pg_get_sequence_data(seqrelid) ORDER BY seqrelid"
        ),
        vec![format!("{}|5", seq_oids[0]), format!("{}|50", seq_oids[1]),],
        "each sequence answers under its OWN oid, with its own value"
    );

    // …and it advances by its own name.
    assert_eq!(one(&mut c, "SELECT nextval('sa.s')"), "50");
    assert_eq!(one(&mut c, "SELECT nextval('s')"), "5");
    assert_eq!(one(&mut c, "SELECT currval('sa.s')"), "50");

    // A constraint is named after the BARE table name and lives in the
    // table's own schema. Built from the key it carried a NUL, and a
    // client truncates there: `sa.t`'s primary key reached `pg_dump`
    // called `sa`, and restoring that dump failed.
    let mut cons = rows(
        &mut c,
        "SELECT conname, conrelid::regclass::text FROM pg_constraint \
         WHERE contype = 'p' ORDER BY 2",
    );
    cons.sort();
    assert_eq!(
        cons,
        vec!["t_pkey|sa.t".to_string(), "t_pkey|t".to_string()]
    );
    assert_eq!(
        one(
            &mut c,
            "SELECT count(DISTINCT connamespace) FROM pg_constraint WHERE contype = 'p'"
        ),
        "2",
        "each constraint is in its own table's schema"
    );

    // Nothing in a schema is TEMPORARY. `relpersistence` was derived
    // from "the key differs from the name", which is true of every
    // relation in a schema — and a temporary relation in a non-temp
    // schema is a contradiction `pg_dump` does not survive.
    assert_eq!(
        one(
            &mut c,
            "SELECT count(*) FROM pg_class WHERE relnamespace <> 11 AND relpersistence <> 'p'"
        ),
        "0"
    );

    // A view's body reaches a client whole. The key's separator
    // truncated it, and `pg_dump` reports a view it cannot read as
    // "appears to be empty".
    assert_eq!(
        one(&mut c, "SELECT pg_get_viewdef('sa.v'::regclass)"),
        " SELECT id\n   FROM sa.t;"
    );
}

#[test]
fn a_relation_is_written_with_the_qualifier_a_client_would_need() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-c9qual-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut c = open(addrs.pgwire.as_ref().unwrap());
    two_schemas(&mut c);

    // PostgreSQL 18.6, measured with a `tz` in `public` and one in `q3`:
    //   search_path=public     'q3.tz'::regclass::text  → q3.tz
    //   search_path=q3,public  'q3.tz'::regclass::text  → tz
    //                          'public.tz'::regclass    → public.tz
    // The qualifier appears exactly when the bare name would reach a
    // DIFFERENT relation — including `public.`, which `display_key`
    // alone can never write.
    assert_eq!(one(&mut c, "SET search_path = public"), "");
    assert_eq!(
        one(
            &mut c,
            "SELECT 'sa.t'::regclass::text, 'public.t'::regclass::text"
        ),
        "sa.t|t"
    );
    assert_eq!(one(&mut c, "SET search_path = sa, public"), "");
    assert_eq!(
        one(
            &mut c,
            "SELECT 'sa.t'::regclass::text, 'public.t'::regclass::text"
        ),
        "t|public.t"
    );

    // A written qualifier is not the search path's to reinterpret. A
    // relation in `public` is keyed by its bare name, so `public.t`
    // reached the path resolver and came back as the one in `sa`.
    assert_eq!(
        one(
            &mut c,
            "SELECT 'public.t'::regclass::oid = 'sa.t'::regclass::oid"
        ),
        "f"
    );
    assert_eq!(one(&mut c, "SELECT a FROM public.t"), "pub");

    // …and the same rule answers from the oid side, which is how every
    // catalog join renders a relation.
    assert_eq!(one(&mut c, "SET search_path = public"), "");
    assert_eq!(
        one(
            &mut c,
            "SELECT oid::regclass::text FROM pg_class \
             WHERE relname = 't' AND relkind = 'r' AND relnamespace <> 2200"
        ),
        "sa.t"
    );
    assert_eq!(one(&mut c, "SELECT to_regclass('sa.t')::text"), "sa.t");
}

#[test]
fn a_written_qualifier_beats_the_search_path() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-c9write-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut c = open(addrs.pgwire.as_ref().unwrap());
    two_schemas(&mut c);
    assert_eq!(one(&mut c, "SET search_path = sa, public"), "");

    // `public`'s relations are keyed by their bare names, so nothing in
    // the key says whether `t` or `public.t` was written — and the path
    // was walked for both. Every one of these read or wrote `sa`'s
    // table. The right-hand column is PostgreSQL 18.6 on the same
    // statements, measured 2026-09-22.
    assert_eq!(rows(&mut c, "SELECT a FROM public.t ORDER BY 1"), ["pub"]);
    assert_eq!(one(&mut c, "INSERT INTO public.t VALUES (3,'ins')"), "");
    assert_eq!(one(&mut c, "SELECT count(*) FROM public.t"), "2");
    assert_eq!(
        one(&mut c, "SELECT count(*) FROM sa.t"),
        "1",
        "and not the other one"
    );
    assert_eq!(one(&mut c, "UPDATE public.t SET a = 'x' WHERE id = 1"), "");
    assert_eq!(one(&mut c, "SELECT a FROM public.t WHERE id = 1"), "x");
    assert_eq!(
        one(&mut c, "SELECT b FROM sa.t"),
        "sa",
        "sa's row is untouched"
    );
    assert_eq!(one(&mut c, "DELETE FROM public.t WHERE id = 3"), "");
    assert_eq!(one(&mut c, "SELECT count(*) FROM public.t"), "1");
    assert_eq!(one(&mut c, "SELECT count(*) FROM sa.t"), "1");

    // The floor: an UNqualified name still follows the path, which is
    // the rule the qualifier is an exception to.
    assert_eq!(one(&mut c, "SELECT b FROM t"), "sa");
}

/// 9.0.2 — sentori's reply to 9.0.1 §2: a written schema was still the
/// search path's to reinterpret OUTSIDE DML — in DDL, in a view's body
/// and in what `pg_dump` reads back. Each expectation is PostgreSQL 18.6's
/// answer on the same statements.
#[test]
fn a_written_schema_is_final_in_ddl_views_and_deparse() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-c9ddl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut c = open(addrs.pgwire.as_ref().unwrap());
    for sql in [
        "CREATE SCHEMA sa",
        "CREATE TABLE public.t (id int PRIMARY KEY, who text)",
        "CREATE TABLE sa.t (id int PRIMARY KEY, who text)",
        "INSERT INTO public.t VALUES (1, 'public')",
        "INSERT INTO sa.t VALUES (1, 'sa')",
        "CREATE INDEX t_v ON public.t (who)",
        "CREATE INDEX t_v ON sa.t (who)",
        "CREATE TABLE sa.s (id serial PRIMARY KEY, v text)",
        "SET search_path = sa, public",
    ] {
        assert_eq!(one(&mut c, sql), "", "{sql}");
    }

    // A qualified CREATE lands where it says. 9.0.1 put all four in `sa`.
    for sql in [
        "CREATE TABLE public.t2 (i int)",
        "CREATE INDEX t2_i ON public.t2 (i)",
        "CREATE VIEW public.v2 AS SELECT who FROM public.t",
        "CREATE SEQUENCE public.q2",
    ] {
        assert_eq!(one(&mut c, sql), "", "{sql}");
    }
    let mut placed = rows(
        &mut c,
        "SELECT n.nspname::text || '.' || c.relname::text FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relname IN ('t2', 't2_i', 'v2', 'q2')",
    );
    placed.sort();
    assert_eq!(
        placed,
        ["public.q2", "public.t2", "public.t2_i", "public.v2"]
    );

    // A view's names are bound when it is CREATED. v1 names `public.` and
    // must read public's row; v3 names nothing and must keep the `sa.t`
    // the path gave it at CREATE, whatever the reader's path is.
    assert_eq!(
        one(&mut c, "CREATE VIEW sa.v1 AS SELECT who FROM public.t"),
        ""
    );
    assert_eq!(one(&mut c, "CREATE VIEW sa.v3 AS SELECT who FROM t"), "");
    assert_eq!(one(&mut c, "SELECT who FROM sa.v1"), "public");
    assert_eq!(one(&mut c, "SELECT who FROM sa.v3"), "sa");
    assert_eq!(one(&mut c, "SET search_path = public"), "");
    assert_eq!(one(&mut c, "SELECT who FROM sa.v1"), "public");
    assert_eq!(
        one(&mut c, "SELECT who FROM sa.v3"),
        "sa",
        "bound at CREATE, not at read"
    );

    // A three-part column reference names the middle part's relation.
    assert_eq!(one(&mut c, "SELECT public.t.who FROM public.t"), "public");

    // What `pg_dump` reads, under its empty path. Each of these named
    // `public`'s object for `sa`'s — the oid was turned into a name and
    // searched again — or wrote `public.sa.t`, which is no name at all.
    assert_eq!(one(&mut c, "SET search_path = ''"), "");
    let sa_view = one(
        &mut c,
        "SELECT pg_get_viewdef(c.oid) FROM pg_class c JOIN pg_namespace n \
         ON n.oid = c.relnamespace WHERE n.nspname = 'sa' AND c.relname = 'v3'",
    );
    assert!(sa_view.contains("FROM sa.t"), "{sa_view}");
    let mut defs = rows(
        &mut c,
        "SELECT pg_get_indexdef(indexrelid) FROM pg_index i JOIN pg_class c \
         ON c.oid = i.indexrelid WHERE c.relname = 't_v'",
    );
    defs.sort();
    assert_eq!(
        defs,
        [
            "CREATE INDEX t_v ON public.t USING btree (who)",
            "CREATE INDEX t_v ON sa.t USING btree (who)",
        ]
    );
    assert_eq!(
        one(
            &mut c,
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef \
             WHERE adrelid = 'sa.s'::regclass"
        ),
        "nextval('sa.s_id_seq'::regclass)"
    );
    // …and bare again once the path reaches the sequence, as PostgreSQL
    // writes it.
    assert_eq!(one(&mut c, "SET search_path = sa, public"), "");
    assert_eq!(
        one(
            &mut c,
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef \
             WHERE adrelid = 'sa.s'::regclass"
        ),
        "nextval('s_id_seq'::regclass)"
    );
}
