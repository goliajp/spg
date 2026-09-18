//! 8.0.3 — a CHECK constraint's and an index predicate's text, in the form
//! PostgreSQL's catalog deparse gives it.
//!
//! SPG keeps these as the text of the expression as it was written, and
//! PostgreSQL prints the ANALYZED expression: a string literal carries the
//! type it was resolved to, an IN list is the array comparison it becomes,
//! LIKE is its operator. Measured on 18.6:
//!
//! ```text
//!   written                       PostgreSQL prints
//!   s IN ('x', 'y')               (s = ANY (ARRAY['x'::text, 'y'::text]))
//!   s NOT IN ('z')                (s <> 'z'::text)
//!   status = 'pending'            (status = 'pending'::text)
//!   m <> 'a'   (m an enum)        (m <> 'a'::md)
//!   s LIKE 'a%'                   (s ~~ 'a%'::text)
//!   WHERE b    (a boolean column) WHERE b
//! ```
//!
//! `pg_dump` copies that text into the dump, so a dump of SPG and a dump
//! of PostgreSQL disagreed on every such constraint — sentori's schema has
//! eleven.
//!
//! Only what can be typed without evaluating is rewritten: a literal is
//! annotated when it meets a text-family or enum column, whose constant
//! PostgreSQL prints unchanged. A literal meeting any other type (PG
//! normalises a timestamp's text, casts a numeric's integer) and every
//! node this file does not know keep SPG's own rendering.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{BinOp, Expr, Literal, UnOp};
use spg_storage::{ColumnSchema, DataType};

/// The catalog form of a stored predicate, or `None` when it does not
/// parse (the stored text then stands).
///
/// `qualify_in` is the catalog when the reader's search path leaves
/// `public` out: user functions and an enum literal's type are then written
/// qualified, as PG does (see `qualify`).
pub(crate) fn predicate_text(
    src: &str,
    cols: &[ColumnSchema],
    qualify_in: Option<&spg_storage::Catalog>,
) -> Option<String> {
    let mut e = spg_sql::parser::parse_expression(src.trim()).ok()?;
    if let Some(cat) = qualify_in {
        crate::qualify::qualify_expr(&mut e, cat, &[]);
    }
    let ctx = Ctx {
        cols,
        qualify: qualify_in.is_some(),
        domain: false,
    };
    Some(render(&e, &ctx))
}

/// [`predicate_text`] for a domain's CHECK, where the one column is the
/// keyword `VALUE`, which PG prints in capitals.
pub(crate) fn domain_check_text(
    src: &str,
    base: DataType,
    qualify_in: Option<&spg_storage::Catalog>,
) -> Option<String> {
    let value = [ColumnSchema::new(VALUE, base, true)];
    let mut e = spg_sql::parser::parse_expression(src.trim()).ok()?;
    if let Some(cat) = qualify_in {
        crate::qualify::qualify_expr(&mut e, cat, &[]);
    }
    let Ok(()) = e.for_each_node_mut::<core::convert::Infallible>(
        &mut |node| {
            if let Expr::Column(c) = node
                && c.qualifier.is_none()
                && c.name.eq_ignore_ascii_case(VALUE)
            {
                c.name = String::from(VALUE);
            }
            Ok(())
        },
        &mut |_| Ok(()),
    );
    let ctx = Ctx {
        cols: &value,
        qualify: qualify_in.is_some(),
        domain: true,
    };
    Some(render(&e, &ctx))
}

const VALUE: &str = "VALUE";

/// What rendering needs besides the expression.
struct Ctx<'a> {
    cols: &'a [ColumnSchema],
    qualify: bool,
    /// Rendering a domain's CHECK, whose `VALUE` is a keyword and not a
    /// column name to quote.
    domain: bool,
}

/// The SQL type a literal compared with `e` is printed as, when `e` is a
/// column whose constants print unchanged.
fn literal_type(e: &Expr, cx: &Ctx<'_>) -> Option<String> {
    let Expr::Column(c) = e else {
        // A call typed text by its own signature (`lower(s)`).
        return crate::describe::describe_expr(e, cx.cols)
            .filter(|sh| matches!(sh.ty, DataType::Text))
            .map(|_| String::from("text"));
    };
    let col = cx.cols.iter().find(|k| k.name == c.name)?;
    if let Some(enum_name) = &col.user_enum_type {
        return Some(crate::qualify::user_object_name(enum_name, cx.qualify));
    }
    matches!(col.ty, DataType::Text | DataType::Varchar(_)).then(|| String::from("text"))
}

fn is_varchar_column(e: &Expr, cx: &Ctx<'_>) -> bool {
    matches!(e, Expr::Column(c) if cx.cols.iter().any(|k| {
        k.name == c.name && matches!(k.ty, DataType::Varchar(_)) && k.user_enum_type.is_none()
    }))
}

/// A varchar column compared with text is printed with its implicit cast.
fn operand(e: &Expr, cx: &Ctx<'_>) -> String {
    if let Expr::Column(c) = e
        && cx.cols.iter().any(|k| {
            k.name == c.name && matches!(k.ty, DataType::Varchar(_)) && k.user_enum_type.is_none()
        })
    {
        return format!("({})::text", c.name);
    }
    render(e, cx)
}

