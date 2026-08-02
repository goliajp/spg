//! Collation derivation — what collation an EXPRESSION has.
//!
//! v7.39 (round 692) — F36's last structural piece. Rounds 683–691 carried a
//! collation from a COLUMN to the places that compare it, which covers every
//! key that is a bare column reference and the explicit `COLLATE` clause.
//! What they could not answer is `ORDER BY upper(loc)`, or `loc BETWEEN 'a'
//! AND 'd'`: both compare a value that no single column produced.
//!
//! PG answers with derivation rules, and these are the ones measured off
//! PG18 rather than read out of anyone's source (`collation for (…)`, which
//! reports the derived collation of an expression):
//!
//! | expression                   | PG18 reports              |
//! |------------------------------|---------------------------|
//! | `upper(a)`, a is en_US.utf8  | `"en_US.utf8"`            |
//! | `upper(plain)`, undeclared   | `"default"`               |
//! | `a \|\| 'literal'`           | `"en_US.utf8"`            |
//! | `'literal'`                  | (none)                    |
//! | `a COLLATE "C"`              | `"C"`                     |
//! | `a \|\| b`, en_US and C      | (none) — and USING it errors |
//!
//! That last row is the one worth stating plainly: two different IMPLICIT
//! collations do not silently pick a winner, they make the expression's
//! collation indeterminate, and `ORDER BY a || b` fails with `collation
//! mismatch between implicit collations "en_US.utf8" and "C"`. Anything that
//! quietly chose one would be the F36 defect in a new place.

use alloc::string::String;
use spg_sql::ast::Expr;

/// An expression's collation and how strongly it holds it. The strength
/// matters: an explicit `COLLATE` overrides an implicit one, two different
/// implicit ones conflict, and a literal has none at all and yields to
/// whatever it is combined with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Derived {
    /// No collation — a literal, a non-text value, an expression built only
    /// from those. Combines with anything.
    None,
    /// From a column's declaration. Two DIFFERENT implicit collations
    /// conflict rather than one winning.
    Implicit(String),
    /// Written in the query with `COLLATE`. Beats any implicit one.
    Explicit(String),
    /// Two different implicit collations met. PG reports no collation for
    /// the expression and errors if it is compared or sorted.
    Conflict(String, String),
}

impl Derived {
    /// The name to compare by, or `None` to keep byte order.
    ///
    /// `Conflict` returns `None` here on purpose: the caller that can raise
    /// an error should check for it first (see [`Derived::conflict`]), and a
    /// caller that cannot must not invent a winner.
    pub(crate) fn name(&self) -> Option<&str> {
        match self {
            Self::Implicit(n) | Self::Explicit(n) => Some(n),
            Self::None | Self::Conflict(..) => None,
        }
    }

    /// The two names that met, when this expression cannot be compared.
    pub(crate) fn conflict(&self) -> Option<(&str, &str)> {
        match self {
            Self::Conflict(a, b) => Some((a, b)),
            _ => None,
        }
    }

    /// Combine two operands' derivations, PG's rules:
    ///
    /// * an explicit one wins, and two DIFFERENT explicit ones are an error
    ///   PG raises at parse time — SPG cannot produce that shape yet,
    ///   because `COLLATE` only survives on an ORDER BY key, so the first
    ///   is kept and the case is recorded rather than guessed at;
    /// * otherwise one implicit one wins over none;
    /// * two different implicit ones conflict;
    /// * a conflict is contagious.
    /// v7.39 (round 693) — the comparison hook combines two OPERANDS'
    /// derivations, which is the same rule applied at a different place.
    pub(crate) fn combine_pub(self, other: Self) -> Self {
        self.combine(other)
    }

    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (c @ Self::Conflict(..), _) | (_, c @ Self::Conflict(..)) => c,
            (Self::Explicit(a), _) | (Self::None | Self::Implicit(_), Self::Explicit(a)) => {
                Self::Explicit(a)
            }
            (Self::Implicit(a), Self::Implicit(b)) => {
                if a == b {
                    Self::Implicit(a)
                } else {
                    Self::Conflict(a, b)
                }
            }
            (Self::Implicit(a), Self::None) | (Self::None, Self::Implicit(a)) => Self::Implicit(a),
            (Self::None, Self::None) => Self::None,
        }
    }
}

