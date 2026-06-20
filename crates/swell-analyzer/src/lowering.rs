//! Lowering: pg_query parse tree → `Expr`.
//!
//! Single pass. Each visitor produces the verdict-ready node, with
//! base column refs resolved against the active `Scope`, function
//! calls categorised against the catalog short-name sets, and casts
//! recursed through.

use crate::analyzed::{Expr, FuncKind, Lit, ResolvedCol};
use crate::query::TableColRef;
use crate::scope::Scope;
use pg_query::protobuf::{a_const::Val, node::Node as NB, Node};

/// Aggregate functions that return `NULL` over an empty input set.
const NULLABLE_AGGS: &[&str] = &[
    "sum", "avg", "min", "max",
    "array_agg", "json_agg", "jsonb_agg",
    "string_agg", "bool_and", "bool_or",
];

/// Functions whose return value is guaranteed non-NULL by construction.
const NEVER_NULL_FUNCS: &[&str] = &[
    "count",
    // Window funcs over a non-empty partition.
    "row_number", "rank", "dense_rank", "ntile", "cume_dist", "percent_rank",
    // Session / time builtins.
    "now", "current_timestamp", "current_date", "current_time",
    "localtimestamp", "localtime",
    "current_user", "session_user", "current_database",
    "current_schema", "current_setting",
    "gen_random_uuid", "uuid_generate_v1", "uuid_generate_v4",
    "pg_advisory_lock", "pg_advisory_xact_lock",
    // JSON / row builders — construct value from args, never short-circuit.
    "jsonb_build_object", "json_build_object",
    "jsonb_build_array", "json_build_array",
    "to_jsonb", "to_json", "row_to_json", "array_to_json",
];

/// Lower a SQL expression node to `Expr`. Returns `Expr::Unknown` for
/// shapes we don't categorise — caller defers to `attnotnull` or
/// renders as nullable.
pub fn lower(node: &Node, scope: &Scope) -> Expr {
    let Some(body) = node.node.as_ref() else { return Expr::Unknown };
    match body {
        NB::AConst(c) => {
            if c.isnull { return Expr::Null; }
            match c.val.as_ref() {
                Some(Val::Sval(s)) => Expr::Literal(Lit::Str(s.sval.clone())),
                Some(Val::Ival(i)) => Expr::Literal(Lit::Num(i.ival.to_string())),
                Some(Val::Fval(f)) => Expr::Literal(Lit::Num(f.fval.clone())),
                Some(Val::Boolval(b)) => Expr::Literal(Lit::Bool(b.boolval)),
                _ => Expr::Unknown,
            }
        }

        // `<expr>::T` — recurse on the inner. NULL::T is the common
        // case PG synthesises for outer-join padding.
        NB::TypeCast(tc) => {
            let inner = tc.arg.as_deref()
                .map(|a| lower(a, scope))
                .unwrap_or(Expr::Unknown);
            Expr::Cast { inner: Box::new(inner) }
        }

        // `ARRAY[…]` — non-null regardless of elements.
        NB::AArrayExpr(_) => Expr::ArrayConstructor,

        // Parse-time COALESCE comes through as a FuncCall; the
        // CoalesceExpr variant exists in the protobuf but post-analysis.
        NB::CoalesceExpr(ce) => Expr::Coalesce(lower_args(&ce.args, scope)),

        NB::CaseExpr(ce) => {
            let has_else_non_null = ce.defresult.as_deref()
                .map(|d| is_non_null(&lower(d, scope)))
                .unwrap_or(false);
            Expr::Case { has_else_non_null }
        }

        NB::FuncCall(fc) => {
            let name = match fc.funcname.last().and_then(|n| n.node.as_ref()) {
                Some(NB::String(s)) => s.sval.as_str(),
                _ => return Expr::Unknown,
            };
            if name == "coalesce" {
                Expr::Coalesce(lower_args(&fc.args, scope))
            } else if NEVER_NULL_FUNCS.contains(&name) {
                Expr::Func { kind: FuncKind::NeverNull, args: lower_args(&fc.args, scope) }
            } else if NULLABLE_AGGS.contains(&name) {
                Expr::Func { kind: FuncKind::NullableAgg, args: lower_args(&fc.args, scope) }
            } else {
                Expr::Func { kind: FuncKind::Other, args: lower_args(&fc.args, scope) }
            }
        }

        NB::ColumnRef(cr) => match lower_column_ref(cr, scope) {
            Some(rc) => Expr::Column(rc),
            None => Expr::Unknown,
        },

        // Scalar subquery: classify by its first output.
        NB::SubLink(sl) => match sl.subselect.as_deref() {
            Some(sub) => {
                // We can only descend if we recognise the subquery as a
                // SelectStmt and its first target.
                if let Some(NB::SelectStmt(s)) = sub.node.as_ref() {
                    let first = s.target_list.first()
                        .and_then(|t| match t.node.as_ref()? {
                            NB::ResTarget(rt) => rt.val.as_deref(),
                            _ => None,
                        });
                    match first {
                        Some(node) => lower(node, scope),
                        None => Expr::Unknown,
                    }
                } else {
                    Expr::Unknown
                }
            }
            None => Expr::Unknown,
        },

        _ => Expr::Unknown,
    }
}

fn lower_args(args: &[Node], scope: &Scope) -> Vec<Expr> {
    args.iter().map(|a| lower(a, scope)).collect()
}

