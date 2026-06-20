//! pg_query parse tree → `Expr`. Single pass; each visitor produces
//! a verdict-ready node.

use crate::analyzed::{Expr, FuncKind, Lit, ResolvedCol};
use crate::query::TableColRef;
use crate::scope::Scope;
use pg_query::protobuf::{a_const::Val, node::Node as NB, Node, SubLinkType, TypeName};

/// Aggregates that return `NULL` on empty input.
const NULLABLE_AGGS: &[&str] = &[
    "sum",
    "avg",
    "min",
    "max",
    "array_agg",
    "json_agg",
    "jsonb_agg",
    "string_agg",
    "bool_and",
    "bool_or",
];

/// Functions guaranteed non-NULL by construction.
const NEVER_NULL_FUNCS: &[&str] = &[
    "count",
    "row_number",
    "rank",
    "dense_rank",
    "ntile",
    "cume_dist",
    "percent_rank",
    "now",
    "current_timestamp",
    "current_date",
    "current_time",
    "localtimestamp",
    "localtime",
    "current_user",
    "session_user",
    "current_database",
    "current_schema",
    "current_setting",
    "gen_random_uuid",
    "uuid_generate_v1",
    "uuid_generate_v4",
    "pg_advisory_lock",
    "pg_advisory_xact_lock",
    "jsonb_build_object",
    "json_build_object",
    "jsonb_build_array",
    "json_build_array",
    "to_jsonb",
    "to_json",
    "row_to_json",
    "array_to_json",
];

pub fn lower(node: &Node, scope: &Scope) -> Expr {
    let Some(body) = node.node.as_ref() else {
        return Expr::Unknown;
    };
    match body {
        NB::AConst(c) => {
            if c.isnull {
                return Expr::Null;
            }
            match c.val.as_ref() {
                Some(Val::Sval(s)) => Expr::Literal(Lit::Str(s.sval.clone())),
                Some(Val::Ival(i)) => Expr::Literal(Lit::Num(i.ival.to_string())),
                Some(Val::Fval(f)) => Expr::Literal(Lit::Num(f.fval.clone())),
                Some(Val::Boolval(b)) => Expr::Literal(Lit::Bool(b.boolval)),
                _ => Expr::Unknown,
            }
        }
        NB::TypeCast(tc) => {
            let inner = tc
                .arg
                .as_deref()
                .map(|a| lower(a, scope))
                .unwrap_or(Expr::Unknown);
            let target_oid = tc
                .type_name
                .as_ref()
                .and_then(|tn| resolve_typename_oid(tn, scope))
                .unwrap_or(0);
            let is_unsafe = match (inner_type_oid(&inner), target_oid) {
                (Some(src), tgt) if tgt != 0 => scope.is_unsafe_cast(src, tgt),
                _ => false,
            };
            Expr::Cast {
                inner: Box::new(inner),
                target_oid,
                is_unsafe,
            }
        }
        NB::AArrayExpr(_) => Expr::ArrayConstructor,
        // Parse-time COALESCE comes through as FuncCall — CoalesceExpr is
        // post-analysis. We handle both.
        NB::CoalesceExpr(ce) => Expr::Coalesce(lower_args(&ce.args, scope)),
        NB::CaseExpr(ce) => Expr::Case {
            has_else_non_null: ce
                .defresult
                .as_deref()
                .is_some_and(|d| is_non_null(&lower(d, scope))),
        },
        NB::FuncCall(fc) => {
            let Some(NB::String(s)) = fc.funcname.last().and_then(|n| n.node.as_ref()) else {
                return Expr::Unknown;
            };
            let name = s.sval.as_str();
            let args = lower_args(&fc.args, scope);
            if name == "coalesce" {
                Expr::Coalesce(args)
            } else if NEVER_NULL_FUNCS.contains(&name) {
                Expr::Func {
                    kind: FuncKind::NeverNull,
                    args,
                }
            } else if NULLABLE_AGGS.contains(&name) {
                Expr::Func {
                    kind: FuncKind::NullableAgg,
                    args,
                }
            } else {
                Expr::Func {
                    kind: FuncKind::Other,
                    args,
                }
            }
        }
        NB::ColumnRef(cr) => lower_column_ref(cr, scope)
            .map(Expr::Column)
            .unwrap_or(Expr::Unknown),
        NB::SubLink(sl) => lower_sublink(sl, scope),
        _ => Expr::Unknown,
    }
}

