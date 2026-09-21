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
    predicate_text_in(src, cols, qualify_in, false)
}

/// [`predicate_text`], in PostgreSQL's pretty form when `pretty`.
pub(crate) fn predicate_text_in(
    src: &str,
    cols: &[ColumnSchema],
    qualify_in: Option<&spg_storage::Catalog>,
    pretty: bool,
) -> Option<String> {
    let mut e = spg_sql::parser::parse_expression(src.trim()).ok()?;
    if let Some(cat) = qualify_in {
        crate::qualify::qualify_expr(&mut e, cat, &[]);
    }
    let ctx = Ctx {
        cols,
        qualify: qualify_in.is_some(),
        pretty,
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
        pretty: false,
        domain: true,
    };
    Some(render(&e, &ctx))
}

const VALUE: &str = "VALUE";

/// What rendering needs besides the expression.
struct Ctx<'a> {
    cols: &'a [ColumnSchema],
    qualify: bool,
    /// 9.0.0 — PG's PRETTY form: the same analysed expression, with the
    /// parentheses the grammar can put back left out. `pg_get_indexdef(…,
    /// pretty)` and `pg_get_constraintdef(…, true)` ask for it, and SPG
    /// answered them from a SECOND deparser (`spg_sql::ast::pretty_expr`)
    /// that did not type a literal — so the pretty CHECK read `a <> 'q'`
    /// where PostgreSQL reads `a <> 'q'::text`.
    pretty: bool,
    /// Rendering a domain's CHECK, whose `VALUE` is a keyword and not a
    /// column name to quote.
    domain: bool,
}

/// The SQL type a literal compared with `e` is printed as.
///
/// 9.0.0 — every type where PostgreSQL's deparse is reproducible without
/// evaluating anything but the literal itself, not text alone. Measured
/// on 18.6:
///
/// ```text
///   n numeric  CHECK (n > 0)          ((n > (0)::numeric))
///   f float8   CHECK (f > 1)          ((f > (1)::double precision))
///   r real     CHECK (r > 1)          ((r > (1)::double precision))
///   d date     CHECK (d > '2020-1-1') ((d > '2020-01-01'::date))
///   u uuid     CHECK (u <> '…A')      ((u <> '…a'::uuid))
///   bi bigint  CHECK (bi > 1)         ((bi > 1))          -- no cast node
///   i int      CHECK (i > 1)          ((i > 1))
/// ```
///
/// `timestamptz` is left out on purpose: PostgreSQL folds the constant at
/// CREATE time and prints it in the session's zone, so the text depends
/// on when and where the statement ran, which SPG's stored source does
/// not record.
fn literal_type(e: &Expr, cx: &Ctx<'_>) -> Option<String> {
    let ty = match e {
        Expr::Column(c) => {
            let col = cx.cols.iter().find(|k| k.name == c.name)?;
            if let Some(enum_name) = &col.user_enum_type {
                return Some(crate::qualify::user_object_name(enum_name, cx.qualify));
            }
            col.ty
        }
        // A call or a cast typed by its own signature (`lower(s)`,
        // `(i)::numeric`).
        other => crate::describe::describe_expr(other, cx.cols)?.ty,
    };
    pg_literal_type_name(ty)
}

/// The name PostgreSQL's deparse gives a constant of this type, or `None`
/// where a constant of it is printed bare.
fn pg_literal_type_name(ty: DataType) -> Option<String> {
    let name = match ty {
        DataType::Text | DataType::Varchar(_) => "text",
        DataType::Numeric { .. } => "numeric",
        DataType::Float | DataType::Real => "double precision",
        DataType::Date => "date",
        DataType::Uuid => "uuid",
        DataType::Jsonb => "jsonb",
        _ => return None,
    };
    Some(String::from(name))
}

fn is_varchar_column(e: &Expr, cx: &Ctx<'_>) -> bool {
    matches!(e, Expr::Column(c) if cx.cols.iter().any(|k| {
        k.name == c.name && matches!(k.ty, DataType::Varchar(_)) && k.user_enum_type.is_none()
    }))
}

/// A varchar column compared with text is printed with its implicit cast.
fn operand(e: &Expr, cx: &Ctx<'_>) -> String {
    operand_at(e, cx, Parent::Top)
}