/// Resolve a `<alias>.<col>` (or bare `<col>` when unambiguous) to a
/// `ResolvedCol` with `not_null` reflecting attnotnull AND outer-join
/// widening AND any all-literal source override.
fn lower_column_ref(cr: &pg_query::protobuf::ColumnRef, scope: &Scope) -> Option<ResolvedCol> {
    let parts: Vec<&str> = cr.fields.iter()
        .filter_map(|n| match n.node.as_ref()? {
            NB::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();
    let (alias, col) = match parts.as_slice() {
        [col] => {
            let resolved = scope.resolve_bare(col)?;
            return Some(ResolvedCol {
                table_ref: TableColRef {
                    schema: resolved.schema.clone(),
                    table: resolved.table.clone(),
                    column: (*col).to_string(),
                },
                alias: resolved.alias,
                not_null: resolved.not_null,
            });
        }
        [alias, col] => (*alias, *col),
        [_schema, alias, col] => (*alias, *col),
        _ => return None,
    };
    if let Some(table) = scope.resolve_alias(alias) {
        let base_not_null = table.col_not_null(col).unwrap_or(false);
        let widened = scope.is_nullable_alias(alias);
        let force_non_null = scope.is_non_null_alias(alias);
        let not_null = (base_not_null || force_non_null) && !widened;
        return Some(ResolvedCol {
            table_ref: TableColRef {
                schema: table.schema.clone(),
                table: table.name.clone(),
                column: col.to_string(),
            },
            alias: alias.to_string(),
            not_null,
        });
    }
    // Derived-table alias (RangeSubselect / CTE) — find the column by
    // name in the pre-lowered derived list and inherit its non-null
    // verdict via `is_non_null(child_expr)`.
    if let Some(derived) = scope.derived(alias) {
        let child = derived.iter().find(|c| c.name == col)?;
        return Some(ResolvedCol {
            table_ref: TableColRef {
                schema: String::new(),
                table: alias.to_string(),
                column: col.to_string(),
            },
            alias: alias.to_string(),
            not_null: is_non_null(&child.expr),
        });
    }
    // Alias isn't a real base table in the plan walk's `alias_to_table`.
    // For literal-source aliases (VALUES, literal unnest) — the plan
    // walk added them to `non_null` — we still want a `ResolvedCol`,
    // since the verdict is "non-null by construction." We don't know
    // the (schema, table) so leave them empty; codegen ignores
    // `table_ref` for these because there's no surrounding interface.
    if scope.is_non_null_alias(alias) && !scope.is_nullable_alias(alias) {
        return Some(ResolvedCol {
            table_ref: TableColRef {
                schema: String::new(),
                table: alias.to_string(),
                column: col.to_string(),
            },
            alias: alias.to_string(),
            not_null: true,
        });
    }
    None
}

/// True iff `e` represents a value provably non-NULL at runtime.
/// Shared by `Coalesce`'s "any arg non-null" check and `Case`'s "ELSE
/// branch non-null" check. For `SetOp`, every branch must be non-null
/// (the union admits NULL if any branch can produce it).
///
/// Casts: a user-defined cast (e.g. `id::text` over a custom domain)
/// can return NULL even on non-null input, so we *don't* propagate
/// non-null through `Cast` over a `Column`. We *do* propagate through
/// `Cast` over a literal-class value (`Literal`, `ArrayConstructor`,
/// `Func { NeverNull }`) — those are immune to cast surprises since
/// the value is fabricated, not transformed from a column.
pub fn is_non_null(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) => true,
        Expr::ArrayConstructor => true,
        Expr::Cast { inner } => matches!(inner.as_ref(),
            Expr::Literal(_) | Expr::ArrayConstructor
            | Expr::Func { kind: FuncKind::NeverNull, .. }),
        Expr::Column(c) => c.not_null,
        Expr::Func { kind: FuncKind::NeverNull, .. } => true,
        Expr::Coalesce(args) => args.iter().any(is_non_null),
        Expr::Case { has_else_non_null } => *has_else_non_null,
        Expr::SubQuery(a) => a.outputs.first().is_some_and(|o| is_non_null(&o.expr)),
        Expr::SetOp(branches) => !branches.is_empty() && branches.iter().all(is_non_null),
        _ => false,
    }
}

/// Strong "this column is nullable" verdict — used to override an
/// otherwise-NOT-NULL base column when, e.g., an outer join widens it.
pub fn is_nullable(e: &Expr) -> bool {
    match e {
        Expr::Null => true,
        Expr::Cast { inner } => is_nullable(inner),
        Expr::Column(c) => !c.not_null,
        Expr::Func { kind: FuncKind::NullableAgg, .. } => true,
        Expr::Case { has_else_non_null: false } => true,
        Expr::SetOp(branches) => branches.iter().any(is_nullable),
        _ => false,
    }
}

/// Peel any `Cast` wrappers and return the underlying `Lit` iff the
/// expression is structurally a single literal.
pub fn as_literal(e: &Expr) -> Option<&Lit> {
    match e {
        Expr::Literal(l) => Some(l),
        Expr::Cast { inner } => as_literal(inner),
        _ => None,
    }
}

/// Peel any `Cast` wrappers and return the underlying base-column
/// reference iff the expression is structurally a single column ref.
/// Used by FULL JOIN row-variant building to know which side an
/// output column came from via `ResolvedCol.alias`.
pub fn as_column(e: &Expr) -> Option<&ResolvedCol> {
    match e {
        Expr::Column(c) => Some(c),
        Expr::Cast { inner } => as_column(inner),
        _ => None,
    }
}