/// SubLinkType drives the verdict:
///   * EXISTS / ARRAY: never NULL.
///   * scalar EXPR_SUBLINK: NULL on zero rows unless provably-one-row
///     (aggregate-only target_list, no GROUP BY); then inner verdict.
///   * ANY / ALL / ROWCOMPARE: NULL via operator semantics.
fn lower_sublink(sl: &pg_query::protobuf::SubLink, scope: &Scope) -> Expr {
    match SubLinkType::try_from(sl.sub_link_type).unwrap_or(SubLinkType::Undefined) {
        SubLinkType::ExistsSublink => Expr::Func {
            kind: FuncKind::NeverNull,
            args: Vec::new(),
        },
        SubLinkType::ArraySublink => Expr::ArrayConstructor,
        SubLinkType::ExprSublink => {
            let Some(sub) = sl.subselect.as_deref() else {
                return Expr::Unknown;
            };
            let Some(NB::SelectStmt(s)) = sub.node.as_ref() else {
                return Expr::Unknown;
            };
            if !is_provably_one_row_select(s) {
                return Expr::Unknown;
            }
            let first = s.target_list.first().and_then(|t| match t.node.as_ref()? {
                NB::ResTarget(rt) => rt.val.as_deref(),
                _ => None,
            });
            first.map(|n| lower(n, scope)).unwrap_or(Expr::Unknown)
        }
        _ => Expr::Unknown,
    }
}

fn is_provably_one_row_select(s: &pg_query::protobuf::SelectStmt) -> bool {
    if !s.group_clause.is_empty() || s.target_list.is_empty() {
        return false;
    }
    s.target_list.iter().all(|t| {
        let Some(NB::ResTarget(rt)) = t.node.as_ref() else {
            return false;
        };
        let Some(val) = rt.val.as_deref() else {
            return false;
        };
        let Some(NB::FuncCall(fc)) = val.node.as_ref() else {
            return false;
        };
        let name = fc.funcname.last().and_then(|n| match n.node.as_ref()? {
            NB::String(s) => Some(s.sval.as_str()),
            _ => None,
        });
        matches!(name, Some(n) if NEVER_NULL_FUNCS.contains(&n) || NULLABLE_AGGS.contains(&n))
    })
}

fn lower_args(args: &[Node], scope: &Scope) -> Vec<Expr> {
    args.iter().map(|a| lower(a, scope)).collect()
}

fn lower_column_ref(cr: &pg_query::protobuf::ColumnRef, scope: &Scope) -> Option<ResolvedCol> {
    let parts: Vec<&str> = cr
        .fields
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            NB::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();
    let (alias, col) = match parts.as_slice() {
        [col] => {
            let r = scope.resolve_bare(col)?;
            return Some(ResolvedCol {
                table_ref: TableColRef {
                    schema: r.schema.clone(),
                    table: r.table.clone(),
                    column: (*col).to_string(),
                },
                alias: r.alias,
                not_null: r.not_null,
                typoid: r.typoid,
            });
        }
        [alias, col] | [_, alias, col] => (*alias, *col),
        _ => return None,
    };
    if let Some(table) = scope.resolve_alias(alias) {
        let base_nn = table.col_not_null(col).unwrap_or(false);
        let widened = scope.is_nullable_alias(alias);
        let force_nn = scope.is_non_null_alias(alias);
        return Some(ResolvedCol {
            table_ref: TableColRef {
                schema: table.schema.clone(),
                table: table.name.clone(),
                column: col.to_string(),
            },
            alias: alias.to_string(),
            not_null: (base_nn || force_nn) && !widened,
            typoid: table.col_typoid(col).unwrap_or(0),
        });
    }
    // Derived table / CTE alias — inherit non-null verdict from the
    // pre-lowered child expression.
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
            typoid: 0,
        });
    }
    // Literal source (VALUES, literal unnest): plan walk marked the
    // alias non-null. No (schema, table) known.
    if scope.is_non_null_alias(alias) && !scope.is_nullable_alias(alias) {
        return Some(ResolvedCol {
            table_ref: TableColRef {
                schema: String::new(),
                table: alias.to_string(),
                column: col.to_string(),
            },
            alias: alias.to_string(),
            not_null: true,
            typoid: 0,
        });
    }
    None
}

