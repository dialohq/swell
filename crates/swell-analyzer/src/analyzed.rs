//! `Analyzed` — colocated SQL AST + plan-derived alias nullability.
//!
//! `Expr` carries verdict-relevant info inline: `Column.not_null` is
//! the effective post-outer-join answer, `FuncKind` is decided at
//! lowering, `Cast.is_unsafe` is precomputed against `pg_cast`. No
//! downstream HashMap lookups.

use crate::query::TableColRef;
use postgres_types::Type;

/// One analysed statement. Recursive via `Expr::SubQuery` and view
/// expansion.
#[derive(Debug, Clone)]
pub struct Analyzed {
    pub outputs: Vec<Output>,
    pub params: Vec<Param>,
}

#[derive(Debug, Clone)]
pub struct Output {
    /// RowDescription name. Trailing `!` / `?` markers are preserved.
    pub name: String,
    pub expr: Expr,
}

#[derive(Debug, Clone)]
pub struct Param {
    /// Direct INSERT / UPDATE binding, if any.
    pub binding: Option<ResolvedCol>,
    pub pg_type: Type,
}

#[derive(Debug, Clone)]
pub enum Expr {
    /// Non-null literal. Value retained so set-op literal unions can
    /// render `"paid" | "open"` without re-tokenising.
    Literal(Lit),
    /// `NULL`, `NULL::T` — always null.
    Null,
    /// `<inner>::T`. `target_oid` lets nested casts use it as their
    /// source; `is_unsafe` flags the specific `(source, target)` pair
    /// as a user-defined `castmethod='f'` cast that could return NULL
    /// on non-NULL input.
    Cast { inner: Box<Expr>, target_oid: u32, is_unsafe: bool },
    /// `ARRAY[…]` — never null regardless of elements.
    ArrayConstructor,
    Column(ResolvedCol),
    Func { kind: FuncKind, args: Vec<Expr> },
    /// Non-null iff any arg is non-null.
    Coalesce(Vec<Expr>),
    /// `has_else_non_null` short-circuits the verdict — a missing ELSE
    /// makes the result nullable.
    Case { has_else_non_null: bool },
    /// Scalar subquery. Verdict comes from its first output.
    SubQuery(Box<Analyzed>),
    /// One output position in a set-op (UNION / INTERSECT / EXCEPT) —
    /// branches are the per-branch lowered expressions.
    SetOp(Vec<Expr>),
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ResolvedCol {
    pub table_ref: TableColRef,
    /// SQL alias the user wrote (or bare relation name). Lets FULL JOIN
    /// row-variant building disambiguate `users a FULL JOIN users b`.
    pub alias: String,
    /// `attnotnull` AND-ed with "alias is not on the nullable side of
    /// an outer join above this expression."
    pub not_null: bool,
    /// `pg_attribute.atttypid` — Cast lowering uses it as the source
    /// side of the `pg_cast` pair lookup.
    pub typoid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuncKind {
    /// `count`, `row_number`, `now`, `jsonb_build_object`, …
    NeverNull,
    /// `sum`, `avg`, `max`, `array_agg`, … — NULL on empty input.
    NullableAgg,
    /// User-defined / unrecognised; caller assumes nullable.
    Other,
}

#[derive(Debug, Clone)]
pub enum Lit {
    /// Raw (not TS-escaped — `to_ts_literal` escapes on render).
    Str(String),
    /// String form preserves PG's arbitrary precision.
    Num(String),
    Bool(bool),
}

impl Lit {
    pub fn to_ts_literal(&self) -> String {
        match self {
            Lit::Str(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
            Lit::Num(n) => n.clone(),
            Lit::Bool(b) => b.to_string(),
        }
    }
}