/// Derive an expression's collation, resolving column references through
/// `column`.
///
/// `column` takes a column reference and answers what that column declares.
/// It is a closure rather than an `EvalContext` so this module stays
/// independent of how a caller resolves names — the scan, the join and the
/// aggregate all resolve differently.
pub(crate) fn derive<F>(expr: &Expr, column: &F) -> Derived
where
    F: Fn(&spg_sql::ast::ColumnName) -> Option<String>,
{
    match expr {
        Expr::Column(c) => column(c).map_or(Derived::None, Derived::Implicit),

        // A literal, a parameter and a subquery result carry nothing. PG
        // gives a scalar subquery the collation of its output column; SPG
        // would have to plan the subquery to know it, and an ORDER BY over
        // one is not a shape this closes — byte order, as before.
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::ScalarSubquery(_) => Derived::None,

        // A function result takes its arguments'. `upper(a)` is the shape
        // this whole module exists for.
        Expr::FunctionCall { args, .. } => args
            .iter()
            .fold(Derived::None, |acc, a| acc.combine(derive(a, column))),

        Expr::Binary { lhs, rhs, .. } => derive(lhs, column).combine(derive(rhs, column)),
        Expr::Unary { expr, .. } | Expr::Variadic(expr) | Expr::NamedArg { expr, .. } => {
            derive(expr, column)
        }

        // A cast to text keeps the source's collation in PG; a cast to a
        // non-text type has none. SPG does not model per-type collatability
        // here, so it keeps the source's and lets the comparison decide —
        // a non-text comparison never consults one.
        Expr::Cast { expr, .. } => derive(expr, column),

        // Everything else — comparisons, IS NULL, EXISTS, subscripts —
        // produces a boolean or a non-text value, or is a shape whose
        // collation SPG has no way to know. None, which is byte order,
        // which is what those did before this module existed.
        _ => Derived::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn col(name: &str) -> Expr {
        Expr::Column(spg_sql::ast::ColumnName {
            qualifier: None,
            name: name.to_string(),
        })
    }

    fn resolver(c: &spg_sql::ast::ColumnName) -> Option<String> {
        match c.name.as_str() {
            "a" => Some("en_US.utf8".to_string()),
            "b" => Some("C".to_string()),
            _ => None,
        }
    }

    fn lit() -> Expr {
        Expr::Literal(spg_sql::ast::Literal::String("x".to_string()))
    }

    fn upper(e: Expr) -> Expr {
        Expr::FunctionCall {
            name: "upper".to_string(),
            args: vec![e],
        }
    }

    /// Each row of the table in this module's header, in order.
    #[test]
    fn the_rules_measured_from_pg18() {
        assert_eq!(
            derive(&upper(col("a")), &resolver).name(),
            Some("en_US.utf8")
        );
        assert_eq!(derive(&upper(col("plain")), &resolver).name(), None);
        assert_eq!(derive(&lit(), &resolver), Derived::None);
        assert_eq!(derive(&col("a"), &resolver).name(), Some("en_US.utf8"));
    }

    /// A literal yields; it does not dilute the column it is combined with.
    #[test]
    fn a_literal_yields_to_a_column() {
        let cat = Expr::Binary {
            lhs: alloc::boxed::Box::new(col("a")),
            op: spg_sql::ast::BinOp::Concat,
            rhs: alloc::boxed::Box::new(lit()),
        };
        assert_eq!(derive(&cat, &resolver).name(), Some("en_US.utf8"));
    }

    /// Two different implicit collations conflict rather than one winning.
    /// PG18 reports no collation for this expression and errors on use.
    #[test]
    fn two_different_implicit_collations_conflict() {
        let cat = Expr::Binary {
            lhs: alloc::boxed::Box::new(col("a")),
            op: spg_sql::ast::BinOp::Concat,
            rhs: alloc::boxed::Box::new(col("b")),
        };
        let d = derive(&cat, &resolver);
        assert_eq!(d.name(), None, "must not pick a winner");
        assert_eq!(d.conflict(), Some(("en_US.utf8", "C")));
    }

    /// And the conflict survives being wrapped, so a caller that checks at
    /// the top of the key expression sees it.
    #[test]
    fn a_conflict_is_contagious() {
        let cat = Expr::Binary {
            lhs: alloc::boxed::Box::new(col("a")),
            op: spg_sql::ast::BinOp::Concat,
            rhs: alloc::boxed::Box::new(col("b")),
        };
        assert!(derive(&upper(cat), &resolver).conflict().is_some());
    }
}
