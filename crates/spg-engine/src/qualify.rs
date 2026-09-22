//! 8.0.3 — the names a deparse writes, schema-qualified when the session's
//! search_path would not find them bare.
//!
//! PostgreSQL's deparse functions (`pg_get_viewdef`, `format_type`,
//! `pg_get_constraintdef`, `pg_get_expr`, …) write an object's name bare
//! when its schema is on the search path and qualified when it is not.
//! Measured on 18.6 with a view over a join, a user function and a
//! composite cast: under the default path `FROM rich1 r`, `f1(…)`,
//! `::addr`; under `search_path = ''` `FROM public.rich1 r`,
//! `public.f1(…)`, `::public.addr`.
//!
//! `pg_dump` sets the path to `''` before it reads a single definition,
//! and its output restores under that same empty path. SPG wrote every
//! name bare, so a restore into PostgreSQL stopped on the first view,
//! foreign key or serial default it met: `relation "rich1_id_seq" does
//! not exist`. sentori's §4.4.
//!
//! SPG has one schema holding user objects, `public`, so the question is
//! only ever whether `public` is on the path.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{CastTarget, CteBody, Expr, Literal, SelectItem, SelectStatement, TableRef};
use spg_storage::Catalog;

/// Whether a bare name of a user object would NOT resolve under
/// `search_path`: no entry names `public`, directly or as the session
/// user's `$user`.
pub(crate) fn public_hidden(search_path: Option<&str>, session_user: &str) -> bool {
    let path = search_path.unwrap_or("\"$user\", public");
    !path.split(',').any(|raw| {
        let entry = raw.trim().trim_matches('"');
        entry == "public" || (entry == "$user" && session_user == "public")
    })
}

/// `name`, qualified with `public.` when `hidden`.
pub(crate) fn user_object_name(name: &str, hidden: bool) -> String {
    if hidden {
        alloc::format!("public.{name}")
    } else {
        String::from(name)
    }
}

/// Qualify every user object a SELECT names: relations, user functions,
/// user types in casts, and relation names in `'…'::regclass` literals.
/// A CTE name is not a relation and stays bare.
pub(crate) fn qualify_select(s: &mut SelectStatement, cat: &Catalog, outer_ctes: &[String]) {
    let mut ctes: Vec<String> = s.ctes.iter().map(|c| c.name.clone()).collect();
    ctes.extend(outer_ctes.iter().cloned());
    for cte in &mut s.ctes {
        if let CteBody::Select(body) = &mut cte.body {
            qualify_select(body, cat, &ctes);
        }
    }
    for item in &mut s.items {
        if let SelectItem::Expr { expr, .. } = item {
            qualify_expr(expr, cat, &ctes);
        }
    }
    if let Some(from) = &mut s.from {
        qualify_table_ref(&mut from.primary, cat, &ctes);
        for j in &mut from.joins {
            qualify_table_ref(&mut j.table, cat, &ctes);
            if let Some(on) = &mut j.on {
                qualify_expr(on, cat, &ctes);
            }
        }
    }
    for e in s
        .where_
        .iter_mut()
        .chain(s.having.iter_mut())
        .chain(s.group_by.iter_mut().flatten())
        .chain(s.order_by.iter_mut().map(|o| &mut o.expr))
    {
        qualify_expr(e, cat, &ctes);
    }
    for (_, peer) in &mut s.unions {
        qualify_select(peer, cat, &ctes);
    }
}

fn qualify_table_ref(t: &mut TableRef, cat: &Catalog, ctes: &[String]) {
    if let Some(sub) = &mut t.lateral_subquery {
        qualify_select(sub, cat, ctes);
        return;
    }
    if !is_plain_relation_ref(t) || ctes.contains(&t.name) {
        return;
    }
    // 9.0.2 (C9) — through the one rule, which knows the relation's OWN
    // schema. Prefixing `public.` onto whatever name was stored wrote
    // `public.sa.t` for a relation in `sa` — not a name at all — and
    // that is what `pg_dump` read back (sentori, measured on 9.0.1).
    if let Some(written) = written_relation_name(cat, &t.name, t.qualified, true) {
        t.name = written;
    } else if is_relation(cat, &t.name) {
        t.name = user_object_name(&t.name, true);
    }
}