/// Provably non-NULL at runtime. Used by Coalesce arg checks, CASE
/// ELSE checks, SetOp all-branches check.
///
/// `Cast { is_unsafe }` blocks propagation — the specific
/// `(source, target)` pair has a user-defined `castmethod='f'` that
/// could return NULL on non-NULL input.
pub fn is_non_null(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) | Expr::ArrayConstructor => true,
        Expr::Cast {
            inner, is_unsafe, ..
        } => !*is_unsafe && is_non_null(inner),
        Expr::Column(c) => c.not_null,
        Expr::Func {
            kind: FuncKind::NeverNull,
            ..
        } => true,
        Expr::Coalesce(args) => args.iter().any(is_non_null),
        Expr::Case { has_else_non_null } => *has_else_non_null,
        Expr::SubQuery(a) => a.outputs.first().is_some_and(|o| is_non_null(&o.expr)),
        Expr::SetOp(branches) => !branches.is_empty() && branches.iter().all(is_non_null),
        _ => false,
    }
}

/// Strong nullable verdict — overrides attnotnull when, e.g., an outer
/// join widens a NOT NULL column or a user-defined cast might return NULL.
pub fn is_nullable(e: &Expr) -> bool {
    match e {
        Expr::Null => true,
        Expr::Cast {
            inner, is_unsafe, ..
        } => is_nullable(inner) || *is_unsafe,
        Expr::Column(c) => !c.not_null,
        Expr::Func {
            kind: FuncKind::NullableAgg,
            ..
        } => true,
        Expr::Case {
            has_else_non_null: false,
        } => true,
        Expr::SetOp(branches) => branches.iter().any(is_nullable),
        _ => false,
    }
}

/// Peel `Cast` wrappers and return the inner `Lit` if it's structurally
/// a single literal.
pub fn as_literal(e: &Expr) -> Option<&Lit> {
    match e {
        Expr::Literal(l) => Some(l),
        Expr::Cast { inner, .. } => as_literal(inner),
        _ => None,
    }
}

/// Peel `Cast` wrappers and return the inner column ref. Used by FULL
/// JOIN side detection to read `ResolvedCol.alias`.
pub fn as_column(e: &Expr) -> Option<&ResolvedCol> {
    match e {
        Expr::Column(c) => Some(c),
        Expr::Cast { inner, .. } => as_column(inner),
        _ => None,
    }
}

/// Source-type OID for the wrapping Cast's `is_unsafe` lookup. None
/// for shapes whose type we don't carry on the lowered tree — caller
/// treats unknown source as default-safe.
fn inner_type_oid(e: &Expr) -> Option<u32> {
    match e {
        Expr::Column(c) => Some(c.typoid),
        Expr::Cast { target_oid, .. } => (*target_oid != 0).then_some(*target_oid),
        _ => None,
    }
}

fn resolve_typename_oid(tn: &TypeName, scope: &Scope) -> Option<u32> {
    let last = tn.names.last()?;
    let NB::String(s) = last.node.as_ref()? else {
        return None;
    };
    scope.typname_oid(&s.sval)
}