fn typed_literal(e: &Expr, ty: Option<&str>, cx: &Ctx<'_>) -> String {
    match (e, ty) {
        (Expr::Literal(Literal::String(s)), Some(t)) => format!("'{}'::{t}", s.replace('\'', "''")),
        _ => render(e, cx),
    }
}

fn render(e: &Expr, cx: &Ctx<'_>) -> String {
    match e {
        Expr::Binary { lhs, op, rhs } if matches!(op, BinOp::And | BinOp::Or) => {
            let mut parts: Vec<String> = Vec::new();
            chain(lhs, *op, cx, &mut parts);
            parts.push(render(rhs, cx));
            format!("({})", parts.join(&format!(" {op} ")))
        }
        Expr::Binary { lhs, op, rhs }
            if matches!(
                op,
                BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
            ) =>
        {
            let ty = literal_type(lhs, cx).or_else(|| literal_type(rhs, cx));
            format!(
                "({} {op} {})",
                side(lhs, ty.as_deref(), cx),
                side(rhs, ty.as_deref(), cx)
            )
        }
        Expr::Unary {
            op: UnOp::Not,
            expr,
        } => format!("(NOT {})", render(expr, cx)),
        Expr::IsNull { expr, negated } => {
            let kw = if *negated { "IS NOT NULL" } else { "IS NULL" };
            format!("({} {kw})", render(expr, cx))
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let ty = literal_type(expr, cx);
            let items: Vec<String> = list
                .iter()
                .map(|i| typed_literal(i, ty.as_deref(), cx))
                .collect();
            let lhs = operand(expr, cx);
            match (items.as_slice(), negated) {
                ([one], false) => format!("({lhs} = {one})"),
                ([one], true) => format!("({lhs} <> {one})"),
                (_, false) => format!("({lhs} = ANY (ARRAY[{}]))", items.join(", ")),
                (_, true) => format!("({lhs} <> ALL (ARRAY[{}]))", items.join(", ")),
            }
        }
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
            ..
        } => {
            let op = match (negated, case_insensitive) {
                (false, false) => "~~",
                (true, false) => "!~~",
                (false, true) => "~~*",
                (true, true) => "!~~*",
            };
            let ty = literal_type(expr, cx);
            format!(
                "({} {op} {})",
                operand(expr, cx),
                typed_literal(pattern, ty.as_deref(), cx)
            )
        }
        // A varchar column handed to a function is printed with the
        // implicit cast PG applies (`length((name)::text)`): SPG's catalog
        // functions over character data all take text.
        Expr::FunctionCall { name, args, .. } if args.iter().any(|a| is_varchar_column(a, cx)) => {
            let rendered: Vec<String> = args.iter().map(|a| operand(a, cx)).collect();
            format!("{name}({})", rendered.join(", "))
        }
        Expr::Column(c) if cx.domain && c.qualifier.is_none() && c.name == VALUE => {
            String::from(VALUE)
        }
        // A trigger's row reference, as PG prints it: lower case.
        Expr::Column(c)
            if c.qualifier.as_deref().is_some_and(|q| {
                q.eq_ignore_ascii_case("new") || q.eq_ignore_ascii_case("old")
            }) =>
        {
            let q = c
                .qualifier
                .as_deref()
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            format!("{q}.{}", c.name)
        }
        // What a dump of either engine hands back: the forms above,
        // already analyzed. Printed the same way, so a restored dump dumps
        // to itself.
        Expr::AnyAll {
            expr,
            op,
            array,
            is_any,
        } => {
            let kw = if *is_any { "ANY" } else { "ALL" };
            let ty = literal_type(expr, cx);
            let array = match array.as_ref() {
                Expr::Array(items) => {
                    let items: Vec<String> = items
                        .iter()
                        .map(|i| typed_literal(i, ty.as_deref(), cx))
                        .collect();
                    format!("ARRAY[{}]", items.join(", "))
                }
                other => render(other, cx),
            };
            format!("({} {op} {kw} ({array}))", operand(expr, cx))
        }
        Expr::Cast { expr, target } => match expr.as_ref() {
            Expr::Literal(Literal::String(lit)) => {
                format!("'{}'::{target}", lit.replace('\'', "''"))
            }
            _ => format!("{e}"),
        },
        other => format!("{other}"),
    }
}

fn side(e: &Expr, ty: Option<&str>, cx: &Ctx<'_>) -> String {
    match e {
        Expr::Literal(Literal::String(_)) => typed_literal(e, ty, cx),
        _ if ty.is_some() => operand(e, cx),
        _ => render(e, cx),
    }
}

fn chain(e: &Expr, op: BinOp, cx: &Ctx<'_>, parts: &mut Vec<String>) {
    if let Expr::Binary {
        lhs,
        op: inner,
        rhs,
    } = e
        && *inner == op
    {
        chain(lhs, op, cx, parts);
        parts.push(render(rhs, cx));
        return;
    }
    parts.push(render(e, cx));
}