/// [`operand`], told what encloses it so the pretty form can decide a
/// pair of parentheses (`((a + b)::text) = t`).
fn operand_at(e: &Expr, cx: &Ctx<'_>, parent: Parent) -> String {
    if let Expr::Column(c) = e
        && cx.cols.iter().any(|k| {
            k.name == c.name && matches!(k.ty, DataType::Varchar(_)) && k.user_enum_type.is_none()
        })
    {
        // 9.0.0 — the pretty form drops the parentheses around a cast's
        // operand: PG prints `(v)::text` plain and `v::text` pretty.
        return if cx.pretty {
            format!("{}::text", c.name)
        } else {
            format!("({})::text", c.name)
        };
    }
    render_at(e, cx, parent)
}

fn typed_literal(e: &Expr, ty: Option<&str>, cx: &Ctx<'_>) -> String {
    match (e, ty) {
        (Expr::Literal(Literal::String(s)), Some(t)) => {
            format!("'{}'::{t}", normalise_constant(s, t).replace('\'', "''"))
        }
        // 9.0.0 — a NUMBER meeting a type it is not. PostgreSQL inserts
        // the coercion and prints it: `n > 0` on a numeric column is
        // `(n > (0)::numeric)`, `f > 1.5` on a float8 one is
        // `(f > (1.5)::double precision)`. A decimal literal already IS
        // numeric, so `n > 1.5` stays bare. The parentheses are the
        // cast's own, and the pretty form drops them like any other.
        (Expr::Literal(lit), Some(t)) if number_needs_cast(lit, t) => {
            let inner = render(e, cx);
            if cx.pretty {
                format!("{inner}::{t}")
            } else {
                format!("({inner})::{t}")
            }
        }
        _ => render(e, cx),
    }
}

/// Whether PostgreSQL prints a cast around this numeric constant when it
/// meets a column of type `target`.
fn number_needs_cast(lit: &Literal, target: &str) -> bool {
    let is_exact_decimal = matches!(lit, Literal::Numeric { .. } | Literal::NumericBig(_));
    let is_number = is_exact_decimal || matches!(lit, Literal::Integer(_) | Literal::Float(_));
    // A negative constant is printed by PostgreSQL in a quoted form of
    // its own (`('-1'::integer)::numeric`), which this does not
    // reproduce; leaving it alone is the rendering SPG already gives.
    let non_negative = match lit {
        Literal::Integer(n) => *n >= 0,
        Literal::Float(x) => *x >= 0.0,
        Literal::Numeric { unscaled, .. } => *unscaled >= 0,
        Literal::NumericBig(s) => !s.starts_with('-'),
        _ => false,
    };
    is_number
        && non_negative
        && match target {
            "numeric" => !is_exact_decimal,
            "double precision" => true,
            _ => false,
        }
}

/// The text PostgreSQL prints inside a typed constant: the value as its
/// own type spells it, not as the statement wrote it (`'2020-1-1'::date`
/// is printed `'2020-01-01'::date`, and a uuid comes back lower case).
/// The literal stands when SPG cannot read it as that type — the same
/// rule the rest of this file follows.
fn normalise_constant(src: &str, ty: &str) -> String {
    let target = match ty {
        "date" => DataType::Date,
        "uuid" => DataType::Uuid,
        "jsonb" => DataType::Jsonb,
        _ => return String::from(src),
    };
    crate::conversions::coerce_value(spg_storage::Value::text(String::from(src)), target, "", 0)
        .map_or_else(|_| String::from(src), |v| crate::eval::value_to_text(&v))
}

/// 9.0.0 — what encloses the node being rendered, which is all the pretty
/// form needs to decide a pair of parentheses. Measured on PostgreSQL
/// 18.6; the non-pretty form parenthesises everything and ignores this.
///
/// ```text
///   (c OR b > 1) AND a <> 'q'::text     OR under AND keeps them
///   c AND e OR b > 1                    AND under OR does not
///   (b + d * 2) > 3                     an operator under a comparison does
///   (b + 1) * 2                         and under a tighter operator
///   NOT b > 0                           a comparison under NOT does not
///   (a = ANY (ARRAY['x'])) OR b IS NULL  an ANY under a connective does
/// ```
#[derive(Clone, Copy, PartialEq, Eq)]
enum Parent {
    /// The whole predicate — PG's pretty form wraps nothing here.
    Top,
    And,
    Or,
    Not,
    /// `=`, `<>`, `<`, `~~` — a comparison, whose operator operands PG
    /// parenthesises whatever the precedence says.
    Compare,
    /// `||`, whose operands PostgreSQL leaves bare.
    Concat,
}