fn is_plain_relation_ref(t: &TableRef) -> bool {
    t.lateral_subquery.is_none()
        && t.unnest_expr.is_none()
        && t.generate_series_args.is_none()
        && t.table_fn_call.is_none()
        && t.json_table.is_none()
}

/// 9.0.2 (C9) — the stored KEY a relation reference names: exactly the
/// written key when the client wrote a schema, else what the session's
/// search path reaches.
pub(crate) fn stored_relation_key(cat: &Catalog, name: &str, qualified: bool) -> Option<String> {
    if let Some(t) = cat.get_written(name, qualified) {
        return Some(t.schema().name.clone());
    }
    // A serial column's sequence is synthesised on demand and is in no
    // registry until the first insert needs it; it is still a relation.
    let implicit: Vec<String> = crate::sequence::implicit_sequences(cat)
        .into_iter()
        .map(|d| d.name)
        .collect();
    let exact = qualified || spg_sql::namespace::is_qualified(name);
    if exact {
        if cat.views_all().contains_key(name)
            || cat.materialized_views().contains_key(name)
            || cat.sequences_all().contains_key(name)
            || implicit.iter().any(|k| k == name)
        {
            return Some(String::from(name));
        }
        return None;
    }
    if let Some(k) = path_order(cat)
        .into_iter()
        .map(|s| spg_sql::namespace::qualified_key(&s, name))
        .find(|k| implicit.contains(k))
    {
        return Some(k);
    }
    if let Some(v) = cat.view(name) {
        return Some(v.name.clone());
    }
    if cat.materialized_views().contains_key(name) {
        return Some(String::from(name));
    }
    // A sequence: the key the path reaches it by.
    cat.path_key(name)
        .filter(|k| cat.sequences_all().contains_key(k))
}

/// The schemas a bare name is looked for in, in order; a session that
/// never set a path looks in `public`.
fn path_order(cat: &Catalog) -> Vec<String> {
    if cat.search_path().is_empty() {
        alloc::vec![String::from(spg_sql::namespace::PUBLIC)]
    } else {
        cat.search_path().to_vec()
    }
}

/// 9.0.2 (C9) — how a deparse WRITES a relation: bare when the
/// session's search path reaches that very relation by its bare name,
/// qualified by its own schema otherwise. `public_hidden` is the
/// `search_path = ''` case `pg_dump` runs under, where even `public`
/// is off the path. `None` when the name is no relation of the catalog.
pub(crate) fn written_relation_name(
    cat: &Catalog,
    name: &str,
    qualified: bool,
    public_hidden: bool,
) -> Option<String> {
    let key = stored_relation_key(cat, name, qualified)?;
    let bare = String::from(cat.listed_name(&key)?);
    let schema = cat.listed_schema(&key);
    let in_public = schema == spg_sql::namespace::PUBLIC;
    // What a client writing the bare name reaches: the catalog's own path
    // walk, or — for a serial's sequence, which is in no registry until
    // it is first used — the first schema on the path that has one.
    let path_reaches = cat.path_key(&bare).as_deref() == Some(key.as_str())
        || (cat.path_key(&bare).is_none()
            && path_order(cat)
                .into_iter()
                .map(|s| spg_sql::namespace::qualified_key(&s, &bare))
                .find(|k| {
                    crate::sequence::implicit_sequences(cat)
                        .iter()
                        .any(|d| &d.name == k)
                })
                .as_deref()
                == Some(key.as_str()));
    let reached = path_reaches && !(in_public && public_hidden);
    Some(if reached {
        bare
    } else if in_public {
        alloc::format!("public.{bare}")
    } else {
        key
    })
}

