//! Single source-of-truth for an analyzed SQL statement.
//!
//! `Analyzed` is the output of merging three things into one walked AST:
//!
//!   - the SQL parse tree (from `pg_query`, structural)
//!   - the PARSE / DESCRIBE response (per-`$N` and per-output type +
//!     `(table_oid, attnum)` for direct refs)
//!   - the EXPLAIN VERBOSE plan tree (which aliases got widened to
//!     nullable by an outer join, which scans are non-null sources)
//!
//! The structural info from the SQL AST is *lowered* into `Expr` — our
//! own enum — with each node already carrying the answers downstream
//! passes would otherwise look up: `ColumnRef.not_null` is the
//! effective NOT NULL bit *after* outer-join widening, `FuncCall.kind`
//! is the verdict-relevant category (never-null / nullable-agg /
//! other) decided once during lowering.
//!
//! Classification, refinement, FULL JOIN row variants, GROUPING SETS
//! row variants, and codegen all walk `Expr` directly. No EXPLAIN text
//! reading, no parallel HashMap lookups, no substring matching.

use crate::query::TableColRef;
use postgres_types::Type;

/// One analyzed statement. Recursive: `Expr::SubQuery` and view
/// expansion produce nested `Analyzed`.
#[derive(Debug, Clone)]
pub struct Analyzed {
    pub outputs: Vec<Output>,
    pub params: Vec<Param>,
}

#[derive(Debug, Clone)]
pub struct Output {
    /// Column name from RowDescription. Preserves the trailing `!`/`?`
    /// SQLx-style nullability override marker — the user wrote it,
    /// they should see it back.
    pub name: String,
    /// The SQL target expression, lowered + resolved.
    pub expr: Expr,
}

#[derive(Debug, Clone)]
pub struct Param {
    /// Direct INSERT / UPDATE binding, if any. When set, codegen emits
    /// `Table["col"]` instead of an inline type; nullability tightens
    /// from the column's `attnotnull`.
    pub binding: Option<ResolvedCol>,
    /// Postgres type from PARSE.
    pub pg_type: Type,
}

/// Lowered expression. Every variant carries the answer to "is this
/// nullable?" inline — either via the `ResolvedCol`'s `not_null`, the
/// `FuncKind`, or the variant itself.
#[derive(Debug, Clone)]
pub enum Expr {
    /// `'foo'`, `42`, `true`, `false` — never null. We keep the value
    /// so set-op literal unions (`SELECT 'paid' UNION SELECT 'open'`)
    /// can render as `"paid" | "open"` without re-tokenising anything.
    Literal(Lit),
    /// `NULL`, `NULL::T` — always null.
    Null,
    /// `<inner>::T`. `target_oid` is `T`'s `pg_type.oid` resolved at
    /// lowering so a nested `Cast` can use it as the source of the
    /// outer cast. `is_unsafe` is `true` iff the specific
    /// `(source_typoid, target_typoid)` pair matches a user-defined
    /// `castmethod='f'` entry in this database's `pg_cast` — the only
    /// cast shape that can return NULL on non-NULL input. The flag is
    /// per-cast, not per-query: an unrelated unsafe cast (`mytype::text`)
    /// doesn't taint the verdict for `id::text` in the same query.
    Cast { inner: Box<Expr>, target_oid: u32, is_unsafe: bool },
    /// `ARRAY[…]` — never null, regardless of elements.
    ArrayConstructor,
    /// Resolved base-table column reference. `not_null` is already the
    /// effective post-outer-join answer.
    Column(ResolvedCol),
    /// Aggregate / scalar function call we have a verdict for.
    Func { kind: FuncKind, args: Vec<Expr> },
    /// `coalesce(a, b, c, …)` — non-null iff any arg is non-null.
    Coalesce(Vec<Expr>),
    /// `CASE … END`. `has_else_non_null` short-circuits the verdict.
    Case { has_else_non_null: bool },
    /// Scalar subquery `(SELECT …)`. The subquery's first output gives
    /// the verdict.
    SubQuery(Box<Analyzed>),
    /// One output position in a set-op query (UNION / INTERSECT /
    /// EXCEPT). Each `branches[i]` is the lowered expression for the
    /// i-th branch's i-th target. The combined verdict is non-null iff
    /// every branch is non-null; nullable iff any branch is nullable.
    SetOp(Vec<Expr>),
    /// Anything we didn't recognise. Caller defers to attnotnull (if
    /// any) or treats as nullable.
    Unknown,
}

/// A `<alias>.<col>` reference, fully resolved.
#[derive(Debug, Clone)]
pub struct ResolvedCol {
    /// `(schema, table, column)` — codegen uses this to emit
    /// `Table["col"]`.
    pub table_ref: TableColRef,
    /// The alias the user wrote (or the bare relation name when
    /// implicit). Lets FULL JOIN row-variant building decide which
    /// side an output column came from without ever consulting the
    /// EXPLAIN deparse — two aliases on the same base table (e.g.
    /// `FROM users a FULL JOIN users b`) disambiguate here.
    pub alias: String,
    /// `attnotnull` AND-ed with "alias is not on the nullable side of
    /// an outer join above this expression." False here means the
    /// column may be NULL in this output position.
    pub not_null: bool,
    /// `pg_attribute.atttypid` — used by Cast lowering to compute the
    /// `(source, target)` pair against the database's unsafe-cast set.
    pub typoid: u32,
}

/// Verdict-relevant classification of a function call. Computed once
/// at lowering by matching `FuncCall.funcname` against the catalog
/// short-name sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuncKind {
    /// `count`, `row_number`, `now`, `jsonb_build_object`, … — never
    /// returns NULL.
    NeverNull,
    /// `sum`, `avg`, `max`, `array_agg`, … — returns NULL over an
    /// empty input set.
    NullableAgg,
    /// User-defined or any function we don't recognise. Caller uses
    /// the output's `Type` from DESCRIBE and assumes nullable.
    Other,
}

/// Non-null literal value. The raw form is kept so codegen can render
/// `Lit::String("paid")` as TS `"paid"` (with escaping) and a
/// `Lit::Bool(true)` as `true`. Casts wrapped around literals
/// (`'paid'::text`) collapse to the inner `Lit` at lowering time.
#[derive(Debug, Clone)]
pub enum Lit {
    /// String value (raw, *not* TS-escaped — codegen does that).
    Str(String),
    /// Numeric value as written. Kept as a string to preserve PG's
    /// arbitrary precision.
    Num(String),
    Bool(bool),
}

impl Lit {
    /// Render as a TypeScript literal type: `Str("a")` → `"a"`,
    /// `Num("42")` → `42`, `Bool(true)` → `true`.
    pub fn to_ts_literal(&self) -> String {
        match self {
            Lit::Str(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
            Lit::Num(n) => n.clone(),
            Lit::Bool(b) => b.to_string(),
        }
    }
}