/// Whether `e` needs a pair of parentheses inside `parent`, in the pretty
/// form.
fn needs_parens(e: &Expr, parent: Parent) -> bool {
    match parent {
        Parent::Top => false,
        Parent::And => matches!(e, Expr::Binary { op: BinOp::Or, .. }) || is_any_all(e),
        Parent::Or => is_any_all(e),
        Parent::Not => {
            matches!(
                e,
                Expr::Binary {
                    op: BinOp::And | BinOp::Or,
                    ..
                } | Expr::Unary { op: UnOp::Not, .. }
            ) || is_any_all(e)
        }
        // A cast is compound exactly when what it casts is —
        // `a::text = t` against `((a + b)::text) = t`, both measured on
        // PG 18.6 (the round-311 fixture reads the same off 18.4).
        Parent::Compare => match e {
            Expr::Cast { expr, .. } => {
                matches!(expr.as_ref(), Expr::Binary { .. } | Expr::Unary { .. })
            }
            other => matches!(other, Expr::Binary { .. } | Expr::Unary { .. }),
        },
        Parent::Concat => false,
    }
}

/// An `= ANY (ARRAY[…])`, however it was written — PG parenthesises one
/// under a connective and leaves a plain comparison bare.
fn is_any_all(e: &Expr) -> bool {
    match e {
        Expr::AnyAll { .. } => true,
        Expr::InList { list, .. } => list.len() > 1,
        _ => false,
    }
}

/// `body` wrapped only where the form asks for it.
fn wrap(body: String, cx: &Ctx<'_>, e: &Expr, parent: Parent) -> String {
    if cx.pretty && !needs_parens(e, parent) {
        body
    } else {
        format!("({body})")
    }
}

fn render(e: &Expr, cx: &Ctx<'_>) -> String {
    render_at(e, cx, Parent::Top)
}

