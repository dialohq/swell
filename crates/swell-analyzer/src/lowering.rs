//! pg_query parse tree → `Expr`. Single pass; each visitor produces
//! a verdict-ready node.

use crate::analyzed::{Expr, FuncKind, Lit, ResolvedCol};
use crate::pg_util::{funcname_last, restarget_val, string_parts};
use crate::query::TableColRef;
use crate::scope::Scope;
use pg_query::protobuf::{a_const::Val, node::Node as NB, Node, SubLinkType, TypeName};

/// Classify a function name. `NeverNull`: provably non-null by
/// construction. `NullableAgg`: aggregate that returns NULL on empty
/// input. `Other`: user-defined / unknown.
fn classify_func(name: &str) -> FuncKind {
    match name {
        "count"
        | "row_number"
        | "rank"
        | "dense_rank"
        | "ntile"
        | "cume_dist"
        | "percent_rank"
        | "now"
        | "current_timestamp"
        | "current_date"
        | "current_time"
        | "localtimestamp"
        | "localtime"
        | "current_user"
        | "session_user"
        | "current_database"
        | "current_schema"
        | "current_setting"
        | "gen_random_uuid"
        | "uuid_generate_v1"
        | "uuid_generate_v4"
        | "pg_advisory_lock"
        | "pg_advisory_xact_lock"
        | "jsonb_build_object"
        | "json_build_object"
        | "jsonb_build_array"
        | "json_build_array"
        | "to_jsonb"
        | "to_json"
        | "row_to_json"
        | "array_to_json" => FuncKind::NeverNull,
        "sum" | "avg" | "min" | "max" | "array_agg" | "json_agg" | "jsonb_agg" | "string_agg"
        | "bool_and" | "bool_or" => FuncKind::NullableAgg,
        _ => FuncKind::Other,
    }
}

pub fn lower(node: &Node, scope: &Scope) -> Expr {
    match node.node.as_ref() {
        Some(NB::AConst(c)) if c.isnull => Expr::Null,
        Some(NB::AConst(c)) => match c.val.as_ref() {
            Some(Val::Sval(s)) => Expr::Literal(Lit::Str(s.sval.clone())),
            Some(Val::Ival(i)) => Expr::Literal(Lit::Num(i.ival.to_string())),
            Some(Val::Fval(f)) => Expr::Literal(Lit::Num(f.fval.clone())),
            Some(Val::Boolval(b)) => Expr::Literal(Lit::Bool(b.boolval)),
            _ => Expr::Unknown,
        },
        Some(NB::TypeCast(tc)) => {
            let inner = tc.arg.as_deref().map_or(Expr::Unknown, |a| lower(a, scope));
            let target_oid = tc
                .type_name
                .as_ref()
                .and_then(|tn| resolve_typename_oid(tn, scope))
                .unwrap_or(0);
            let is_unsafe = matches!((inner_type_oid(&inner), target_oid),
                (Some(src), tgt) if tgt != 0 && scope.is_unsafe_cast(src, tgt));
            Expr::Cast {
                inner: Box::new(inner),
                target_oid,
                is_unsafe,
            }
        }
        Some(NB::AArrayExpr(_)) => Expr::ArrayConstructor,
        // Parse-time COALESCE comes through as FuncCall — CoalesceExpr is
        // post-analysis. We handle both.
        Some(NB::CoalesceExpr(ce)) => Expr::Coalesce(lower_args(&ce.args, scope)),
        Some(NB::CaseExpr(ce)) => Expr::Case {
            has_else_non_null: ce
                .defresult
                .as_deref()
                .is_some_and(|d| is_non_null(&lower(d, scope))),
        },
        Some(NB::FuncCall(fc)) => {
            let Some(name) = funcname_last(fc) else {
                return Expr::Unknown;
            };
            let args = lower_args(&fc.args, scope);
            if name == "coalesce" {
                Expr::Coalesce(args)
            } else {
                Expr::Func {
                    kind: classify_func(name),
                    args,
                }
            }
        }
        Some(NB::ColumnRef(cr)) => lower_column_ref(cr, scope)
            .map(Expr::Column)
            .unwrap_or(Expr::Unknown),
        Some(NB::SubLink(sl)) => lower_sublink(sl, scope),
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
        SubLinkType::ExprSublink => sl
            .subselect
            .as_deref()
            .and_then(|sub| match sub.node.as_ref()? {
                NB::SelectStmt(s) if is_provably_one_row_select(s) => s.target_list.first(),
                _ => None,
            })
            .and_then(|t| match t.node.as_ref()? {
                NB::ResTarget(rt) => rt.val.as_deref(),
                _ => None,
            })
            .map(|n| lower(n, scope))
            .unwrap_or(Expr::Unknown),
        _ => Expr::Unknown,
    }
}

fn is_provably_one_row_select(s: &pg_query::protobuf::SelectStmt) -> bool {
    if !s.group_clause.is_empty() || s.target_list.is_empty() {
        return false;
    }
    s.target_list.iter().all(|t| {
        let Some(NB::FuncCall(fc)) = restarget_val(t).and_then(|v| v.node.as_ref()) else {
            return false;
        };
        funcname_last(fc).is_some_and(|n| !matches!(classify_func(n), FuncKind::Other))
    })
}

fn lower_args(args: &[Node], scope: &Scope) -> Vec<Expr> {
    args.iter().map(|a| lower(a, scope)).collect()
}

fn lower_column_ref(cr: &pg_query::protobuf::ColumnRef, scope: &Scope) -> Option<ResolvedCol> {
    let parts = string_parts(&cr.fields);
    let (alias, col) = match parts.as_slice() {
        [col] => {
            let r = scope.resolve_bare(col)?;
            return Some(ResolvedCol {
                table_ref: TableColRef {
                    schema: r.schema,
                    table: r.table,
                    column: col.clone(),
                },
                alias: r.alias,
                not_null: r.not_null,
                typoid: r.typoid,
            });
        }
        [alias, col] | [_, alias, col] => (alias.as_str(), col.as_str()),
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
        return Some(alias_only(alias, col, is_non_null(&child.expr)));
    }
    // Literal source (VALUES, literal unnest): plan walk marked the
    // alias non-null. No (schema, table) known.
    if scope.is_non_null_alias(alias) && !scope.is_nullable_alias(alias) {
        return Some(alias_only(alias, col, true));
    }
    None
}

/// `ResolvedCol` for a derived / literal-source alias where we know
/// the alias but not the originating `(schema, table)`.
fn alias_only(alias: &str, col: &str, not_null: bool) -> ResolvedCol {
    ResolvedCol {
        table_ref: TableColRef {
            schema: String::new(),
            table: alias.to_string(),
            column: col.to_string(),
        },
        alias: alias.to_string(),
        not_null,
        typoid: 0,
    }
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
    scope.typname_oid(string_parts(&tn.names).last()?)
}