/// Every relation reference a SELECT makes, subqueries and CTE bodies
/// included, with the CTE names in scope at that reference.
fn for_each_relation_ref(
    s: &mut SelectStatement,
    outer_ctes: &[String],
    f: &mut dyn FnMut(&mut TableRef, &[String]),
) {
    let mut ctes: Vec<String> = s.ctes.iter().map(|c| c.name.clone()).collect();
    ctes.extend(outer_ctes.iter().cloned());
    for cte in &mut s.ctes {
        if let CteBody::Select(body) = &mut cte.body {
            for_each_relation_ref(body, &ctes, f);
        }
    }
    let mut in_expr = |e: &mut Expr, f: &mut dyn FnMut(&mut TableRef, &[String])| {
        let Ok(()) =
            e.for_each_node_mut::<core::convert::Infallible>(&mut |_| Ok(()), &mut |sub| {
                for_each_relation_ref(sub, &ctes, f);
                Ok(())
            });
    };
    for item in &mut s.items {
        if let SelectItem::Expr { expr, .. } = item {
            in_expr(expr, f);
        }
    }
    if let Some(from) = &mut s.from {
        let mut refs: Vec<&mut TableRef> = alloc::vec![&mut from.primary];
        for j in &mut from.joins {
            if let Some(on) = &mut j.on {
                in_expr(on, f);
            }
            refs.push(&mut j.table);
        }
        for t in refs {
            if let Some(sub) = &mut t.lateral_subquery {
                for_each_relation_ref(sub, &ctes, f);
            } else if is_plain_relation_ref(t) && !ctes.contains(&t.name) {
                f(t, &ctes);
            }
        }
    }
    for e in s
        .where_
        .iter_mut()
        .chain(s.having.iter_mut())
        .chain(s.group_by.iter_mut().flatten())
        .chain(s.order_by.iter_mut().map(|o| &mut o.expr))
    {
        in_expr(e, f);
    }
    for (_, peer) in &mut s.unions {
        for_each_relation_ref(peer, &ctes, f);
    }
}

/// 9.0.2 (C9) — bind a view body's relation names when the view is
/// CREATED, as PostgreSQL does.
///
/// SPG stored the text and resolved it on every read, so the rows a view
/// returned depended on the READER's `search_path`. Measured by sentori on
/// 9.0.1 with a `t` in `public` and in `sa`, under `search_path = sa,
/// public`: `CREATE VIEW sa.v1 AS SELECT … FROM public.t` read `sa`'s
/// table, and `CREATE VIEW sa.v3 AS SELECT … FROM t` read `public`'s
/// once the reader's path was `public`. PostgreSQL 18.6 reads `public`
/// for v1 and `sa` for v3, under every path.
pub(crate) fn bind_relations(s: &mut SelectStatement, cat: &Catalog) {
    for_each_relation_ref(s, &[], &mut |t, _| {
        if let Some(key) = stored_relation_key(cat, &t.name, t.qualified) {
            t.name = key;
            t.qualified = true;
        }
    });
}

/// 9.0.2 (C9) — write each relation of a bound body the way the session's
/// path needs it (see [`written_relation_name`]). The deparse half of
/// [`bind_relations`]: a bound body names every relation exactly, and a
/// reader under the default path should still see `FROM t`.
pub(crate) fn write_relations_for_path(
    s: &mut SelectStatement,
    cat: &Catalog,
    public_hidden: bool,
) {
    for_each_relation_ref(s, &[], &mut |t, _| {
        if let Some(written) = written_relation_name(cat, &t.name, t.qualified, public_hidden) {
            t.name = written;
            t.qualified = false;
        }
    });
}