fn render_at(e: &Expr, cx: &Ctx<'_>, parent: Parent) -> String {
    match e {
        Expr::Binary { lhs, op, rhs } if matches!(op, BinOp::And | BinOp::Or) => {
            let inner = if *op == BinOp::And {
                Parent::And
            } else {
                Parent::Or
            };
            let mut parts: Vec<String> = Vec::new();
            chain(lhs, *op, cx, inner, &mut parts);
            parts.push(render_at(rhs, cx, inner));
            wrap(parts.join(&format!(" {op} ")), cx, e, parent)
        }
        // 9.0.0 — `||` types its literal the way a comparison does. An
        // index key `(a || 'x')` is deparsed by PostgreSQL as
        // `((a || 'x'::text))`, in BOTH forms; SPG printed the literal
        // bare, so a dump of the index did not match PG's.
        Expr::Binary { lhs, op, rhs }
            if matches!(
                op,
                BinOp::Eq
                    | BinOp::NotEq
                    | BinOp::Lt
                    | BinOp::LtEq
                    | BinOp::Gt
                    | BinOp::GtEq
                    | BinOp::Concat
            ) =>
        {
            let ty = literal_type(lhs, cx).or_else(|| literal_type(rhs, cx));
            let inner = if *op == BinOp::Concat {
                Parent::Concat
            } else {
                Parent::Compare
            };
            wrap(
                format!(
                    "{} {op} {}",
                    side(lhs, ty.as_deref(), cx, inner),
                    side(rhs, ty.as_deref(), cx, inner)
                ),
                cx,
                e,
                parent,
            )
        }
        Expr::Unary {
            op: UnOp::Not,
            expr,
        } => wrap(
            format!("NOT {}", render_at(expr, cx, Parent::Not)),
            cx,
            e,
            parent,
        ),
        Expr::IsNull { expr, negated } => {
            let kw = if *negated { "IS NOT NULL" } else { "IS NULL" };
            wrap(format!("{} {kw}", render(expr, cx)), cx, e, parent)
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
            let body = match (items.as_slice(), negated) {
                ([one], false) => format!("{lhs} = {one}"),
                ([one], true) => format!("{lhs} <> {one}"),
                (_, false) => format!("{lhs} = ANY (ARRAY[{}])", items.join(", ")),
                (_, true) => format!("{lhs} <> ALL (ARRAY[{}])", items.join(", ")),
            };
            wrap(body, cx, e, parent)
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
            wrap(
                format!(
                    "{} {op} {}",
                    operand(expr, cx),
                    typed_literal(pattern, ty.as_deref(), cx)
                ),
                cx,
                e,
                parent,
            )
        }
        // A varchar column handed to a function is printed with the
        // implicit cast PG applies (`length((name)::text)`): SPG's catalog
        // functions over character data all take text.
        //
        // 9.0.0 — and the arguments are analysed like any other operand,
        // so a literal among them carries its type (`COALESCE(a,
        // 'z'::text)`) and a nested expression is rendered by this file
        // rather than printed back as written (`lower(('Q'::text ||
        // a))`). Four calls are keywords in PostgreSQL's deparse and come
        // back upper case; every other name is already lower-cased by the
        // parser, as PG's catalog spells a built-in.
        Expr::FunctionCall { name, args, .. } => {
            // Only the four whose result type IS their arguments' type
            // take a sibling's. An ordinary function's parameter type is
            // its own — PostgreSQL prints `to_tsvector('simple'::regconfig,
            // doc)`, not `::text` — and SPG has no per-argument type to
            // read (`pg_proc.proargtypes` is empty and `arity.rs` counts
            // arguments without typing them), so a literal argument is
            // left as written rather than typed from the wrong place.
            let sibling_typed =
                matches!(name.as_str(), "coalesce" | "nullif" | "greatest" | "least");
            let ty = sibling_typed
                .then(|| args.iter().find_map(|a| literal_type(a, cx)))
                .flatten();
            let rendered: Vec<String> = args
                .iter()
                .map(|a| side(a, ty.as_deref(), cx, Parent::Top))
                .collect();
            let spelled = match name.as_str() {
                "coalesce" => "COALESCE",
                "nullif" => "NULLIF",
                "greatest" => "GREATEST",
                "least" => "LEAST",
                other => other,
            };
            format!("{spelled}({})", rendered.join(", "))
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
            wrap(
                format!("{} {op} {kw} ({array})", operand(expr, cx)),
                cx,
                e,
                parent,
            )
        }
        // 9.0.0 — PostgreSQL's deparse spells a boolean constant in
        // lower case (`CHECK ((bo = true))`); SPG's Display, which is
        // also the one error messages use, spells it `TRUE`.
        Expr::Literal(Literal::Bool(b)) => String::from(if *b { "true" } else { "false" }),
        Expr::Cast { expr, target } => match expr.as_ref() {
            Expr::Literal(Literal::String(lit)) => {
                format!("'{}'::{target}", lit.replace('\'', "''"))
            }
            // 9.0.0 — the pretty form leaves the operand bare, as PG
            // does (`b::text` against the plain `(b)::text`). An
            // expression operand keeps whatever parentheses it needs.
            inner if cx.pretty => wrap(
                format!("{}::{target}", render_at(inner, cx, Parent::Compare)),
                cx,
                e,
                parent,
            ),
            _ => format!("{e}"),
        },
        // 9.0.0 — everything this file does not analyse. The plain form
        // is SPG's own Display, which parenthesises every operand and so
        // matches PostgreSQL's plain deparse (`(b + (d * 2))`); the
        // pretty form is the precedence printer, which is what PG's
        // pretty deparse gives (`b + d * 2`).
        other if cx.pretty => wrap(spg_sql::ast::pretty_expr(other), cx, other, parent),
        other => format!("{other}"),
    }
}

fn side(e: &Expr, ty: Option<&str>, cx: &Ctx<'_>, parent: Parent) -> String {
    match e {
        // 9.0.0 — any constant, not a string alone: a number meeting a
        // numeric or float column carries the coercion PostgreSQL
        // inserts. `typed_literal` leaves the ones PG prints bare.
        Expr::Literal(_) => typed_literal(e, ty, cx),
        _ if ty.is_some() => operand_at(e, cx, parent),
        _ => render_at(e, cx, parent),
    }
}

fn chain(e: &Expr, op: BinOp, cx: &Ctx<'_>, parent: Parent, parts: &mut Vec<String>) {
    if let Expr::Binary {
        lhs,
        op: inner,
        rhs,
    } = e
        && *inner == op
    {
        chain(lhs, op, cx, parent, parts);
        parts.push(render_at(rhs, cx, parent));
        return;
    }
    parts.push(render_at(e, cx, parent));
}
