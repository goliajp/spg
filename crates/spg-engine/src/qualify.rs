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
    let plain = t.unnest_expr.is_none()
        && t.generate_series_args.is_none()
        && t.table_fn_call.is_none()
        && t.json_table.is_none();
    if plain && !ctes.contains(&t.name) && is_relation(cat, &t.name) {
        t.name = user_object_name(&t.name, true);
    }
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
                    if let Expr::Literal(Literal::String(rel)) = expr.as_mut()
                        && !rel.contains('.')
                        && is_relation(cat, rel)
                    {
                        *rel = user_object_name(rel, true);
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
        Expr::FunctionCall { name, args } => {
            let rendered: Vec<String> = args.iter().map(render_call_argument).collect();
            alloc::format!("{name}({})", rendered.join(", "))
        }
        other => render_call_argument(other),
    }
}