/// Qualify the user objects an expression names. `true` when anything
/// changed, so a caller holding the expression's source text re-renders
/// only when it has to.
pub(crate) fn qualify_expr(e: &mut Expr, cat: &Catalog, ctes: &[String]) -> bool {
    let changed = core::cell::Cell::new(false);
    let Ok(()) = e.for_each_node_mut::<core::convert::Infallible>(
        &mut |node| {
            match node {
                Expr::FunctionCall { name, .. }
                    if !name.contains('.') && !cat.functions_named(name).is_empty() =>
                {
                    *name = user_object_name(name, true);
                    changed.set(true);
                }
                Expr::Cast {
                    target: CastTarget::Named(ty),
                    ..
                } if !ty.contains('.') && is_user_type(cat, ty) => {
                    *ty = user_object_name(ty, true);
                    changed.set(true);
                }
                Expr::Cast {
                    expr,
                    target: CastTarget::RegClass,
                } => {
                    // 9.0.2 (C9) — through the one rule, which knows the
                    // relation's OWN schema; prefixing `public.` named a
                    // sequence that does not exist for a serial in `sa`.
                    if let Expr::Literal(Literal::String(rel)) = expr.as_mut()
                        && let Some(w) = written_regclass_literal(cat, rel, true)
                        && w != *rel
                    {
                        *rel = w;
                        changed.set(true);
                    }
                }
                _ => {}
            }
            Ok(())
        },
        &mut |sub| {
            qualify_select(sub, cat, ctes);
            changed.set(true);
            Ok(())
        },
    );
    changed.get()
}

/// 9.0.2 (C9) — a `'…'::regclass` literal written the way the session's
/// path needs it, by [`written_relation_name`]'s rule.
fn written_regclass_literal(cat: &Catalog, rel: &str, public_hidden: bool) -> Option<String> {
    let key = spg_sql::namespace::key_from_text(rel);
    let written = written_relation_name(cat, &key, rel.contains('.'), public_hidden)?;
    Some(spg_sql::namespace::display_key(&written))
}

/// 9.0.2 (C9) — `pg_get_expr` under a path that DOES reach `public`: a
/// relation named in a `::regclass` literal is still written by its own
/// schema's visibility. PostgreSQL 18.6 writes a serial default in `zz`
/// as `nextval('zz.s_id_seq'::regclass)` until `zz` joins the path, and
/// `nextval('s_id_seq'::regclass)` after. `true` when anything changed.
pub(crate) fn write_regclass_for_path(e: &mut Expr, cat: &Catalog) -> bool {
    let changed = core::cell::Cell::new(false);
    let Ok(()) = e.for_each_node_mut::<core::convert::Infallible>(
        &mut |node| {
            if let Expr::Cast {
                expr,
                target: CastTarget::RegClass,
            } = node
                && let Expr::Literal(Literal::String(rel)) = expr.as_mut()
                && let Some(w) = written_regclass_literal(cat, rel, false)
                && w != *rel
            {
                *rel = w;
                changed.set(true);
            }
            Ok(())
        },
        &mut |_| Ok(()),
    );
    changed.get()
}

fn is_relation(cat: &Catalog, name: &str) -> bool {
    cat.get(name).is_some()
        || cat.has_view(name)
        || cat.has_sequence(name)
        || crate::sequence::implicit_sequences(cat)
            .iter()
            .any(|d| d.name == name)
}

pub(crate) fn is_user_type(cat: &Catalog, name: &str) -> bool {
    cat.enum_types().contains_key(name)
        || cat.domain_types().contains_key(name)
        || cat.composite_types().contains_key(name)
}

/// A call argument as PostgreSQL prints it: a typed string literal is
/// `'x'::regclass`, where SPG's Display writes `('x')::regclass`.
pub(crate) fn render_call_argument(a: &Expr) -> String {
    match a {
        Expr::Cast { expr, target } => match expr.as_ref() {
            Expr::Literal(Literal::String(lit)) => {
                alloc::format!("'{}'::{target}", lit.replace('\'', "''"))
            }
            _ => alloc::format!("{a}"),
        },
        other => alloc::format!("{other}"),
    }
}

/// A stored expression re-rendered after qualification: a call's typed
/// literal arguments keep PostgreSQL's spelling (the form `pg_get_expr`
/// printed before qualifying), everything else is Display.
pub(crate) fn render_qualified_expr(e: &Expr) -> String {
    match e {
        Expr::FunctionCall { name, args, .. } => {
            let rendered: Vec<String> = args.iter().map(render_call_argument).collect();
            alloc::format!("{name}({})", rendered.join(", "))
        }
        other => render_call_argument(other),
    }
}
